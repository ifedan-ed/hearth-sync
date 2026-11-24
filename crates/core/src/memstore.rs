//! `MemStore`: the state database in memory, for the simulator and the
//! core's own tests, and the reference the SQLite store is compared with.
//!
//! Why it exists beside SQLite: the simulator runs millions of short runs,
//! and a store in memory makes each a few microseconds per step. It keeps
//! the same promises as the real store (all-or-nothing batches, the same
//! constraints, sorted answers), checked by the same conformance suite.
//!
//! How a batch is all or nothing: every write is done through a primitive
//! setter that returns what it replaced; the replaced values are kept in an
//! undo list, and if any write of the batch fails the list is replayed
//! backwards. Ordered maps everywhere, so no answer depends on hashing.

use std::collections::{BTreeMap, BTreeSet};

use crate::ids::{ConflictId, FileId, HeldId, Kind, MountId, NodeId, OpId, RemoteId};
use crate::names::NameKey;
use crate::store::{Batch, MetaKey, Store, StoreError, StoreResult, Write};
use crate::tree::{
    ConflictRow, HeldRow, LocalRow, MountRow, NodeRecord, OpRow, ProblemRow, RemoteRow, Subject,
    SyncedRow, TransferRow,
};

/// The state database in memory. Everything committed survives a dropped
/// engine (a simulated crash): the store is what the next engine opens.
#[derive(Clone, Debug, Default)]
pub struct MemStore {
    meta: BTreeMap<MetaKey, String>,
    mounts: BTreeMap<MountId, MountRow>,
    nodes: BTreeMap<NodeId, NodeRecord>,
    by_remote: BTreeMap<RemoteId, NodeId>,
    by_file: BTreeMap<FileId, BTreeSet<NodeId>>,
    local_kids: BTreeMap<NodeId, BTreeMap<NameKey, NodeId>>,
    remote_kids: BTreeMap<RemoteId, BTreeSet<NodeId>>,
    synced_kids: BTreeMap<NodeId, BTreeSet<NodeId>>,
    mount_nodes: BTreeMap<MountId, BTreeSet<NodeId>>,
    ops: BTreeMap<OpId, OpRow>,
    transfers: BTreeMap<OpId, TransferRow>,
    problems: BTreeMap<Subject, ProblemRow>,
    held: BTreeMap<HeldId, HeldRow>,
    conflicts: BTreeMap<ConflictId, ConflictRow>,
    dirty: BTreeSet<NodeId>,
    waits: BTreeMap<NodeId, NodeId>,
    waiters: BTreeMap<NodeId, BTreeSet<NodeId>>,
    unselected: BTreeSet<NodeId>,
    fail_after: Option<usize>,
}

/// What one primitive change replaced, to undo it.
enum Undo {
    Meta(MetaKey, Option<String>),
    Mount(MountId, Option<MountRow>),
    NodeCreated(NodeId),
    NodeRemoved(NodeId, MountId),
    NodeMount(NodeId, MountId),
    Remote(NodeId, Option<RemoteRow>),
    Local(NodeId, Option<LocalRow>),
    Synced(NodeId, Option<SyncedRow>),
    Op(OpId, Option<OpRow>),
    Transfer(OpId, Option<TransferRow>),
    Problem(Subject, Option<ProblemRow>),
    Held(HeldId, Option<HeldRow>),
    Conflict(ConflictId, Option<ConflictRow>),
    Dirty(NodeId, bool),
    Wait(NodeId, Option<NodeId>),
    Unselected(NodeId, bool),
}

fn constraint<T>(msg: String) -> StoreResult<T> {
    Err(StoreError::Constraint(msg))
}

impl MemStore {
    /// An empty store (a first start, or a deleted database).
    pub fn new() -> MemStore {
        MemStore::default()
    }

    fn record(&mut self, n: NodeId) -> StoreResult<&mut NodeRecord> {
        match self.nodes.get_mut(&n) {
            Some(r) => Ok(r),
            None => constraint(format!("{n} does not exist")),
        }
    }

