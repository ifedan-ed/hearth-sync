//! A naive "perfect sync" for testing the model against itself.
//!
//! It plays a scenario on plain trees: people's acts change a device or
//! Hearth; after each act every online device syncs by taking the
//! model's own settled outcome ([`reconcile`]) and making it so, one
//! request and one disk step at a time, the way an engine would; it
//! records every step as the observations the checkers read. Faults are
//! played as simply as they can be while keeping their meaning.
//!
//! Each [`Mutant`] breaks one rule on purpose. A checker that does not
//! report its mutant does not bite (tests/mutants.rs).
//!
//! This is test code: it is a stand-in for the engine, not a second
//! engine. It runs in one thread, reads no clock and no randomness, so a
//! scenario gives the same trace every time (I13).

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use hearth_sync_model::apply::{Namespace, apply_act, lookup};
use hearth_sync_model::name::nfc;
use hearth_sync_model::reconcile::{
    CopyReason, DeviceContext, Listing, MountRule, Settled, perms, reconcile,
};
use hearth_sync_model::scenario::{Act, AdminOp, FaultOp, Op, Scenario, Side};
use hearth_sync_model::trace::{
    Condition, DeviceView, EngineAction, Event, MountView, Outcome, Request, RequestKind,
    RootState, ServerView, Snapshot, Step, Trace, Write, WriteOp,
};
use hearth_sync_model::tree::{Hash, IdTree, Kind, Node, NodeId, PathTree, ROOT, is_within};

/// One deliberate defect each, named for the guarantee it breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mutant {
    /// No defect.
    None,
    /// I1: removes and replaces local files without holding them.
    UnlinkWithoutHolding,
    /// I1: empties the holding area after every round, whatever it holds.
    PurgeHoldingEarly,
    /// I2 and I9: drops the conflict copy and overwrites the local edit.
    DropConflictCopy,
    /// I3: writes to Hearth without conditions.
    BlindWrites,
    /// I4: lets a download show at its name before it is complete.
    PartialVisible,
    /// I5: puts an altered download in place without checking its hash.
    TrustCorruptDownload,
    /// I6: syncs a missing root as an empty folder.
    DeleteOnMissingRoot,
    /// I6: reads a restored server's missing files as deletions.
    EpochAsRetention,
    /// I7: keeps uploading after it settled.
    SyncLoop,
    /// I8: sends local names as they are on disk, not in NFC.
    NoNfc,
    /// I9: makes two conflict copies of one losing version.
    DuplicateCopy,
    /// I10: renames local files whose names Hearth refuses, to send them.
    SanitizeRefusedNames,
    /// I11: uploads again after a crash what was already uploaded.
    ReuploadAfterCrash,
    /// I12: downloads everything again after losing its database.
    RedownloadAfterDbLoss,
    /// I13: writes something into its log that differs between runs.
    NondeterministicLog,
    /// 3.11: deletes many files without pausing.
    NoMassGuard,
    /// Decides conflicts by modification time: the newer wins, no copy.
    TrustMtimes,
}

/// A counter that survives between runs: what [`Mutant::NondeterministicLog`]
/// leaks into its log.
static RUNS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct Dev {
    ctx: DeviceContext,
    tree: IdTree,
    base: IdTree,
    holding: BTreeSet<Hash>,
    listed: BTreeMap<String, String>,
    online: bool,
    root: RootState,
    paused: Option<String>,
    saved_tree: Option<IdTree>,
    skew: i64,
    remote_view: PathTree,
    /// Nodes kept on the device only ("<name> (no longer shared)").
    local_only: BTreeSet<NodeId>,
    /// Paths the device may not touch (a denied path).
    denied: BTreeSet<String>,
    mass_confirmed: bool,
    // Pending one-shot faults.
    fail_next_request: bool,
    lose_next_answer: bool,
    crash_next_round: bool,
    corrupt_next_download: bool,
    disk_full_next: bool,
    after_crash: bool,
    after_db_loss: bool,
}

#[derive(Debug, Clone)]
struct Mount {
    path: String,
    trash: bool,
    versions: bool,
    perms: u8,
    present: bool,
    hidden: Option<IdTree>,
}

struct World {
    sc: Scenario,
    mutant: Mutant,
    server: IdTree,
    versions: BTreeSet<Hash>,
    trash: BTreeSet<Hash>,
    mounts: Vec<Mount>,
    refused: BTreeMap<String, String>,
    backup: (IdTree, BTreeSet<Hash>, BTreeSet<Hash>),
    devs: BTreeMap<String, Dev>,
    next_id: NodeId,
    time: i64,
    trace: Trace,
}

