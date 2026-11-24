//! What the engine lets checkers and status screens see
//! (docs/observation.md). A snapshot, read from the store in one pass; it
//! says what the engine holds, never how it decides.

use std::collections::{BTreeMap, BTreeSet};

use crate::engine::PauseReason;
use crate::ids::{
    Cursor, Etag, FileId, Hash, HeldId, Kind, MountId, NodeId, OpId, Perms, RemoteId,
};
use crate::names::{LocalName, LocalPath, NameKey};
use crate::store::{Store, StoreResult};
use crate::tree::{Condition, HeldReason, MountRow, OpKind, ProblemKind, PurgeBasis, Subject};
use crate::view;

/// Where the engine is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Before the first full start (hello, root guard, recovery).
    Starting,
    /// Syncing or idle.
    Running,
    /// Stopped by a guard.
    Paused(PauseReason),
}

/// One mount as observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountView {
    /// Hearth's id.
    pub id: MountId,
    /// The storage key.
    pub storage: String,
    /// The restore epoch.
    pub epoch: String,
    /// The node of its root entry.
    pub root_node: NodeId,
    /// Where it sits under the sync root.
    pub local_path: Vec<String>,
    /// The cursor durably applied.
    pub cursor: Option<Cursor>,
    /// Caught up, not listing, nothing departed waiting (6.6 rule 7).
    pub settled: bool,
    /// The person chose to sync it.
    pub selected: bool,
    /// Rights bits.
    pub perms: Perms,
    /// Deletes go to Hearth's trash.
    pub trash: bool,
    /// Replaced content becomes a version.
    pub versions: bool,
}

/// Hearth's picture of a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteView {
    /// Entry id.
    pub id: RemoteId,
    /// Parent entry id.
    pub parent: Option<RemoteId>,
    /// NFC name.
    pub name: String,
    /// File or folder.
    pub kind: Kind,
    /// Bytes.
    pub size: u64,
    /// Etag.
    pub etag: Option<Etag>,
    /// Hash.
    pub hash: Option<Hash>,
    /// Execute bit.
    pub exec: bool,
    /// Sequence number.
    pub seq: u64,
    /// Modification time, microseconds.
    pub mtime_us: i64,
}

/// The disk's picture of a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalView {
    /// Parent node.
    pub parent: Option<NodeId>,
    /// Name bytes on disk.
    pub name: LocalName,
    /// Comparison key on this volume.
    pub name_key: NameKey,
    /// File or folder.
    pub kind: Kind,
    /// File id.
    pub file_id: FileId,
    /// Bytes.
    pub size: u64,
    /// Modification time, ns.
    pub mtime_ns: i64,
    /// Status change time, ns.
    pub ctime_ns: i64,
    /// Execute bit.
    pub exec: bool,
    /// Hash, once hashed at rest.
    pub hash: Option<Hash>,
}

/// The base of a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedView {
    /// Parent node then.
    pub parent: Option<NodeId>,
    /// NFC name then.
    pub name: String,
    /// File or folder.
    pub kind: Kind,
    /// Bytes.
    pub size: u64,
    /// Hash then.
    pub hash: Option<Hash>,
    /// Hearth's etag then.
    pub remote_etag: Option<Etag>,
    /// Local file id then.
    pub local_file_id: FileId,
}

/// One node as observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    /// The engine's id.
    pub node: NodeId,
    /// The mount it belongs to.
    pub mount: MountId,
    /// Hearth's picture.
    pub remote: Option<RemoteView>,
    /// The disk's picture.
    pub local: Option<LocalView>,
    /// The base.
    pub synced: Option<SyncedView>,
    /// Its path by the local picture.
    pub local_path: Option<LocalPath>,
    /// Its path by Hearth's picture (mount path and names).
    pub remote_path: Option<Vec<String>>,
    /// Its listed problem, if any.
    pub problem: Option<ProblemKind>,
    /// Out of sync by choice or rule: below a folder the person chose not
    /// to sync, below an end-to-end encrypted folder, or (on Hearth only)
    /// matched by the ignore rules. Such a node is not expected to
    /// converge (I8 speaks of synced paths).
    pub excluded: bool,
}