    fn set_remote(&mut self, n: NodeId, new: Option<RemoteRow>) -> StoreResult<Option<RemoteRow>> {
        if let Some(r) = &new {
            if r.node != n {
                return constraint(format!("remote row of {} put on {n}", r.node));
            }
            if let Some(other) = self.by_remote.get(&r.id)
                && *other != n
            {
                return constraint(format!("remote id {} already on {other}", r.id));
            }
        }
        let old = std::mem::replace(&mut self.record(n)?.remote, new.clone());
        if let Some(o) = &old {
            self.by_remote.remove(&o.id);
            if let Some(p) = &o.parent
                && let Some(set) = self.remote_kids.get_mut(p)
            {
                set.remove(&n);
                if set.is_empty() {
                    self.remote_kids.remove(p);
                }
            }
        }
        if let Some(r) = &new {
            self.by_remote.insert(r.id.clone(), n);
            if let Some(p) = &r.parent {
                self.remote_kids.entry(p.clone()).or_default().insert(n);
            }
        }
        Ok(old)
    }

    fn set_local(&mut self, n: NodeId, new: Option<LocalRow>) -> StoreResult<Option<LocalRow>> {
        if let Some(l) = &new {
            if l.node != n {
                return constraint(format!("local row of {} put on {n}", l.node));
            }
            if let Some(p) = l.parent
                && let Some(other) = self.local_kids.get(&p).and_then(|m| m.get(&l.key))
                && *other != n
            {
                return constraint(format!(
                    "name key {:?} in {p} already held by {other}",
                    l.key
                ));
            }
        }
        let old = std::mem::replace(&mut self.record(n)?.local, new.clone());
        if let Some(o) = &old {
            if let Some(set) = self.by_file.get_mut(&o.file_id) {
                set.remove(&n);
                if set.is_empty() {
                    self.by_file.remove(&o.file_id);
                }
            }
            if let Some(p) = o.parent
                && let Some(m) = self.local_kids.get_mut(&p)
            {
                m.remove(&o.key);
                if m.is_empty() {
                    self.local_kids.remove(&p);
                }
            }
        }
        if let Some(l) = &new {
            self.by_file.entry(l.file_id).or_default().insert(n);
            if let Some(p) = l.parent {
                self.local_kids
                    .entry(p)
                    .or_default()
                    .insert(l.key.clone(), n);
            }
        }
        Ok(old)
    }

    fn set_synced(&mut self, n: NodeId, new: Option<SyncedRow>) -> StoreResult<Option<SyncedRow>> {
        if let Some(s) = &new
            && s.node != n
        {
            return constraint(format!("base row of {} put on {n}", s.node));
        }
        let old = std::mem::replace(&mut self.record(n)?.synced, new.clone());
        if let Some(p) = old.as_ref().and_then(|o| o.parent)
            && let Some(set) = self.synced_kids.get_mut(&p)
        {
            set.remove(&n);
            if set.is_empty() {
                self.synced_kids.remove(&p);
            }
        }
        if let Some(p) = new.as_ref().and_then(|s| s.parent) {
            self.synced_kids.entry(p).or_default().insert(n);
        }
        Ok(old)
    }

    fn create_node(&mut self, n: NodeId, mount: MountId) {
        self.mount_nodes.entry(mount.clone()).or_default().insert(n);
        self.nodes.insert(
            n,
            NodeRecord {
                node: n,
                mount,
                remote: None,
                local: None,
                synced: None,
            },
        );
    }

    fn remove_node_record(&mut self, n: NodeId) {
        if let Some(rec) = self.nodes.remove(&n)
            && let Some(set) = self.mount_nodes.get_mut(&rec.mount)
        {
            set.remove(&n);
            if set.is_empty() {
                self.mount_nodes.remove(&rec.mount);
            }
        }
    }

    fn set_node_mount(&mut self, n: NodeId, mount: MountId) -> StoreResult<MountId> {
        let old = std::mem::replace(&mut self.record(n)?.mount, mount.clone());
        if let Some(set) = self.mount_nodes.get_mut(&old) {
            set.remove(&n);
            if set.is_empty() {
                self.mount_nodes.remove(&old);
            }
        }
        self.mount_nodes.entry(mount).or_default().insert(n);
        Ok(old)
    }

    /// Sets or ends a node's wait; returns the target it had.
    fn set_wait(&mut self, n: NodeId, target: Option<NodeId>) -> Option<NodeId> {
        let old = match target {
            Some(t) => self.waits.insert(n, t),
            None => self.waits.remove(&n),
        };
        if let Some(o) = old
            && let Some(set) = self.waiters.get_mut(&o)
        {
            set.remove(&n);
            if set.is_empty() {
                self.waiters.remove(&o);
            }
        }
        if let Some(t) = target {
            self.waiters.entry(t).or_default().insert(n);
        }
        old
    }

