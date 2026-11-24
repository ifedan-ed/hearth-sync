//! The disk's side: folders read and compared with the local picture, and
//! files hashed at rest (docs/sync.md 3.3, 3.4).
//!
//! A watcher's event is a hint that a folder needs looking at, never a
//! fact: the folder is listed again and compared. A full scan starts at
//! the sync root and walks everything. An item missing from its folder is
//! not taken as deleted at once: it is held as "missing" until every
//! queued folder has been read, so an item moved to a folder read later
//! is found there by its file id (3.4 rule 3) instead of being deleted on
//! Hearth and uploaded again. The planner does not run while a scan is
//! half done.
//!
//! Matching, in the plan's order: same folder and name key is the same
//! node, whatever its file id (rule 1 and 2: editors save by rename); a
//! known file id elsewhere whose old place is empty, with equal size and
//! mtime for a file or a known child still inside for a folder, is a move
//! (rule 3); anything else is new, and a node not found is deleted (4).

use std::collections::BTreeSet;

use super::{Engine, EngineResult, PauseReason};
use crate::env::Env;
use crate::fs::{EntryKind, FsError, Meta};
use crate::ids::{Kind, NodeId};
use crate::names::{LocalName, LocalPath, is_engine_name, name_key};
use crate::store::{Store, Write};
use crate::tree::{LocalRow, ProblemKind, Subject};
use crate::view;

impl<S: Store> Engine<S> {
    /// Queues a scan of everything.
    pub(super) fn queue_full_scan(&mut self) {
        match self.root {
            Some(r) => {
                self.scan_queue.insert(r, true);
            }
            None => self.full_scan_wanted = true,
        }
    }

    /// Queues the folder a watcher named (or its nearest known ancestor).
    pub(super) fn hint_folder(&mut self, path: &LocalPath) {
        let Some(root) = self.root else {
            self.full_scan_wanted = true;
            return;
        };
        let mut cur = root;
        for name in &path.0 {
            let Some(key) = name_key(name, &self.cfg.volume) else {
                break;
            };
            match self.store.local_child_by_key(cur, &key) {
                Ok(Some(n)) => cur = n,
                _ => break,
            }
        }
        match self.store.node(cur) {
            Ok(Some(rec)) if rec.local.as_ref().is_some_and(|l| l.kind == Kind::Folder) => {
                self.scan_queue.entry(cur).or_insert(false);
            }
            Ok(Some(rec)) => {
                // A file: read the folder that holds it.
                if let Some(p) = rec.local.and_then(|l| l.parent) {
                    self.scan_queue.entry(p).or_insert(false);
                }
            }
            _ => {
                self.scan_queue.entry(root).or_insert(false);
            }
        }
    }

