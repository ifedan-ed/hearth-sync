//! A small Hearth in memory, written from docs/sync-api.md alone, enough
//! for the core's own tests: one home mount on one storage, the feed with
//! sequence numbers and markers, listings, conditional writes, upload
//! sessions, the hash check, a trash and versions. The simulator's Hearth
//! (hearth-sync-sim) is the full one, with several mounts and every fault.
//!
//! It also plays "another device": the `client_*` methods change files the
//! way the web app or a second computer would, through the same commit
//! step, so the engine meets them in the feed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::ids::{Cursor, Etag, Hash, Kind, MountId, RemoteId};
use crate::names::{LocalName, remote_refusal};
use crate::remote::{
    ApiError, Change, ChangesPage, Details, Entry, EntryInfo, ErrorCode, FileCondition,
    FinishAnswer, GoneReason, HaveHash, Hello, Limits, MountInfo, Mounts, MoveCondition,
    MoveRequest, ReadAnswer, Remote, RemoteError, RemoteResult, TreePage, UploadSession,
    UploadStart, UploadState, UploadTarget, WaitAnswer,
};

#[derive(Clone, Debug)]
struct FEntry {
    id: u64,
    parent: Option<u64>,
    name: String,
    kind: Kind,
    content: Vec<u8>,
    hash: Option<Hash>,
    etag: u64,
    seq: u64,
    mtime_us: i64,
    exec: bool,
}

#[derive(Clone, Debug)]
struct Session {
    start: UploadStart,
    chunks: BTreeMap<u64, Vec<u8>>,
    finished: Option<Entry>,
}

/// One injected fault for the next request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The request never arrives: no effect, no answer.
    LostBefore,
    /// The request takes effect, the answer is lost.
    LostAfter,
    /// Answered with this refusal instead of being done.
    Refuse(u16, String),
}

/// The fake Hearth.
#[derive(Clone, Debug)]
pub struct FakeRemote {
    /// The instance id it answers with.
    pub instance: String,
    /// The storage's restore epoch.
    pub epoch: String,
    entries: BTreeMap<u64, FEntry>,
    markers: Vec<(u64, u64, GoneReason)>,
    /// Moves inside the mount, (seq, id): their markers are delivered when
    /// the entry is no longer in the mount at reading time (6.3, Q6).
    moves: Vec<(u64, u64)>,
    root: u64,
    next_id: u64,
    next_etag: u64,
    seq: u64,
    oldest_kept: u64,
    uploads: BTreeMap<String, Session>,
    /// Every content that was ever a version or went to the trash.
    pub kept: BTreeSet<Hash>,
    /// Faults for the next requests, in order.
    pub faults: VecDeque<Fault>,
    /// Every request, by name, in order (for quiescence checks).
    pub log: Vec<String>,
    /// The mount's rights bits.
    pub perms: u32,
    /// How many of the next reads return damaged bytes (a fault in the
    /// network or a proxy; I5 must keep them off the disk).
    pub corrupt_reads: u32,
    /// The limits it announces.
    pub limits: Limits,
}

fn api(status: u16, code: &str, details: Details) -> RemoteError {
    RemoteError::Api(Box::new(ApiError {
        status,
        code: ErrorCode::parse(code, status),
        message: code.to_owned(),
        details,
        retry_after_s: None,
    }))
}

impl Default for FakeRemote {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRemote {
    /// An empty home folder.
    pub fn new() -> FakeRemote {
        let mut entries = BTreeMap::new();
        entries.insert(
            100,
            FEntry {
                id: 100,
                parent: None,
                name: "home".into(),
                kind: Kind::Folder,
                content: Vec::new(),
                hash: None,
                etag: 0,
                seq: 1,
                mtime_us: 1,
                exec: false,
            },
        );
        FakeRemote {
            instance: "instance-1".into(),
            epoch: "e1".into(),
            entries,
            markers: Vec::new(),
            moves: Vec::new(),
            root: 100,
            next_id: 101,
            next_etag: 1,
            seq: 1,
            oldest_kept: 0,
            uploads: BTreeMap::new(),
            kept: BTreeSet::new(),
            faults: VecDeque::new(),
            log: Vec::new(),
            perms: 31,
            corrupt_reads: 0,
            limits: Limits {
                page_max: 1000,
                wait_max: 55,
                body_max: 50 << 20,
                chunk_min: 1 << 10,
                chunk_max: 50 << 20,
                chunks_max: 10_000,
                upload_ttl: 86_400,
                name_bytes_max: 250,
                hashes_max: 1000,
                gone_days: 90,
            },
        }
    }

