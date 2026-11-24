//! Observations: what a run looked like after every step. The checkers
//! read only this.
//!
//! It holds what can be seen from outside the engine's decisions: the
//! trees on Hearth and on each disk, the holding areas, what each device
//! lists as not syncable, the requests made and how they ended, and a few
//! facts of the engine's state (its synced tree and its view of Hearth)
//! that the contract makes observable. It never holds a plan or a reason
//! for a decision. docs/model-needs.md says what a runner must fill in.

use std::collections::{BTreeMap, BTreeSet};

use crate::scenario::{Act, AdminOp, FaultOp, Scenario, Side};
use crate::tree::{Hash, PathTree};

/// A whole run: the scenario and a snapshot after every step.
#[derive(Debug, Clone)]
pub struct Trace {
    /// What was run (devices, mounts, and the ops).
    pub scenario: Scenario,
    /// The state before the first step.
    pub start: Snapshot,
    /// Every step, in order.
    pub steps: Vec<Step>,
    /// The run's log, line by line, for the determinism check (I13).
    pub log: Vec<String>,
}

/// One step: one thing that happened, and the state right after it.
#[derive(Debug, Clone)]
pub struct Step {
    /// What happened.
    pub event: Event,
    /// The state after it.
    pub after: Snapshot,
}

/// The state of everything at one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Simulated time, microseconds since 1970 UTC (Hearth's clock).
    pub time: i64,
    /// Hearth.
    pub server: ServerView,
    /// Each device, by name.
    pub devices: BTreeMap<String, DeviceView>,
}

/// Hearth as the person's tree shows it, plus where displaced content
/// went.
#[derive(Debug, Clone, Default)]
pub struct ServerView {
    /// The person's tree: home with every present mount at its path.
    pub tree: PathTree,
    /// Every content kept as an older version.
    pub versions: BTreeSet<Hash>,
    /// Every content held in the trash.
    pub trash: BTreeSet<Hash>,
    /// The mounts present now, by path ("" is home).
    pub mounts: BTreeMap<String, MountView>,
}

/// One mount as observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountView {
    /// A delete goes to the trash.
    pub trash: bool,
    /// A replaced content is kept as a version.
    pub versions: bool,
    /// Rights bits.
    pub perms: u8,
}

/// Whether a device's sync root is there (sync.md 3.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RootState {
    /// There, with its marker.
    #[default]
    Present,
    /// Gone (unplugged, unmounted).
    Missing,
    /// A folder is there but without the engine's marker.
    Replaced,
}

/// One device as observed.
#[derive(Debug, Clone, Default)]
pub struct DeviceView {
    /// The synced folder as a person sees it: without the engine's own
    /// `.hearthsync` folder and `*.hearthtmp` files. Names as on disk.
    pub tree: PathTree,
    /// Every content in the holding area ("Removed by sync", sync.md
    /// 3.7).
    pub holding: BTreeSet<Hash>,
    /// What the device lists as not syncable: path -> reason code.
    pub listed: BTreeMap<String, String>,
    /// The engine's synced tree (its base, sync.md 1), by path.
    pub synced: PathTree,
    /// The engine's view of Hearth (its remote tree, sync.md 1), by path.
    pub remote_view: PathTree,
    /// The root's state.
    pub root: RootState,
    /// The device has network.
    pub online: bool,
    /// The engine is paused waiting for the person, and why
    /// (`mass_change`, `root`, `instance`).
    pub paused: Option<String>,
}