    fn set_flag(set: &mut BTreeSet<NodeId>, n: NodeId, on: bool) -> bool {
        let was = set.contains(&n);
        if on {
            set.insert(n);
        } else {
            set.remove(&n);
        }
        was
    }

    /// Applies one write, pushing what it replaced onto `undo`.
    fn apply_one(&mut self, w: Write, undo: &mut Vec<Undo>) -> StoreResult<()> {
        match w {
            Write::PutMeta(k, v) => undo.push(Undo::Meta(k, self.meta.insert(k, v))),
            Write::DelMeta(k) => undo.push(Undo::Meta(k, self.meta.remove(&k))),
            Write::PutMount(m) => {
                let id = m.id.clone();
                undo.push(Undo::Mount(id.clone(), self.mounts.insert(id, m)));
            }
            Write::DelMount(id) => {
                let old = self.mounts.remove(&id);
                undo.push(Undo::Mount(id, old));
            }
            Write::PutNode(n, mount) => {
                if self.nodes.contains_key(&n) {
                    let old = self.set_node_mount(n, mount)?;
                    undo.push(Undo::NodeMount(n, old));
                } else {
                    self.create_node(n, mount);
                    undo.push(Undo::NodeCreated(n));
                }
            }
            Write::DelNode(n) => {
                if self.ops.values().any(|o| o.node == n) {
                    return constraint(format!("{n} removed while an operation acts on it"));
                }
                if self.local_kids.contains_key(&n) || self.synced_kids.contains_key(&n) {
                    return constraint(format!("{n} removed while it has children"));
                }
                let mount = self.record(n)?.mount.clone();
                undo.push(Undo::Remote(n, self.set_remote(n, None)?));
                undo.push(Undo::Local(n, self.set_local(n, None)?));
                undo.push(Undo::Synced(n, self.set_synced(n, None)?));
                undo.push(Undo::Dirty(n, Self::set_flag(&mut self.dirty, n, false)));
                undo.push(Undo::Wait(n, self.set_wait(n, None)));
                undo.push(Undo::Unselected(
                    n,
                    Self::set_flag(&mut self.unselected, n, false),
                ));
                let subject = Subject::Node(n);
                let p = self.problems.remove(&subject);
                undo.push(Undo::Problem(subject, p));
                self.remove_node_record(n);
                undo.push(Undo::NodeRemoved(n, mount));
            }
            Write::PutRemote(r) => {
                let n = r.node;
                undo.push(Undo::Remote(n, self.set_remote(n, Some(r))?));
            }
            Write::DelRemote(n) => undo.push(Undo::Remote(n, self.set_remote(n, None)?)),
            Write::PutLocal(l) => {
                let n = l.node;
                undo.push(Undo::Local(n, self.set_local(n, Some(l))?));
            }
            Write::DelLocal(n) => {
                if self.local_kids.contains_key(&n) {
                    return constraint(format!("local row of {n} removed while it has children"));
                }
                undo.push(Undo::Local(n, self.set_local(n, None)?));
            }
            Write::PutSynced(s) => {
                let n = s.node;
                undo.push(Undo::Synced(n, self.set_synced(n, Some(s))?));
            }
            Write::DelSynced(n) => undo.push(Undo::Synced(n, self.set_synced(n, None)?)),
            Write::PutOp(o) => {
                if !self.nodes.contains_key(&o.node) {
                    return constraint(format!("{} on missing {}", o.id, o.node));
                }
                let id = o.id;
                undo.push(Undo::Op(id, self.ops.insert(id, o)));
            }
            Write::DelOp(id) => {
                undo.push(Undo::Transfer(id, self.transfers.remove(&id)));
                undo.push(Undo::Op(id, self.ops.remove(&id)));
            }
            Write::PutTransfer(t) => {
                if !self.ops.contains_key(&t.op) {
                    return constraint(format!("transfer of missing {}", t.op));
                }
                let id = t.op;
                undo.push(Undo::Transfer(id, self.transfers.insert(id, t)));
            }
            Write::PutProblem(p) => {
                let s = p.subject.clone();
                undo.push(Undo::Problem(s.clone(), self.problems.insert(s, p)));
            }
            Write::DelProblem(s) => {
                let old = self.problems.remove(&s);
                undo.push(Undo::Problem(s, old));
            }
            Write::PutHeld(h) => {
                let id = h.id;
                undo.push(Undo::Held(id, self.held.insert(id, h)));
            }
            Write::DelHeld(id) => undo.push(Undo::Held(id, self.held.remove(&id))),
            Write::PutConflict(c) => {
                let id = c.id;
                undo.push(Undo::Conflict(id, self.conflicts.insert(id, c)));
            }
            Write::MarkDirty(n) => {
                if !self.nodes.contains_key(&n) {
                    return constraint(format!("dirty mark on missing {n}"));
                }
                undo.push(Undo::Wait(n, self.set_wait(n, None)));
                undo.push(Undo::Dirty(n, Self::set_flag(&mut self.dirty, n, true)));
            }
            Write::Wait(n, t) => {
                if !self.nodes.contains_key(&n) {
                    return constraint(format!("wait of missing {n}"));
                }
                undo.push(Undo::Dirty(n, Self::set_flag(&mut self.dirty, n, false)));
                undo.push(Undo::Wait(n, self.set_wait(n, Some(t))));
            }
            Write::ClearDirty(n) => {
                undo.push(Undo::Dirty(n, Self::set_flag(&mut self.dirty, n, false)));
                undo.push(Undo::Wait(n, self.set_wait(n, None)));
            }
            Write::Unselect(n) => undo.push(Undo::Unselected(
                n,
                Self::set_flag(&mut self.unselected, n, true),
            )),
            Write::Reselect(n) => undo.push(Undo::Unselected(
                n,
                Self::set_flag(&mut self.unselected, n, false),
            )),
        }
        Ok(())
    }