    fn rid(id: u64) -> RemoteId {
        RemoteId(id.to_string())
    }

    fn parse_id(id: &RemoteId) -> Option<u64> {
        id.0.parse().ok()
    }

    fn alive(&self, id: u64) -> bool {
        self.entries.contains_key(&id)
    }

    fn entry_of(&self, e: &FEntry) -> Entry {
        Entry {
            id: Self::rid(e.id),
            parent: if e.id == self.root {
                None
            } else {
                e.parent.map(Self::rid)
            },
            name: e.name.clone(),
            kind: e.kind,
            size: (e.kind == Kind::File).then_some(e.content.len() as u64),
            mtime_us: e.mtime_us,
            etag: (e.kind == Kind::File).then(|| Etag(format!("t{}", e.etag))),
            hash: e.hash,
            exec: e.exec,
            perms: None,
            crypt: None,
            seq: e.seq,
            list: false,
        }
    }

    fn details_entry(&self, id: u64) -> Details {
        Details {
            entry: self.entries.get(&id).map(|e| self.entry_of(e)),
            ..Details::default()
        }
    }

    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn new_etag(&mut self) -> u64 {
        self.next_etag += 1;
        self.next_etag
    }

    /// Runs the fault queue for one request: Some(answer) to give instead.
    fn fault(&mut self, name: &str) -> Option<Result<(), RemoteError>> {
        self.log.push(name.to_owned());
        match self.faults.pop_front() {
            None => None,
            Some(Fault::LostBefore) => Some(Err(RemoteError::Lost("cut before".into()))),
            Some(Fault::LostAfter) => Some(Ok(())),
            Some(Fault::Refuse(status, code)) => Some(Err(api(status, &code, Details::default()))),
        }
    }

    /// Wraps a request that changes state: faults before or after it.
    fn guarded<T>(
        &mut self,
        name: &str,
        f: impl FnOnce(&mut Self) -> RemoteResult<T>,
    ) -> RemoteResult<T> {
        match self.fault(name) {
            None => f(self),
            Some(Err(e)) => Err(e),
            Some(Ok(())) => {
                let _ = f(self);
                Err(RemoteError::Lost("answer lost".into()))
            }
        }
    }

    fn child_named(&self, parent: u64, name: &str) -> Option<u64> {
        self.entries
            .values()
            .find(|e| e.parent == Some(parent) && e.name == name && e.id != self.root)
            .map(|e| e.id)
    }

    fn check_name(name: &str) -> RemoteResult<()> {
        if !unicode_normalization::is_nfc(name) {
            return Err(api(
                422,
                "name_refused",
                Details {
                    reason: Some("not_nfc".into()),
                    ..Details::default()
                },
            ));
        }
        if let Some(r) = remote_refusal(&LocalName(name.as_bytes().to_vec()), 250) {
            return Err(api(
                422,
                "name_refused",
                Details {
                    reason: Some(format!("{r:?}")),
                    ..Details::default()
                },
            ));
        }
        Ok(())
    }

    fn check_folder(&self, id: u64) -> RemoteResult<()> {
        match self.entries.get(&id) {
            None => Err(api(404, "not_found", Details::default())),
            Some(e) if e.kind != Kind::Folder => Err(api(409, "not_folder", Details::default())),
            Some(_) => Ok(()),
        }
    }

    fn check_file_condition(&self, id: u64, c: &FileCondition) -> RemoteResult<()> {
        let e = self
            .entries
            .get(&id)
            .ok_or_else(|| api(404, "not_found", Details::default()))?;
        let ok = match c {
            FileCondition::IfMatch(t) => t.0 == format!("t{}", e.etag),
            FileCondition::IfHash(h) => match e.hash {
                Some(x) => x == *h,
                None => return Err(api(409, "hash_unknown", self.details_entry(id))),
            },
        };
        if ok {
            Ok(())
        } else {
            let mut d = self.details_entry(id);
            d.failed = Some("ifMatch".into());
            Err(api(412, "precondition", d))
        }
    }