/// What one step was.
#[derive(Debug, Clone)]
pub enum Event {
    /// A person acted. `applied` is false when the act was skipped
    /// because its target was missing or its name taken at that moment.
    Person {
        /// Where.
        side: Side,
        /// What.
        act: Act,
        /// Whether it happened.
        applied: bool,
    },
    /// The operator or another person acted on Hearth.
    Admin(AdminOp),
    /// A fault was set or happened on a device.
    Fault {
        /// Which device.
        device: String,
        /// What.
        fault: FaultOp,
    },
    /// The person answered a pause with "go on".
    Confirm {
        /// Which device.
        device: String,
    },
    /// The engine of one device did one thing.
    Engine {
        /// Which device.
        device: String,
        /// What.
        action: EngineAction,
    },
    /// The runner declares the run settled: faults stopped, every device
    /// online and with nothing left to do. The convergence checks run
    /// here (sync.md 7.3).
    Settled,
}

/// One action of an engine.
#[derive(Debug, Clone)]
pub enum EngineAction {
    /// A request to Hearth and how it ended.
    Request(Request),
    /// A change on the device's disk (download put in place, a file
    /// held, a rename, a folder made). The snapshot shows its effect; the
    /// text is for people reading a counterexample.
    Disk(String),
    /// The holding area was emptied of old entries (sync.md 3.7).
    Purge,
    /// Anything else the runner wants in the record (a scan, a commit).
    Note(String),
}

/// One request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What was asked.
    pub kind: RequestKind,
    /// How it ended.
    pub outcome: Outcome,
}

/// What a request asked, in the terms the checkers need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// `/sync/wait` (the only request allowed once quiet, I7).
    Wait,
    /// `/sync/changes`.
    Changes,
    /// `/sync/tree`.
    Tree,
    /// `/sync/mounts`.
    Mounts,
    /// Reading a file's content.
    Download {
        /// The path on Hearth.
        path: String,
        /// The content asked for.
        hash: Hash,
    },
    /// Sending a file's content (one request or a session's finish).
    Upload {
        /// The path on Hearth.
        path: String,
        /// The hash announced.
        hash: Hash,
    },
    /// A write that changes or removes something already on Hearth, or
    /// makes something new (sync-api 9).
    Write(Write),
    /// Anything else (ack, token rotation, the device grant).
    Other(String),
}

/// A write to Hearth with its condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    /// Which operation.
    pub op: WriteOp,
    /// The entry's path on Hearth when the write was sent.
    pub path: String,
    /// The condition sent with it (sync-api 8.1).
    pub condition: Option<Condition>,
    /// What Hearth held at `path` when the write arrived: a file's hash,
    /// or `None` for a folder or nothing.
    pub before: Option<Hash>,
}

/// The kinds of write (sync-api 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Create a file (9.1) or finish an upload session as a new file.
    CreateFile,
    /// Replace a file's content (9.2).
    Replace,
    /// Make a folder (9.4).
    Mkdir,
    /// Move or rename (9.5), to this path.
    Move(String),
    /// Delete a file (9.7).
    DeleteFile,
    /// Delete a folder with all it holds (9.7).
    DeleteFolder,
}

/// The condition of a write (sync-api 8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// `If-Match` or `X-Hearth-If-Hash`: the file version the device
    /// expects, given here by its content hash.
    Version(Hash),
    /// Create only if nothing has the name (`If-None-Match: *`).
    Absent,
    /// A folder's delete: nothing below changed since the cursor.
    UnchangedSince,
    /// A folder's move: its `seq`.
    Seq,
}

/// How a request ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Done, and the answer arrived.
    Done,
    /// Refused with this status and code; nothing changed.
    Refused(u16, String),
    /// Done by Hearth, but the answer was lost (the device cannot know).
    AnswerLost,
    /// Failed before Hearth acted (refused connection, reset, stall).
    Failed,
}

impl Snapshot {
    /// The device `name`, if it exists in this snapshot.
    pub fn device(&self, name: &str) -> Option<&DeviceView> {
        self.devices.get(name)
    }
}

impl Trace {
    /// The snapshot before step `i` (the start for step 0).
    pub fn before(&self, i: usize) -> &Snapshot {
        if i == 0 {
            &self.start
        } else {
            self.steps.get(i - 1).map_or(&self.start, |s| &s.after)
        }
    }
}
