//! The engine: one deterministic state machine per sync root, stepped by
//! its host (docs/observation.md 1 says how).
//!
//! Why a state machine stepped from outside: the simulator must be able to
//! stop it between any two of its calls to the outside world (a crash),
//! fail any call (a fault), and replay a whole run from one seed (I13).
//! So the engine owns no thread or timer; each `step` does one unit of
//! work through `Env` and commits what it learned before returning. All it
//! keeps in memory can be rebuilt from the store on the next start.
//!
//! The work, in the order a step looks for it:
//!
//! 1. Start: Hearth's hello and instance check, the root guard, recovery
//!    of operations a crash interrupted (docs/sync.md 3.10, 3.11).
//! 2. Hearth's side: the mounts list, listings, feed pages (contract 6).
//! 3. The disk's side: one folder scanned or one file hashed (3.3).
//! 4. Planning, only when every mount is caught up and no scan is half
//!    done (contract 6.6 rule 7): a pass over dirty nodes (3.5).
//! 5. One step of one operation, chosen by the scheduler (3.6, 3.7).
//! 6. Otherwise idle, with the next timer.

mod exec;
mod overlay;
mod plan_pass;
mod remote_sync;
mod scan;
mod startup;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::config::{Config, ConfigError};
use crate::env::Env;
use crate::ids::{ConflictId, HeldId, MountId, NodeId, OpId, RemoteId};
use crate::ignore::IgnoreRules;
use crate::names::{LocalPath, clean_device_name};
use crate::observe::{MassChangeView, Observation};
use crate::remote::Hello;
use crate::store::{Batch, MetaKey, Store, StoreError, Write};
use crate::tree::{ConflictRow, ProblemKind, ProblemRow, Subject};

/// What one step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepOutcome {
    /// Did one unit of work; call again.
    Worked,
    /// Nothing to do until an event, Hearth's wait, or `wake_at` (ns).
    Idle {
        /// The next timer, if any.
        wake_at: Option<i64>,
    },
    /// A guard stopped syncing; nothing changes until it is resolved.
    Paused(PauseReason),
}

/// Why the engine stopped (docs/sync.md 3.11, 3.10 "stop the root").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseReason {
    /// The sync root is missing, or its marker or volume is not the one
    /// set up (unplugged disk, restored backup, folder replaced): I6.
    RootUnavailable,
    /// Hearth answered as another instance than the one set up.
    InstanceChanged {
        /// The instance set up.
        was: String,
        /// The one answering now.
        now: String,
    },
    /// A plan would delete or overwrite more than the thresholds allow.
    MassChange(MassChangeView),
    /// Signed out, the device removed, or the account disabled: the
    /// person must connect again. Hearth's message is kept.
    SignedOut(String),
    /// Hearth refused this client version (`client_refused`).
    ClientRefused(String),
    /// Hearth speaks no contract version this engine speaks.
    ApiVersion,
    /// The disk is too full for the next download.
    DiskFull,
    /// Hearth lists no home mount, so the sync root has no counterpart.
    NoHomeMount,
}

/// Something the host saw happen outside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The watcher says something in this folder changed (a hint, never a
    /// fact: the folder is read again).
    FolderChanged(LocalPath),
    /// The watcher lost events (overflow, restart without a resume
    /// point): everything is scanned.
    WatcherOverflow,
    /// The event stream or a push says Hearth has news.
    RemoteHint,
    /// The network came back or changed: retry what waits for it now.
    NetworkChanged,
}

/// An error the engine cannot handle itself. A host drops the engine and
/// starts a new one on the same store; the simulator treats it as a crash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The settings were refused.
    Config(ConfigError),
    /// The state database failed (its last committed state stands).
    Store(StoreError),
}