    fn create(
        &mut self,
        parent: u64,
        name: &str,
        kind: Kind,
        body: &[u8],
        mtime_us: i64,
        exec: bool,
    ) -> RemoteResult<Entry> {
        self.check_folder(parent)?;
        Self::check_name(name)?;
        if let Some(o) = self.child_named(parent, name) {
            return Err(api(409, "exists", self.details_entry(o)));
        }
        let id = self.next_id;
        self.next_id += 1;
        let seq = self.bump();
        let etag = self.new_etag();
        let e = FEntry {
            id,
            parent: Some(parent),
            name: name.to_owned(),
            kind,
            content: body.to_vec(),
            hash: (kind == Kind::File).then(|| Hash::of(body)),
            etag,
            seq,
            mtime_us,
            exec,
        };
        let out = self.entry_of(&e);
        self.entries.insert(id, e);
        Ok(out)
    }

    fn set_content(&mut self, id: u64, body: &[u8], mtime_us: i64) -> Entry {
        let h = Hash::of(body);
        let same = self
            .entries
            .get(&id)
            .is_some_and(|e| e.hash == Some(h) && e.mtime_us == mtime_us);
        if !same {
            let seq = self.bump();
            let etag = self.new_etag();
            if let Some(e) = self.entries.get_mut(&id) {
                if let Some(old) = e.hash
                    && old != h
                {
                    self.kept.insert(old);
                }
                e.content = body.to_vec();
                e.hash = Some(h);
                e.seq = seq;
                e.etag = etag;
                e.mtime_us = mtime_us;
            }
        }
        self.entry_of(&self.entries[&id])
    }

    fn below(&self, id: u64) -> Vec<u64> {
        let mut out = vec![id];
        let mut i = 0;
        while i < out.len() {
            let cur = out[i];
            out.extend(
                self.entries
                    .values()
                    .filter(|e| e.parent == Some(cur) && e.id != self.root)
                    .map(|e| e.id),
            );
            i += 1;
        }
        out
    }

    fn trash(&mut self, id: u64) {
        let seq = self.bump();
        for x in self.below(id) {
            if let Some(e) = self.entries.remove(&x)
                && let Some(h) = e.hash
            {
                self.kept.insert(h);
            }
        }
        self.markers.push((seq, id, GoneReason::Trashed));
    }

    fn cursor_seq(&self, c: &Cursor) -> RemoteResult<u64> {
        let Some((epoch, seq)) = c.0.strip_prefix('c').and_then(|r| r.split_once('.')) else {
            return Err(api(400, "cursor_invalid", Details::default()));
        };
        let seq: u64 = seq
            .parse()
            .map_err(|_| api(400, "cursor_invalid", Details::default()))?;
        if epoch != self.epoch {
            return Err(api(
                410,
                "cursor_expired",
                Details {
                    reason: Some("epoch".into()),
                    ..Details::default()
                },
            ));
        }
        if seq > self.seq {
            return Err(api(
                410,
                "cursor_expired",
                Details {
                    reason: Some("ahead".into()),
                    ..Details::default()
                },
            ));
        }
        if seq < self.oldest_kept {
            return Err(api(
                410,
                "cursor_expired",
                Details {
                    reason: Some("retention".into()),
                    ..Details::default()
                },
            ));
        }
        Ok(seq)
    }

    /// The mounts tag: changes when the rights or the epoch change (4).
    fn tag(&self) -> String {
        format!("m-{}-{}", self.perms, self.epoch)
    }

    fn cursor(&self, seq: u64) -> Cursor {
        Cursor(format!("c{}.{seq}", self.epoch))
    }

    // ----- what another device or the web app does -----

    /// The id at a path of names under home, if any.
    pub fn id_at(&self, path: &[&str]) -> Option<RemoteId> {
        let mut cur = self.root;
        for p in path {
            cur = self.child_named(cur, p)?;
        }
        Some(Self::rid(cur))
    }

    fn folder_at(&mut self, path: &[&str]) -> u64 {
        let mut cur = self.root;
        for p in path {
            cur = match self.child_named(cur, p) {
                Some(c) => c,
                None => match self.create(cur, p, Kind::Folder, &[], 1, false) {
                    Ok(e) => Self::parse_id(&e.id).unwrap_or(0),
                    Err(_) => return 0,
                },
            };
        }
        cur
    }