/// The condition an operation's remote write carries.
pub type ConditionView = Condition;

/// One operation in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpView {
    /// Its id.
    pub id: OpId,
    /// What it does.
    pub kind: OpKind,
    /// The node.
    pub node: NodeId,
    /// The durable point reached, for logs.
    pub state: String,
    /// Tries so far.
    pub attempt: u32,
    /// Not before (ns).
    pub next_try: Option<i64>,
    /// Its remote write's condition.
    pub condition: Option<ConditionView>,
}

/// One listed problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemView {
    /// The node, if it has one.
    pub node: Option<NodeId>,
    /// A path for people.
    pub path: String,
    /// The kind.
    pub kind: ProblemKind,
    /// Words for people.
    pub reason: String,
    /// Times seen.
    pub count: u32,
}

/// One holding-area entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldView {
    /// Its id.
    pub id: HeldId,
    /// Where it was.
    pub original_path: LocalPath,
    /// Where it is.
    pub held_path: LocalPath,
    /// Its content.
    pub hash: Option<Hash>,
    /// Bytes.
    pub size: u64,
    /// Why.
    pub reason: HeldReason,
    /// When (ns).
    pub at: i64,
    /// Why it may be emptied.
    pub basis: PurgeBasis,
}

/// Which side of a mount a mass change hits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Side {
    /// The device.
    Local,
    /// Hearth.
    Remote,
}

/// One mount and side over the mass-change thresholds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MassChangeItem {
    /// The mount.
    pub mount: MountId,
    /// The side.
    pub side: Side,
    /// Files the plan would delete or overwrite there.
    pub count: u64,
    /// Items the mount has.
    pub total: u64,
    /// The first few paths, for the person.
    pub examples: Vec<String>,
}

/// A paused mass change (docs/sync.md 3.11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MassChangeView {
    /// Pass this to `Engine::approve_mass_change` to let exactly this plan
    /// go on.
    pub token: u64,
    /// What is over the thresholds.
    pub items: Vec<MassChangeItem>,
}

/// Everything observable at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// Where the engine is.
    pub phase: Phase,
    /// The mounts.
    pub mounts: Vec<MountView>,
    /// Every node, by id.
    pub nodes: Vec<NodeView>,
    /// Operations in flight, by id.
    pub ops: Vec<OpView>,
    /// Listed problems.
    pub problems: Vec<ProblemView>,
    /// The holding area.
    pub held: Vec<HeldView>,
    /// A paused mass change.
    pub mass_change: Option<MassChangeView>,
    /// Mount roots found missing on disk (their mounts are left alone).
    pub absent_roots: Vec<NodeId>,
}

impl Observation {
    /// The node with this id.
    pub fn node(&self, n: NodeId) -> Option<&NodeView> {
        self.nodes
            .binary_search_by_key(&n, |v| v.node)
            .ok()
            .map(|i| &self.nodes[i])
    }
}

