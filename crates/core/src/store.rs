//! The core's interface to its state database (docs/sync.md 3.2), and the
//! consistency checker that runs after every simulated crash.
//!
//! Why the interface is shaped this way:
//!
//! - Reads are small and indexed (a node, its children, a lookup by id or
//!   by name key), so the planner works through indexes from a dirty set
//!   and never holds the tree in memory (3.12: a million files in 150 MB).
//! - Every change is a `Batch`: a list of writes applied all or nothing.
//!   A tree update and the journal entry of the operation that caused it
//!   commit together (3.2, "completion last"). There is no other way to
//!   write, so no code path can leave half a change behind.
//! - Two implementations follow the same contract: `MemStore` here (for
//!   the simulator, and the reference for the other) and the SQLite store
//!   in `hearth-sync-store`. The conformance suite (`store::conformance`)
//!   runs against both.

use std::fmt;

use crate::ids::{FileId, MountId, NodeId, OpId, RemoteId};
use crate::names::NameKey;
use crate::tree::{
    ConflictRow, HeldRow, LocalRow, MountRow, NodeRecord, OpRow, ProblemRow, RemoteRow, Subject,
    SyncedRow, TransferRow,
};

#[cfg(any(test, feature = "testkit"))]
pub mod conformance;

/// Keys of the `meta` table: facts about the account, the server, the
/// device and the root, and the engine's counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MetaKey {
    /// Hearth's instance id the engine first connected to.
    InstanceId,
    /// The random id written in `.hearthsync/root` at set-up.
    RootMarker,
    /// The sync root's volume, as probed at set-up.
    Volume,
    /// The device id Hearth gave (sync-api.md 3).
    DeviceId,
    /// The last `mountsTag` read.
    MountsTag,
    /// The next node id to give.
    NextNode,
    /// The next operation id to give.
    NextOp,
    /// The next holding-area id to give.
    NextHeld,
    /// The next conflict record id to give.
    NextConflict,
    /// The feed round counter (contract 6.6 rule 7).
    FeedRound,
    /// The next listing generation.
    NextGeneration,
    /// The ignore rules, one per line.
    IgnoreRules,
    /// When the holding area was last trimmed (ns).
    LastPurge,
    /// The node of the sync root, kept across a forgotten instance.
    RootNode,
}

impl MetaKey {
    /// Every key, for storage and dumps.
    pub const ALL: [MetaKey; 14] = [
        MetaKey::InstanceId,
        MetaKey::RootMarker,
        MetaKey::Volume,
        MetaKey::DeviceId,
        MetaKey::MountsTag,
        MetaKey::NextNode,
        MetaKey::NextOp,
        MetaKey::NextHeld,
        MetaKey::NextConflict,
        MetaKey::FeedRound,
        MetaKey::NextGeneration,
        MetaKey::IgnoreRules,
        MetaKey::LastPurge,
        MetaKey::RootNode,
    ];

    /// The key's name in the table.
    pub fn as_str(self) -> &'static str {
        match self {
            MetaKey::InstanceId => "instance_id",
            MetaKey::RootMarker => "root_marker",
            MetaKey::Volume => "volume",
            MetaKey::DeviceId => "device_id",
            MetaKey::MountsTag => "mounts_tag",
            MetaKey::NextNode => "next_node",
            MetaKey::NextOp => "next_op",
            MetaKey::NextHeld => "next_held",
            MetaKey::NextConflict => "next_conflict",
            MetaKey::FeedRound => "feed_round",
            MetaKey::NextGeneration => "next_generation",
            MetaKey::IgnoreRules => "ignore_rules",
            MetaKey::LastPurge => "last_purge",
            MetaKey::RootNode => "root_node",
        }
    }
}