    /// Writes a file at a path (creating folders), as another device.
    pub fn client_write(&mut self, path: &[&str], body: &[u8]) {
        let (name, dirs) = path.split_last().expect("a path");
        let parent = self.folder_at(dirs);
        match self.child_named(parent, name) {
            Some(id) => {
                let t = self.seq as i64 * 1_000_000;
                self.set_content(id, body, t);
            }
            None => {
                let _ = self.create(parent, name, Kind::File, body, 1_000_000, false);
            }
        }
    }

    /// Makes folders down a path.
    pub fn client_mkdir(&mut self, path: &[&str]) {
        self.folder_at(path);
    }

    /// Moves or renames an entry by path.
    pub fn client_move(&mut self, from: &[&str], to: &[&str]) {
        let Some(id) = self.id_at(from).and_then(|r| Self::parse_id(&r)) else {
            return;
        };
        let (name, dirs) = to.split_last().expect("a path");
        let parent = self.folder_at(dirs);
        if self.child_named(parent, name).is_some() {
            return;
        }
        let seq = self.bump();
        let etag = self.new_etag();
        self.moves.push((seq, id));
        if let Some(e) = self.entries.get_mut(&id) {
            e.parent = Some(parent);
            e.name = (*name).to_owned();
            e.seq = seq;
            if e.kind == Kind::File {
                e.etag = etag;
            }
        }
    }

    /// Deletes an entry by path (to the trash).
    pub fn client_delete(&mut self, path: &[&str]) {
        if let Some(id) = self.id_at(path).and_then(|r| Self::parse_id(&r)) {
            self.trash(id);
        }
    }

    /// The bytes of the file at a path.
    pub fn read_path(&self, path: &[&str]) -> Option<Vec<u8>> {
        let id = self.id_at(path).and_then(|r| Self::parse_id(&r))?;
        let e = &self.entries[&id];
        (e.kind == Kind::File).then(|| e.content.clone())
    }

    /// Every current file, by path (`a/b/c`) and content.
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for e in self.entries.values() {
            if e.kind != Kind::File {
                continue;
            }
            let mut parts = vec![e.name.clone()];
            let mut cur = e.parent;
            while let Some(p) = cur {
                if p == self.root {
                    break;
                }
                let pe = &self.entries[&p];
                parts.push(pe.name.clone());
                cur = pe.parent;
            }
            parts.reverse();
            out.insert(parts.join("/"), e.content.clone());
        }
        out
    }

    /// Every content Hearth holds: files, versions and trash (I1).
    pub fn all_hashes(&self) -> BTreeSet<Hash> {
        let mut s = self.kept.clone();
        s.extend(self.entries.values().filter_map(|e| e.hash));
        s
    }

    /// Restores the database from a backup in the sense of 6.8: a new
    /// epoch (every cursor is then expired with reason `epoch`).
    pub fn new_epoch(&mut self, epoch: &str) {
        self.epoch = epoch.into();
    }

    /// Drops markers older than now, so older cursors expire (`retention`).
    pub fn expire_markers(&mut self) {
        self.oldest_kept = self.seq;
        self.markers.clear();
        self.moves.clear();
    }

    /// Requests that change something or ask for something other than
    /// the wait, since index `from` of the log.
    pub fn busy_requests_since(&self, from: usize) -> Vec<String> {
        self.log[from..]
            .iter()
            .filter(|r| r.as_str() != "wait")
            .cloned()
            .collect()
    }
}