    /// Puts back what `undo` recorded, newest first. Each step returns the
    /// store to a state it was in a moment before, so the setters'
    /// constraints hold; their results are not needed.
    fn rollback(&mut self, undo: Vec<Undo>) {
        for u in undo.into_iter().rev() {
            match u {
                Undo::Meta(k, v) => {
                    match v {
                        Some(v) => self.meta.insert(k, v),
                        None => self.meta.remove(&k),
                    };
                }
                Undo::Mount(id, v) => {
                    match v {
                        Some(v) => self.mounts.insert(id, v),
                        None => self.mounts.remove(&id),
                    };
                }
                Undo::NodeCreated(n) => self.remove_node_record(n),
                Undo::NodeRemoved(n, m) => self.create_node(n, m),
                Undo::NodeMount(n, m) => {
                    let _ = self.set_node_mount(n, m);
                }
                Undo::Remote(n, v) => {
                    let _ = self.set_remote(n, v);
                }
                Undo::Local(n, v) => {
                    let _ = self.set_local(n, v);
                }
                Undo::Synced(n, v) => {
                    let _ = self.set_synced(n, v);
                }
                Undo::Op(id, v) => {
                    match v {
                        Some(v) => self.ops.insert(id, v),
                        None => self.ops.remove(&id),
                    };
                }
                Undo::Transfer(id, v) => {
                    match v {
                        Some(v) => self.transfers.insert(id, v),
                        None => self.transfers.remove(&id),
                    };
                }
                Undo::Problem(s, v) => {
                    match v {
                        Some(v) => self.problems.insert(s, v),
                        None => self.problems.remove(&s),
                    };
                }
                Undo::Held(id, v) => {
                    match v {
                        Some(v) => self.held.insert(id, v),
                        None => self.held.remove(&id),
                    };
                }
                Undo::Conflict(id, v) => {
                    match v {
                        Some(v) => self.conflicts.insert(id, v),
                        None => self.conflicts.remove(&id),
                    };
                }
                Undo::Dirty(n, was) => {
                    Self::set_flag(&mut self.dirty, n, was);
                }
                Undo::Wait(n, t) => {
                    self.set_wait(n, t);
                }
                Undo::Unselected(n, was) => {
                    Self::set_flag(&mut self.unselected, n, was);
                }
            }
        }
    }
}

impl Store for MemStore {
    fn meta(&self, key: MetaKey) -> StoreResult<Option<String>> {
        Ok(self.meta.get(&key).cloned())
    }

    fn mounts(&self) -> StoreResult<Vec<MountRow>> {
        Ok(self.mounts.values().cloned().collect())
    }