/// Plays `sc` with the naive sync (with `mutant`'s defect) and returns
/// the trace.
pub(crate) fn run(sc: &Scenario, mutant: Mutant) -> Trace {
    let mut w = World {
        sc: sc.clone(),
        mutant,
        server: IdTree::new(),
        versions: BTreeSet::new(),
        trash: BTreeSet::new(),
        mounts: sc
            .mounts
            .iter()
            .map(|m| Mount {
                path: m.path.clone(),
                trash: m.trash,
                versions: m.versions,
                perms: m.perms,
                present: m.at_start,
                hidden: None,
            })
            .collect(),
        refused: BTreeMap::new(),
        backup: (IdTree::new(), BTreeSet::new(), BTreeSet::new()),
        devs: BTreeMap::new(),
        next_id: 1,
        time: 1_795_615_402_000_000,
        trace: Trace {
            scenario: sc.clone(),
            start: Snapshot::default(),
            steps: Vec::new(),
            log: Vec::new(),
        },
    };
    for m in w.mounts.clone() {
        if m.present {
            w.make_mount_folder(&m.path);
        }
    }
    for d in &sc.devices {
        w.devs.insert(
            d.name.clone(),
            Dev {
                ctx: DeviceContext {
                    device: d.name.clone(),
                    utc_offset_secs: d.utc_offset_secs,
                    volume: d.volume,
                    mounts: Vec::new(),
                },
                tree: w.server.clone(),
                base: w.server.clone(),
                holding: BTreeSet::new(),
                listed: BTreeMap::new(),
                online: true,
                root: RootState::Present,
                paused: None,
                saved_tree: None,
                skew: 0,
                remote_view: w.server.to_paths(),
                local_only: BTreeSet::new(),
                denied: BTreeSet::new(),
                mass_confirmed: false,
                fail_next_request: false,
                lose_next_answer: false,
                crash_next_round: false,
                corrupt_next_download: false,
                disk_full_next: false,
                after_crash: false,
                after_db_loss: false,
            },
        );
    }
    w.backup = (w.server.clone(), BTreeSet::new(), BTreeSet::new());
    w.trace.start = w.snapshot();
    if mutant == Mutant::NondeterministicLog {
        let n = RUNS.fetch_add(1, Ordering::SeqCst);
        w.trace.log.push(format!("run {n}"));
    }
    for op in sc.ops.clone() {
        w.op(&op);
    }
    w.trace
}

impl World {
    fn make_mount_folder(&mut self, path: &str) {
        let mut next = self.next_id;
        apply_act(
            &mut self.server,
            Namespace::Hearth,
            &Act::Mkdir { path: path.into() },
            self.time,
            &mut next,
        );
        self.next_id = next;
    }

    fn snapshot(&self) -> Snapshot {
        let mut mounts = BTreeMap::new();
        mounts.insert(
            String::new(),
            MountView {
                trash: true,
                versions: true,
                perms: perms::ALL,
            },
        );
        for m in self.mounts.iter().filter(|m| m.present) {
            mounts.insert(
                m.path.clone(),
                MountView {
                    trash: m.trash,
                    versions: m.versions,
                    perms: m.perms,
                },
            );
        }
        let devices = self
            .devs
            .iter()
            .map(|(name, d)| {
                let tree = if d.root == RootState::Present {
                    d.tree.to_paths()
                } else {
                    PathTree::new()
                };
                (
                    name.clone(),
                    DeviceView {
                        tree,
                        holding: d.holding.clone(),
                        listed: d.listed.clone(),
                        synced: d.base.to_paths(),
                        remote_view: d.remote_view.clone(),
                        root: d.root,
                        online: d.online,
                        paused: d.paused.clone(),
                    },
                )
            })
            .collect();
        Snapshot {
            time: self.time,
            server: ServerView {
                tree: self.server.to_paths(),
                versions: self.versions.clone(),
                trash: self.trash.clone(),
                mounts,
            },
            devices,
        }
    }

    fn record(&mut self, event: Event) {
        self.time += 1_000_000;
        self.trace
            .log
            .push(format!("{} {:?}", self.trace.steps.len(), event));
        let after = self.snapshot();
        self.trace.steps.push(Step { event, after });
    }

    fn engine(&mut self, device: &str, action: EngineAction) {
        self.record(Event::Engine {
            device: device.to_string(),
            action,
        });
    }

    fn request(&mut self, device: &str, kind: RequestKind, outcome: Outcome) {
        self.engine(device, EngineAction::Request(Request { kind, outcome }));
    }

