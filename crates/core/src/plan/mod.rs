//! The planner: from the three pictures of a node, the next single thing to
//! do for it (docs/sync.md 3.5).
//!
//! It is a pure function of the store's rows and a few sets the engine
//! keeps: it reads no clock, disk or network. For each dirty node it
//! answers one `Decision`: nothing, wait, a change of the store alone
//! (record an agreement, forget a node, merge or split nodes), one
//! operation with its condition, or a problem to list.
//!
//! Why one step per node and pass: a composite cell of the table ("move,
//! then download") is done as its first step; when that completes the base
//! records the part both sides now agree on, the node is dirty again, and
//! the next pass finds it in a simpler cell. Each step's precondition is
//! then checked against the world as it is after the previous one, never
//! against a plan made before it. Conditions on every remote write (I3)
//! make a stale step fail safely rather than act on old facts.
//!
//! The table itself is `table`: data, exhaustive. This module checks what
//! the table assumes before a step may run: the target folder exists on
//! that side, the name is free (or who keeps it, by "server keeps the
//! name"), the device can hold the name and Hearth accepts it, the mount's
//! rights allow it, no cycle would be made, and nothing is excluded by
//! selective sync, ignore rules or end-to-end encryption.

pub mod table;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::ids::{Hash, Kind, MountId, NodeId, Perms};
use crate::ignore::IgnoreRules;
use crate::names::{HoldRefusal, RemoteRefusal, hold_refusal, key_of_str, remote_refusal};
use crate::store::{Store, StoreResult, Write};
use crate::tree::{Condition, MountRow, NodeRecord, OpKind, ProblemKind, SyncedRow};
use crate::view::{self, Place};
use table::{FileChange, FileStep, FolderCell, FolderChange, file_cell, file_step, folder_cell};

/// One operation the planner asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedOp {
    /// What it does.
    pub kind: OpKind,
    /// The condition of its remote write, when it writes to Hearth.
    pub condition: Option<Condition>,
    /// The folder it puts something in.
    pub target_parent: Option<NodeId>,
    /// The name it gives.
    pub target_name: Option<String>,
    /// The content it moves.
    pub expect_hash: Option<Hash>,
    /// A problem to list with it (a read-only restore says why).
    pub problem: Option<(ProblemKind, String)>,
}

impl PlannedOp {
    fn new(kind: OpKind) -> PlannedOp {
        PlannedOp {
            kind,
            condition: None,
            target_parent: None,
            target_name: None,
            expect_hash: None,
            problem: None,
        }
    }

    fn cond(mut self, c: Condition) -> PlannedOp {
        self.condition = Some(c);
        self
    }

    fn to(mut self, place: &Place) -> PlannedOp {
        self.target_parent = place.parent;
        self.target_name = Some(place.name.clone());
        self
    }

    fn expect(mut self, h: Option<Hash>) -> PlannedOp {
        self.expect_hash = h;
        self
    }

    fn listing(mut self, kind: ProblemKind, why: &str) -> PlannedOp {
        self.problem = Some((kind, why.to_owned()));
        self
    }
}

/// The planner's answer for one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Converged, or nothing this engine may do: no longer dirty.
    Clean,
    /// Something else must happen first (a parent made, a name freed, a
    /// child moved away): planned again when the node named changes, or,
    /// with none named, when any operation completes.
    Wait(Option<NodeId>),
    /// A fact is missing that only a change will bring (a file not yet
    /// hashed at rest): no longer dirty; the change that brings the fact
    /// makes it dirty again.
    NotReady,
    /// A change of the store alone; the node is planned again after it.
    Record(Vec<Write>),
    /// One operation.
    Op(PlannedOp),
    /// List it (I10) and leave it alone until something changes.
    Problem(ProblemKind, String),
}

/// A side of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    /// The disk.
    Local,
    /// Hearth.
    Remote,
}

/// What a check of a target found.
enum Target {
    /// Free: go ahead.
    Free,
    /// Not yet: wait (for this node, when known).
    Wait(Option<NodeId>),
    /// The decision to take instead.
    Instead(Decision),
}

/// The planner for one pass. Built by the engine with the sets it keeps;
/// read-only over the store.
pub struct Planner<'a> {
    /// The state database.
    pub store: &'a dyn Store,
    /// The settings.
    pub cfg: &'a Config,
    /// Every mount, by id.
    pub mounts: &'a BTreeMap<MountId, MountRow>,
    /// Folders the person chose not to sync.
    pub unselected: &'a BTreeSet<NodeId>,
    /// Mount roots found missing on disk (their mounts are left alone).
    pub absent_roots: &'a BTreeSet<NodeId>,
    /// The compiled ignore rules.
    pub ignore: &'a IgnoreRules,
    /// Nodes with an operation in flight, and those given one in this
    /// pass.
    in_flight: RefCell<BTreeSet<NodeId>>,
    /// Folders whose one-request delete Hearth refused: delete child by
    /// child.
    pub split_deletes: &'a BTreeSet<NodeId>,
    /// Hearth's longest name.
    pub name_bytes_max: usize,
    /// Nodes listed as problems (they will not move until something
    /// changes).
    pub problems: &'a BTreeSet<NodeId>,
    /// Nodes whose move Hearth refused for lack of rights (a 403 after the
    /// rights changed): their local move is undone (R18).
    pub refused_moves: &'a BTreeSet<NodeId>,
    /// The sync root's node.
    pub root: NodeId,
    /// The next node id to give (for splits); the engine stores it after.
    pub next_node: &'a Cell<u64>,
    deletable: RefCell<BTreeMap<NodeId, bool>>,
    /// Per pass: each folder's exclusion and its path on Hearth, so a
    /// first sync of a million files reads each folder once, not once per
    /// file below it.
    excluded: RefCell<BTreeMap<NodeId, view::Exclusion>>,
    notes: RefCell<BTreeMap<NodeId, String>>,
    remote_paths: RefCell<BTreeMap<NodeId, Option<Vec<String>>>>,
}