impl Remote for FakeRemote {
    fn hello(&mut self) -> RemoteResult<Hello> {
        if let Some(Err(e)) = self.fault("hello") {
            return Err(e);
        }
        Ok(Hello {
            instance_id: self.instance.clone(),
            api_min: 1,
            api_max: 1,
            features: ["feed", "wait", "uploads", "hashes", "copy", "exec"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            limits: self.limits.clone(),
        })
    }

    fn mounts(&mut self) -> RemoteResult<Mounts> {
        if let Some(Err(e)) = self.fault("mounts") {
            return Err(e);
        }
        Ok(Mounts {
            tag: self.tag(),
            mounts: vec![MountInfo {
                id: MountId("home".into()),
                kind: "home".into(),
                storage: "home:1".into(),
                epoch: self.epoch.clone(),
                root: Self::rid(self.root),
                root_type: "folder".into(),
                path: String::new(),
                perms: self.perms,
                trash: true,
                versions: true,
                prompt: "always".into(),
            }],
        })
    }

    fn changes(
        &mut self,
        _mount: &MountId,
        cursor: &Cursor,
        limit: u32,
    ) -> RemoteResult<ChangesPage> {
        if let Some(Err(e)) = self.fault("changes") {
            return Err(e);
        }
        let from = self.cursor_seq(cursor)?;
        let mut items: Vec<(u64, u8, Change)> = Vec::new();
        for (seq, id, reason) in &self.markers {
            if *seq > from {
                items.push((
                    *seq,
                    0,
                    Change::Gone {
                        id: Self::rid(*id),
                        seq: *seq,
                        reason: *reason,
                    },
                ));
            }
        }
        for e in self.entries.values() {
            if e.seq > from {
                items.push((e.seq, 1, Change::Upsert(self.entry_of(e))));
            }
        }
        // 6.3, decided at reading time: a move inside the mount whose entry
        // has left the mount since (with a folder it was moved into).
        for (seq, id) in &self.moves {
            if *seq > from && !self.entries.contains_key(id) {
                items.push((
                    *seq,
                    0,
                    Change::Gone {
                        id: Self::rid(*id),
                        seq: *seq,
                        reason: GoneReason::Moved,
                    },
                ));
            }
        }
        items.sort_by_key(|a| (a.0, a.1));
        let limit = limit.max(1) as usize;
        let mut out = Vec::new();
        let mut last = from;
        let mut more = false;
        for (i, (seq, _, c)) in items.iter().enumerate() {
            if out.len() >= limit && *seq != last {
                more = i < items.len();
                break;
            }
            out.push(c.clone());
            last = *seq;
        }
        let cursor_seq = if more { last } else { self.seq };
        Ok(ChangesPage {
            epoch: self.epoch.clone(),
            changes: out,
            cursor: self.cursor(cursor_seq),
            more,
            mounts_tag: self.tag(),
        })
    }

    fn tree(
        &mut self,
        _mount: &MountId,
        root: Option<&RemoteId>,
        page: Option<&str>,
        limit: u32,
    ) -> RemoteResult<TreePage> {
        if let Some(Err(e)) = self.fault("tree") {
            return Err(e);
        }
        let top = match root {
            Some(r) => {
                Self::parse_id(r).ok_or_else(|| api(404, "not_found", Details::default()))?
            }
            None => self.root,
        };
        if !self.alive(top) {
            return Err(api(404, "not_found", Details::default()));
        }
        let (offset, cur) = match page {
            Some(p) => {
                let (o, c) = p
                    .strip_prefix('p')
                    .and_then(|x| x.split_once(':'))
                    .ok_or_else(|| api(400, "page_invalid", Details::default()))?;
                (
                    o.parse::<usize>().unwrap_or(0),
                    c.parse::<u64>().unwrap_or(0),
                )
            }
            None => (0, self.seq),
        };
        let mut ids = self.below(top);
        ids.sort_unstable();
        let limit = limit.max(1) as usize;
        let slice: Vec<Entry> = ids
            .iter()
            .skip(offset)
            .take(limit)
            .map(|i| self.entry_of(&self.entries[i]))
            .collect();
        let next = (offset + limit < ids.len()).then(|| format!("p{}:{cur}", offset + limit));
        let root_size = ids
            .iter()
            .map(|i| self.entries[i].content.len() as u64)
            .sum();
        Ok(TreePage {
            epoch: self.epoch.clone(),
            root_size: Some(root_size),
            entries: slice,
            page: next,
            cursor: self.cursor(cur),
            mounts_tag: self.tag(),
        })
    }

    fn entry(&mut self, id: &RemoteId) -> RemoteResult<EntryInfo> {
        if let Some(Err(e)) = self.fault("entry") {
            return Err(e);
        }
        let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
        let e = self
            .entries
            .get(&i)
            .ok_or_else(|| api(404, "not_found", Details::default()))?;
        Ok(EntryInfo {
            mount: MountId("home".into()),
            entry: self.entry_of(e),
            lock: None,
        })
    }

    fn wait(
        &mut self,
        cursors: &[(MountId, Cursor)],
        tag: &str,
        _wait_s: u32,
    ) -> RemoteResult<WaitAnswer> {
        if let Some(Err(e)) = self.fault("wait") {
            return Err(e);
        }
        let mut changed = Vec::new();
        for (m, c) in cursors {
            match self.cursor_seq(c) {
                Ok(s) if s >= self.seq => {}
                _ => changed.push(m.clone()),
            }
        }
        Ok(WaitAnswer {
            changed,
            mounts_changed: tag != self.tag(),
            mounts_tag: self.tag(),
        })
    }

    fn read(
        &mut self,
        id: &RemoteId,
        etag: &Etag,
        offset: u64,
        len: u64,
    ) -> RemoteResult<ReadAnswer> {
        if let Some(Err(e)) = self.fault("read") {
            return Err(e);
        }
        let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
        self.check_file_condition(i, &FileCondition::IfMatch(etag.clone()))?;
        let e = &self.entries[&i];
        let total = e.content.len() as u64;
        if offset > 0 && offset >= total {
            return Err(api(416, "range", Details::default()));
        }
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(e.content.len());
        let end = start
            .saturating_add(usize::try_from(len).unwrap_or(usize::MAX))
            .min(e.content.len());
        let mut bytes = e.content[start..end].to_vec();
        let hash = e.hash;
        if self.corrupt_reads > 0 {
            self.corrupt_reads -= 1;
            if let Some(b) = bytes.first_mut() {
                *b ^= 0x20;
            }
        }
        Ok(ReadAnswer { bytes, total, hash })
    }

    fn create_file(
        &mut self,
        parent: &RemoteId,
        name: &str,
        body: &[u8],
        hash: Hash,
        mtime_us: i64,
        exec: bool,
    ) -> RemoteResult<Entry> {
        let perms = self.perms;
        self.guarded("create_file", |s| {
            if perms & 4 == 0 {
                return Err(api(403, "no_permission", Details::default()));
            }
            if Hash::of(body) != hash {
                return Err(api(422, "hash_mismatch", Details::default()));
            }
            let p =
                Self::parse_id(parent).ok_or_else(|| api(404, "not_found", Details::default()))?;
            s.create(p, name, Kind::File, body, mtime_us, exec)
        })
    }

    fn replace_content(
        &mut self,
        id: &RemoteId,
        condition: &FileCondition,
        body: &[u8],
        hash: Hash,
        mtime_us: i64,
    ) -> RemoteResult<Entry> {
        let perms = self.perms;
        self.guarded("replace_content", |s| {
            let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
            s.check_file_condition(i, condition)?;
            if perms & 2 == 0 {
                return Err(api(403, "no_permission", Details::default()));
            }
            if Hash::of(body) != hash {
                return Err(api(422, "hash_mismatch", Details::default()));
            }
            Ok(s.set_content(i, body, mtime_us))
        })
    }

    fn make_folder(&mut self, parent: &RemoteId, name: &str) -> RemoteResult<Entry> {
        let perms = self.perms;
        self.guarded("make_folder", |s| {
            if perms & 4 == 0 {
                return Err(api(403, "no_permission", Details::default()));
            }
            let p =
                Self::parse_id(parent).ok_or_else(|| api(404, "not_found", Details::default()))?;
            s.create(p, name, Kind::Folder, &[], 1, false)
        })
    }

    fn move_entry(&mut self, id: &RemoteId, req: &MoveRequest) -> RemoteResult<Entry> {
        let perms = self.perms;
        self.guarded("move", |s| {
            let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
            if i == s.root {
                return Err(api(403, "mount_root", Details::default()));
            }
            let e = s
                .entries
                .get(&i)
                .cloned()
                .ok_or_else(|| api(404, "not_found", Details::default()))?;
            let p = Self::parse_id(&req.parent)
                .ok_or_else(|| api(404, "not_found", Details::default()))?;
            if e.parent == Some(p) && e.name == req.name {
                return Ok(s.entry_of(&e));
            }
            match &req.condition {
                MoveCondition::File(c) => s.check_file_condition(i, c)?,
                MoveCondition::Folder(seq) => {
                    if *seq != e.seq {
                        let mut d = s.details_entry(i);
                        d.failed = Some("ifSeq".into());
                        return Err(api(412, "precondition", d));
                    }
                }
            }
            if perms & 12 != 12 {
                return Err(api(403, "no_permission", Details::default()));
            }
            s.check_folder(p)?;
            Self::check_name(&req.name)?;
            if s.below(i).contains(&p) {
                return Err(api(409, "into_itself", Details::default()));
            }
            if let Some(o) = s.child_named(p, &req.name) {
                return Err(api(409, "exists", s.details_entry(o)));
            }
            let seq = s.bump();
            let etag = s.new_etag();
            s.moves.push((seq, i));
            let x = s
                .entries
                .get_mut(&i)
                .ok_or_else(|| api(404, "not_found", Details::default()))?;
            x.parent = Some(p);
            x.name = req.name.clone();
            x.seq = seq;
            if x.kind == Kind::File {
                x.etag = etag;
            }
            let out = x.clone();
            Ok(s.entry_of(&out))
        })
    }

    fn copy(
        &mut self,
        id: &RemoteId,
        parent: &RemoteId,
        name: &str,
        if_hash: Hash,
    ) -> RemoteResult<Entry> {
        self.guarded("copy", |s| {
            let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
            s.check_file_condition(i, &FileCondition::IfHash(if_hash))?;
            let src = s.entries[&i].clone();
            let p =
                Self::parse_id(parent).ok_or_else(|| api(404, "not_found", Details::default()))?;
            s.create(p, name, Kind::File, &src.content, src.mtime_us, src.exec)
        })
    }

    fn delete_file(
        &mut self,
        id: &RemoteId,
        condition: &FileCondition,
    ) -> RemoteResult<GoneReason> {
        let perms = self.perms;
        self.guarded("delete_file", |s| {
            let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
            s.check_file_condition(i, condition)?;
            if perms & 8 == 0 {
                return Err(api(403, "no_permission", Details::default()));
            }
            s.trash(i);
            Ok(GoneReason::Trashed)
        })
    }

    fn delete_folder(
        &mut self,
        id: &RemoteId,
        unchanged_since: &Cursor,
    ) -> RemoteResult<GoneReason> {
        let perms = self.perms;
        self.guarded("delete_folder", |s| {
            let i = Self::parse_id(id).ok_or_else(|| api(404, "not_found", Details::default()))?;
            if !s.alive(i) {
                return Err(api(404, "not_found", Details::default()));
            }
            if perms & 8 == 0 {
                return Err(api(403, "no_permission", Details::default()));
            }
            let since = s.cursor_seq(unchanged_since).unwrap_or_default();
            let changed = s.below(i).iter().any(|x| s.entries[x].seq > since);
            if changed {
                let mut d = s.details_entry(i);
                d.failed = Some("unchangedSince".into());
                return Err(api(412, "precondition", d));
            }
            s.trash(i);
            Ok(GoneReason::Trashed)
        })
    }

    fn upload_start(&mut self, start: &UploadStart) -> RemoteResult<UploadSession> {
        self.guarded("upload_start", |s| {
            if let Some(x) = s.uploads.get(&start.id) {
                if x.start != *start {
                    return Err(api(409, "upload_mismatch", Details::default()));
                }
                return Ok(UploadSession {
                    id: start.id.clone(),
                    state: UploadState::Open,
                    chunks: x.chunks.keys().copied().collect(),
                });
            }
            match &start.target {
                UploadTarget::Create { parent, name } => {
                    let p = Self::parse_id(parent)
                        .ok_or_else(|| api(404, "not_found", Details::default()))?;
                    s.check_folder(p)?;
                    if let Some(o) = s.child_named(p, name) {
                        return Err(api(409, "exists", s.details_entry(o)));
                    }
                }
                UploadTarget::Replace { file, condition } => {
                    let i = Self::parse_id(file)
                        .ok_or_else(|| api(404, "not_found", Details::default()))?;
                    s.check_file_condition(i, condition)?;
                }
            }
            s.uploads.insert(
                start.id.clone(),
                Session {
                    start: start.clone(),
                    chunks: BTreeMap::new(),
                    finished: None,
                },
            );
            Ok(UploadSession {
                id: start.id.clone(),
                state: UploadState::Open,
                chunks: Vec::new(),
            })
        })
    }

    fn upload_chunk(&mut self, id: &str, n: u64, body: &[u8], hash: Hash) -> RemoteResult<()> {
        self.guarded("upload_chunk", |s| {
            let sess = s
                .uploads
                .get_mut(id)
                .ok_or_else(|| api(404, "upload_gone", Details::default()))?;
            let cs = sess.start.chunk_size;
            let count = sess.start.size.div_ceil(cs);
            if n < 1 || n > count {
                return Err(api(422, "chunk_range", Details::default()));
            }
            let expect = if n == count {
                sess.start.size - (n - 1) * cs
            } else {
                cs
            };
            if body.len() as u64 != expect {
                return Err(api(422, "chunk_size", Details::default()));
            }
            if Hash::of(body) != hash {
                return Err(api(422, "hash_mismatch", Details::default()));
            }
            sess.chunks.insert(n, body.to_vec());
            Ok(())
        })
    }

    fn upload_status(&mut self, id: &str) -> RemoteResult<UploadSession> {
        if let Some(Err(e)) = self.fault("upload_status") {
            return Err(e);
        }
        let sess = self
            .uploads
            .get(id)
            .ok_or_else(|| api(404, "upload_gone", Details::default()))?;
        Ok(UploadSession {
            id: id.to_owned(),
            state: match &sess.finished {
                Some(e) => UploadState::Finished(e.clone()),
                None => UploadState::Open,
            },
            chunks: sess.chunks.keys().copied().collect(),
        })
    }

    fn upload_finish(&mut self, id: &str) -> RemoteResult<FinishAnswer> {
        let perms = self.perms;
        self.guarded("upload_finish", |s| {
            let sess = s
                .uploads
                .get(id)
                .cloned()
                .ok_or_else(|| api(404, "upload_gone", Details::default()))?;
            if let Some(e) = sess.finished {
                return Ok(FinishAnswer::Done(e));
            }
            let count = sess.start.size.div_ceil(sess.start.chunk_size);
            let missing: Vec<u64> = (1..=count)
                .filter(|n| !sess.chunks.contains_key(n))
                .collect();
            if !missing.is_empty() {
                return Err(api(
                    409,
                    "upload_incomplete",
                    Details {
                        missing,
                        ..Details::default()
                    },
                ));
            }
            let body: Vec<u8> = sess.chunks.values().flatten().copied().collect();
            if Hash::of(&body) != sess.start.hash {
                return Err(api(422, "hash_mismatch", Details::default()));
            }
            let entry = match &sess.start.target {
                UploadTarget::Create { parent, name } => {
                    if perms & 4 == 0 {
                        return Err(api(403, "no_permission", Details::default()));
                    }
                    let p = Self::parse_id(parent)
                        .ok_or_else(|| api(404, "not_found", Details::default()))?;
                    s.create(
                        p,
                        name,
                        Kind::File,
                        &body,
                        sess.start.mtime_us,
                        sess.start.exec,
                    )?
                }
                UploadTarget::Replace { file, condition } => {
                    let i = Self::parse_id(file)
                        .ok_or_else(|| api(404, "not_found", Details::default()))?;
                    s.check_file_condition(i, condition)?;
                    s.set_content(i, &body, sess.start.mtime_us)
                }
            };
            if let Some(x) = s.uploads.get_mut(id) {
                x.finished = Some(entry.clone());
            }
            Ok(FinishAnswer::Done(entry))
        })
    }

    fn upload_abort(&mut self, id: &str) -> RemoteResult<()> {
        self.guarded("upload_abort", |s| {
            s.uploads.remove(id);
            Ok(())
        })
    }

    fn hashes_have(&mut self, _mount: &MountId, hashes: &[Hash]) -> RemoteResult<Vec<HaveHash>> {
        if let Some(Err(e)) = self.fault("hashes_have") {
            return Err(e);
        }
        let mut out = Vec::new();
        for h in hashes {
            if let Some(e) = self.entries.values().find(|e| e.hash == Some(*h)) {
                out.push(HaveHash {
                    hash: *h,
                    id: Self::rid(e.id),
                    size: e.content.len() as u64,
                });
            }
        }
        Ok(out)
    }
}