    /// The mount holding server path `path`.
    fn mount_at(&self, path: &str) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|m| m.present && is_within(path, &m.path))
            .max_by_key(|m| m.path.len())
    }

    fn keeps_trash(&self, path: &str) -> bool {
        self.mount_at(path).is_none_or(|m| m.trash)
    }

    fn keeps_versions(&self, path: &str) -> bool {
        self.mount_at(path).is_none_or(|m| m.versions)
    }

    fn op(&mut self, op: &Op) {
        match op {
            Op::Person { side, act } => self.person(side, act),
            Op::Admin(a) => self.admin(a),
            Op::Fault { device, fault } => self.fault(device, fault),
            Op::Confirm { device } => {
                if let Some(d) = self.devs.get_mut(device) {
                    d.mass_confirmed = true;
                    if d.root == RootState::Replaced {
                        // "Go on" with a replaced root: a new, empty root;
                        // nothing is deleted anywhere (the model's reading,
                        // docs/contract-questions.md M5).
                        d.root = RootState::Present;
                        d.tree = IdTree::new();
                        d.base = IdTree::new();
                    }
                    d.paused = None;
                }
                self.record(Event::Confirm {
                    device: device.clone(),
                });
                self.sync_all();
            }
            Op::Settle => self.settle(),
        }
    }

    fn person(&mut self, side: &Side, act: &Act) {
        let applied = match side {
            Side::Web => self.web_act(act),
            Side::Device(name) => {
                let time = self.time;
                let mut next = self.next_id;
                let applied = match self.devs.get_mut(name) {
                    Some(d) if d.root == RootState::Present => {
                        let mtime = time + d.skew * 1_000_000;
                        apply_act(
                            &mut d.tree,
                            Namespace::Device(d.ctx.volume),
                            act,
                            mtime,
                            &mut next,
                        )
                    }
                    _ => false,
                };
                self.next_id = next;
                applied
            }
        };
        self.record(Event::Person {
            side: side.clone(),
            act: act.clone(),
            applied,
        });
        self.sync_all();
    }

    /// A web act: Hearth keeps what it displaces (versions, trash).
    fn web_act(&mut self, act: &Act) -> bool {
        let before = self.server.to_paths();
        let mut next = self.next_id;
        let applied = apply_act(
            &mut self.server,
            Namespace::Hearth,
            act,
            self.time,
            &mut next,
        );
        self.next_id = next;
        if applied {
            self.keep_displaced(&before);
        }
        applied
    }

    /// After Hearth's tree changed from `before`: replaced contents become
    /// versions and removed ones go to the trash, where the mount keeps
    /// them (sync-api 4).
    fn keep_displaced(&mut self, before: &PathTree) {
        let after = self.server.to_paths();
        for (p, k) in &before.entries {
            let Some(h) = k.hash() else { continue };
            if after.holds(h) {
                continue;
            }
            match after.get(p) {
                Some(Kind::File { .. }) => {
                    if self.keeps_versions(p) {
                        self.versions.insert(h.clone());
                    }
                }
                _ => {
                    if self.keeps_trash(p) {
                        self.trash.insert(h.clone());
                    }
                }
            }
        }
    }

    fn admin(&mut self, a: &AdminOp) {
        match a {
            AdminOp::ShareArrive { path } => {
                if let Some(i) = self
                    .mounts
                    .iter()
                    .position(|m| m.path == *path && !m.present)
                {
                    self.mounts[i].present = true;
                    match self.mounts[i].hidden.take() {
                        Some(sub) => self.server.nodes.extend(sub.nodes),
                        None => self.make_mount_folder(path),
                    }
                }
            }
            AdminOp::ShareLeave { path } => {
                if let Some(i) = self
                    .mounts
                    .iter()
                    .position(|m| m.path == *path && m.present)
                {
                    let root = lookup(&self.server, Namespace::Hearth, path);
                    let mut hidden = IdTree::new();
                    if let Some(root) = root {
                        for id in self.server.subtree(root) {
                            if let Some(n) = self.server.nodes.remove(&id) {
                                hidden.nodes.insert(id, n);
                            }
                        }
                        for name in self.devs.keys().cloned().collect::<Vec<_>>() {
                            self.share_left_on(&name, root, path);
                        }
                    }
                    self.mounts[i].present = false;
                    self.mounts[i].hidden = Some(hidden);
                }
            }
            AdminOp::Restore => {
                let (s, v, t) = self.backup.clone();
                self.server = s;
                self.versions = v;
                self.trash = t;
                for d in self.devs.values_mut() {
                    if self.mutant != Mutant::EpochAsRetention {
                        // A changed epoch: full resync by hashes (3.11).
                        d.base = IdTree::new();
                    }
                }
            }
            AdminOp::Restart | AdminOp::CursorExpire => {}
            AdminOp::EmptyTrash => self.trash.clear(),
            AdminOp::Refuse { path, code } => {
                self.refused.insert(nfc(path), code.clone());
            }
        }
        self.record(Event::Admin(a.clone()));
        self.sync_all();
    }

    /// A share left: unchanged local files go to the holding area; local
    /// changes not uploaded stay in "<name> (no longer shared)", listed
    /// (sync.md 4.2).
    fn share_left_on(&mut self, name: &str, root: NodeId, path: &str) {
        let Some(d) = self.devs.get_mut(name) else {
            return;
        };
        if !d.tree.has(root) {
            return;
        }
        let keep_name = format!("{} (no longer shared)", path);
        let kept_id = self.next_id;
        self.next_id += 1;
        let mut kept_any = false;
        for id in d.tree.subtree(root) {
            let Some(node) = d.tree.get(id).cloned() else {
                continue;
            };
            if id == root {
                continue;
            }
            let unchanged = d
                .base
                .get(id)
                .is_some_and(|b| b.kind.same_content(&node.kind));
            if node.kind.is_folder() {
                continue;
            }
            if unchanged {
                if let Some(h) = node.kind.hash() {
                    d.holding.insert(h.clone());
                }
            } else {
                d.local_only.insert(id);
                kept_any = true;
                d.tree.insert(id, kept_id, &node.name, node.kind.clone());
                d.base.nodes.remove(&id);
            }
        }
        for id in d.tree.subtree(root) {
            d.tree.nodes.remove(&id);
            d.base.nodes.remove(&id);
        }
        if kept_any {
            d.tree.insert(kept_id, ROOT, &keep_name, Kind::Folder);
            d.local_only.insert(kept_id);
            d.listed.insert(keep_name, "no_longer_shared".into());
        }
    }

    fn fault(&mut self, device: &str, f: &FaultOp) {
        if let Some(d) = self.devs.get_mut(device) {
            match f {
                FaultOp::Offline => d.online = false,
                FaultOp::Online => d.online = true,
                FaultOp::Drop(_) | FaultOp::Stall(_) => d.fail_next_request = true,
                FaultOp::LoseAnswer(_) => d.lose_next_answer = true,
                FaultOp::Crash(_) => d.crash_next_round = true,
                FaultOp::DiskFull(_) => d.disk_full_next = true,
                FaultOp::Deny(p) => {
                    d.denied.insert(nfc(p));
                }
                FaultOp::Skew(s) => d.skew += s,
                FaultOp::Corrupt(_) => d.corrupt_next_download = true,
                FaultOp::RootVanish => {
                    if d.root == RootState::Present {
                        d.root = RootState::Missing;
                        d.saved_tree = Some(std::mem::take(&mut d.tree));
                    }
                }
                FaultOp::RootBack => {
                    if d.root == RootState::Missing {
                        d.root = RootState::Present;
                        d.tree = d.saved_tree.take().unwrap_or_default();
                    }
                }
                FaultOp::RootReplaced => {
                    d.root = RootState::Replaced;
                    d.tree = IdTree::new();
                    d.saved_tree = None;
                    d.paused = Some("root".into());
                }
                FaultOp::DbDeleted => {
                    d.base = IdTree::new();
                    d.after_db_loss = true;
                }
            }
        }
        self.record(Event::Fault {
            device: device.to_string(),
            fault: f.clone(),
        });
        self.sync_all();
    }

    fn settle(&mut self) {
        for d in self.devs.values_mut() {
            d.online = true;
            d.fail_next_request = false;
            d.lose_next_answer = false;
            d.crash_next_round = false;
            d.disk_full_next = false;
            if d.root == RootState::Missing {
                d.root = RootState::Present;
                d.tree = d.saved_tree.take().unwrap_or_default();
            }
        }
        self.sync_all();
        let names: Vec<String> = self.devs.keys().cloned().collect();
        self.record(Event::Settled);
        for name in &names {
            if self.mutant == Mutant::SyncLoop {
                let file = self
                    .server
                    .to_paths()
                    .entries
                    .into_iter()
                    .find_map(|(p, k)| k.hash().map(|h| (p, h.clone())));
                if let Some((path, hash)) = file {
                    self.request(name, RequestKind::Upload { path, hash }, Outcome::Done);
                }
            }
            self.request(name, RequestKind::Wait, Outcome::Done);
        }
        for d in self.devs.values_mut() {
            d.mass_confirmed = false;
            d.after_crash = false;
            d.after_db_loss = false;
        }
        self.backup = (
            self.server.clone(),
            self.versions.clone(),
            self.trash.clone(),
        );
    }

    /// Every online device syncs, round after round, until a round
    /// changes nothing (bounded: a loop here is a bug of this stand-in).
    fn sync_all(&mut self) {
        for _ in 0..50 {
            let mut changed = false;
            let names: Vec<String> = self.devs.keys().cloned().collect();
            for name in names {
                changed |= self.sync_device(&name);
            }
            if !changed {
                return;
            }
        }
    }

    fn context(&self, d: &Dev) -> DeviceContext {
        let mut ctx = d.ctx.clone();
        ctx.mounts = self
            .mounts
            .iter()
            .filter(|m| m.present)
            .filter_map(|m| {
                lookup(&self.server, Namespace::Hearth, &m.path).map(|root| MountRule {
                    root,
                    perms: m.perms,
                })
            })
            .collect();
        ctx
    }

    /// One sync round of one device. Returns whether it changed anything.
    fn sync_device(&mut self, name: &str) -> bool {
        let Some(dev) = self.devs.get(name).cloned() else {
            return false;
        };
        let missing_root_sync =
            dev.root == RootState::Missing && self.mutant == Mutant::DeleteOnMissingRoot;
        if !dev.online
            || dev.paused.is_some()
            || (dev.root != RootState::Present && !missing_root_sync)
        {
            return false;
        }
        // The feed: the device learns Hearth's present state.
        let server_paths = self.server.to_paths();
        if dev.remote_view != server_paths {
            if let Some(d) = self.devs.get_mut(name) {
                d.remote_view = server_paths.clone();
            }
            self.request(name, RequestKind::Changes, Outcome::Done);
        }
        let ctx = self.context(&dev);
        let mut local = dev.tree.clone();
        for id in &dev.local_only {
            local.nodes.remove(id);
        }
        let mut s = match reconcile(&dev.base, &local, &self.server, &ctx, self.next_id) {
            Ok(s) => s,
            Err(e) => panic!("the stand-in made trees the model cannot read: {e:?}"),
        };
        self.next_id = self.next_id.max(s.server.next_id()).max(s.device.next_id()) + 1;
        self.mutate(&dev, &mut s);
        let mut listed: BTreeMap<String, String> = BTreeMap::new();
        for (id, why) in &s.listed {
            let device_path = s.device.path(*id).map(|p| p.join("/"));
            let server_path = s.server.path(*id).map(|p| p.join("/"));
            match (why, device_path, server_path) {
                // Hearth renamed it to a name this device cannot hold: the
                // Hearth item is listed with the reason, the local copy
                // left where it was (contract-questions M10).
                (Listing::DeviceRefused(_) | Listing::EngineName, Some(d), Some(h))
                    if nfc(&d) != h =>
                {
                    listed.insert(h, why.code());
                    listed.insert(d, "other_name".into());
                }
                (Listing::HearthRefused(_), Some(d), Some(h)) if nfc(&d) != h => {
                    listed.insert(d, why.code());
                    listed.insert(h, "other_name".into());
                }
                // Covered by its folder's listing.
                (Listing::InsideListed, _, _) => {}
                (_, Some(p), _) | (_, None, Some(p)) => {
                    listed.insert(p, why.code());
                }
                _ => {}
            }
        }
        self.hold_back(&dev, &mut s, &mut listed);
        for (p, why) in &dev.listed {
            if why == "no_longer_shared" {
                listed.insert(p.clone(), why.clone());
            }
        }
        for id in &dev.local_only {
            if let Some(n) = dev.tree.get(*id) {
                s.device.nodes.insert(*id, n.clone());
            }
        }

        let server_changes = s.server != self.server;
        let device_changes = s.device != dev.tree;
        if !server_changes && !device_changes {
            if let Some(d) = self.devs.get_mut(name) {
                d.base = s.base;
                let new_listing = d.listed != listed;
                d.listed = listed;
                if new_listing {
                    self.engine(name, EngineAction::Note("listing".into()));
                }
            }
            return false;
        }
        if self.mass_change(&dev, &s) {
            if let Some(d) = self.devs.get_mut(name) {
                d.paused = Some("mass_change".into());
            }
            self.engine(name, EngineAction::Note("paused: mass change".into()));
            return false;
        }
        if dev.fail_next_request {
            if let Some(d) = self.devs.get_mut(name) {
                d.fail_next_request = false;
            }
            self.request(name, RequestKind::Changes, Outcome::Failed);
            return true;
        }
        if dev.disk_full_next {
            // The disk refuses the round's first write: nothing done.
            if let Some(d) = self.devs.get_mut(name) {
                d.disk_full_next = false;
            }
            self.engine(name, EngineAction::Note("disk full".into()));
            return true;
        }
        // The round's listings are known before it acts.
        if let Some(d) = self.devs.get_mut(name) {
            d.listed = listed;
        }
        let uploads = self.push_to_server(name, &s);
        self.pull_to_device(name, &dev, &s);
        if let Some(d) = self.devs.get_mut(name) {
            d.base = s.base.clone();
            d.remote_view = self.server.to_paths();
        }
        if dev.crash_next_round {
            // Killed right after the round's commit; the next start finds
            // everything done (a crash anywhere else is the simulator's
            // business: this stand-in only needs one for its mutant).
            if let Some(d) = self.devs.get_mut(name) {
                d.crash_next_round = false;
                d.after_crash = true;
            }
            self.engine(name, EngineAction::Note("crash".into()));
            if self.mutant == Mutant::ReuploadAfterCrash {
                for (path, hash) in uploads {
                    self.request(name, RequestKind::Upload { path, hash }, Outcome::Done);
                }
            }
        }
        if self.mutant == Mutant::PurgeHoldingEarly {
            if let Some(d) = self.devs.get_mut(name) {
                d.holding.clear();
            }
            self.engine(name, EngineAction::Purge);
        }
        true
    }

    /// Applies the mutant's defect to a settled outcome.
    fn mutate(&mut self, dev: &Dev, s: &mut Settled) {
        match self.mutant {
            Mutant::DropConflictCopy => {
                for c in s.copies.clone() {
                    if c.reason == CopyReason::BothEdited {
                        s.server.nodes.remove(&c.id);
                        s.device.nodes.remove(&c.id);
                    }
                }
            }
            Mutant::DuplicateCopy => {
                for c in s.copies.clone() {
                    if let Some(n) = s.server.get(c.id).cloned() {
                        let id = self.next_id;
                        self.next_id += 1;
                        let dup = Node {
                            name: format!("{} copy", n.name),
                            ..n
                        };
                        s.server.nodes.insert(id, dup.clone());
                        s.device.nodes.insert(id, dup);
                    }
                }
            }
            Mutant::TrustMtimes => {
                for c in s.copies.clone() {
                    if c.reason != CopyReason::BothEdited {
                        continue;
                    }
                    // The node the copy came from: same folder, the name the
                    // copy's version wanted.
                    let original = s
                        .server
                        .nodes
                        .iter()
                        .find(|(id, n)| **id != c.id && n.name == c.original_name)
                        .map(|(id, _)| *id);
                    let local_mtime = s.server.get(c.id).map(|n| mtime(&n.kind)).unwrap_or(0);
                    s.server.nodes.remove(&c.id);
                    s.device.nodes.remove(&c.id);
                    if let Some(o) = original {
                        let remote_mtime = s.server.get(o).map(|n| mtime(&n.kind)).unwrap_or(0);
                        if local_mtime > remote_mtime {
                            let kind = Kind::File {
                                hash: c.hash.clone(),
                                mtime: local_mtime,
                            };
                            for t in [&mut s.server, &mut s.device, &mut s.base] {
                                if let Some(n) = t.nodes.get_mut(&o) {
                                    n.kind = kind.clone();
                                }
                            }
                        }
                    }
                }
            }
            Mutant::NoNfc => {
                // Keeps the on-disk spelling everywhere, so it never
                // corrects itself in a later round.
                for t in [&mut s.server, &mut s.device, &mut s.base] {
                    for (id, n) in t.nodes.iter_mut() {
                        if let Some(l) = dev.tree.get(*id)
                            && nfc(&l.name) == n.name
                        {
                            n.name = l.name.clone();
                        }
                    }
                }
            }
            Mutant::SanitizeRefusedNames => {
                for (id, why) in s.listed.clone() {
                    // Only once it was listed: "fixing" the name later.
                    let was_listed = s
                        .device
                        .path(id)
                        .is_some_and(|p| dev.listed.contains_key(&p.join("/")));
                    if let Listing::HearthRefused(_) = why
                        && was_listed
                        && let Some(n) = s.device.get(id).cloned()
                    {
                        let clean: String = n
                            .name
                            .chars()
                            .map(|c| if c == '\\' || c.is_control() { '_' } else { c })
                            .collect();
                        let clean = if clean.trim().is_empty()
                            || clean.eq_ignore_ascii_case(".htaccess")
                            || clean.eq_ignore_ascii_case(".part")
                        {
                            format!("_{clean}")
                        } else {
                            clean
                        };
                        let fixed = Node { name: clean, ..n };
                        s.device.nodes.insert(id, fixed.clone());
                        s.server.nodes.insert(id, fixed.clone());
                        s.base.nodes.insert(id, fixed);
                        s.listed.remove(&id);
                    }
                }
            }
            _ => {}
        }
    }

    /// Items under a path Hearth refuses or the device may not touch keep
    /// their state on both sides and are listed (I10).
    fn hold_back(&self, dev: &Dev, s: &mut Settled, listed: &mut BTreeMap<String, String>) {
        let mut blocked: Vec<(String, String)> = self
            .refused
            .iter()
            .map(|(p, c)| (p.clone(), format!("refused:{c}")))
            .collect();
        blocked.extend(dev.denied.iter().map(|p| (p.clone(), "denied".to_string())));
        if blocked.is_empty() {
            return;
        }
        let ids: BTreeSet<NodeId> = s
            .server
            .nodes
            .keys()
            .chain(self.server.nodes.keys())
            .chain(s.device.nodes.keys())
            .chain(dev.tree.nodes.keys())
            .copied()
            .collect();
        let mut blocked_nodes: Vec<NodeId> = Vec::new();
        for id in ids {
            let paths: Vec<String> = [
                self.server.path(id),
                s.server.path(id),
                dev.tree.path(id),
                s.device.path(id),
            ]
            .into_iter()
            .flatten()
            .map(|p| nfc(&p.join("/")))
            .collect();
            // A refused path is Hearth's, a denied one the device's own.
            let local_paths: Vec<String> = [dev.tree.path(id), s.device.path(id)]
                .into_iter()
                .flatten()
                .map(|p| nfc(&p.join("/")))
                .collect();
            let Some((root, reason)) = blocked.iter().find(|(b, r)| {
                let candidates = if r == "denied" { &local_paths } else { &paths };
                candidates.iter().any(|p| is_within(p, b))
            }) else {
                continue;
            };
            // Hearth's refusal stops only this device's writes to Hearth;
            // a denied path stops everything the device does there.
            let is_denied = reason == "denied";
            let changes = if is_denied {
                s.server.get(id) != self.server.get(id) || s.device.get(id) != dev.tree.get(id)
            } else {
                s.server.get(id) != self.server.get(id)
            };
            if !changes {
                continue;
            }
            match self.server.get(id) {
                Some(n) => s.server.nodes.insert(id, n.clone()),
                None => s.server.nodes.remove(&id),
            };
            match dev.tree.get(id) {
                Some(n) => s.device.nodes.insert(id, n.clone()),
                None => s.device.nodes.remove(&id),
            };
            match dev.base.get(id) {
                Some(n) => s.base.nodes.insert(id, n.clone()),
                None => s.base.nodes.remove(&id),
            };
            if changes {
                listed.insert(root.clone(), reason.clone());
                blocked_nodes.push(id);
                // Kept back under two names: the other one is listed too.
                if let (Some(d), Some(h)) = (dev.tree.path(id), self.server.path(id)) {
                    let (d, h) = (d.join("/"), h.join("/"));
                    if nfc(&d) != h {
                        listed.insert(d, "other_name".into());
                    }
                }
            }
        }
        // Nor a conflict copy of a kept-back item: it would be made again
        // every round.
        for c in s.copies.clone() {
            let of_blocked = blocked_nodes.iter().any(|b| {
                self.server
                    .get(*b)
                    .or(dev.tree.get(*b))
                    .is_some_and(|n| nfc(&n.name) == c.original_name)
            });
            if of_blocked && !dev.tree.has(c.id) {
                s.server.nodes.remove(&c.id);
                s.device.nodes.remove(&c.id);
                s.base.nodes.remove(&c.id);
            }
        }
        // Whatever was kept back may have lost its folder to an act the
        // person made around it: such orphans go (this stand-in keeps
        // trees well formed; the simulator plays denied paths for real).
        for t in [&mut s.server, &mut s.device, &mut s.base] {
            loop {
                let orphans: Vec<NodeId> = t
                    .nodes
                    .iter()
                    .filter(|(_, n)| {
                        n.parent != ROOT && !t.get(n.parent).is_some_and(|p| p.kind.is_folder())
                    })
                    .map(|(id, _)| *id)
                    .collect();
                if orphans.is_empty() {
                    break;
                }
                for id in orphans {
                    t.nodes.remove(&id);
                }
            }
        }
        // A blocked item never leaves a conflict copy behind half made.
        let keep: BTreeSet<NodeId> = s.server.nodes.keys().copied().collect();
        s.device.nodes.retain(|id, n| {
            keep.contains(id)
                || dev.tree.has(*id)
                || !n.name.contains(" (conflict ")
                || s.listed.contains_key(id)
        });
    }

    fn mass_change(&self, dev: &Dev, s: &Settled) -> bool {
        if self.mutant == Mutant::NoMassGuard || dev.mass_confirmed {
            return false;
        }
        let mut deleted: BTreeMap<String, usize> = BTreeMap::new();
        let after_server = s.server.to_paths();
        for (p, k) in self.server.to_paths().entries {
            if let Some(h) = k.hash()
                && !after_server.entries.contains_key(&p)
                && !after_server.holds(h)
            {
                let m = self
                    .mount_at(&p)
                    .map(|m| m.path.clone())
                    .unwrap_or_default();
                *deleted.entry(m).or_insert(0) += 1;
            }
        }
        let after_device = s.device.to_paths();
        for (p, k) in dev.tree.to_paths().entries {
            if let Some(h) = k.hash()
                && !after_device.entries.contains_key(&p)
                && !after_device.holds(h)
            {
                let m = self
                    .mount_at(&nfc(&p))
                    .map(|m| m.path.clone())
                    .unwrap_or_default();
                *deleted.entry(m).or_insert(0) += 1;
            }
        }
        let files = self.server.to_paths();
        deleted.iter().any(|(m, n)| {
            let total = files
                .entries
                .iter()
                .filter(|(p, k)| !k.is_folder() && is_within(p, m))
                .count()
                .max(1);
            *n > 50 && n * 4 > total
        })
    }

    fn condition(&self, c: Condition) -> Option<Condition> {
        if self.mutant == Mutant::BlindWrites {
            None
        } else {
            Some(c)
        }
    }

    /// Sends the server's side of the outcome: the contents first, then
    /// one write per change, each changing Hearth at its own step.
    /// Returns the uploads made.
    fn push_to_server(&mut self, name: &str, s: &Settled) -> Vec<(String, Hash)> {
        let old = self.server.clone();
        let ids: BTreeSet<NodeId> = old
            .nodes
            .keys()
            .chain(s.server.nodes.keys())
            .copied()
            .collect();
        let mut sends: Vec<(String, Hash)> = Vec::new();
        // (order, request, what it changes on Hearth)
        let mut writes: Vec<(usize, RequestKind, Change)> = Vec::new();
        for id in ids {
            let (o, n) = (old.get(id), s.server.get(id));
            let old_path = old.path(id).map(|p| p.join("/")).unwrap_or_default();
            let new_path = s.server.path(id).map(|p| p.join("/")).unwrap_or_default();
            match (o, n) {
                (None, Some(n)) => {
                    if let Some(h) = n.kind.hash() {
                        sends.push((new_path.clone(), h.clone()));
                    }
                    let op = if n.kind.is_folder() {
                        WriteOp::Mkdir
                    } else {
                        WriteOp::CreateFile
                    };
                    let depth = new_path.matches('/').count();
                    writes.push((
                        2 + depth,
                        RequestKind::Write(Write {
                            op,
                            path: new_path,
                            condition: self.condition(Condition::Absent),
                            before: None,
                        }),
                        Change::Insert(id, n.clone()),
                    ));
                }
                (Some(o), None) => {
                    // Below a deleted folder nothing is sent on its own.
                    if o.parent != ROOT && !s.server.has(o.parent) && old.has(o.parent) {
                        continue;
                    }
                    let (op, c) = match o.kind.hash() {
                        Some(h) => (WriteOp::DeleteFile, Condition::Version(h.clone())),
                        None => (WriteOp::DeleteFolder, Condition::UnchangedSince),
                    };
                    writes.push((
                        0,
                        RequestKind::Write(Write {
                            op,
                            path: old_path,
                            condition: self.condition(c),
                            before: o.kind.hash().cloned(),
                        }),
                        Change::Remove(id),
                    ));
                }
                (Some(o), Some(n)) => {
                    if o.parent != n.parent || o.name != n.name {
                        let c = match o.kind.hash() {
                            Some(h) => Condition::Version(h.clone()),
                            None => Condition::Seq,
                        };
                        writes.push((
                            1,
                            RequestKind::Write(Write {
                                op: WriteOp::Move(new_path.clone()),
                                path: old_path,
                                condition: self.condition(c),
                                before: o.kind.hash().cloned(),
                            }),
                            Change::Place(id, n.parent, n.name.clone()),
                        ));
                    }
                    if !o.kind.same_content(&n.kind)
                        && let (Some(oh), Some(nh)) = (o.kind.hash(), n.kind.hash())
                    {
                        sends.push((new_path.clone(), nh.clone()));
                        writes.push((
                            1000,
                            RequestKind::Write(Write {
                                op: WriteOp::Replace,
                                path: new_path,
                                condition: self.condition(Condition::Version(oh.clone())),
                                before: Some(oh.clone()),
                            }),
                            Change::Content(id, n.kind.clone()),
                        ));
                    }
                }
                (None, None) => {}
            }
        }
        // Deletes, then moves, then creates parents before children, then
        // replaces (stable, so the same scenario gives the same order).
        writes.sort_by_key(|(order, _, _)| *order);
        for (path, hash) in &sends {
            self.request(
                name,
                RequestKind::Upload {
                    path: path.clone(),
                    hash: hash.clone(),
                },
                Outcome::Done,
            );
        }
        let lose = self.devs.get(name).is_some_and(|d| d.lose_next_answer);
        let n = writes.len();
        for (i, (_, kind, change)) in writes.into_iter().enumerate() {
            let before = self.server.to_paths();
            change.apply(&mut self.server);
            self.keep_displaced(&before);
            let outcome = if lose && i + 1 == n {
                Outcome::AnswerLost
            } else {
                Outcome::Done
            };
            self.request(name, kind, outcome);
        }
        // Whatever the writes did not reach (none, normally) is made so.
        if self.server != s.server {
            let before = self.server.to_paths();
            self.server = s.server.clone();
            self.keep_displaced(&before);
        }
        if let Some(d) = self.devs.get_mut(name) {
            d.lose_next_answer = false;
            d.remote_view = self.server.to_paths();
        }
        sends
    }

    /// Makes the device's side of the outcome: hold what goes, download
    /// what comes, put it in place.
    fn pull_to_device(&mut self, name: &str, dev: &Dev, s: &Settled) {
        let mut held: Vec<Hash> = Vec::new();
        let mut kept = dev.tree.clone();
        for (id, node) in &dev.tree.nodes {
            let after = s.device.get(*id);
            let goes = match after {
                None => true,
                Some(a) => !a.kind.same_content(&node.kind),
            };
            if goes {
                if let Some(h) = node.kind.hash() {
                    held.push(h.clone());
                }
                if after.is_none() || !node.kind.is_folder() {
                    kept.nodes.remove(id);
                }
            }
        }
        // Children of removed folders go too.
        loop {
            let orphans: Vec<NodeId> = kept
                .nodes
                .iter()
                .filter(|(_, n)| n.parent != ROOT && !kept.has(n.parent))
                .map(|(id, _)| *id)
                .collect();
            if orphans.is_empty() {
                break;
            }
            for id in orphans {
                if let Some(n) = kept.nodes.remove(&id)
                    && let Some(h) = n.kind.hash()
                {
                    held.push(h.clone());
                }
            }
        }
        // A replaced content that becomes a conflict copy is put beside the
        // target in the same step, never only held (sync.md 3.7 step 5).
        for (id, n) in &s.device.nodes {
            if !dev.tree.has(*id)
                && kept.has(n.parent)
                && n.kind.hash().is_some_and(|h| held.contains(h))
            {
                kept.nodes.insert(*id, n.clone());
            }
        }
        if kept != dev.tree {
            if let Some(d) = self.devs.get_mut(name) {
                d.tree = kept;
                if self.mutant != Mutant::UnlinkWithoutHolding {
                    d.holding.extend(held);
                }
            }
            self.engine(name, EngineAction::Disk("held what goes".into()));
        }
        let incoming: Vec<(String, Hash)> = s
            .device
            .nodes
            .iter()
            .filter(|(id, n)| {
                n.kind.hash().is_some()
                    && !dev
                        .tree
                        .get(**id)
                        .is_some_and(|o| o.kind.same_content(&n.kind))
            })
            .filter(|(_, n)| {
                !dev.tree
                    .to_paths()
                    .holds(n.kind.hash().map_or("", String::as_str))
            })
            .filter_map(|(id, n)| {
                s.device
                    .path(*id)
                    .map(|p| (p.join("/"), n.kind.hash().cloned().unwrap_or_default()))
            })
            .collect();
        let redownload_all = self.mutant == Mutant::RedownloadAfterDbLoss && dev.after_db_loss;
        let mut downloads = incoming.clone();
        if redownload_all {
            for (p, k) in s.device.to_paths().entries {
                if let Some(h) = k.hash()
                    && !downloads.iter().any(|(q, _)| *q == p)
                {
                    downloads.push((p, h.clone()));
                }
            }
        }
        let mut corrupt = dev.corrupt_next_download;
        let mut placed_corrupt: Option<(String, Hash)> = None;
        for (path, hash) in &downloads {
            self.request(
                name,
                RequestKind::Download {
                    path: path.clone(),
                    hash: hash.clone(),
                },
                Outcome::Done,
            );
            if corrupt {
                corrupt = false;
                if self.mutant == Mutant::TrustCorruptDownload {
                    placed_corrupt = Some((path.clone(), format!("corrupt:{hash}")));
                } else {
                    // The hash does not match: fetched again (3.6).
                    self.engine(name, EngineAction::Note("hash mismatch".into()));
                    self.request(
                        name,
                        RequestKind::Download {
                            path: path.clone(),
                            hash: hash.clone(),
                        },
                        Outcome::Done,
                    );
                }
            }
        }
        if let Some(d) = self.devs.get_mut(name) {
            d.corrupt_next_download = corrupt;
        }
        if self.mutant == Mutant::PartialVisible
            && let Some((path, hash)) = incoming.first()
        {
            let mut partial = self
                .devs
                .get(name)
                .map(|d| d.tree.clone())
                .unwrap_or_default();
            if let Some(id) = s
                .device
                .at_path(&path.split('/').map(str::to_string).collect::<Vec<_>>())
                && let Some(n) = s.device.get(id)
            {
                partial.insert(
                    id,
                    n.parent,
                    &n.name,
                    Kind::File {
                        hash: format!("partial:{hash}"),
                        mtime: 0,
                    },
                );
                if partial.path(id).is_some()
                    && let Some(d) = self.devs.get_mut(name)
                {
                    d.tree = partial;
                    self.engine(name, EngineAction::Disk("writing".into()));
                }
            }
        }
        let mut finished = s.device.clone();
        if let Some((path, bad)) = placed_corrupt
            && let Some(id) =
                finished.at_path(&path.split('/').map(str::to_string).collect::<Vec<_>>())
            && let Some(n) = finished.nodes.get_mut(&id)
        {
            n.kind = Kind::File {
                hash: bad,
                mtime: 0,
            };
        }
        if let Some(d) = self.devs.get_mut(name) {
            d.tree = finished;
        }
        self.engine(name, EngineAction::Disk("put in place".into()));
    }
}

/// One write's effect on Hearth's tree.
enum Change {
    Insert(NodeId, Node),
    Remove(NodeId),
    Place(NodeId, NodeId, String),
    Content(NodeId, Kind),
}

impl Change {
    fn apply(&self, tree: &mut IdTree) {
        match self {
            Change::Insert(id, n) => {
                tree.nodes.insert(*id, n.clone());
            }
            Change::Remove(id) => {
                for gone in tree.subtree(*id) {
                    tree.nodes.remove(&gone);
                }
            }
            Change::Place(id, parent, name) => {
                if let Some(n) = tree.nodes.get_mut(id) {
                    n.parent = *parent;
                    n.name = name.clone();
                }
            }
            Change::Content(id, kind) => {
                if let Some(n) = tree.nodes.get_mut(id) {
                    n.kind = kind.clone();
                }
            }
        }
    }
}

fn mtime(k: &Kind) -> i64 {
    match k {
        Kind::File { mtime, .. } => *mtime,
        Kind::Folder => 0,
    }
}