impl From<StoreError> for EngineError {
    fn from(e: StoreError) -> Self {
        EngineError::Store(e)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for EngineError {}

/// Shorthand for engine results.
pub type EngineResult<T> = Result<T, EngineError>;

/// The start-up stages, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Hello,
    Root,
    Recover,
    Running,
}

/// The engine's id counters, loaded from the store and written back in the
/// batch that uses them, so an id is never given twice.
#[derive(Clone, Debug, Default)]
pub(super) struct Counters {
    pub(super) node: u64,
    op: u64,
    held: u64,
    conflict: u64,
    round: u64,
    generation: u64,
    pub(super) changed: bool,
}

/// The state of an upload kept only in memory (rebuilt from Hearth's
/// session status after a restart, as 3.6 says).
#[derive(Clone, Default)]
struct OpRuntime {
    /// The whole file's hash as far as the chunks were read.
    hasher: Option<crate::ids::ContentHasher>,
    /// The next chunk (from 1) to read.
    next_chunk: u64,
    /// Chunks Hearth already holds.
    stored: BTreeSet<u64>,
}

/// One sync root's engine.
pub struct Engine<S: Store> {
    store: S,
    cfg: Config,
    device: String,
    ignore: IgnoreRules,
    stage: Stage,
    pause: Option<PauseReason>,
    hello: Option<Hello>,
    ids: Counters,
    root: Option<NodeId>,
    // Hearth's side.
    mounts_stale: bool,
    wake: BTreeSet<MountId>,
    folder_lists: BTreeMap<(MountId, RemoteId), Option<String>>,
    remote_retry_at: Option<i64>,
    remote_attempt: u32,
    // The disk's side.
    scan_queue: BTreeMap<NodeId, bool>,
    full_scan_wanted: bool,
    missing: BTreeSet<NodeId>,
    hash_queue: BTreeSet<NodeId>,
    hash_tries: BTreeMap<NodeId, u32>,
    absent_roots: BTreeSet<NodeId>,
    // Planning and operations.
    with_problem: BTreeSet<NodeId>,
    split_deletes: BTreeSet<NodeId>,
    runtime: BTreeMap<OpId, OpRuntime>,
    mass_approved: Option<u64>,
    // Timers.
    next_rescan_ns: Option<i64>,
    next_problem_retry_ns: Option<i64>,
}

impl<S: Store> Engine<S> {
    /// An engine for one sync root over its store. Nothing is read or
    /// written until the first `step`.
    pub fn new(store: S, cfg: Config) -> EngineResult<Engine<S>> {
        cfg.validate().map_err(EngineError::Config)?;
        let device = clean_device_name(&cfg.device_name)
            .ok_or(EngineError::Config(ConfigError::DeviceName))?;
        let (ignore, _bad) = IgnoreRules::new(&cfg.ignore);
        Ok(Engine {
            store,
            cfg,
            device,
            ignore,
            stage: Stage::Hello,
            pause: None,
            hello: None,
            ids: Counters::default(),
            root: None,
            mounts_stale: true,
            wake: BTreeSet::new(),
            folder_lists: BTreeMap::new(),
            remote_retry_at: None,
            remote_attempt: 0,
            scan_queue: BTreeMap::new(),
            full_scan_wanted: false,
            missing: BTreeSet::new(),
            hash_queue: BTreeSet::new(),
            hash_tries: BTreeMap::new(),
            absent_roots: BTreeSet::new(),
            with_problem: BTreeSet::new(),
            split_deletes: BTreeSet::new(),
            runtime: BTreeMap::new(),
            mass_approved: None,
            next_rescan_ns: None,
            next_problem_retry_ns: None,
        })
    }

    /// The store, for the host's status screens and the tests.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The store, mutable: for the simulator's fault injection
    /// (`Store::fail_writes_after`), never for changing rows behind the
    /// engine's back.
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// Gives the store back (the simulator keeps it across a crash).
    pub fn into_store(self) -> S {
        self.store
    }

    /// The settings in force.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Tells the engine what the host saw happen.
    pub fn notify(&mut self, event: Event) {
        match event {
            Event::FolderChanged(path) => self.hint_folder(&path),
            Event::WatcherOverflow => self.queue_full_scan(),
            Event::RemoteHint => {
                self.wake_all_mounts();
                self.remote_retry_at = None;
            }
            Event::NetworkChanged => {
                self.remote_retry_at = None;
                self.remote_attempt = 0;
                self.wake_all_mounts();
            }
        }
    }