    /// One unit of disk work: a folder read, a file hashed, or the missing
    /// items settled. False when there is none.
    pub(super) fn scan_work(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        if self.full_scan_wanted
            && let Some(r) = self.root
        {
            self.full_scan_wanted = false;
            self.scan_queue.insert(r, true);
        }
        if let Some((&n, &recursive)) = self.scan_queue.iter().next() {
            self.scan_queue.remove(&n);
            self.scan_folder(env, n, recursive)?;
            return Ok(true);
        }
        if let Some(&n) = self.hash_queue.iter().next() {
            self.hash_queue.remove(&n);
            self.hash_file(env, n)?;
            return Ok(true);
        }
        if !self.missing.is_empty() {
            self.settle_missing()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Whether a node is some mount's root.
    fn is_mount_root(&self, n: NodeId) -> EngineResult<bool> {
        Ok(self.store.mounts()?.iter().any(|m| m.root_node == n))
    }

    /// Reads one folder and brings the local picture in line with it.
    fn scan_folder(&mut self, env: &mut Env<'_>, dir: NodeId, recursive: bool) -> EngineResult<()> {
        let root = self.root.unwrap_or(dir);
        let Some(dir_rec) = self.store.node(dir)? else {
            return Ok(());
        };
        let now = env.clock.now_ns();
        let mut batch = Vec::new();
        let path = if dir == root {
            // The root's own picture: the scan is what makes it.
            match env.fs.stat(&LocalPath::root()) {
                Ok(Some(m)) if m.kind == EntryKind::Folder => {
                    let row = LocalRow {
                        node: dir,
                        parent: None,
                        name: LocalName(Vec::new()),
                        key: crate::names::NameKey(String::new()),
                        kind: Kind::Folder,
                        file_id: m.file_id,
                        size: 0,
                        mtime_ns: m.mtime_ns,
                        ctime_ns: m.ctime_ns,
                        exec: false,
                        hash: None,
                    };
                    if dir_rec.local.as_ref() != Some(&row) {
                        batch.push(Write::PutLocal(row));
                        batch.push(Write::MarkDirty(dir));
                    }
                }
                _ => {
                    self.pause = Some(PauseReason::RootUnavailable);
                    return Ok(());
                }
            }
            LocalPath::root()
        } else {
            match view::local_path(&self.store, dir)? {
                Some(p) => p,
                None => return Ok(()),
            }
        };
        let entries = match env.fs.list_dir(&path) {
            Ok(e) => e,
            Err(FsError::NotFound | FsError::NotAFolder) if dir != root => {
                // The folder went; its own folder says where (a move).
                self.mark_missing(dir)?;
                if let Some(p) = dir_rec.local.and_then(|l| l.parent) {
                    self.scan_queue.entry(p).or_insert(false);
                }
                return Ok(());
            }
            Err(FsError::RootUnavailable | FsError::NotFound | FsError::NotAFolder) => {
                self.pause = Some(PauseReason::RootUnavailable);
                return Ok(());
            }
            Err(e) => {
                let w = self.problem_write(
                    Subject::Node(dir),
                    ProblemKind::Refused,
                    format!("this folder cannot be read: {e}"),
                    now,
                )?;
                self.commit(vec![w])?;
                return Ok(());
            }
        };
        // Path problems of this folder are listed afresh.
        for p in self.store.problems()? {
            if let Subject::Path(pp) = &p.subject
                && pp.parent().as_ref() == Some(&path)
            {
                batch.push(Write::DelProblem(p.subject.clone()));
            }
        }
        let mut entries = entries;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let existing = self.store.local_children(dir)?;
        let mut by_key = std::collections::BTreeMap::new();
        for c in &existing {
            if let Some(l) = self.store.node(*c)?.and_then(|r| r.local) {
                by_key.insert(l.key.clone(), *c);
            }
        }
        let mut seen = BTreeSet::new();
        let mut used = BTreeSet::new();
        let mounts = self.store.mounts()?;
        // First, what each entry is; problems and ignored names are settled
        // here and take no part in matching.
        let mut cands: Vec<(crate::fs::DirEntry, crate::names::NameKey, Kind)> = Vec::new();
        for e in entries {
            let child_path = path.join(&e.name);
            if let Some(s) = e.name.as_str()
                && is_engine_name(s, dir == root)
            {
                continue;
            }
            let Some(key) = name_key(&e.name, &self.cfg.volume) else {
                batch.push(self.problem_write(
                    Subject::Path(child_path),
                    ProblemKind::NameRefused,
                    "the name is not valid UTF-8".into(),
                    now,
                )?);
                continue;
            };
            let kind = match &e.meta.kind {
                EntryKind::File => Kind::File,
                EntryKind::Folder => Kind::Folder,
                EntryKind::Symlink(t) => {
                    batch.push(self.problem_write(
                        Subject::Path(child_path),
                        ProblemKind::Symlink,
                        format!("symbolic link to {t}: not synced"),
                        now,
                    )?);
                    continue;
                }
                EntryKind::Special => {
                    batch.push(self.problem_write(
                        Subject::Path(child_path),
                        ProblemKind::Special,
                        "a special file: not synced".into(),
                        now,
                    )?);
                    continue;
                }
            };
            if !used.insert(key.clone()) {
                batch.push(self.problem_write(
                    Subject::Path(child_path),
                    ProblemKind::NameClash,
                    "another item here has the same name on Hearth".into(),
                    now,
                )?);
                continue;
            }
            if let Some(parts) = view::path_strs(&child_path)
                && self.ignore.is_ignored(&parts, kind == Kind::Folder)
            {
                // Ignored: never uploaded, never deleted (3.14). A node
                // already held under this name is kept as it is.
                if let Some(n) = by_key.get(&key) {
                    seen.insert(*n);
                }
                continue;
            }
            cands.push((e, key, kind));
        }
        // Then matching, in an order that follows identity before names, so
        // the result does not depend on the order of the names: (rule 1)
        // same name and same file id; (rule 3) a file id known elsewhere
        // whose old place no longer holds it; (rule 2) same name, other file
        // id, the node not claimed by a move; (rule 4) new.
        let mut claimed: Vec<Option<NodeId>> = vec![None; cands.len()];
        for (i, (e, key, kind)) in cands.iter().enumerate() {
            if let Some(n) = by_key.get(key).copied()
                && let Some(l) = self.store.node(n)?.and_then(|r| r.local)
                && l.file_id == e.meta.file_id
                && l.kind == *kind
            {
                claimed[i] = Some(n);
                seen.insert(n);
            }
        }
        for (i, (e, _, kind)) in cands.iter().enumerate() {
            if claimed[i].is_some() {
                continue;
            }
            if let Some(c) = self.find_moved(env, dir, &e.meta, *kind, &seen)? {
                claimed[i] = Some(c);
                seen.insert(c);
                self.missing.remove(&c);
            }
        }
        for (i, (_, key, _)) in cands.iter().enumerate() {
            if claimed[i].is_none()
                && let Some(n) = by_key.get(key).copied()
                && !seen.contains(&n)
            {
                claimed[i] = Some(n);
                seen.insert(n);
            }
        }
        // A name here now held by an item that moved in: the node that held
        // it is gone from this place, and lets go of the name in this batch
        // (if it moved somewhere not yet read, it is found there as new:
        // the plan's fallback, which costs a transfer, never data).
        let mut freed = BTreeSet::new();
        for (i, (_, key, _)) in cands.iter().enumerate() {
            if let Some(x) = by_key.get(key).copied()
                && claimed[i] != Some(x)
                && !seen.contains(&x)
                && !self.is_mount_root(x)?
                && self.remove_local_subtree(x, &mut batch, &mut freed)?
            {
                seen.insert(x);
                self.missing.remove(&x);
            }
        }
        // Rows: nodes that move or are new take their names after every
        // node that leaves a name here has let go of it.
        let mut later = Vec::new();
        for (i, (e, key, kind)) in cands.into_iter().enumerate() {
            let Some(n) = claimed[i] else {
                later.push((e, key, kind));
                continue;
            };
            self.absent_roots.remove(&n);
            let Some(l) = self.store.node(n)?.and_then(|r| r.local) else {
                continue;
            };
            if l.kind != kind {
                // The name changed kind (3.5, retype): the old node's local
                // picture goes now, and the new item is a new node.
                let mut removed = BTreeSet::new();
                if self.remove_local_subtree(n, &mut batch, &mut removed)? {
                    later.push((e, key, kind));
                }
                continue;
            }
            let row = local_row(n, Some(dir), &e.name, key, kind, &e.meta, &l);
            if row != l {
                if row.hash.is_none() && kind == Kind::File {
                    self.hash_queue.insert(n);
                }
                batch.push(Write::PutLocal(row));
                batch.push(Write::MarkDirty(n));
            } else if kind == Kind::File && l.hash.is_none() {
                self.hash_queue.insert(n);
            }
            if kind == Kind::Folder && recursive {
                self.scan_queue.insert(n, true);
            }
        }
        for (e, key, kind) in later {
            // New. If a mount's root belongs exactly here and has no local
            // picture yet, this folder is that root.
            let mut node = None;
            for m in &mounts {
                if let Some((last, above)) = m.local_path.split_last() {
                    let here = view::local_node_at(&self.store, root, above, &self.cfg.volume)?;
                    if here == Some(dir)
                        && crate::names::key_of_str(last, &self.cfg.volume) == key
                        && self
                            .store
                            .node(m.root_node)?
                            .is_none_or(|r| r.local.is_none())
                    {
                        node = Some((m.root_node, m.id.clone()));
                    }
                }
            }
            let (n, mount) = match node {
                Some(x) => x,
                None => (self.alloc_node(), dir_rec.mount.clone()),
            };
            batch.push(Write::PutNode(n, mount));
            let fresh = LocalRow {
                node: n,
                parent: Some(dir),
                name: e.name.clone(),
                key,
                kind,
                file_id: e.meta.file_id,
                size: if kind == Kind::File { e.meta.size } else { 0 },
                mtime_ns: e.meta.mtime_ns,
                ctime_ns: e.meta.ctime_ns,
                exec: e.meta.exec,
                hash: None,
            };
            batch.push(Write::PutLocal(fresh));
            batch.push(Write::MarkDirty(n));
            match kind {
                Kind::File => {
                    self.hash_queue.insert(n);
                }
                Kind::Folder => {
                    self.scan_queue.insert(n, true);
                }
            }
        }
        for c in existing {
            if !seen.contains(&c) {
                if self.is_mount_root(c)? {
                    // A mount's folder gone from disk: its mount is left
                    // alone, nothing is deleted (I6 for mounts).
                    self.absent_roots.insert(c);
                    batch.push(Write::MarkDirty(c));
                } else {
                    self.missing.insert(c);
                }
            }
        }
        self.commit(batch)?;
        Ok(())
    }

    /// Rule 3: a node known by this file id elsewhere, whose old place is
    /// now empty, with the same size and mtime (a file) or a known child
    /// still inside (a folder).
    fn find_moved(
        &mut self,
        env: &mut Env<'_>,
        dir: NodeId,
        meta: &Meta,
        kind: Kind,
        claimed: &BTreeSet<NodeId>,
    ) -> EngineResult<Option<NodeId>> {
        for c in self.store.nodes_by_file_id(meta.file_id)? {
            if Some(c) == self.root || claimed.contains(&c) {
                continue;
            }
            let Some(l) = self.store.node(c)?.and_then(|r| r.local) else {
                continue;
            };
            if l.kind != kind {
                continue;
            }
            // Never into itself.
            let mut up = Some(dir);
            let mut inside = false;
            while let Some(u) = up {
                if u == c {
                    inside = true;
                    break;
                }
                up = self
                    .store
                    .node(u)?
                    .and_then(|r| r.local)
                    .and_then(|l| l.parent);
            }
            if inside {
                continue;
            }
            let old_empty = if self.missing.contains(&c) {
                true
            } else {
                match view::local_path(&self.store, c)? {
                    Some(p) => match env.fs.stat(&p) {
                        Ok(Some(m)) => m.file_id != meta.file_id,
                        Ok(None) => true,
                        Err(_) => false,
                    },
                    None => false,
                }
            };
            if !old_empty {
                continue;
            }
            let same = match kind {
                Kind::File => l.size == meta.size && l.mtime_ns == meta.mtime_ns,
                Kind::Folder => self.folder_still_holds_a_child(env, c, dir, meta)?,
            };
            if same {
                return Ok(Some(c));
            }
        }
        Ok(None)
    }

    /// A known child of folder `c` is inside the folder found at a new
    /// place (with the found folder's name not yet known, the check looks
    /// in every entry of `dir` with the folder's file id).
    fn folder_still_holds_a_child(
        &mut self,
        env: &mut Env<'_>,
        c: NodeId,
        dir: NodeId,
        meta: &Meta,
    ) -> EngineResult<bool> {
        // The known children's file ids: any one of them inside the found
        // folder (under any name: it may have been renamed too) will do.
        let mut known = BTreeSet::new();
        for k in self.store.local_children(c)? {
            if let Some(kl) = self.store.node(k)?.and_then(|r| r.local) {
                known.insert(kl.file_id);
            }
        }
        if known.is_empty() {
            return Ok(false);
        }
        let Some(dir_path) = (if Some(dir) == self.root {
            Some(LocalPath::root())
        } else {
            view::local_path(&self.store, dir)?
        }) else {
            return Ok(false);
        };
        let Ok(entries) = env.fs.list_dir(&dir_path) else {
            return Ok(false);
        };
        for e in entries {
            if e.meta.file_id == meta.file_id
                && let Ok(inside) = env.fs.list_dir(&dir_path.join(&e.name))
                && inside.iter().any(|x| known.contains(&x.meta.file_id))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn mark_missing(&mut self, n: NodeId) -> EngineResult<()> {
        if self.is_mount_root(n)? {
            self.absent_roots.insert(n);
        } else {
            self.missing.insert(n);
        }
        Ok(())
    }

    /// Every queued folder has been read: what is still missing is gone
    /// from the disk. Its local picture and those below it are removed,
    /// deepest first; a mount's root below is kept (and its mount left
    /// alone), and so are the folders above it.
    fn settle_missing(&mut self) -> EngineResult<()> {
        let missing = std::mem::take(&mut self.missing);
        let mut batch = Vec::new();
        let mut removed = BTreeSet::new();
        for n in missing {
            self.remove_local_subtree(n, &mut batch, &mut removed)?;
        }
        self.commit(batch)?;
        Ok(())
    }

    /// Post-order removal; returns whether `n`'s picture was removed.
    fn remove_local_subtree(
        &mut self,
        n: NodeId,
        batch: &mut Vec<Write>,
        removed: &mut BTreeSet<NodeId>,
    ) -> EngineResult<bool> {
        if removed.contains(&n) {
            return Ok(true);
        }
        if self.is_mount_root(n)? {
            self.absent_roots.insert(n);
            batch.push(Write::MarkDirty(n));
            return Ok(false);
        }
        let mut all = true;
        for c in self.store.local_children(n)? {
            all &= self.remove_local_subtree(c, batch, removed)?;
        }
        if !all {
            return Ok(false);
        }
        if self.store.node(n)?.and_then(|r| r.local).is_some() {
            batch.push(Write::DelLocal(n));
            batch.push(Write::MarkDirty(n));
        }
        self.hash_queue.remove(&n);
        removed.insert(n);
        Ok(true)
    }

    /// Hashes one file at rest: its metadata must be the same before and
    /// after the read (3.3), else it is tried again, and listed as still
    /// being written after a few tries.
    fn hash_file(&mut self, env: &mut Env<'_>, n: NodeId) -> EngineResult<()> {
        let Some(l) = self.store.node(n)?.and_then(|r| r.local) else {
            return Ok(());
        };
        if l.kind != Kind::File {
            return Ok(());
        }
        let Some(path) = view::local_path(&self.store, n)? else {
            return Ok(());
        };
        let now = env.clock.now_ns();
        let before = match env.fs.stat(&path) {
            Ok(Some(m)) if m.kind == EntryKind::File => m,
            Ok(_) | Err(FsError::NotFound) => {
                if let Some(p) = l.parent {
                    self.scan_queue.entry(p).or_insert(false);
                }
                return Ok(());
            }
            Err(_) => {
                self.retry_hash(n, now)?;
                return Ok(());
            }
        };
        let hash = match read_hash(env, &path, before.size, self.cfg.range_size) {
            Ok(h) => h,
            Err(_) => {
                self.retry_hash(n, now)?;
                return Ok(());
            }
        };
        let after = env.fs.stat(&path).ok().flatten();
        if after.as_ref() != Some(&before) {
            self.retry_hash(n, now)?;
            return Ok(());
        }
        let mut row = l.clone();
        row.file_id = before.file_id;
        row.size = before.size;
        row.mtime_ns = before.mtime_ns;
        row.ctime_ns = before.ctime_ns;
        row.exec = before.exec;
        row.hash = Some(hash);
        self.hash_tries.remove(&n);
        let mut b = vec![Write::PutLocal(row), Write::MarkDirty(n)];
        self.clear_problem(n, &mut b);
        self.commit(b)?;
        Ok(())
    }

    fn retry_hash(&mut self, n: NodeId, now: i64) -> EngineResult<()> {
        let tries = self.hash_tries.entry(n).or_insert(0);
        *tries += 1;
        if *tries >= self.cfg.hash_attempts {
            self.hash_tries.remove(&n);
            let w = self.problem_write(
                Subject::Node(n),
                ProblemKind::StillChanging,
                "still being written: not uploaded until it settles".into(),
                now,
            )?;
            self.commit(vec![w])?;
        } else {
            self.hash_queue.insert(n);
        }
        Ok(())
    }
}

/// The local picture from a directory entry, keeping the known hash when
/// the metadata the scanner compares has not changed.
fn local_row(
    n: NodeId,
    parent: Option<NodeId>,
    name: &LocalName,
    key: crate::names::NameKey,
    kind: Kind,
    meta: &Meta,
    old: &LocalRow,
) -> LocalRow {
    let unchanged = old.file_id == meta.file_id
        && old.size == meta.size
        && old.mtime_ns == meta.mtime_ns
        && old.ctime_ns == meta.ctime_ns;
    LocalRow {
        node: n,
        parent,
        name: name.clone(),
        key,
        kind,
        file_id: meta.file_id,
        size: if kind == Kind::File { meta.size } else { 0 },
        mtime_ns: if kind == Kind::File {
            meta.mtime_ns
        } else {
            old.mtime_ns
        },
        ctime_ns: if kind == Kind::File {
            meta.ctime_ns
        } else {
            old.ctime_ns
        },
        exec: meta.exec,
        hash: if unchanged && kind == Kind::File {
            old.hash
        } else {
            None
        },
    }
}

/// Reads a whole file through the disk interface and hashes it.
pub(super) fn read_hash(
    env: &mut Env<'_>,
    path: &LocalPath,
    size: u64,
    piece: u64,
) -> Result<crate::ids::Hash, FsError> {
    let mut h = crate::ids::ContentHasher::new();
    let mut off = 0u64;
    while off < size {
        let want = piece.min(size - off);
        let bytes = env.fs.read(path, off, want)?;
        if bytes.is_empty() {
            break;
        }
        off += bytes.len() as u64;
        h.update(&bytes);
    }
    Ok(h.finish())
}