impl<'a> Planner<'a> {
    /// A planner for one pass.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: &'a dyn Store,
        cfg: &'a Config,
        mounts: &'a BTreeMap<MountId, MountRow>,
        unselected: &'a BTreeSet<NodeId>,
        absent_roots: &'a BTreeSet<NodeId>,
        ignore: &'a IgnoreRules,
        in_flight: BTreeSet<NodeId>,
        split_deletes: &'a BTreeSet<NodeId>,
        name_bytes_max: usize,
        problems: &'a BTreeSet<NodeId>,
        refused_moves: &'a BTreeSet<NodeId>,
        root: NodeId,
        next_node: &'a Cell<u64>,
    ) -> Planner<'a> {
        Planner {
            store,
            cfg,
            mounts,
            unselected,
            absent_roots,
            ignore,
            in_flight: RefCell::new(in_flight),
            split_deletes,
            name_bytes_max,
            problems,
            refused_moves,
            root,
            next_node,
            deletable: RefCell::new(BTreeMap::new()),
            excluded: RefCell::new(BTreeMap::new()),
            notes: RefCell::new(BTreeMap::new()),
            remote_paths: RefCell::new(BTreeMap::new()),
        }
    }

    /// Records that a node was given an operation in this pass, so later
    /// decisions of the pass treat it as busy.
    pub fn mark_busy(&self, n: NodeId) {
        self.in_flight.borrow_mut().insert(n);
    }

    fn busy(&self, n: &NodeId) -> bool {
        self.in_flight.borrow().contains(n)
    }

    /// The decision for one node.
    pub fn plan(&self, n: NodeId) -> StoreResult<Decision> {
        if self.busy(&n) {
            // Its own operation's end makes it dirty again.
            return Ok(Decision::Wait(Some(n)));
        }
        let Some(rec) = self.store.node(n)? else {
            return Ok(Decision::Clean);
        };
        if rec.local.is_none() && rec.remote.is_none() && rec.synced.is_none() {
            return self.drop_node(&rec);
        }
        let Some(mount) = self.mounts.get(&rec.mount) else {
            return Ok(Decision::Clean);
        };
        if self.absent_roots.contains(&mount.root_node) {
            return Ok(if n == mount.root_node {
                Decision::Problem(
                    ProblemKind::MountRootMissing,
                    "the folder of this mount is missing on disk; nothing is changed".into(),
                )
            } else {
                Decision::Wait(Some(mount.root_node))
            });
        }
        let local_only = rec.local.is_some() && rec.remote.is_none() && rec.synced.is_none();
        let remote_only = rec.remote.is_some() && rec.local.is_none() && rec.synced.is_none();
        if !mount.selected {
            return Ok(if local_only {
                Decision::Problem(ProblemKind::NotSelected, "in a mount not synced".into())
            } else {
                Decision::Clean
            });
        }
        match self.exclusion(&rec)? {
            view::Exclusion::None => {}
            view::Exclusion::E2ee => {
                return Ok(if local_only {
                    Decision::Problem(
                        ProblemKind::E2eeUnsupported,
                        "end-to-end encrypted folders are not yet supported on this device".into(),
                    )
                } else {
                    Decision::Clean
                });
            }
            view::Exclusion::Unselected => return self.plan_unselected(&rec, mount),
        }
        if remote_only && self.remote_ignored(&rec)? {
            return Ok(Decision::Clean);
        }
        // A local picture of another kind than the others: the name changed
        // kind on disk. It becomes a node of its own (3.5, retype).
        if let Some(l) = &rec.local {
            let other = rec
                .remote
                .as_ref()
                .map(|r| r.kind)
                .or(rec.synced.as_ref().map(|s| s.kind));
            if other.is_some_and(|k| k != l.kind) {
                return self.split_local(&rec);
            }
        }
        if n == mount.root_node {
            return self.plan_mount_root(&rec, mount);
        }
        // A local move into another mount's folder: that is a copy there and
        // a delete here (4.3), so the local picture becomes a new node.
        if let (Some(l), Some(_)) = (&rec.local, &rec.synced)
            && let Some(p) = l.parent
            && let Some(prec) = self.store.node(p)?
            && prec.mount != rec.mount
        {
            // Out of a mount without the right to delete, or into one
            // without the right to create: undone on the device (R18),
            // so the file's synced place is never the one it left.
            let into = self.mounts.get(&prec.mount);
            if !mount.perms.allows(Perms::DELETE)
                || into.is_none_or(|m| !m.perms.allows(Perms::CREATE))
            {
                return self.undo_move(&rec);
            }
            return self.split_local(&rec);
        }
        match &rec.synced {
            Some(s) => self.plan_with_base(&rec, s, mount),
            None => self.plan_creation(&rec, mount),
        }
    }

    // ----- exclusions -----

    /// Whether a node is out of sync by selection or encryption (the same
    /// answer as `view::exclusion`), with each folder's answer kept for the
    /// pass.
    fn exclusion(&self, rec: &NodeRecord) -> StoreResult<view::Exclusion> {
        if self.unselected.contains(&rec.node) {
            return Ok(view::Exclusion::Unselected);
        }
        let mut parents = Vec::new();
        if let Some(p) = rec.local.as_ref().and_then(|l| l.parent) {
            parents.push(p);
        }
        if let Some(p) = rec.remote.as_ref().and_then(|r| r.parent.clone())
            && let Some(pn) = self.store.node_by_remote(&p)?
        {
            parents.push(pn);
        }
        for p in parents {
            let e = self.folder_exclusion(p, 0)?;
            if e != view::Exclusion::None {
                return Ok(e);
            }
        }
        Ok(view::Exclusion::None)
    }

    /// A folder's exclusion for what is below it: unselected, the top of
    /// an end-to-end folder, or below either.
    fn folder_exclusion(&self, n: NodeId, depth: usize) -> StoreResult<view::Exclusion> {
        if let Some(e) = self.excluded.borrow().get(&n) {
            return Ok(*e);
        }
        // A provisional answer first: the two pictures' parents can form a
        // cycle for a moment (a local move against a remote one), and the
        // walk must end there instead of recursing for ever.
        self.excluded.borrow_mut().insert(n, view::Exclusion::None);
        let e = if depth > 1_000 {
            view::Exclusion::None
        } else if self.unselected.contains(&n) {
            view::Exclusion::Unselected
        } else {
            match self.store.node(n)? {
                None => view::Exclusion::None,
                Some(rec)
                    if rec
                        .remote
                        .as_ref()
                        .is_some_and(|r| r.crypt == crate::tree::Crypt::E2ee) =>
                {
                    view::Exclusion::E2ee
                }
                Some(rec) => {
                    let mut out = view::Exclusion::None;
                    let mut parents = Vec::new();
                    if let Some(p) = rec.local.as_ref().and_then(|l| l.parent) {
                        parents.push(p);
                    }
                    if let Some(p) = rec.remote.as_ref().and_then(|r| r.parent.clone())
                        && let Some(pn) = self.store.node_by_remote(&p)?
                    {
                        parents.push(pn);
                    }
                    for p in parents {
                        if p == n {
                            continue;
                        }
                        let x = self.folder_exclusion(p, depth + 1)?;
                        if x != view::Exclusion::None {
                            out = x;
                            break;
                        }
                    }
                    out
                }
            }
        };
        self.excluded.borrow_mut().insert(n, e);
        Ok(e)
    }

    /// Whether Hearth's item is matched by the ignore rules at its path,
    /// with each folder's path kept for the pass.
    fn remote_ignored(&self, rec: &NodeRecord) -> StoreResult<bool> {
        let Some(r) = &rec.remote else {
            return Ok(false);
        };
        let parent_path = match &r.parent {
            None => return Ok(false),
            Some(p) => match self.store.node_by_remote(p)? {
                Some(pn) => self.folder_remote_path(pn)?,
                None => None,
            },
        };
        let Some(mut parts) = parent_path else {
            return Ok(false);
        };
        parts.push(r.name.clone());
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        Ok(self.ignore.is_ignored(&refs, r.kind == Kind::Folder))
    }

    fn folder_remote_path(&self, n: NodeId) -> StoreResult<Option<Vec<String>>> {
        if let Some(p) = self.remote_paths.borrow().get(&n) {
            return Ok(p.clone());
        }
        let p = view::remote_path(self.store, self.mounts, n)?;
        self.remote_paths.borrow_mut().insert(n, p.clone());
        Ok(p)
    }

    /// A node inside a folder the person chose not to sync (3.13): what is
    /// on disk and synced leaves through the holding area; what is only
    /// local is listed; what is only on Hearth stays there.
    fn plan_unselected(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        match (&rec.local, &rec.remote, &rec.synced) {
            (Some(_), None, None) => Ok(Decision::Problem(
                ProblemKind::NotSelected,
                "a local item where syncing is switched off; not uploaded".into(),
            )),
            (Some(l), Some(r), Some(s)) => {
                let same_place = self.local_place(rec) == self.remote_place(rec)?;
                if l.kind == Kind::Folder {
                    if !same_place {
                        return self.plan_with_base(rec, s, mount);
                    }
                    if self.store.local_children(rec.node)?.is_empty() {
                        Ok(Decision::Op(PlannedOp::new(OpKind::RemoveLocalFolder)))
                    } else {
                        Ok(Decision::Wait(None))
                    }
                } else if l.hash.is_none() {
                    Ok(Decision::NotReady)
                } else if same_place && l.hash == s.hash && r.hash == s.hash {
                    Ok(Decision::Op(
                        PlannedOp::new(OpKind::DeleteLocal).expect(l.hash),
                    ))
                } else {
                    // Not yet the same on both sides: sync it first.
                    self.plan_with_base(rec, s, mount)
                }
            }
            _ => Ok(Decision::Clean),
        }
    }

    // ----- places -----

    fn local_place(&self, rec: &NodeRecord) -> Option<Place> {
        view::local_place(rec)
    }

    /// Hearth's place of a node as this device can follow it. When Hearth
    /// gives an item a name this device cannot hold, only the name stays
    /// behind (R10): the item keeps its local name in Hearth's folder, its
    /// content (and a folder's contents) go on syncing by identity, and the
    /// name is noted so the pass lists it (`other_name`).
    fn remote_place(&self, rec: &NodeRecord) -> StoreResult<Option<Place>> {
        let Some(p) =
            view::remote_place(self.store, self.mounts, self.root, &self.cfg.volume, rec)?
        else {
            return Ok(None);
        };
        let Some(lp) = self.local_place(rec) else {
            return Ok(Some(p));
        };
        let at_root = p.parent == Some(self.root);
        match (rec.remote.as_ref(), p.parent) {
            (Some(r), Some(_)) if hold_refusal(&p.name, &self.cfg.volume, at_root).is_some() => {
                self.notes.borrow_mut().insert(
                    rec.node,
                    format!(
                        "other_name: Hearth calls it {:?}, which this device cannot hold",
                        r.name
                    ),
                );
                Ok(Some(Place {
                    parent: p.parent,
                    name: lp.name,
                }))
            }
            _ => Ok(Some(p)),
        }
    }

    /// The name note R10 made for a node in this pass, if any.
    pub fn take_note(&self, n: NodeId) -> Option<String> {
        self.notes.borrow_mut().remove(&n)
    }

    // ----- the table -----

    /// How the local side changed against the base; none while the local
    /// content is not known yet.
    fn local_file_change(&self, rec: &NodeRecord, s: &SyncedRow) -> Option<FileChange> {
        let Some(l) = &rec.local else {
            return Some(FileChange::Delete);
        };
        let moved = self.local_place(rec) != view::base_place(rec);
        let edited = Some(l.hash?) != s.hash;
        Some(combine(edited, moved))
    }

    /// How Hearth's side changed against the base.
    fn remote_file_change(&self, rec: &NodeRecord, s: &SyncedRow) -> StoreResult<FileChange> {
        let Some(r) = &rec.remote else {
            return Ok(FileChange::Delete);
        };
        let moved = self.remote_place(rec)? != view::base_place(rec);
        let edited = match r.hash {
            Some(h) => Some(h) != s.hash,
            None => r.etag != s.remote_etag,
        };
        Ok(combine(edited, moved))
    }

    fn plan_with_base(
        &self,
        rec: &NodeRecord,
        s: &SyncedRow,
        mount: &MountRow,
    ) -> StoreResult<Decision> {
        match s.kind {
            Kind::File => self.plan_file(rec, s, mount),
            Kind::Folder => self.plan_folder(rec, s, mount),
        }
    }

    fn plan_file(
        &self,
        rec: &NodeRecord,
        s: &SyncedRow,
        mount: &MountRow,
    ) -> StoreResult<Decision> {
        let Some(lc) = self.local_file_change(rec, s) else {
            return Ok(Decision::NotReady);
        };
        let rc = self.remote_file_change(rec, s)?;
        let cell = file_cell(lc, rc);
        let lp = self.local_place(rec);
        let rp = self.remote_place(rec)?;
        let same_place = lp.is_some() && lp == rp;
        let l_hash = rec.local.as_ref().and_then(|l| l.hash);
        let r_hash = rec.remote.as_ref().and_then(|r| r.hash);
        let same_content = l_hash.is_some() && l_hash == r_hash;
        let step = file_step(cell, same_place, same_content);
        // R8: deleted on one side only because its folder was, after the
        // other side moved it out of that folder: it survives there.
        if step == FileStep::DeleteLocal && self.moved_out_of_deleted(rec, Side::Remote)? {
            return self.forget_bases_below(rec, Side::Remote);
        }
        if step == FileStep::DeleteRemote && self.moved_out_of_deleted(rec, Side::Local)? {
            return self.forget_bases_below(rec, Side::Local);
        }
        match step {
            FileStep::Nothing => {
                // The etag may have moved without a change of content (a
                // touch on Hearth): record it, so a file Hearth has not
                // hashed is not taken for edited later.
                let r = rec.remote.as_ref();
                if r.is_some_and(|r| r.etag != s.remote_etag) {
                    let mut s2 = s.clone();
                    s2.remote_etag = r.and_then(|r| r.etag.clone());
                    Ok(Decision::Record(vec![Write::PutSynced(s2)]))
                } else {
                    Ok(Decision::Clean)
                }
            }
            FileStep::Adopt => Ok(self.adopt(rec, s)),
            FileStep::Download => Ok(self.download(rec, None)),
            FileStep::DownloadNew => self.download_new(rec),
            FileStep::Upload => {
                let r = rec.remote.as_ref();
                if !mount.perms.allows(Perms::UPDATE) {
                    // Read-only: Hearth's version comes back; the edit is
                    // kept beside it as a conflict copy by the replace (3.5).
                    return Ok(match self.download(rec, None) {
                        Decision::Op(op) => Decision::Op(
                            op.listing(ProblemKind::ReadOnly, "not uploaded: read-only"),
                        ),
                        other => other,
                    });
                }
                let Some(cond) = r.and_then(file_condition) else {
                    return Ok(Decision::Wait(None));
                };
                Ok(Decision::Op(
                    PlannedOp::new(OpKind::Upload).cond(cond).expect(l_hash),
                ))
            }
            FileStep::UploadNew => self.upload_new(rec, mount),
            FileStep::MoveLocal => self.move_local(rec),
            FileStep::MoveRemote => self.move_remote(rec, mount),
            FileStep::DeleteLocal => Ok(Decision::Op(
                PlannedOp::new(OpKind::DeleteLocal).expect(s.hash),
            )),
            FileStep::DeleteRemote => {
                if !mount.perms.allows(Perms::DELETE) {
                    // A delete in a read-only folder: the file comes back.
                    return Ok(match self.download_new(rec)? {
                        Decision::Op(op) => Decision::Op(op.listing(
                            ProblemKind::ReadOnly,
                            "deleted locally in a read-only folder",
                        )),
                        other => other,
                    });
                }
                if self.under_whole_delete(rec)? {
                    return Ok(Decision::Wait(None));
                }
                let Some(cond) = rec.remote.as_ref().and_then(file_condition) else {
                    return Ok(Decision::Wait(None));
                };
                Ok(Decision::Op(
                    PlannedOp::new(OpKind::DeleteRemote).cond(cond),
                ))
            }
            FileStep::Drop => self.drop_node(rec),
        }
    }

    fn plan_folder(
        &self,
        rec: &NodeRecord,
        s: &SyncedRow,
        mount: &MountRow,
    ) -> StoreResult<Decision> {
        let bp = view::base_place(rec);
        let lc = match &rec.local {
            None => FolderChange::Delete,
            Some(_) if self.local_place(rec) != bp => FolderChange::Move,
            Some(_) => FolderChange::None,
        };
        let rc = match &rec.remote {
            None => FolderChange::Delete,
            Some(_) if self.remote_place(rec)? != bp => FolderChange::Move,
            Some(_) => FolderChange::None,
        };
        match folder_cell(lc, rc) {
            FolderCell::Nothing => Ok(Decision::Clean),
            FolderCell::MoveLocally => self.move_local(rec),
            FolderCell::MoveOnHearth => self.move_remote(rec, mount),
            FolderCell::C => {
                if self.local_place(rec) == self.remote_place(rec)? {
                    Ok(self.adopt(rec, s))
                } else {
                    self.move_local(rec)
                }
            }
            FolderCell::RemoveLocallyF | FolderCell::GLocalMoveRemoteDelete => {
                if self.moved_out_of_deleted(rec, Side::Remote)? {
                    return self.forget_bases_below(rec, Side::Remote);
                }
                self.folder_deleted_on_hearth(rec, mount)
            }
            FolderCell::DeleteOnHearthF | FolderCell::GLocalDeleteRemoteMove => {
                if self.moved_out_of_deleted(rec, Side::Local)? {
                    return self.forget_bases_below(rec, Side::Local);
                }
                self.folder_deleted_locally(rec, mount)
            }
            FolderCell::DropNode => self.drop_node(rec),
        }
    }

    /// R8: whether a node gone on side `gone` is gone only because a folder
    /// was deleted there after the other side had moved it (or a folder
    /// above it) out of that folder. Walks up the base: every node on the
    /// way is gone on that side; the first that the other side moved must
    /// have had, in the base, a folder that is itself gone on that side.
    /// A node deleted by itself (its folder still there) is not covered:
    /// cell D stands.
    fn moved_out_of_deleted(&self, rec: &NodeRecord, gone: Side) -> StoreResult<bool> {
        let mut cur = rec.clone();
        for _ in 0..10_000 {
            let present = match gone {
                Side::Remote => cur.remote.is_some(),
                Side::Local => cur.local.is_some(),
            };
            if present {
                return Ok(false);
            }
            let base = view::base_place(&cur);
            let moved = match gone {
                Side::Remote => cur.local.is_some() && self.local_place(&cur) != base,
                Side::Local => cur.remote.is_some() && self.remote_place(&cur)? != base,
            };
            let Some(p) = base.and_then(|b| b.parent) else {
                return Ok(false);
            };
            let Some(prec) = self.store.node(p)? else {
                return Ok(moved);
            };
            let parent_gone = match gone {
                Side::Remote => prec.remote.is_none(),
                Side::Local => prec.local.is_none(),
            };
            if moved {
                return Ok(parent_gone);
            }
            if !parent_gone {
                return Ok(false);
            }
            cur = prec;
        }
        Ok(false)
    }

    /// A survivor by R8 becomes new on the side it is gone from: its base,
    /// and that of everything below it that is gone on that side too, is
    /// forgotten, so the next pass uploads (or downloads) it as a creation.
    /// Safe by construction: an item with nothing on one side and no base
    /// is only ever copied to that side, never deleted from the other.
    fn forget_bases_below(&self, rec: &NodeRecord, gone: Side) -> StoreResult<Decision> {
        let mut w = Vec::new();
        let mut stack = vec![rec.node];
        let mut seen = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) || seen.len() > 1_000_000 {
                continue;
            }
            let Some(r) = self.store.node(n)? else {
                continue;
            };
            let gone_here = match gone {
                Side::Remote => r.remote.is_none(),
                Side::Local => r.local.is_none(),
            };
            if !gone_here || r.synced.is_none() {
                continue;
            }
            if self.busy(&n) {
                return Ok(Decision::Wait(Some(n)));
            }
            w.push(Write::DelSynced(n));
            w.push(Write::MarkDirty(n));
            stack.extend(self.store.synced_children(n)?);
        }
        Ok(Decision::Record(w))
    }

    /// F, Hearth's side gone: the folder leaves the disk only when empty;
    /// if anything in it survives, it is made again on Hearth.
    fn folder_deleted_on_hearth(
        &self,
        rec: &NodeRecord,
        mount: &MountRow,
    ) -> StoreResult<Decision> {
        let kids = self.store.local_children(rec.node)?;
        if kids.is_empty() {
            return Ok(Decision::Op(PlannedOp::new(OpKind::RemoveLocalFolder)));
        }
        let mut unknown = false;
        let mut leaving = None;
        for c in kids {
            match self.local_child_survives(c)? {
                Some(true) => return self.mkdir_remote(rec, mount),
                Some(false) => leaving = leaving.or(Some(c)),
                None => unknown = true,
            }
        }
        // Wait for a child that leaves: its going (or its being listed)
        // wakes this folder.
        Ok(if unknown {
            Decision::NotReady
        } else {
            Decision::Wait(leaving)
        })
    }

    /// Whether a local item inside a folder Hearth deleted stays (it is
    /// new, or edited, or holds something that stays). None: not known yet.
    fn local_child_survives(&self, n: NodeId) -> StoreResult<Option<bool>> {
        let Some(rec) = self.store.node(n)? else {
            return Ok(Some(false));
        };
        // A listed item is left where it is (I10), so it stays here, and so
        // does the folder that holds it.
        if self.problems.contains(&n) && rec.local.is_some() {
            return Ok(Some(true));
        }
        let (Some(l), Some(s)) = (&rec.local, &rec.synced) else {
            return Ok(Some(rec.local.is_some()));
        };
        if rec.remote.is_some() {
            // Moved into this folder locally: it stays (and so does the
            // folder). Otherwise Hearth has it elsewhere: it moves out.
            return Ok(Some(self.local_place(&rec) != view::base_place(&rec)));
        }
        match l.kind {
            Kind::File => Ok(l.hash.map(|h| Some(h) != s.hash)),
            Kind::Folder => {
                let mut unknown = false;
                for c in self.store.local_children(n)? {
                    match self.local_child_survives(c)? {
                        Some(true) => return Ok(Some(true)),
                        Some(false) => {}
                        None => unknown = true,
                    }
                }
                Ok(if unknown { None } else { Some(false) })
            }
        }
    }

    /// F, the local side gone: one request for the whole folder when
    /// nothing below changed on Hearth; otherwise child by child, and the
    /// folder is made again on disk if anything in it survives.
    fn folder_deleted_locally(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        if !mount.perms.allows(Perms::DELETE) {
            return Ok(match self.mkdir_local(rec)? {
                Decision::Op(op) => Decision::Op(op.listing(
                    ProblemKind::ReadOnly,
                    "deleted locally in a read-only folder",
                )),
                other => other,
            });
        }
        if self.under_whole_delete(rec)? {
            return Ok(Decision::Wait(None));
        }
        if self.deletable_whole(rec.node)? {
            let Some(cursor) = mount.cursor.clone() else {
                return Ok(Decision::Wait(None));
            };
            return Ok(Decision::Op(
                PlannedOp::new(OpKind::DeleteRemoteFolder).cond(Condition::UnchangedSince(cursor)),
            ));
        }
        let Some(r) = &rec.remote else {
            return Ok(Decision::Clean);
        };
        let kids = self.store.remote_children(&r.id)?;
        if kids.is_empty() {
            // Hearth's folder is empty but the one-request delete was
            // refused: list nothing, plan it again when the feed moves.
            return Ok(Decision::Wait(None));
        }
        let first = kids.first().copied();
        for c in kids {
            if self.remote_child_survives(c)? {
                return self.mkdir_local(rec);
            }
        }
        Ok(Decision::Wait(first))
    }

    /// Whether an entry inside a folder deleted locally stays (new or
    /// edited on Hearth, or holding such an entry).
    fn remote_child_survives(&self, n: NodeId) -> StoreResult<bool> {
        let Some(rec) = self.store.node(n)? else {
            return Ok(false);
        };
        // A listed item is left where it is on Hearth too.
        if self.problems.contains(&n) && rec.remote.is_some() {
            return Ok(true);
        }
        let (Some(r), Some(s)) = (&rec.remote, &rec.synced) else {
            return Ok(rec.remote.is_some());
        };
        if rec.local.is_some() {
            // Moved into this folder on Hearth: it stays. Otherwise the
            // disk has it elsewhere: it moves out on Hearth.
            return Ok(self.remote_place(&rec)? != view::base_place(&rec));
        }
        match r.kind {
            Kind::File => Ok(match r.hash {
                Some(h) => Some(h) != s.hash,
                None => r.etag != s.remote_etag,
            }),
            Kind::Folder => {
                for c in self.store.remote_children(&r.id)? {
                    if self.remote_child_survives(c)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// Whether a folder deleted locally can go from Hearth in one request:
    /// everything below it is gone locally and unchanged on Hearth since
    /// the base (3.5, last rule of F).
    fn deletable_whole(&self, n: NodeId) -> StoreResult<bool> {
        if let Some(v) = self.deletable.borrow().get(&n) {
            return Ok(*v);
        }
        let v = self.compute_deletable_whole(n)?;
        self.deletable.borrow_mut().insert(n, v);
        Ok(v)
    }

    fn compute_deletable_whole(&self, n: NodeId) -> StoreResult<bool> {
        // A folder whose own delete is in flight still counts: what is
        // below it waits for that delete instead of going alone.
        if self.split_deletes.contains(&n) {
            return Ok(false);
        }
        let Some(rec) = self.store.node(n)? else {
            return Ok(false);
        };
        let (None, Some(r), Some(s)) = (&rec.local, &rec.remote, &rec.synced) else {
            return Ok(false);
        };
        if r.kind != Kind::Folder || s.kind != Kind::Folder {
            return Ok(false);
        }
        let Some(m) = self.mounts.get(&rec.mount) else {
            return Ok(false);
        };
        if m.root_node == n || m.cursor.is_none() {
            return Ok(false);
        }
        for c in self.store.remote_children(&r.id)? {
            let Some(cr) = self.store.node(c)? else {
                return Ok(false);
            };
            let (None, Some(crr), Some(cs)) = (&cr.local, &cr.remote, &cr.synced) else {
                return Ok(false);
            };
            if self.busy(&c) {
                return Ok(false);
            }
            let ok = match crr.kind {
                Kind::File => match crr.hash {
                    Some(h) => Some(h) == cs.hash,
                    None => crr.etag == cs.remote_etag,
                },
                Kind::Folder => self.deletable_whole(c)?,
            };
            if !ok {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether a folder above this node (on Hearth) will be deleted in one
    /// request: then this node waits for it instead of going alone.
    fn under_whole_delete(&self, rec: &NodeRecord) -> StoreResult<bool> {
        let mut cur = rec.remote.as_ref().and_then(|r| r.parent.clone());
        let mut guard = 0;
        while let Some(p) = cur {
            guard += 1;
            if guard > 10_000 {
                return Ok(false);
            }
            let Some(pn) = self.store.node_by_remote(&p)? else {
                return Ok(false);
            };
            if self.deletable_whole(pn)? {
                return Ok(true);
            }
            let Some(prec) = self.store.node(pn)? else {
                return Ok(false);
            };
            if prec.local.is_some() {
                return Ok(false);
            }
            cur = prec.remote.and_then(|r| r.parent);
        }
        Ok(false)
    }

    // ----- creations (no base) -----

    fn plan_creation(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        match (&rec.local, &rec.remote) {
            (Some(l), None) => {
                if l.kind == Kind::File && l.hash.is_none() {
                    return Ok(Decision::NotReady);
                }
                match l.kind {
                    Kind::Folder => self.mkdir_remote(rec, mount),
                    Kind::File => self.upload_new(rec, mount),
                }
            }
            (None, Some(r)) => match r.kind {
                Kind::Folder => self.mkdir_local(rec),
                Kind::File => self.download_new(rec),
            },
            (Some(l), Some(r)) => {
                // Linked on both sides without a base: after a resync
                // without trusted history (6.8, I12).
                let lp = self.local_place(rec);
                if lp != self.remote_place(rec)? {
                    return self.move_local(rec);
                }
                if l.kind == Kind::Folder {
                    return Ok(self.adopt_new(rec));
                }
                match l.hash {
                    None => Ok(Decision::NotReady),
                    Some(h) if Some(h) == r.hash => Ok(self.adopt_new(rec)),
                    Some(_) => Ok(self.download(rec, None)),
                }
            }
            (None, None) => self.drop_node(rec),
        }
    }

    /// The node of a mount's root: Hearth decides where it sits; it is
    /// never moved, deleted or created on Hearth by sync (5.2).
    fn plan_mount_root(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        if mount.local_path.is_empty() {
            // The sync root itself: present by the root guard (3.11).
            return Ok(match (&rec.remote, &rec.synced) {
                (Some(_), None) if rec.local.is_some() => self.adopt_new(rec),
                _ => Decision::Clean,
            });
        }
        match (&rec.local, &rec.remote, &rec.synced) {
            (_, None, _) => Ok(Decision::Clean),
            (None, Some(r), None) => match r.kind {
                Kind::Folder => self.mkdir_local(rec),
                Kind::File => self.download_new(rec),
            },
            (None, Some(_), Some(_)) => Ok(Decision::Problem(
                ProblemKind::MountRootMissing,
                "the folder of this mount is missing on disk; nothing is changed".into(),
            )),
            (Some(l), Some(r), base) => {
                if self.local_place(rec) != self.remote_place(rec)? {
                    return self.move_local(rec);
                }
                match base {
                    None if l.kind == Kind::Folder || l.hash == r.hash => Ok(self.adopt_new(rec)),
                    None => Ok(self.download(rec, None)),
                    Some(s) if s.kind == Kind::File => self.plan_file(rec, s, mount),
                    Some(_) => Ok(Decision::Clean),
                }
            }
        }
    }

    // ----- steps with their checks -----

    /// A download of Hearth's content: read on the etag the feed announced
    /// (sync-api.md 7: `If-Match` on every read), to the node's present
    /// local place or, for a new local file, to `place`.
    fn download(&self, rec: &NodeRecord, place: Option<&Place>) -> Decision {
        let Some(r) = &rec.remote else {
            return Decision::Wait(None);
        };
        let Some(etag) = r.etag.clone() else {
            // A file always has an etag (1.7); never read without one.
            return Decision::Problem(ProblemKind::Refused, "Hearth gave this file no etag".into());
        };
        let mut op = PlannedOp::new(OpKind::Download)
            .cond(Condition::IfMatch(etag))
            .expect(r.hash);
        if let Some(p) = place {
            op = op.to(p);
        }
        Decision::Op(op)
    }

    fn download_new(&self, rec: &NodeRecord) -> StoreResult<Decision> {
        let Some(place) = self.remote_place(rec)? else {
            return Ok(Decision::Wait(None));
        };
        match self.check_local_target(rec, &place)? {
            Target::Free => Ok(self.download(rec, Some(&place))),
            Target::Wait(t) => Ok(Decision::Wait(t)),
            Target::Instead(d) => Ok(d),
        }
    }

    fn mkdir_local(&self, rec: &NodeRecord) -> StoreResult<Decision> {
        let Some(place) = self.remote_place(rec)? else {
            return Ok(Decision::Wait(None));
        };
        match self.check_local_target(rec, &place)? {
            Target::Free => Ok(Decision::Op(PlannedOp::new(OpKind::MkdirLocal).to(&place))),
            Target::Wait(t) => Ok(Decision::Wait(t)),
            Target::Instead(d) => Ok(d),
        }
    }

    fn move_local(&self, rec: &NodeRecord) -> StoreResult<Decision> {
        let Some(place) = self.remote_place(rec)? else {
            return Ok(Decision::Wait(None));
        };
        if self.local_cycle(rec.node, place.parent)? {
            return Ok(Decision::Wait(None));
        }
        match self.check_local_target(rec, &place)? {
            Target::Free => Ok(Decision::Op(PlannedOp::new(OpKind::MoveLocal).to(&place))),
            Target::Wait(t) => Ok(Decision::Wait(t)),
            Target::Instead(d) => Ok(d),
        }
    }

    fn upload_new(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        let Some(place) = self.local_place(rec) else {
            return Ok(Decision::Clean);
        };
        if !mount.perms.allows(Perms::CREATE) {
            return Ok(Decision::Problem(
                ProblemKind::ReadOnly,
                if rec.synced.is_some() {
                    "read_only_edit: edited here and deleted on Hearth, in a read-only folder"
                        .into()
                } else {
                    "not uploaded: the folder is read-only".into()
                },
            ));
        }
        match self.check_remote_target(rec, &place)? {
            Target::Free => {}
            Target::Wait(t) => return Ok(Decision::Wait(t)),
            Target::Instead(d) => return Ok(d),
        }
        let hash = rec.local.as_ref().and_then(|l| l.hash);
        Ok(Decision::Op(
            PlannedOp::new(OpKind::UploadNew)
                .cond(Condition::IfNoneMatch)
                .to(&place)
                .expect(hash),
        ))
    }

    fn mkdir_remote(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        let Some(place) = self.local_place(rec) else {
            return Ok(Decision::Clean);
        };
        if !mount.perms.allows(Perms::CREATE) {
            return Ok(Decision::Problem(
                ProblemKind::ReadOnly,
                "not uploaded: the folder is read-only".into(),
            ));
        }
        match self.check_remote_target(rec, &place)? {
            Target::Free => Ok(Decision::Op(
                PlannedOp::new(OpKind::MkdirRemote)
                    .cond(Condition::IfNoneMatch)
                    .to(&place),
            )),
            Target::Wait(t) => Ok(Decision::Wait(t)),
            Target::Instead(d) => Ok(d),
        }
    }

    fn move_remote(&self, rec: &NodeRecord, mount: &MountRow) -> StoreResult<Decision> {
        let Some(place) = self.local_place(rec) else {
            return Ok(Decision::Clean);
        };
        let Some(r) = &rec.remote else {
            return Ok(Decision::Clean);
        };
        if !mount.perms.allows(Perms::DELETE | Perms::CREATE)
            || self.refused_moves.contains(&rec.node)
        {
            return self.undo_move(rec);
        }
        // A move that would put a folder into itself on Hearth: Hearth's
        // place wins (C, the cycle case).
        if self.remote_cycle(rec.node, place.parent)? {
            return self.move_local(rec);
        }
        match self.check_remote_target(rec, &place)? {
            Target::Free => {}
            Target::Wait(t) => return Ok(Decision::Wait(t)),
            Target::Instead(d) => return Ok(d),
        }
        let cond = match r.kind {
            Kind::Folder => Condition::IfSeq(r.seq),
            Kind::File => match file_condition(r) {
                Some(c) => c,
                None => return Ok(Decision::Wait(None)),
            },
        };
        Ok(Decision::Op(
            PlannedOp::new(OpKind::MoveRemote).cond(cond).to(&place),
        ))
    }

    /// A local move the mount's rights refuse is undone on the device: the
    /// item goes back to where Hearth has it (R18). Only if it cannot go
    /// back (its old name is held by an item that stays, or the device
    /// cannot hold the name) does it stay where the person put it, listed
    /// `read_only_move`, with nothing sent.
    fn undo_move(&self, rec: &NodeRecord) -> StoreResult<Decision> {
        Ok(match self.move_local(rec)? {
            Decision::Problem(_, why) => Decision::Problem(
                ProblemKind::ReadOnly,
                format!(
                    "read_only_move: the folder is read-only and the item cannot go back ({why})"
                ),
            ),
            other => other,
        })
    }

    /// Checks a place on disk for a node to be put at: the folder is on
    /// disk, the device can hold the name, and the name is free or its
    /// holder will leave.
    fn check_local_target(&self, rec: &NodeRecord, place: &Place) -> StoreResult<Target> {
        let Some(parent) = place.parent else {
            return Ok(Target::Free);
        };
        let Some(prec) = self.store.node(parent)? else {
            return Ok(Target::Wait(None));
        };
        match &prec.local {
            Some(pl) if pl.kind == Kind::Folder => {}
            // The folder is not on disk yet: its own step brings it (unless
            // it waits, through others, for this node).
            _ => return self.wait_or_break_cycle(rec, parent),
        }
        if let Some(why) = hold_refusal(&place.name, &self.cfg.volume, parent == self.root) {
            return Ok(Target::Instead(Decision::Problem(
                ProblemKind::NameNotHoldable,
                hold_reason(why).into(),
            )));
        }
        let key = key_of_str(&place.name, &self.cfg.volume);
        let Some(o) = self.store.local_child_by_key(parent, &key)? else {
            return Ok(Target::Free);
        };
        if o == rec.node {
            // The node itself: a case-only or normalisation-only rename.
            return Ok(Target::Free);
        }
        let Some(orec) = self.store.node(o)? else {
            return Ok(Target::Wait(Some(o)));
        };
        let o_local_only = orec.remote.is_none() && orec.synced.is_none();
        let me_remote_only = rec.local.is_none() && rec.synced.is_none();
        if o_local_only && me_remote_only {
            return Ok(Target::Instead(self.pair(&orec, rec, rec.node)?));
        }
        let o_name = self.local_place(&orec).map(|p| p.name);
        if self.unselected.contains(&o) {
            // A folder shut out of sync (a mount that left, R6) holds the
            // name: this one is the name clash and waits.
            return Ok(Target::Instead(Decision::Problem(
                ProblemKind::NameClash,
                "the name is held on this device by a folder that is not synced".into(),
            )));
        }
        if self.problems.contains(&o) {
            // Its holder cannot move (it is listed): this one cannot either.
            return Ok(Target::Instead(Decision::Problem(
                ProblemKind::NameClash,
                format!(
                    "the name is held on this device by {:?}, which cannot sync",
                    o_name.unwrap_or_default()
                ),
            )));
        }
        if o_name.as_deref() == Some(place.name.as_str()) {
            // The holder has exactly this name: it leaves, steps aside or
            // pairs by its own decision; unless the two (or more) wait for
            // each other's places (Hearth swapped them).
            return self.wait_or_break_cycle(rec, o);
        }
        // One name on this device, two on Hearth (case or normalisation).
        let o_stays = o_local_only || self.local_place(&orec) == self.remote_place(&orec)?;
        if o_stays {
            return Ok(Target::Instead(Decision::Problem(
                ProblemKind::NameClash,
                format!(
                    "name clash on this device with {:?}",
                    o_name.unwrap_or_default()
                ),
            )));
        }
        self.wait_or_break_cycle(rec, o)
    }

    /// What a node waits for before it can take its place on disk (Hearth's
    /// place): the folder, if that is not on disk yet, or the node holding
    /// the name there. None when it does not want a new place on disk.
    fn local_blocker(&self, n: NodeId) -> StoreResult<Option<NodeId>> {
        let Some(rec) = self.store.node(n)? else {
            return Ok(None);
        };
        let Some(rp) = self.remote_place(&rec)? else {
            return Ok(None);
        };
        if self.local_place(&rec).as_ref() == Some(&rp) {
            return Ok(None);
        }
        let Some(parent) = rp.parent else {
            return Ok(None);
        };
        let on_disk = self
            .store
            .node(parent)?
            .is_some_and(|p| p.local.is_some_and(|l| l.kind == Kind::Folder));
        if !on_disk {
            return Ok(Some(parent));
        }
        let key = key_of_str(&rp.name, &self.cfg.volume);
        Ok(self
            .store
            .local_child_by_key(parent, &key)?
            .filter(|o| *o != n))
    }

    /// Whether a node is on disk somewhere else than Hearth's place for it,
    /// so it can step to a parking name to break a circle of waits.
    fn can_park(&self, n: NodeId) -> StoreResult<bool> {
        let Some(rec) = self.store.node(n)? else {
            return Ok(false);
        };
        Ok(rec.local.is_some()
            && rec.synced.is_some()
            && self.local_place(&rec) != self.remote_place(&rec)?)
    }

    /// Waits for the holder of a name, unless the holders wait for each
    /// other in a circle (Hearth swapped two names, or rotated several):
    /// then the lowest node of the circle first moves to a parking name in
    /// its own folder, which frees its place and breaks the circle. The
    /// parking name is one of the engine's temp names, so it is never
    /// uploaded; the next pass moves the node on to Hearth's place.
    fn wait_or_break_cycle(&self, rec: &NodeRecord, o: NodeId) -> StoreResult<Target> {
        let me = rec.node;
        let mut cycle = vec![me, o];
        let mut cur = o;
        for _ in 0..64 {
            match self.local_blocker(cur)? {
                Some(next) if next == me => {
                    // The circle is broken by its lowest member that is on
                    // disk and can step aside; the others wait for it.
                    let mut parker: Option<NodeId> = None;
                    for m in &cycle {
                        if self.can_park(*m)? && parker.is_none_or(|p: NodeId| *m < p) {
                            parker = Some(*m);
                        }
                    }
                    if parker == Some(me) {
                        let Some(l) = &rec.local else {
                            return Ok(Target::Wait(Some(o)));
                        };
                        let park = Place {
                            parent: l.parent,
                            // One of the engine's own names (R15: it
                            // invents none other on the device), tied to
                            // this node, which the scanner follows.
                            name: format!(".{}.moving{}", me.0, crate::names::TEMP_SUFFIX),
                        };
                        return Ok(Target::Instead(Decision::Op(
                            PlannedOp::new(OpKind::MoveLocal).to(&park),
                        )));
                    }
                    return Ok(Target::Wait(Some(o)));
                }
                Some(next) if cycle.contains(&next) => break,
                Some(next) => {
                    cycle.push(next);
                    cur = next;
                }
                None => break,
            }
        }
        Ok(Target::Wait(Some(o)))
    }

    /// Checks a place on Hearth for a node to be put at: the folder exists
    /// there, Hearth accepts the name, and the name is free or its holder
    /// will leave; otherwise Hearth keeps the name and the local item
    /// steps aside (3.5, "server keeps the name").
    fn check_remote_target(&self, rec: &NodeRecord, place: &Place) -> StoreResult<Target> {
        let Some(l) = &rec.local else {
            return Ok(Target::Wait(None));
        };
        // The engine's own names never go to Hearth (R14): a node parked
        // under one goes back to where Hearth has it instead.
        if crate::names::is_engine_name(
            &place.name,
            place.parent == Some(self.root),
            &self.cfg.volume,
        ) {
            return Ok(Target::Instead(self.move_local(rec)?));
        }
        if let Some(why) = remote_refusal(&l.name, self.name_bytes_max) {
            return Ok(Target::Instead(Decision::Problem(
                ProblemKind::NameRefused,
                format!("Hearth does not accept this name ({})", refusal_reason(why)),
            )));
        }
        let Some(parent) = place.parent else {
            return Ok(Target::Wait(None));
        };
        let Some(prec) = self.store.node(parent)? else {
            return Ok(Target::Wait(None));
        };
        let Some(pr) = &prec.remote else {
            // The folder is not on Hearth yet: its own step brings it.
            return Ok(Target::Wait(Some(parent)));
        };
        if pr.kind != Kind::Folder {
            return Ok(Target::Wait(Some(parent)));
        }
        for o in self.store.remote_children(&pr.id)? {
            if o == rec.node {
                continue;
            }
            let Some(orec) = self.store.node(o)? else {
                continue;
            };
            let Some(or) = &orec.remote else {
                continue;
            };
            if or.name != place.name {
                continue;
            }
            let o_remote_only = orec.local.is_none() && orec.synced.is_none();
            let me_local_only = rec.remote.is_none() && rec.synced.is_none();
            if o_remote_only && me_local_only {
                return Ok(Target::Instead(self.pair(rec, &orec, rec.node)?));
            }
            // A new local folder where Hearth has a folder that is gone from
            // the disk: most likely the same folder, found again (moved and
            // its contents renamed). Folders merge (3.5, creations), which
            // also spares the delete of the old one waiting on its children.
            if me_local_only
                && orec.local.is_none()
                && or.kind == Kind::Folder
                && rec.local.as_ref().is_some_and(|l| l.kind == Kind::Folder)
                && !self.busy(&o)
            {
                return Ok(Target::Instead(Decision::Record(self.merge(rec, &orec)?)));
            }
            // Its holder cannot move (it is listed): this one cannot either.
            if self.problems.contains(&o) {
                return Ok(Target::Instead(Decision::Problem(
                    ProblemKind::NameClash,
                    "the name is held on Hearth by an item that cannot sync".into(),
                )));
            }
            // Will the holder leave Hearth's name? It does when its local
            // side moved away or is gone (and it is unchanged on Hearth).
            let o_leaves = match (&orec.local, &orec.synced) {
                // It leaves only if Hearth still has it where the base
                // does and the disk moved it away. If Hearth itself put it
                // at this name, Hearth's place wins and it stays.
                (Some(_), Some(_)) => {
                    let base = view::base_place(&orec);
                    self.remote_place(&orec)? == base && self.local_place(&orec) != base
                }
                // Gone from the disk: it leaves Hearth's name by a delete,
                // unless Hearth itself moved it to this name (then Hearth's
                // place wins and it comes back to the disk there: E, G).
                (None, Some(os)) => {
                    let unchanged = match or.kind {
                        Kind::File => or.hash == os.hash,
                        Kind::Folder => true,
                    };
                    unchanged && self.remote_place(&orec)? == view::base_place(&orec)
                }
                _ => false,
            };
            if o_leaves || self.busy(&o) {
                return Ok(Target::Wait(Some(o)));
            }
            return Ok(Target::Instead(Decision::Op(PlannedOp::new(
                OpKind::ConflictAsideLocal,
            ))));
        }
        Ok(Target::Free)
    }

    /// A local-only item and a Hearth-only item at one place (3.5,
    /// creations): equal files and two folders become one node; otherwise
    /// the file steps aside as a conflict copy on its own side. `me` is the
    /// node being planned: a step that acts on the other node is left to
    /// that node's own decision, and this one waits for it.
    fn pair(&self, local: &NodeRecord, remote: &NodeRecord, me: NodeId) -> StoreResult<Decision> {
        let aside = |on: &NodeRecord, op: PlannedOp| {
            if on.node == me {
                Decision::Op(op)
            } else {
                Decision::Wait(Some(on.node))
            }
        };
        let (Some(l), Some(r)) = (&local.local, &remote.remote) else {
            return Ok(Decision::Wait(None));
        };
        if self.busy(&local.node) {
            return Ok(Decision::Wait(Some(local.node)));
        }
        if self.busy(&remote.node) {
            return Ok(Decision::Wait(Some(remote.node)));
        }
        match (l.kind, r.kind) {
            (Kind::Folder, Kind::Folder) => Ok(Decision::Record(self.merge(local, remote)?)),
            (Kind::File, Kind::File) => match l.hash {
                None => Ok(Decision::NotReady),
                Some(h) if Some(h) == r.hash => Ok(Decision::Record(self.merge(local, remote)?)),
                Some(_) => Ok(aside(local, PlannedOp::new(OpKind::ConflictAsideLocal))),
            },
            (Kind::File, Kind::Folder) => {
                Ok(aside(local, PlannedOp::new(OpKind::ConflictAsideLocal)))
            }
            (Kind::Folder, Kind::File) => {
                let Some(m) = self.mounts.get(&remote.mount) else {
                    return Ok(Decision::Wait(None));
                };
                if !m.perms.allows(Perms::DELETE | Perms::CREATE) {
                    return Ok(Decision::Problem(
                        ProblemKind::NameClash,
                        "a file on Hearth has this folder's name, in a read-only folder".into(),
                    ));
                }
                match file_condition(r) {
                    Some(c) => Ok(aside(
                        remote,
                        PlannedOp::new(OpKind::ConflictAsideRemote).cond(c),
                    )),
                    None => Ok(Decision::Wait(None)),
                }
            }
        }
    }

    /// The writes that make a local-only node and a Hearth-only node one:
    /// the local picture (and the local children of a folder) move to the
    /// Hearth node, and the local node is forgotten.
    fn merge(&self, local: &NodeRecord, remote: &NodeRecord) -> StoreResult<Vec<Write>> {
        let Some(l) = &local.local else {
            return Ok(Vec::new());
        };
        let mut w = Vec::new();
        let kids = self.store.local_children(local.node)?;
        for c in &kids {
            if let Some(cl) = self.store.node(*c)?.and_then(|r| r.local) {
                let mut cl = cl;
                cl.parent = Some(remote.node);
                w.push(Write::PutLocal(cl));
                w.push(Write::MarkDirty(*c));
            }
        }
        w.push(Write::DelLocal(local.node));
        let mut moved = l.clone();
        moved.node = remote.node;
        w.push(Write::PutLocal(moved));
        w.push(Write::DelNode(local.node));
        w.push(Write::MarkDirty(remote.node));
        Ok(w)
    }

    /// The writes that give a node's local picture a node of its own (a
    /// retype, a move into another mount).
    fn split_local(&self, rec: &NodeRecord) -> StoreResult<Decision> {
        let Some(l) = &rec.local else {
            return Ok(Decision::Clean);
        };
        let new = NodeId(self.next_node.get());
        self.next_node.set(new.0 + 1);
        let mount = match l.parent {
            Some(p) => self
                .store
                .node(p)?
                .map(|r| r.mount)
                .unwrap_or(rec.mount.clone()),
            None => rec.mount.clone(),
        };
        let mut w = vec![Write::PutNode(new, mount)];
        for c in self.store.local_children(rec.node)? {
            if let Some(mut cl) = self.store.node(c)?.and_then(|r| r.local) {
                cl.parent = Some(new);
                w.push(Write::PutLocal(cl));
                w.push(Write::MarkDirty(c));
            }
        }
        w.push(Write::DelLocal(rec.node));
        let mut moved = l.clone();
        moved.node = new;
        w.push(Write::PutLocal(moved));
        w.push(Write::MarkDirty(new));
        w.push(Write::MarkDirty(rec.node));
        Ok(Decision::Record(w))
    }

    /// Records what both sides agree on: the place, and the content when the
    /// two hashes are equal.
    fn adopt(&self, rec: &NodeRecord, s: &SyncedRow) -> Decision {
        let (Some(l), Some(r)) = (&rec.local, &rec.remote) else {
            return Decision::Wait(None);
        };
        let Some(place) = self.local_place(rec) else {
            return Decision::Wait(None);
        };
        let mut s2 = s.clone();
        s2.parent = place.parent;
        s2.name = place.name;
        if l.kind == Kind::Folder || (l.hash.is_some() && l.hash == r.hash) {
            s2.hash = l.hash;
            s2.size = l.size;
            s2.remote_etag = r.etag.clone();
            s2.local_file_id = l.file_id;
            s2.local_mtime_ns = l.mtime_ns;
            s2.exec = l.exec;
        }
        Decision::Record(vec![Write::PutSynced(s2), Write::MarkDirty(rec.node)])
    }

    /// The first base of a node linked on both sides with equal content.
    fn adopt_new(&self, rec: &NodeRecord) -> Decision {
        let (Some(l), Some(r)) = (&rec.local, &rec.remote) else {
            return Decision::Wait(None);
        };
        let Some(place) = self.local_place(rec) else {
            return Decision::Wait(None);
        };
        Decision::Record(vec![
            Write::PutSynced(SyncedRow {
                node: rec.node,
                parent: place.parent,
                name: place.name,
                kind: l.kind,
                size: l.size,
                hash: l.hash,
                remote_etag: r.etag.clone(),
                local_file_id: l.file_id,
                local_mtime_ns: l.mtime_ns,
                exec: l.exec,
            }),
            Write::MarkDirty(rec.node),
        ])
    }

    /// Forgets a node gone from both sides, once nothing hangs below it.
    /// A mount's root is never forgotten while its mount is known.
    fn drop_node(&self, rec: &NodeRecord) -> StoreResult<Decision> {
        if self.mounts.values().any(|m| m.root_node == rec.node) {
            return Ok(Decision::Clean);
        }
        // Children go first; wait for one of them (its removal wakes this).
        if let Some(c) = self.store.local_children(rec.node)?.first() {
            return Ok(Decision::Wait(Some(*c)));
        }
        let synced_kids = self.store.synced_children(rec.node)?;
        if let Some(c) = synced_kids.first() {
            // Children whose base still names this folder while neither side
            // has them here any more (moved away on both sides, one move
            // listed because it cannot be done: a name clash, a name this
            // device cannot hold) would keep it for ever. Their base is
            // forgotten: a node without a base and present on both sides is
            // compared by content (equal adopted, different kept as a
            // conflict copy), and one present on one side is only copied to
            // the other; neither is ever deleted because of it. Then this
            // folder can go.
            let mut forget = Vec::new();
            for k in &synced_kids {
                if self.busy(k) {
                    return Ok(Decision::Wait(Some(*k)));
                }
                let Some(kr) = self.store.node(*k)? else {
                    continue;
                };
                let local_here = kr
                    .local
                    .as_ref()
                    .is_some_and(|l| l.parent == Some(rec.node));
                let remote_here = match kr.remote.as_ref().and_then(|r| r.parent.clone()) {
                    Some(p) => self.store.node_by_remote(&p)? == Some(rec.node),
                    None => false,
                };
                if local_here || remote_here {
                    return Ok(Decision::Wait(Some(*c)));
                }
                forget.push(Write::DelSynced(*k));
                forget.push(Write::MarkDirty(*k));
            }
            forget.push(Write::MarkDirty(rec.node));
            return Ok(Decision::Record(forget));
        }
        if let Some(r) = &rec.remote
            && let Some(c) = self.store.remote_children(&r.id)?.first()
        {
            return Ok(Decision::Wait(Some(*c)));
        }
        Ok(Decision::Record(vec![Write::DelNode(rec.node)]))
    }

    /// Whether putting `node` into `parent` on disk would put a folder
    /// into itself.
    fn local_cycle(&self, node: NodeId, parent: Option<NodeId>) -> StoreResult<bool> {
        let mut cur = parent;
        let mut guard = 0;
        while let Some(p) = cur {
            if p == node {
                return Ok(true);
            }
            guard += 1;
            if guard > 100_000 {
                return Ok(true);
            }
            cur = self
                .store
                .node(p)?
                .and_then(|r| r.local)
                .and_then(|l| l.parent);
        }
        Ok(false)
    }

    /// Whether putting `node` into `parent` on Hearth would put a folder
    /// into itself.
    fn remote_cycle(&self, node: NodeId, parent: Option<NodeId>) -> StoreResult<bool> {
        let mut cur = parent;
        let mut guard = 0;
        while let Some(p) = cur {
            if p == node {
                return Ok(true);
            }
            guard += 1;
            if guard > 100_000 {
                return Ok(true);
            }
            cur = match self
                .store
                .node(p)?
                .and_then(|r| r.remote)
                .and_then(|r| r.parent)
            {
                Some(rid) => self.store.node_by_remote(&rid)?,
                None => None,
            };
        }
        Ok(false)
    }
}

/// Edit and move flags to the table's change kind.
fn combine(edited: bool, moved: bool) -> FileChange {
    match (edited, moved) {
        (false, false) => FileChange::None,
        (true, false) => FileChange::Edit,
        (false, true) => FileChange::Move,
        (true, true) => FileChange::Both,
    }
}

/// The condition for writing over or removing a file Hearth reported: its
/// etag, or its hash when (against the contract) no etag was given.
pub fn file_condition(r: &crate::tree::RemoteRow) -> Option<Condition> {
    match (&r.etag, r.hash) {
        (Some(e), _) => Some(Condition::IfMatch(e.clone())),
        (None, Some(h)) => Some(Condition::IfHash(h)),
        (None, None) => None,
    }
}

fn hold_reason(r: HoldRefusal) -> &'static str {
    match r {
        HoldRefusal::Character => "this device cannot hold a character of the name",
        HoldRefusal::TrailingDotOrSpace => {
            "this device cannot hold a name ending in a dot or space"
        }
        HoldRefusal::ReservedDevice => "this device keeps the name for itself",
        HoldRefusal::TooLong => "the name is too long for this device",
        HoldRefusal::EngineName => "the name is the sync engine's own",
    }
}

fn refusal_reason(r: RemoteRefusal) -> &'static str {
    match r {
        RemoteRefusal::NotUtf8 => "not_utf8",
        RemoteRefusal::Empty => "empty",
        RemoteRefusal::Dot => "dot",
        RemoteRefusal::TooLong => "too_long",
        RemoteRefusal::Separator => "separator",
        RemoteRefusal::Control => "control",
        RemoteRefusal::Blank => "blank",
        RemoteRefusal::Reserved => "reserved",
    }
}
