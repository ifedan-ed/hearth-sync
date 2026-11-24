//! A write batch that can be read while it is being built.
//!
//! Why: a feed page or a listing page holds up to a thousand entries, and
//! each must see the effect of the ones before it (a folder created, then
//! a file inside it; a marker, then the entry coming back). Committing
//! each as its own transaction would cost a thousand flushes per page
//! (SQLite's synchronous=FULL), which no first sync of a million files
//! could afford (docs/sync.md 7.9). So the page is one batch, and reads
//! during it go through this overlay of the rows written so far.

use std::collections::{BTreeMap, BTreeSet};

use crate::ids::{NodeId, RemoteId};
use crate::store::{Batch, Store, StoreResult, Write};
use crate::tree::{NodeRecord, RemoteRow};

/// Pending writes over a store, readable for the remote picture.
pub(super) struct Overlay<'a> {
    store: &'a dyn Store,
    nodes: BTreeMap<NodeId, Option<NodeRecord>>,
    by_remote: BTreeMap<RemoteId, Option<NodeId>>,
    added_kids: BTreeMap<RemoteId, BTreeSet<NodeId>>,
    /// The writes, in order.
    pub(super) writes: Batch,
}

impl<'a> Overlay<'a> {
    /// An empty overlay over a store.
    pub(super) fn new(store: &'a dyn Store) -> Overlay<'a> {
        Overlay {
            store,
            nodes: BTreeMap::new(),
            by_remote: BTreeMap::new(),
            added_kids: BTreeMap::new(),
            writes: Vec::new(),
        }
    }

    /// A node as it will be after the writes so far.
    pub(super) fn node(&self, n: NodeId) -> StoreResult<Option<NodeRecord>> {
        match self.nodes.get(&n) {
            Some(r) => Ok(r.clone()),
            None => self.store.node(n),
        }
    }

    /// The node holding a remote id, after the writes so far.
    pub(super) fn node_by_remote(&self, id: &RemoteId) -> StoreResult<Option<NodeId>> {
        match self.by_remote.get(id) {
            Some(n) => Ok(*n),
            None => self.store.node_by_remote(id),
        }
    }

    /// The nodes whose remote parent is `parent`, after the writes so far.
    pub(super) fn remote_children(&self, parent: &RemoteId) -> StoreResult<Vec<NodeId>> {
        let mut out: BTreeSet<NodeId> = self.store.remote_children(parent)?.into_iter().collect();
        if let Some(extra) = self.added_kids.get(parent) {
            out.extend(extra.iter().copied());
        }
        let mut v = Vec::new();
        for n in out {
            if let Some(rec) = self.node(n)? {
                if rec.remote.as_ref().and_then(|r| r.parent.as_ref()) == Some(parent) {
                    v.push(n);
                }
            }
        }
        Ok(v)
    }

    /// Adds a write that creates a node.
    pub(super) fn create_node(&mut self, n: NodeId, mount: crate::ids::MountId) {
        self.nodes.insert(
            n,
            Some(NodeRecord {
                node: n,
                mount: mount.clone(),
                remote: None,
                local: None,
                synced: None,
            }),
        );
        self.writes.push(Write::PutNode(n, mount));
    }

    /// Adds a write that moves a node to another mount.
    pub(super) fn set_mount(&mut self, n: NodeId, mount: crate::ids::MountId) -> StoreResult<()> {
        if let Some(mut rec) = self.node(n)? {
            rec.mount = mount.clone();
            self.nodes.insert(n, Some(rec));
        }
        self.writes.push(Write::PutNode(n, mount));
        Ok(())
    }

    /// Adds a write of the remote picture.
    pub(super) fn put_remote(&mut self, row: RemoteRow) -> StoreResult<()> {
        let n = row.node;
        let mut rec = match self.node(n)? {
            Some(r) => r,
            None => return Ok(()),
        };
        if let Some(old) = &rec.remote {
            self.by_remote.insert(old.id.clone(), None);
        }
        self.by_remote.insert(row.id.clone(), Some(n));
        if let Some(p) = &row.parent {
            self.added_kids.entry(p.clone()).or_default().insert(n);
        }
        rec.remote = Some(row.clone());
        self.nodes.insert(n, Some(rec));
        self.writes.push(Write::PutRemote(row));
        Ok(())
    }

    /// Adds a write removing the remote picture.
    pub(super) fn del_remote(&mut self, n: NodeId) -> StoreResult<()> {
        if let Some(mut rec) = self.node(n)? {
            if let Some(old) = rec.remote.take() {
                self.by_remote.insert(old.id, None);
            }
            self.nodes.insert(n, Some(rec));
        }
        self.writes.push(Write::DelRemote(n));
        Ok(())
    }

    /// Adds any other write (one that changes nothing the overlay reads).
    pub(super) fn push(&mut self, w: Write) {
        self.writes.push(w);
    }
}