    fn node(&self, node: NodeId) -> StoreResult<Option<NodeRecord>> {
        Ok(self.nodes.get(&node).cloned())
    }

    fn node_by_remote(&self, id: &RemoteId) -> StoreResult<Option<NodeId>> {
        Ok(self.by_remote.get(id).copied())
    }

    fn nodes_by_file_id(&self, fid: FileId) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .by_file
            .get(&fid)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default())
    }

    fn local_children(&self, parent: NodeId) -> StoreResult<Vec<NodeId>> {
        let mut v: Vec<NodeId> = self
            .local_kids
            .get(&parent)
            .map(|m| m.values().copied().collect())
            .unwrap_or_default();
        v.sort();
        Ok(v)
    }

    fn local_child_by_key(&self, parent: NodeId, key: &NameKey) -> StoreResult<Option<NodeId>> {
        Ok(self
            .local_kids
            .get(&parent)
            .and_then(|m| m.get(key))
            .copied())
    }

    fn remote_children(&self, parent: &RemoteId) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .remote_kids
            .get(parent)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default())
    }

    fn synced_children(&self, parent: NodeId) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .synced_kids
            .get(&parent)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default())
    }

    fn mount_nodes(&self, mount: &MountId) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .mount_nodes
            .get(mount)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default())
    }

    fn count_nodes(&self, mount: &MountId) -> StoreResult<u64> {
        Ok(self
            .mount_nodes
            .get(mount)
            .map(|s| s.len() as u64)
            .unwrap_or(0))
    }

    fn all_nodes(&self) -> StoreResult<Vec<NodeId>> {
        Ok(self.nodes.keys().copied().collect())
    }

    fn ops(&self) -> StoreResult<Vec<OpRow>> {
        Ok(self.ops.values().cloned().collect())
    }

    fn transfer(&self, op: OpId) -> StoreResult<Option<TransferRow>> {
        Ok(self.transfers.get(&op).cloned())
    }

    fn problems(&self) -> StoreResult<Vec<ProblemRow>> {
        Ok(self.problems.values().cloned().collect())
    }

    fn held(&self) -> StoreResult<Vec<HeldRow>> {
        Ok(self.held.values().cloned().collect())
    }

    fn conflicts(&self) -> StoreResult<Vec<ConflictRow>> {
        Ok(self.conflicts.values().cloned().collect())
    }

    fn dirty(&self, after: Option<NodeId>, limit: usize) -> StoreResult<Vec<NodeId>> {
        let start = match after {
            Some(a) => std::ops::Bound::Excluded(a),
            None => std::ops::Bound::Unbounded,
        };
        Ok(self
            .dirty
            .range((start, std::ops::Bound::Unbounded))
            .take(limit)
            .copied()
            .collect())
    }

    fn waiters_on(&self, target: NodeId) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .waiters
            .get(&target)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default())
    }

    fn waiting(&self, limit: usize) -> StoreResult<Vec<NodeId>> {
        Ok(self.waits.keys().take(limit).copied().collect())
    }

    fn remote_departed(&self) -> StoreResult<Vec<NodeId>> {
        Ok(self
            .nodes
            .values()
            .filter(|r| r.remote.as_ref().is_some_and(|r| r.departed.is_some()))
            .map(|r| r.node)
            .collect())
    }

    fn unselected(&self) -> StoreResult<Vec<NodeId>> {
        Ok(self.unselected.iter().copied().collect())
    }

    fn apply(&mut self, batch: Batch) -> StoreResult<()> {
        let mut undo = Vec::with_capacity(batch.len() * 2);
        for w in batch {
            if let Some(n) = self.fail_after.as_mut() {
                if *n == 0 {
                    self.rollback(undo);
                    return Err(StoreError::Injected);
                }
                *n -= 1;
            }
            if let Err(e) = self.apply_one(w, &mut undo) {
                self.rollback(undo);
                return Err(e);
            }
        }
        Ok(())
    }

    fn fail_writes_after(&mut self, n: Option<usize>) {
        self.fail_after = n;
    }
}

/// Whether a node's local picture is a folder (a small helper the checks
/// and the engine share).
pub fn is_local_folder(rec: &NodeRecord) -> bool {
    rec.local.as_ref().is_some_and(|l| l.kind == Kind::Folder)
}