/// Reads the snapshot.
pub fn observe(
    store: &dyn Store,
    phase: Phase,
    absent_roots: &BTreeSet<NodeId>,
    ignore: &crate::ignore::IgnoreRules,
) -> StoreResult<Observation> {
    let unselected: BTreeSet<NodeId> = store.unselected()?.into_iter().collect();
    let mount_rows = store.mounts()?;
    let mounts_by_id: BTreeMap<MountId, MountRow> = mount_rows
        .iter()
        .map(|m| (m.id.clone(), m.clone()))
        .collect();
    let departed = !store.remote_departed()?.is_empty();
    let mounts = mount_rows
        .iter()
        .map(|m| MountView {
            id: m.id.clone(),
            storage: m.storage.clone(),
            epoch: m.epoch.clone(),
            root_node: m.root_node,
            local_path: m.local_path.clone(),
            cursor: m.cursor.clone(),
            settled: m.cursor.is_some() && m.listing.is_none() && m.caught_up && !departed,
            selected: m.selected,
            perms: m.perms,
            trash: m.trash,
            versions: m.versions,
        })
        .collect();
    let problems_rows = store.problems()?;
    let mut node_problem = BTreeMap::new();
    let mut problems = Vec::new();
    for p in &problems_rows {
        let (node, path) = match &p.subject {
            Subject::Node(n) => {
                node_problem.insert(*n, p.kind);
                (
                    Some(*n),
                    view::local_path(store, *n)?
                        .map(|lp| lp.display())
                        .or(view::remote_path(store, &mounts_by_id, *n)?.map(|v| v.join("/")))
                        .unwrap_or_default(),
                )
            }
            Subject::Path(lp) => (None, lp.display()),
        };
        problems.push(ProblemView {
            node,
            path,
            kind: p.kind,
            reason: p.reason.clone(),
            count: p.count,
        });
    }
    let mut nodes = Vec::new();
    for n in store.all_nodes()? {
        let Some(rec) = store.node(n)? else { continue };
        nodes.push(NodeView {
            node: n,
            mount: rec.mount.clone(),
            remote: rec.remote.as_ref().map(|r| RemoteView {
                id: r.id.clone(),
                parent: r.parent.clone(),
                name: r.name.clone(),
                kind: r.kind,
                size: r.size,
                etag: r.etag.clone(),
                hash: r.hash,
                exec: r.exec,
                seq: r.seq,
                mtime_us: r.mtime_us,
            }),
            local: rec.local.as_ref().map(|l| LocalView {
                parent: l.parent,
                name: l.name.clone(),
                name_key: l.key.clone(),
                kind: l.kind,
                file_id: l.file_id,
                size: l.size,
                mtime_ns: l.mtime_ns,
                ctime_ns: l.ctime_ns,
                exec: l.exec,
                hash: l.hash,
            }),
            synced: rec.synced.as_ref().map(|s| SyncedView {
                parent: s.parent,
                name: s.name.clone(),
                kind: s.kind,
                size: s.size,
                hash: s.hash,
                remote_etag: s.remote_etag.clone(),
                local_file_id: s.local_file_id,
            }),
            local_path: if rec.local.is_some() {
                view::local_path(store, n)?
            } else {
                None
            },
            remote_path: if rec.remote.is_some() {
                view::remote_path(store, &mounts_by_id, n)?
            } else {
                None
            },
            problem: node_problem.get(&n).copied(),
            excluded: view::exclusion(store, &unselected, &rec)? != view::Exclusion::None
                || mounts_by_id.get(&rec.mount).is_none_or(|m| !m.selected)
                || (rec.local.is_none()
                    && rec.synced.is_none()
                    && view::remote_ignored(store, &mounts_by_id, ignore, &rec)?),
        });
    }
    let ops = store
        .ops()?
        .into_iter()
        .map(|o| OpView {
            id: o.id,
            kind: o.kind,
            node: o.node,
            state: format!("{:?}", o.state),
            attempt: o.attempt,
            next_try: o.next_try_ns,
            condition: o.condition,
        })
        .collect();
    let held = store
        .held()?
        .into_iter()
        .map(|h| HeldView {
            id: h.id,
            original_path: h.original,
            held_path: h.held,
            hash: h.hash,
            size: h.size,
            reason: h.reason,
            at: h.at_ns,
            basis: h.basis,
        })
        .collect();
    let mass_change = match &phase {
        Phase::Paused(PauseReason::MassChange(v)) => Some(v.clone()),
        _ => None,
    };
    Ok(Observation {
        phase,
        mounts,
        nodes,
        ops,
        problems,
        held,
        mass_change,
        absent_roots: absent_roots.iter().copied().collect(),
    })
}
