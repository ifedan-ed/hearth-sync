//! Hearth's side: the mounts list, whole-mount listings, folder listings,
//! feed pages and the wait (sync-api.md 4 and 6), kept as the remote
//! picture of every node.
//!
//! The client's half of the feed is contract 6.6, followed rule by rule:
//! changes applied in order (1); an upsert replaces only with a higher
//! `seq` (2); an unknown id is inserted, its parent possibly arriving later
//! (3); an id held in another mount moves there with no transfer (4); a
//! `gone` removes the entry and what is below it, or marks it departed for
//! a move (5); stale markers are ignored (6); planning waits until every
//! followed mount has answered `more: false` and departed entries are
//! resolved by a round over the other mounts (7).

use super::overlay::Overlay;
use super::{Engine, EngineResult, PauseReason};
use crate::env::Env;
use crate::failure::{Failure, StopReason, backoff_ms, classify_remote};
use crate::ids::{Cursor, Kind, MountId, NodeId, Perms, RemoteId};
use crate::remote::{Change, Entry, ErrorCode, GoneReason, RemoteError};
use crate::store::{MetaKey, Store, Write};
use crate::tree::{
    Crypt, ListReason, Listing, MountKind, MountRow, ProblemKind, RemoteRow, Subject,
};

impl<S: Store> Engine<S> {
    /// One request to Hearth if one is due. False when none is.
    pub(super) fn remote_work(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        if self.hello.is_none() {
            return Ok(false);
        }
        if self.mounts_stale {
            self.refresh_mounts(env)?;
            return Ok(true);
        }
        let mounts = self.store.mounts()?;
        for m in &mounts {
            if m.selected && m.listing.is_some() {
                self.listing_page(env, m.clone())?;
                return Ok(true);
            }
        }
        if let Some(((mount, folder), page)) = self
            .folder_lists
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
        {
            self.folder_list_page(env, &mount, &folder, page)?;
            return Ok(true);
        }
        for m in &mounts {
            if m.selected && m.cursor.is_some() && (self.wake.contains(&m.id) || !m.caught_up) {
                self.changes_page(env, m.clone())?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A request failed: stop the root, or back off before the next one.
    /// Returns true when the engine is now paused.
    pub(super) fn remote_unreachable(
        &mut self,
        env: &mut Env<'_>,
        e: &RemoteError,
    ) -> EngineResult<bool> {
        match classify_remote(e) {
            Failure::Stop(r) => {
                self.stop(r);
                Ok(true)
            }
            Failure::Retry { after_s } => {
                self.back_off(env, after_s);
                Ok(false)
            }
            // A feed request refused for another reason: read the mounts
            // again (a share may be gone), after a pause.
            Failure::Replan | Failure::WaitForChange(..) => {
                self.mounts_stale = true;
                self.back_off(env, None);
                Ok(false)
            }
        }
    }

    fn back_off(&mut self, env: &mut Env<'_>, after_s: Option<u64>) {
        self.remote_attempt = self.remote_attempt.saturating_add(1);
        let ms = backoff_ms(
            self.remote_attempt,
            self.cfg.backoff_base_ms,
            self.cfg.backoff_cap_ms,
            env.random.next_u64(),
            after_s,
        );
        let ms = i64::try_from(ms).unwrap_or(i64::MAX / 2_000_000);
        self.remote_retry_at = Some(env.clock.now_ns().saturating_add(ms * 1_000_000));
    }

    fn remote_ok(&mut self) {
        self.remote_attempt = 0;
        self.remote_retry_at = None;
    }

    /// Stops the root for a reason the person must act on.
    pub(super) fn stop(&mut self, r: StopReason) {
        self.pause = Some(match r {
            StopReason::SignedOut(m) => PauseReason::SignedOut(m),
            StopReason::ClientRefused(m) => PauseReason::ClientRefused(m),
            StopReason::ApiVersion => PauseReason::ApiVersion,
            StopReason::InstanceChanged(now) => PauseReason::InstanceChanged {
                was: self
                    .hello
                    .as_ref()
                    .map(|h| h.instance_id.clone())
                    .unwrap_or_default(),
                now,
            },
            StopReason::RootUnavailable => PauseReason::RootUnavailable,
            StopReason::DiskFull => PauseReason::DiskFull,
        });
    }

    pub(super) fn wake_all_mounts(&mut self) {
        if let Ok(ms) = self.store.mounts() {
            for m in ms {
                self.wake.insert(m.id);
            }
        }
    }

    fn page_limit(&self) -> u32 {
        self.hello
            .as_ref()
            .map_or(1000, |h| h.limits.page_max.max(1))
    }

    fn new_generation(&mut self) -> u64 {
        let g = self.ids.generation;
        self.ids.generation += 1;
        self.ids.changed = true;
        g
    }

    /// `GET /sync/mounts`, and the mount rows brought in line with it.
    fn refresh_mounts(&mut self, env: &mut Env<'_>) -> EngineResult<()> {
        let ans = match env.remote.mounts() {
            Ok(a) => a,
            Err(e) => {
                self.remote_unreachable(env, &e)?;
                return Ok(());
            }
        };
        self.remote_ok();
        let now = env.clock.now_ns();
        let held: Vec<MountRow> = self.store.mounts()?;
        let mut batch = vec![Write::PutMeta(MetaKey::MountsTag, ans.tag.clone())];
        let Some(home) = ans.mounts.iter().find(|m| m.path.is_empty()) else {
            self.pause = Some(PauseReason::NoHomeMount);
            return Ok(());
        };
        let mut changed_storages = std::collections::BTreeSet::new();
        let mut infos = ans.mounts.clone();
        infos.sort_by(|a, b| a.id.cmp(&b.id));
        for info in &infos {
            let Some(kind) = MountKind::parse(&info.kind) else {
                continue;
            };
            let path: Vec<String> = info
                .path
                .split('/')
                .filter(|p| !p.is_empty())
                .map(crate::names::to_nfc)
                .collect();
            let old = held.iter().find(|m| m.id == info.id).cloned();
            let mut row = match old.clone() {
                Some(m) => m,
                None => {
                    changed_storages.insert(info.storage.clone());
                    let root_node = if info.id == home.id {
                        match self.store.meta(MetaKey::RootNode)? {
                            Some(v) => NodeId(v.parse().unwrap_or(0)),
                            None => {
                                let n = self.alloc_node();
                                batch.push(Write::PutMeta(MetaKey::RootNode, n.0.to_string()));
                                n
                            }
                        }
                    } else {
                        match self.store.node_by_remote(&info.root)? {
                            Some(n) => n,
                            None => self.alloc_node(),
                        }
                    };
                    let generation = self.new_generation();
                    MountRow {
                        id: info.id.clone(),
                        kind,
                        storage: info.storage.clone(),
                        epoch: info.epoch.clone(),
                        root_remote: info.root.clone(),
                        root_node,
                        local_path: path.clone(),
                        perms: Perms(info.perms),
                        trash: info.trash,
                        versions: info.versions,
                        prompt_ask: info.prompt == "ask",
                        selected: info.prompt != "ask",
                        cursor: None,
                        listing: Some(Listing {
                            reason: ListReason::First,
                            page: None,
                            cursor: None,
                            generation,
                        }),
                        caught_up: false,
                        caught_up_round: 0,
                    }
                }
            };
            if row.cursor.is_none() && row.listing.is_none() {
                // A mount that came back after it left (R6): its folder is
                // synced with it again as a root without history (R5), by
                // content. Or one that never finished its first listing.
                let came_back = self.store.unselected()?.contains(&row.root_node);
                if came_back {
                    row.selected = info.prompt != "ask";
                    batch.push(Write::Reselect(row.root_node));
                }
                let generation = self.new_generation();
                row.listing = Some(Listing {
                    reason: if came_back {
                        ListReason::Untrusted
                    } else {
                        ListReason::Retention
                    },
                    page: None,
                    cursor: None,
                    generation,
                });
            }
            if old.is_some() && row.epoch != info.epoch {
                // Hearth was restored: nothing it lacks is a deletion.
                let generation = self.new_generation();
                row.listing = Some(Listing {
                    reason: ListReason::Untrusted,
                    page: None,
                    cursor: None,
                    generation,
                });
            }
            let rights_changed = row.perms.0 != info.perms;
            let path_changed = row.local_path != path;
            row.kind = kind;
            row.storage = info.storage.clone();
            row.epoch = info.epoch.clone();
            row.local_path = path;
            row.perms = Perms(info.perms);
            row.trash = info.trash;
            row.versions = info.versions;
            row.prompt_ask = info.prompt == "ask";
            batch.push(Write::PutNode(row.root_node, row.id.clone()));
            if rights_changed || path_changed {
                // Items listed as read-only, or a mount the person moved in
                // the web app, are planned again.
                for n in self.store.mount_nodes(&row.id)? {
                    batch.push(Write::MarkDirty(n));
                }
            }
            if info.id == home.id {
                self.root = Some(row.root_node);
            }
            batch.push(Write::DelProblem(Subject::Node(row.root_node)));
            batch.push(Write::PutMount(row));
        }
        // Mounts that are gone: the person left a share, or it was
        // withdrawn. Its local folder is left alone and listed; nothing is
        // deleted on either side (the conservative reading; see
        // docs/contract-questions.md).
        for m in &held {
            if infos.iter().any(|i| i.id == m.id) || !m.selected && m.cursor.is_none() {
                continue;
            }
            changed_storages.insert(m.storage.clone());
            let mut gone = m.clone();
            gone.selected = false;
            gone.cursor = None;
            gone.listing = None;
            for n in self.store.mount_nodes(&m.id)? {
                batch.push(Write::DelRemote(n));
                batch.push(Write::DelSynced(n));
                batch.push(Write::MarkDirty(n));
            }
            batch.push(self.problem_write(
                Subject::Node(m.root_node),
                ProblemKind::MountGone,
                "no_longer_shared: the local folder is kept as it is and not synced".into(),
                now,
            )?);
            batch.push(Write::Unselect(m.root_node));
            batch.push(Write::PutMount(gone));
        }
        // Entries may have passed between mounts of a storage that gained
        // or lost a mount: list those mounts again (sync-api.md 4).
        for m in &held {
            if infos.iter().any(|i| i.id == m.id)
                && changed_storages.contains(&m.storage)
                && m.cursor.is_some()
                && m.listing.is_none()
            {
                let mut again = m.clone();
                if let Some(Write::PutMount(r)) = batch
                    .iter()
                    .rev()
                    .find(|w| matches!(w, Write::PutMount(r) if r.id == m.id))
                {
                    again = r.clone();
                }
                if again.listing.is_none() {
                    let generation = self.new_generation();
                    again.listing = Some(Listing {
                        reason: ListReason::Relist,
                        page: None,
                        cursor: None,
                        generation,
                    });
                    batch.push(Write::PutMount(again));
                }
            }
        }
        self.commit(batch)?;
        self.mounts_stale = false;
        self.wake_all_mounts();
        Ok(())
    }

    /// One page of a whole-mount listing (sync-api.md 6.4).
    fn listing_page(&mut self, env: &mut Env<'_>, m: MountRow) -> EngineResult<()> {
        let Some(mut listing) = m.listing.clone() else {
            return Ok(());
        };
        let limit = self.page_limit();
        let page = match env.remote.tree(&m.id, None, listing.page.as_deref(), limit) {
            Ok(p) => p,
            Err(e) => {
                if matches!(e.code(), Some(ErrorCode::PageInvalid)) {
                    // The page went stale: start the listing again.
                    let generation = self.new_generation();
                    let mut m2 = m.clone();
                    m2.listing = Some(Listing {
                        page: None,
                        cursor: None,
                        generation,
                        ..listing
                    });
                    self.commit(vec![Write::PutMount(m2)])?;
                    return Ok(());
                }
                if matches!(e.code(), Some(ErrorCode::NotFound)) {
                    self.mounts_stale = true;
                    return Ok(());
                }
                self.remote_unreachable(env, &e)?;
                return Ok(());
            }
        };
        self.remote_ok();
        let mut m = m;
        if page.mounts_tag != self.store.meta(MetaKey::MountsTag)?.unwrap_or_default() {
            self.mounts_stale = true;
        }
        let mut ov = Overlay::new(&self.store);
        let mut list_folders = Vec::new();
        for e in &page.entries {
            Self::upsert(
                &mut self.ids,
                &mut ov,
                &m,
                e,
                Some(listing.generation),
                &mut list_folders,
            )?;
        }
        if listing.cursor.is_none() {
            listing.cursor = Some(page.cursor.clone());
        }
        let mut writes = std::mem::take(&mut ov.writes);
        drop(ov);
        match page.page {
            Some(next) => {
                listing.page = Some(next);
                m.listing = Some(listing);
                writes.push(Write::PutMount(m));
                self.commit(writes)?;
            }
            None => {
                // The last page's rows first: what the listing did not
                // deliver can only be judged once every delivered row is
                // marked. A crash between the two commits reads the last
                // page again, which is harmless (F2).
                if listing.cursor != m.listing.as_ref().and_then(|l| l.cursor.clone()) {
                    let mut first = m.clone();
                    first.listing = Some(listing.clone());
                    writes.push(Write::PutMount(first));
                }
                self.commit(writes)?;
                let mut done = self.finish_listing(&m, &listing)?;
                m.cursor = listing.cursor.clone();
                m.listing = None;
                m.caught_up = false;
                m.epoch = page.epoch.clone();
                self.wake.insert(m.id.clone());
                done.push(Write::PutMount(m));
                self.commit(done)?;
            }
        }
        Ok(())
    }

    /// What a finished listing means for entries it did not deliver
    /// (sync-api.md 6.8's table), and, for an untrusted history, the base
    /// forgotten so everything is compared again by content.
    fn finish_listing(&mut self, m: &MountRow, listing: &Listing) -> EngineResult<Vec<Write>> {
        let mut w = Vec::new();
        let round = self.ids.round;
        for n in self.store.mount_nodes(&m.id)? {
            let Some(rec) = self.store.node(n)? else {
                continue;
            };
            if let Some(r) = &rec.remote
                && r.mount == m.id
                && r.listed != listing.generation
            {
                match listing.reason {
                    ListReason::Relist => {
                        let mut r2 = r.clone();
                        r2.departed = Some(round);
                        w.push(Write::PutRemote(r2));
                    }
                    ListReason::First | ListReason::Retention | ListReason::Untrusted => {
                        w.push(Write::DelRemote(n));
                    }
                }
                w.push(Write::MarkDirty(n));
            }
            if listing.reason == ListReason::Untrusted && rec.synced.is_some() {
                w.push(Write::DelSynced(n));
                w.push(Write::MarkDirty(n));
            }
        }
        if listing.reason == ListReason::Relist {
            self.wake_all_mounts();
        }
        Ok(w)
    }

    /// One page of a folder's listing, for a folder that came into view
    /// with `list: true` (sync-api.md 6.3, 6.4). The mount's own cursor is
    /// kept.
    fn folder_list_page(
        &mut self,
        env: &mut Env<'_>,
        mount: &MountId,
        folder: &RemoteId,
        page: Option<String>,
    ) -> EngineResult<()> {
        let key = (mount.clone(), folder.clone());
        let Some(m) = self.store.mounts()?.into_iter().find(|m| &m.id == mount) else {
            self.folder_lists.remove(&key);
            return Ok(());
        };
        let limit = self.page_limit();
        let ans = match env.remote.tree(mount, Some(folder), page.as_deref(), limit) {
            Ok(a) => a,
            Err(e) => {
                match e.code() {
                    Some(ErrorCode::NotFound | ErrorCode::NotFolder) => {
                        self.folder_lists.remove(&key);
                    }
                    Some(ErrorCode::PageInvalid) => {
                        self.folder_lists.insert(key, None);
                    }
                    _ => {
                        self.remote_unreachable(env, &e)?;
                    }
                }
                return Ok(());
            }
        };
        self.remote_ok();
        let mut ov = Overlay::new(&self.store);
        let mut list_folders = Vec::new();
        for e in &ans.entries {
            Self::upsert(&mut self.ids, &mut ov, &m, e, None, &mut list_folders)?;
        }
        let writes = std::mem::take(&mut ov.writes);
        drop(ov);
        self.commit(writes)?;
        match ans.page {
            Some(p) => {
                self.folder_lists.insert(key, Some(p));
            }
            None => {
                self.folder_lists.remove(&key);
            }
        }
        for f in list_folders {
            self.folder_lists.entry((mount.clone(), f)).or_insert(None);
        }
        Ok(())
    }

    /// One page of the feed (sync-api.md 6.3).
    fn changes_page(&mut self, env: &mut Env<'_>, m: MountRow) -> EngineResult<()> {
        let Some(cursor) = m.cursor.clone() else {
            return Ok(());
        };
        self.ids.round += 1;
        self.ids.changed = true;
        let round = self.ids.round;
        let limit = self.page_limit();
        let page = match env.remote.changes(&m.id, &cursor, limit) {
            Ok(p) => p,
            Err(RemoteError::Api(a)) if a.code == ErrorCode::CursorExpired => {
                let reason = if a.details.reason.as_deref() == Some("retention") {
                    ListReason::Retention
                } else {
                    ListReason::Untrusted
                };
                let generation = self.new_generation();
                let mut m2 = m.clone();
                m2.listing = Some(Listing {
                    reason,
                    page: None,
                    cursor: None,
                    generation,
                });
                self.commit(vec![Write::PutMount(m2)])?;
                return Ok(());
            }
            Err(e) => {
                if matches!(e.code(), Some(ErrorCode::NotFound)) {
                    self.mounts_stale = true;
                    self.wake.remove(&m.id);
                    return Ok(());
                }
                self.remote_unreachable(env, &e)?;
                return Ok(());
            }
        };
        self.remote_ok();
        if page.mounts_tag != self.store.meta(MetaKey::MountsTag)?.unwrap_or_default() {
            self.mounts_stale = true;
        }
        let mut ov = Overlay::new(&self.store);
        let mut list_folders = Vec::new();
        let mut departed = false;
        for c in &page.changes {
            match c {
                Change::Upsert(e) => {
                    Self::upsert(&mut self.ids, &mut ov, &m, e, None, &mut list_folders)?
                }
                Change::Gone { id, seq, reason } => {
                    departed |= *reason == GoneReason::Moved;
                    Self::gone(&mut ov, &m, id, *seq, *reason, round)?;
                }
            }
        }
        let mut writes = std::mem::take(&mut ov.writes);
        drop(ov);
        let mut m2 = m.clone();
        m2.cursor = Some(page.cursor.clone());
        m2.caught_up = !page.more;
        if !page.more {
            m2.caught_up_round = round;
            self.wake.remove(&m.id);
        }
        writes.push(Write::PutMount(m2));
        self.commit(writes)?;
        for f in list_folders {
            self.folder_lists.entry((m.id.clone(), f)).or_insert(None);
        }
        if departed {
            // Rule 7: a round over every other mount resolves it.
            for other in self.store.mounts()? {
                if other.id != m.id {
                    self.wake.insert(other.id);
                }
            }
        }
        Ok(())
    }

    /// Applies an upsert (rules 2, 3 and 4). `listed` marks rows a
    /// whole-mount listing delivered; `list_folders` collects folders to
    /// list.
    pub(super) fn upsert(
        ids: &mut super::Counters,
        ov: &mut Overlay<'_>,
        m: &MountRow,
        e: &Entry,
        listed: Option<u64>,
        list_folders: &mut Vec<RemoteId>,
    ) -> EngineResult<()> {
        let held = match ov.node_by_remote(&e.id)? {
            Some(n) => Some(n),
            None if e.id == m.root_remote => Some(m.root_node),
            None => None,
        };
        let row = |node: NodeId, old_listed: u64| RemoteRow {
            node,
            id: e.id.clone(),
            mount: m.id.clone(),
            parent: if e.id == m.root_remote {
                None
            } else {
                e.parent.clone()
            },
            name: e.name.clone(),
            kind: e.kind,
            size: e.size.unwrap_or(0),
            mtime_us: e.mtime_us,
            etag: e.etag.clone(),
            hash: e.hash,
            exec: e.exec,
            seq: e.seq,
            crypt: match e.crypt.as_deref() {
                Some("e2ee") => Crypt::E2ee,
                Some("rest") => Crypt::Rest,
                _ => Crypt::None,
            },
            departed: None,
            listed: listed.unwrap_or(old_listed),
        };
        match held {
            Some(n) => {
                let rec = match ov.node(n)? {
                    Some(r) => r,
                    None => {
                        ov.create_node(n, m.id.clone());
                        ov.node(n)?.ok_or_else(|| {
                            crate::store::StoreError::Corrupt(format!("{n} vanished"))
                        })?
                    }
                };
                match &rec.remote {
                    Some(r) if r.mount != m.id => {
                        // Rule 4: it moved between mounts and kept its id.
                        ov.set_mount(n, m.id.clone())?;
                        ov.put_remote(row(n, r.listed))?;
                        ov.push(Write::MarkDirty(n));
                        // What is inside a folder that crossed mounts keeps
                        // numbers this mount's feed will not send, and the
                        // old mount marked it departed: list the folder so
                        // its contents follow it here (listing is always
                        // correct, 6.3; see docs/contract-questions.md).
                        if e.kind == Kind::Folder {
                            list_folders.push(e.id.clone());
                        }
                    }
                    Some(r) if e.seq > r.seq || (r.departed.is_some() && e.seq >= r.seq) => {
                        ov.put_remote(row(n, r.listed))?;
                        ov.push(Write::MarkDirty(n));
                    }
                    Some(r) => {
                        if let Some(g) = listed
                            && r.listed != g
                        {
                            let mut same = r.clone();
                            same.listed = g;
                            ov.put_remote(same)?;
                        }
                    }
                    None => {
                        if rec.mount != m.id {
                            ov.set_mount(n, m.id.clone())?;
                        }
                        ov.put_remote(row(n, 0))?;
                        ov.push(Write::MarkDirty(n));
                    }
                }
            }
            None => {
                let n = NodeId(ids.node);
                ids.node += 1;
                ids.changed = true;
                ov.create_node(n, m.id.clone());
                ov.put_remote(row(n, 0))?;
                ov.push(Write::MarkDirty(n));
                if e.kind == Kind::Folder && e.list {
                    list_folders.push(e.id.clone());
                }
            }
        }
        Ok(())
    }

    /// Applies a removal marker (rules 5 and 6).
    fn gone(
        ov: &mut Overlay<'_>,
        m: &MountRow,
        id: &RemoteId,
        seq: u64,
        reason: GoneReason,
        round: u64,
    ) -> EngineResult<()> {
        let held = match ov.node_by_remote(id)? {
            Some(n) => ov.node(n)?.and_then(|rec| rec.remote).map(|r| (n, r)),
            None => None,
        };
        let Some((n, r)) = held.filter(|(_, r)| r.mount == m.id) else {
            // Rule 6 (amended): an id not held here. What was kept aside
            // below it (arrived before its parent) with a number at or
            // below the marker's goes, or it would wait for ever.
            for c in ov.remote_children(id)? {
                if let Some(cr) = ov.node(c)?.and_then(|x| x.remote)
                    && cr.mount == m.id
                    && cr.seq <= seq
                {
                    Self::drop_remote_subtree(ov, c, cr)?;
                }
            }
            return Ok(());
        };
        if r.seq > seq {
            return Ok(());
        }
        // The entry and everything held below it, deepest last is fine:
        // remote rows have no constraint between parent and child.
        let mut stack = vec![(n, r)];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((node, row)) = stack.pop() {
            if !seen.insert(node) {
                continue;
            }
            for c in ov.remote_children(&row.id)? {
                if let Some(cr) = ov.node(c)?.and_then(|x| x.remote) {
                    stack.push((c, cr));
                }
            }
            match reason {
                GoneReason::Trashed | GoneReason::Deleted => ov.del_remote(node)?,
                GoneReason::Moved => {
                    let mut d = row.clone();
                    d.departed = Some(round);
                    ov.put_remote(d)?;
                }
            }
            ov.push(Write::MarkDirty(node));
        }
        Ok(())
    }

    /// Removes the remote pictures of a node and of everything held below
    /// it on Hearth's side.
    fn drop_remote_subtree(ov: &mut Overlay<'_>, n: NodeId, r: RemoteRow) -> EngineResult<()> {
        let mut stack = vec![(n, r)];
        let mut seen = std::collections::BTreeSet::new();
        while let Some((node, row)) = stack.pop() {
            if !seen.insert(node) {
                continue;
            }
            for c in ov.remote_children(&row.id)? {
                if let Some(cr) = ov.node(c)?.and_then(|x| x.remote) {
                    stack.push((c, cr));
                }
            }
            ov.del_remote(node)?;
            ov.push(Write::MarkDirty(node));
        }
        Ok(())
    }

    /// Rule 7: a departed entry that no other mount picked up after a full
    /// round of requests left the person's view, and is removed.
    pub(super) fn resolve_departed(&mut self) -> EngineResult<bool> {
        let departed = self.store.remote_departed()?;
        if departed.is_empty() {
            return Ok(false);
        }
        let mounts = self.store.mounts()?;
        let mut w = Vec::new();
        for n in departed {
            let Some(r) = self.store.node(n)?.and_then(|r| r.remote) else {
                continue;
            };
            let Some(at) = r.departed else { continue };
            let all_rounded = mounts
                .iter()
                .filter(|o| o.id != r.mount && o.selected && o.cursor.is_some())
                .all(|o| o.caught_up && o.caught_up_round > at);
            if all_rounded {
                w.push(Write::DelRemote(n));
                w.push(Write::MarkDirty(n));
            }
        }
        if w.is_empty() {
            return Ok(false);
        }
        self.commit(w)?;
        Ok(true)
    }

    /// Whether every followed mount is caught up and nothing on Hearth's
    /// side waits (rule 7), and no scan is half done.
    pub(super) fn ready_to_plan(&self) -> EngineResult<bool> {
        if self.hello.is_none()
            || self.root.is_none()
            || self.mounts_stale
            || !self.folder_lists.is_empty()
            || !self.scan_queue.is_empty()
            || !self.hash_queue.is_empty()
            || !self.missing.is_empty()
        {
            return Ok(false);
        }
        for m in self.store.mounts()? {
            if m.selected && (m.cursor.is_none() || m.listing.is_some() || !m.caught_up) {
                return Ok(false);
            }
        }
        Ok(self.store.remote_departed()?.is_empty())
    }

    /// `POST /sync/wait` over every followed mount.
    pub(super) fn remote_wait(&mut self, env: &mut Env<'_>, wait_s: u32) -> EngineResult<()> {
        if self.pause.is_some() || self.hello.is_none() {
            return Ok(());
        }
        let cursors: Vec<(MountId, Cursor)> = self
            .store
            .mounts()?
            .into_iter()
            .filter(|m| m.selected)
            .filter_map(|m| m.cursor.map(|c| (m.id, c)))
            .collect();
        if cursors.is_empty() {
            return Ok(());
        }
        let tag = self.store.meta(MetaKey::MountsTag)?.unwrap_or_default();
        let wait = self
            .hello
            .as_ref()
            .map_or(wait_s, |h| wait_s.min(h.limits.wait_max));
        match env.remote.wait(&cursors, &tag, wait) {
            Ok(a) => {
                self.remote_ok();
                for m in a.changed {
                    self.wake.insert(m);
                }
                if a.mounts_changed {
                    self.mounts_stale = true;
                }
            }
            Err(e) => {
                self.remote_unreachable(env, &e)?;
            }
        }
        Ok(())
    }

    /// Forgets everything known of Hearth (a new instance the person
    /// confirmed): remote pictures, bases and mounts. Local pictures stay,
    /// so nothing is deleted; everything is compared again by content.
    pub(super) fn forget_remote_history(
        &mut self,
        new_instance: Option<String>,
    ) -> EngineResult<()> {
        let mut w = Vec::new();
        for n in self.store.all_nodes()? {
            w.push(Write::DelRemote(n));
            w.push(Write::DelSynced(n));
            w.push(Write::MarkDirty(n));
        }
        for m in self.store.mounts()? {
            w.push(Write::DelMount(m.id));
        }
        match new_instance {
            Some(i) => w.push(Write::PutMeta(MetaKey::InstanceId, i)),
            None => w.push(Write::DelMeta(MetaKey::InstanceId)),
        }
        self.commit(w)?;
        self.mounts_stale = true;
        Ok(())
    }
}