/// One change inside a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// Set a meta value.
    PutMeta(MetaKey, String),
    /// Remove a meta value.
    DelMeta(MetaKey),
    /// Insert or replace a mount.
    PutMount(MountRow),
    /// Remove a mount (its nodes are removed by their own writes).
    DelMount(MountId),
    /// Create a node, or move it to another mount.
    PutNode(NodeId, MountId),
    /// Remove a node and every row about it (pictures, dirty mark,
    /// problems). Operations on it must be removed first.
    DelNode(NodeId),
    /// Insert or replace the remote picture. Fails if another node holds
    /// the same remote id.
    PutRemote(RemoteRow),
    /// Remove the remote picture.
    DelRemote(NodeId),
    /// Insert or replace the local picture. Fails if another node in the
    /// same folder holds the same name key.
    PutLocal(LocalRow),
    /// Remove the local picture.
    DelLocal(NodeId),
    /// Insert or replace the base.
    PutSynced(SyncedRow),
    /// Remove the base.
    DelSynced(NodeId),
    /// Insert or replace an operation.
    PutOp(OpRow),
    /// Remove an operation and its transfer.
    DelOp(OpId),
    /// Insert or replace a transfer's state. Its operation must exist.
    PutTransfer(TransferRow),
    /// Insert or replace a problem.
    PutProblem(ProblemRow),
    /// Remove a problem.
    DelProblem(Subject),
    /// Insert or replace a holding-area entry.
    PutHeld(HeldRow),
    /// Remove a holding-area entry (it was emptied).
    DelHeld(crate::ids::HeldId),
    /// Record a conflict copy.
    PutConflict(ConflictRow),
    /// Mark a node as needing the planner (and end any wait of it).
    MarkDirty(NodeId),
    /// Park a dirty node until another node changes: the second id is the
    /// node it waits for, or `WAIT_ANY` for "any completed operation".
    /// Parked nodes are not returned by `dirty` (so a first sync of a
    /// million files does not plan the waiting ones again after every
    /// step), and are made dirty again by the engine in the batch that
    /// changes what they wait for.
    Wait(NodeId, NodeId),
    /// Unmark it (dirty or waiting).
    ClearDirty(NodeId),
    /// The person chose not to sync this folder (3.13).
    Unselect(NodeId),
    /// The person chose to sync it again.
    Reselect(NodeId),
}

/// A list of writes applied all or nothing.
pub type Batch = Vec<Write>;

/// The target of a wait for "any completed operation" (no node has id 0).
pub const WAIT_ANY: NodeId = NodeId(0);