    /// Does one unit of work (docs/observation.md 1).
    pub fn step(&mut self, env: &mut Env<'_>) -> EngineResult<StepOutcome> {
        if let Some(p) = &self.pause {
            return Ok(StepOutcome::Paused(p.clone()));
        }
        let worked = match self.stage {
            Stage::Hello => self.start_hello(env)?,
            Stage::Root => self.start_root(env)?,
            Stage::Recover => self.start_recover(env)?,
            Stage::Running => self.run(env)?,
        };
        if let Some(p) = &self.pause {
            return Ok(StepOutcome::Paused(p.clone()));
        }
        if worked {
            Ok(StepOutcome::Worked)
        } else {
            Ok(StepOutcome::Idle {
                wake_at: self.next_wake()?,
            })
        }
    }

    /// One step of the running engine: the first kind of work found.
    fn run(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        let now = env.clock.now_ns();
        self.fire_timers(now)?;
        let remote_ready = self.remote_retry_at.is_none_or(|t| t <= now);
        if remote_ready && self.remote_work(env)? {
            return Ok(true);
        }
        if self.resolve_departed()? {
            return Ok(true);
        }
        if self.root.is_some() && self.scan_work(env)? {
            return Ok(true);
        }
        if self.ready_to_plan()? && self.plan_pass(env)? {
            return Ok(true);
        }
        if self.pause.is_some() {
            return Ok(false);
        }
        if self.op_work(env, now)? {
            return Ok(true);
        }
        if self.purge_held(env, now)? {
            return Ok(true);
        }
        Ok(false)
    }

    /// The one request an idle engine makes: Hearth's wait over every
    /// followed mount (contract 6.7). Mounts with news are read on the
    /// following steps.
    pub fn wait_remote(&mut self, env: &mut Env<'_>, wait_s: u32) -> EngineResult<()> {
        self.remote_wait(env, wait_s)
    }

    /// Lets a paused mass change go on: exactly the plan whose token is
    /// given (from `PauseReason::MassChange`).
    pub fn approve_mass_change(&mut self, token: u64) {
        if let Some(PauseReason::MassChange(v)) = &self.pause
            && v.token == token
        {
            self.mass_approved = Some(token);
            self.pause = None;
        }
    }

    /// The person confirmed that the new Hearth instance is theirs: what
    /// the engine knew of the old one is forgotten, nothing is deleted on
    /// either side, and everything is compared again by content (I6, I12).
    pub fn confirm_new_instance(&mut self) -> EngineResult<()> {
        if let Some(PauseReason::InstanceChanged { now, .. }) = self.pause.clone() {
            self.forget_remote_history(Some(now))?;
            self.pause = None;
            self.stage = Stage::Hello;
        }
        Ok(())
    }

    /// Tries again after a pause whose cause the person or host fixed (the
    /// disk plugged in again, space freed, signed in again).
    pub fn resume(&mut self) {
        if matches!(
            self.pause,
            Some(
                PauseReason::RootUnavailable
                    | PauseReason::DiskFull
                    | PauseReason::SignedOut(_)
                    | PauseReason::ClientRefused(_)
                    | PauseReason::ApiVersion
                    | PauseReason::NoHomeMount
            )
        ) {
            self.pause = None;
            self.stage = Stage::Hello;
        }
    }

    /// Stops syncing a folder (3.13): what is on disk and synced leaves
    /// through the holding area; nothing is deleted on Hearth.
    pub fn unselect(&mut self, folder: NodeId) -> EngineResult<()> {
        let mut b = vec![Write::Unselect(folder), Write::MarkDirty(folder)];
        for n in self.subtree(folder)? {
            b.push(Write::MarkDirty(n));
        }
        self.commit(b)
    }

    /// Syncs a folder again.
    pub fn reselect(&mut self, folder: NodeId) -> EngineResult<()> {
        let mut b = vec![Write::Reselect(folder), Write::MarkDirty(folder)];
        for n in self.subtree(folder)? {
            b.push(Write::MarkDirty(n));
        }
        self.commit(b)?;
        self.queue_full_scan();
        Ok(())
    }

    /// Syncs a mount Hearth said to ask about (`prompt: ask`), or stops.
    pub fn select_mount(&mut self, mount: &MountId, selected: bool) -> EngineResult<()> {
        let Some(mut m) = self.store.mounts()?.into_iter().find(|m| &m.id == mount) else {
            return Ok(());
        };
        m.selected = selected;
        let root = m.root_node;
        self.commit(vec![Write::PutMount(m), Write::MarkDirty(root)])?;
        self.wake.insert(mount.clone());
        Ok(())
    }