/// What went wrong in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// A write broke a promise of the schema (a duplicate name key, a
    /// remote id on two nodes, a transfer without its operation). Always
    /// a bug in the engine; the batch was not applied.
    Constraint(String),
    /// The database could not be read or written (disk full, I/O error).
    /// The last committed state stands.
    Io(String),
    /// The database's content is not what this engine wrote.
    Corrupt(String),
    /// A failure the tests injected.
    Injected,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Constraint(s) => write!(f, "store constraint: {s}"),
            StoreError::Io(s) => write!(f, "store I/O: {s}"),
            StoreError::Corrupt(s) => write!(f, "store corrupt: {s}"),
            StoreError::Injected => write!(f, "store failure injected by a test"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Shorthand for store results.
pub type StoreResult<T> = Result<T, StoreError>;

/// The state database as the core sees it. Lists come back sorted by their
/// key (node id, op id...) so nothing the engine decides depends on a
/// map's order (I13).
pub trait Store {
    /// A meta value.
    fn meta(&self, key: MetaKey) -> StoreResult<Option<String>>;
    /// Every mount, by id.
    fn mounts(&self) -> StoreResult<Vec<MountRow>>;
    /// Everything about one node.
    fn node(&self, node: NodeId) -> StoreResult<Option<NodeRecord>>;
    /// The node holding a remote id.
    fn node_by_remote(&self, id: &RemoteId) -> StoreResult<Option<NodeId>>;
    /// The nodes whose local picture has this file id (hard links share
    /// one; usually zero or one).
    fn nodes_by_file_id(&self, fid: FileId) -> StoreResult<Vec<NodeId>>;
    /// The nodes whose local picture is in this folder.
    fn local_children(&self, parent: NodeId) -> StoreResult<Vec<NodeId>>;
    /// The node whose local picture is in this folder under this key.
    fn local_child_by_key(&self, parent: NodeId, key: &NameKey) -> StoreResult<Option<NodeId>>;
    /// The nodes whose remote picture names this folder as parent.
    fn remote_children(&self, parent: &RemoteId) -> StoreResult<Vec<NodeId>>;
    /// The nodes whose base names this folder as parent.
    fn synced_children(&self, parent: NodeId) -> StoreResult<Vec<NodeId>>;
    /// Every node of a mount (rare: end of a listing, resync).
    fn mount_nodes(&self, mount: &MountId) -> StoreResult<Vec<NodeId>>;
    /// How many nodes a mount has (the mass-change guard's denominator).
    fn count_nodes(&self, mount: &MountId) -> StoreResult<u64>;
    /// Every node (observation and checks only).
    fn all_nodes(&self) -> StoreResult<Vec<NodeId>>;
    /// Every operation in flight.
    fn ops(&self) -> StoreResult<Vec<OpRow>>;
    /// A transfer's state.
    fn transfer(&self, op: OpId) -> StoreResult<Option<TransferRow>>;
    /// Every problem.
    fn problems(&self) -> StoreResult<Vec<ProblemRow>>;
    /// Every holding-area entry.
    fn held(&self) -> StoreResult<Vec<HeldRow>>;
    /// Every recorded conflict copy.
    fn conflicts(&self) -> StoreResult<Vec<ConflictRow>>;
    /// Up to `limit` dirty nodes above `after`, lowest ids first (not the
    /// waiting ones).
    fn dirty(&self, after: Option<NodeId>, limit: usize) -> StoreResult<Vec<NodeId>>;
    /// The nodes waiting for `target` (or for `WAIT_ANY`).
    fn waiters_on(&self, target: NodeId) -> StoreResult<Vec<NodeId>>;
    /// Up to `limit` waiting nodes, lowest ids first.
    fn waiting(&self, limit: usize) -> StoreResult<Vec<NodeId>>;
    /// Nodes whose remote picture is marked departed (contract 6.6 rule 5).
    fn remote_departed(&self) -> StoreResult<Vec<NodeId>>;
    /// Folders the person chose not to sync.
    fn unselected(&self) -> StoreResult<Vec<NodeId>>;
    /// Applies a batch: every write or none.
    fn apply(&mut self, batch: Batch) -> StoreResult<()>;
    /// Makes the next `apply` calls fail at their `n`-th write (counting
    /// from zero, across calls) with `StoreError::Injected`, leaving the
    /// committed state as it was; `None` turns it off. For the crash tests
    /// and the simulator's power cuts (docs/sync.md 7.6).
    fn fail_writes_after(&mut self, n: Option<usize>);
}

/// Checks the schema's promises (docs/sync.md 3.2): every local parent
/// exists and is a folder, no cycles, unique name keys per folder, every
/// base parent exists, no operation on a missing node, every transfer has
/// its operation, every mount's root node exists. Returns what is wrong;
/// empty means consistent. Runs after every simulated crash (I11).
pub fn check_consistency(store: &dyn Store) -> StoreResult<Vec<String>> {
    let mut bad = Vec::new();
    let nodes = store.all_nodes()?;
    for &n in &nodes {
        let Some(rec) = store.node(n)? else {
            bad.push(format!("{n} listed but not readable"));
            continue;
        };
        if let Some(l) = &rec.local {
            if l.node != n {
                bad.push(format!("{n}: local row names {}", l.node));
            }
            if let Some(p) = l.parent {
                match store.node(p)? {
                    Some(pr) => match &pr.local {
                        Some(pl) if pl.kind == crate::ids::Kind::Folder => {}
                        Some(_) => bad.push(format!("{n}: local parent {p} is a file")),
                        None => bad.push(format!("{n}: local parent {p} has no local row")),
                    },
                    None => bad.push(format!("{n}: local parent {p} missing")),
                }
                if store.local_child_by_key(p, &l.key)? != Some(n) {
                    bad.push(format!("{n}: name key not indexed under {p}"));
                }
            }
            // Walk up: a cycle would never reach a root.
            let mut seen = 0usize;
            let mut cur = l.parent;
            while let Some(p) = cur {
                seen += 1;
                if seen > nodes.len() + 1 {
                    bad.push(format!("{n}: local parents form a cycle"));
                    break;
                }
                cur = store.node(p)?.and_then(|r| r.local).and_then(|l| l.parent);
            }
        }
        if let Some(s) = &rec.synced
            && let Some(p) = s.parent
            && store.node(p)?.is_none()
        {
            bad.push(format!("{n}: base parent {p} missing"));
        }
        if let Some(r) = &rec.remote
            && store.node_by_remote(&r.id)? != Some(n)
        {
            bad.push(format!("{n}: remote id {} not indexed", r.id));
        }
    }
    for op in store.ops()? {
        if store.node(op.node)?.is_none() {
            bad.push(format!("{}: node {} missing", op.id, op.node));
        }
    }
    for m in store.mounts()? {
        if store.node(m.root_node)?.is_none() {
            bad.push(format!("mount {}: root node {} missing", m.id, m.root_node));
        }
    }
    Ok(bad)
}