    /// Every conflict copy made (I9).
    pub fn conflict_copies(&self) -> EngineResult<Vec<ConflictRow>> {
        Ok(self.store.conflicts()?)
    }

    /// A snapshot of everything observable (docs/observation.md 2).
    pub fn observe(&self) -> EngineResult<Observation> {
        Ok(crate::observe::observe(
            &self.store,
            self.phase(),
            &self.absent_roots,
            &self.ignore,
        )?)
    }

    /// The phase for the observation.
    fn phase(&self) -> crate::observe::Phase {
        use crate::observe::Phase;
        match (&self.pause, self.stage) {
            (Some(p), _) => Phase::Paused(p.clone()),
            (None, Stage::Running) => Phase::Running,
            (None, _) => Phase::Starting,
        }
    }

    // ----- shared helpers -----

    /// Commits a batch, with the id counters it used, and wakes the nodes
    /// waiting for what it changes: a node marked dirty or removed wakes
    /// those waiting for it; a completed operation wakes those waiting
    /// for any (`WAIT_ANY`). The wake is in the same transaction as the
    /// change, so a crash cannot lose it.
    fn commit(&mut self, mut batch: Batch) -> EngineResult<()> {
        let mut woken = Vec::new();
        let removed: BTreeSet<NodeId> = batch
            .iter()
            .filter_map(|w| match w {
                Write::DelNode(n) => Some(*n),
                _ => None,
            })
            .collect();
        let mut targets: BTreeSet<NodeId> = BTreeSet::new();
        for w in &batch {
            match w {
                Write::MarkDirty(n) => {
                    targets.insert(*n);
                }
                Write::DelNode(n) => {
                    targets.insert(*n);
                    targets.insert(crate::store::WAIT_ANY);
                }
                Write::DelOp(_) => {
                    targets.insert(crate::store::WAIT_ANY);
                }
                // A node listed as a problem will not move: those waiting
                // for it decide again (they become problems themselves).
                Write::PutProblem(p) => {
                    if let crate::tree::Subject::Node(n) = p.subject {
                        targets.insert(n);
                    }
                }
                _ => {}
            }
        }
        // Waits written in this same batch count too: a node parked here on
        // something this batch changes must not sleep through it.
        let parked: Vec<(NodeId, NodeId)> = batch
            .iter()
            .filter_map(|w| match w {
                Write::Wait(n, t) => Some((*n, *t)),
                _ => None,
            })
            .collect();
        for t in &targets {
            let mut ws = self.store.waiters_on(*t)?;
            ws.extend(parked.iter().filter(|(_, pt)| pt == t).map(|(n, _)| *n));
            for w in ws {
                if !removed.contains(&w) {
                    woken.push(Write::MarkDirty(w));
                }
            }
        }
        batch.extend(woken);
        if self.ids.changed {
            batch.push(Write::PutMeta(MetaKey::NextNode, self.ids.node.to_string()));
            batch.push(Write::PutMeta(MetaKey::NextOp, self.ids.op.to_string()));
            batch.push(Write::PutMeta(MetaKey::NextHeld, self.ids.held.to_string()));
            batch.push(Write::PutMeta(
                MetaKey::NextConflict,
                self.ids.conflict.to_string(),
            ));
            batch.push(Write::PutMeta(
                MetaKey::FeedRound,
                self.ids.round.to_string(),
            ));
            batch.push(Write::PutMeta(
                MetaKey::NextGeneration,
                self.ids.generation.to_string(),
            ));
        }
        if batch.is_empty() {
            return Ok(());
        }
        self.store.apply(batch)?;
        self.ids.changed = false;
        Ok(())
    }

    fn load_counters(&mut self) -> EngineResult<()> {
        let get = |s: &S, k: MetaKey| -> EngineResult<u64> {
            Ok(s.meta(k)?.and_then(|v| v.parse().ok()).unwrap_or(1))
        };
        self.ids = Counters {
            node: get(&self.store, MetaKey::NextNode)?,
            op: get(&self.store, MetaKey::NextOp)?,
            held: get(&self.store, MetaKey::NextHeld)?,
            conflict: get(&self.store, MetaKey::NextConflict)?,
            round: get(&self.store, MetaKey::FeedRound)?,
            generation: get(&self.store, MetaKey::NextGeneration)?,
            changed: false,
        };
        Ok(())
    }

    fn alloc_node(&mut self) -> NodeId {
        let n = NodeId(self.ids.node);
        self.ids.node += 1;
        self.ids.changed = true;
        n
    }

    fn alloc_op(&mut self) -> OpId {
        let n = OpId(self.ids.op);
        self.ids.op += 1;
        self.ids.changed = true;
        n
    }

    fn alloc_held(&mut self) -> HeldId {
        let n = HeldId(self.ids.held);
        self.ids.held += 1;
        self.ids.changed = true;
        n
    }

    fn alloc_conflict(&mut self) -> ConflictId {
        let n = ConflictId(self.ids.conflict);
        self.ids.conflict += 1;
        self.ids.changed = true;
        n
    }

    /// A problem row for a node or path (I10), counted if already listed.
    fn problem_write(
        &mut self,
        subject: Subject,
        kind: ProblemKind,
        reason: String,
        now: i64,
    ) -> EngineResult<Write> {
        let old = self
            .store
            .problems()?
            .into_iter()
            .find(|p| p.subject == subject);
        if let Subject::Node(n) = &subject {
            self.with_problem.insert(*n);
        }
        Ok(Write::PutProblem(ProblemRow {
            subject,
            kind,
            reason,
            first_ns: old.as_ref().map_or(now, |o| o.first_ns),
            last_ns: now,
            count: old.map_or(1, |o| o.count.saturating_add(1)),
        }))
    }

    /// Removes a node's problem, if it has one.
    fn clear_problem(&mut self, n: NodeId, batch: &mut Batch) {
        if self.with_problem.remove(&n) {
            batch.push(Write::DelProblem(Subject::Node(n)));
        }
    }

    /// Every node below a folder by any picture (for selective sync).
    fn subtree(&self, folder: NodeId) -> EngineResult<Vec<NodeId>> {
        let mut out = Vec::new();
        let mut stack = vec![folder];
        let mut seen = BTreeSet::new();
        while let Some(n) = stack.pop() {
            if !seen.insert(n) {
                continue;
            }
            if n != folder {
                out.push(n);
            }
            stack.extend(self.store.local_children(n)?);
            if let Some(r) = self.store.node(n)?.and_then(|r| r.remote) {
                stack.extend(self.store.remote_children(&r.id)?);
            }
        }
        Ok(out)
    }

    /// The earliest timer.
    fn next_wake(&self) -> EngineResult<Option<i64>> {
        let mut t: Option<i64> = None;
        let mut take = |v: Option<i64>| {
            if let Some(v) = v {
                t = Some(t.map_or(v, |x| x.min(v)));
            }
        };
        take(self.next_rescan_ns);
        take(self.next_problem_retry_ns);
        take(self.remote_retry_at);
        for op in self.store.ops()? {
            take(op.next_try_ns);
        }
        Ok(t)
    }

    /// Timers that came due put their work in the queues.
    fn fire_timers(&mut self, now: i64) -> EngineResult<()> {
        if self.next_rescan_ns.is_some_and(|t| t <= now) {
            self.next_rescan_ns = Some(now + secs(self.cfg.rescan_interval_s));
            self.queue_full_scan();
        }
        if self.next_problem_retry_ns.is_some_and(|t| t <= now) {
            self.next_problem_retry_ns = Some(now + secs(self.cfg.problem_retry_s));
            let mut b = Vec::new();
            for p in self.store.problems()? {
                if let Subject::Node(n) = p.subject
                    && self.store.node(n)?.is_some()
                {
                    b.push(Write::MarkDirty(n));
                }
            }
            self.commit(b)?;
        }
        Ok(())
    }
}

/// Seconds to nanoseconds, saturating.
fn secs(s: u64) -> i64 {
    i64::try_from(s.saturating_mul(1_000_000_000)).unwrap_or(i64::MAX)
}
