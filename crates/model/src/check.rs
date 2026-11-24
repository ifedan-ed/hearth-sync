//! The checkers: one function per guarantee of docs/sync.md section 2
//! (and the mass-change guard of 3.11), each reading a [`Trace`] and
//! returning every violation with the step, the device, the path, what
//! was expected and what was found.
//!
//! They read only observations (see [`crate::trace`]). Where a guarantee
//! speaks of something no observer can see (a plan, a commit), the
//! checker states the observable consequence it checks instead, in its
//! own comment, and docs/model.md lists those readings.
//!
//! The settled-outcome checks (lost versions, content that came back,
//! the exact outcome where the documents fix it) are in
//! [`crate::outcome`]; [`check_trace`] runs both.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::name::{device_refuses, hearth_refuses_local, is_engine_name, name_key, nfc};
use crate::reconcile::perms;
use crate::scenario::{Act, AdminOp, FaultOp, Side};
use crate::trace::{
    Condition, DeviceView, EngineAction, Event, Outcome, Request, RequestKind, RootState,
    ServerView, Snapshot, Trace, Write, WriteOp,
};
use crate::tree::{Hash, Kind, PathTree, base_name, is_within, nfc_path, parent_path};

/// Which guarantee a violation breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Guarantee {
    /// No saved bytes lost.
    I1,
    /// No local overwrite of unsynced content.
    I2,
    /// No remote write without the condition of a seen version.
    I3,
    /// Never half written at a real name.
    I4,
    /// Bytes verified end to end.
    I5,
    /// A missing root (or a restored server) is not a deletion.
    I6,
    /// Convergence to the settled outcome, and quiescence.
    I7,
    /// Equal after convergence.
    I8,
    /// One conflict copy per losing version.
    I9,
    /// What cannot sync is listed and blocks nothing.
    I10,
    /// Survives a crash anywhere (no completed transfer redone).
    I11,
    /// The state database is a cache.
    I12,
    /// Same seed, same run.
    I13,
    /// The mass-change guard of sync.md 3.11.
    MassChange,
    /// The runner broke its own rules (docs/model-needs.md); not the
    /// engine's fault, but the checks cannot be trusted for that run.
    Runner,
}

impl fmt::Display for Guarantee {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Guarantee::I1 => "I1 no saved bytes lost",
            Guarantee::I2 => "I2 no overwrite of unsynced local content",
            Guarantee::I3 => "I3 no remote write without a seen condition",
            Guarantee::I4 => "I4 never half written",
            Guarantee::I5 => "I5 verified end to end",
            Guarantee::I6 => "I6 missing root or restored server is not a deletion",
            Guarantee::I7 => "I7 convergence and quiescence",
            Guarantee::I8 => "I8 equal after convergence",
            Guarantee::I9 => "I9 one conflict copy per losing version",
            Guarantee::I10 => "I10 unsyncable items listed and untouched",
            Guarantee::I11 => "I11 no completed transfer redone",
            Guarantee::I12 => "I12 the state database is a cache",
            Guarantee::I13 => "I13 same seed, same run",
            Guarantee::MassChange => "3.11 mass change pauses",
            Guarantee::Runner => "runner rule",
        };
        f.write_str(s)
    }
}

/// One broken guarantee, as a reader needs it to find the fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// What was broken.
    pub guarantee: Guarantee,
    /// The step (index into [`Trace::steps`]) where it was seen.
    pub step: Option<usize>,
    /// The device, if one is concerned.
    pub device: Option<String>,
    /// The path, if one is concerned.
    pub path: Option<String>,
    /// What the documents require.
    pub expected: String,
    /// What the run did.
    pub found: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.guarantee)?;
        if let Some(s) = self.step {
            write!(f, " at step {s}")?;
        }
        if let Some(d) = &self.device {
            write!(f, " on device {d:?}")?;
        }
        if let Some(p) = &self.path {
            write!(f, " path {p:?}")?;
        }
        write!(f, ": expected {}; found {}", self.expected, self.found)
    }
}

/// Settings of the checks, each one explicit.
#[derive(Debug, Clone)]
pub struct CheckConfig {
    /// The most engine steps allowed between the last change and the
    /// runner's `Settled` (I7: "in bounded steps").
    pub settle_bound: usize,
    /// Mass change: more than this many files... (sync.md 3.11: 50).
    pub mass_files: usize,
    /// ...and more than this fraction of the mount, as numerator and
    /// denominator (3.11: a quarter).
    pub mass_fraction: (usize, usize),
}

impl CheckConfig {
    /// The settings sync.md 3.11 names, and a settle bound for small
    /// scenarios (the simulator states its own for large ones).
    pub fn plan_defaults() -> Self {
        CheckConfig {
            settle_bound: 10_000,
            mass_files: 50,
            mass_fraction: (1, 4),
        }
    }
}

fn violation(
    g: Guarantee,
    step: usize,
    device: Option<&str>,
    path: Option<&str>,
    expected: String,
    found: String,
) -> Violation {
    Violation {
        guarantee: g,
        step: Some(step),
        device: device.map(str::to_string),
        path: path.map(str::to_string),
        expected,
        found,
    }
}

/// Runs every checker over the trace (I13 needs two runs: see
/// [`check_i13`]).
pub fn check_trace(trace: &Trace, cfg: &CheckConfig) -> Vec<Violation> {
    let mut out = Vec::new();
    out.extend(check_i1(trace));
    out.extend(check_i2(trace));
    out.extend(check_i3(trace));
    out.extend(check_i4_i5(trace));
    out.extend(check_i6(trace));
    out.extend(check_i7_quiescence(trace, cfg));
    out.extend(check_i8(trace));
    out.extend(check_i9(trace));
    out.extend(check_i10(trace));
    out.extend(check_i11(trace));
    out.extend(check_i12(trace));
    out.extend(check_mass_change(trace, cfg));
    out.extend(crate::outcome::check_outcomes(trace));
    out
}

/// The device whose engine did step `event`, if it was an engine step.
pub fn engine_device(event: &Event) -> Option<&str> {
    match event {
        Event::Engine { device, .. } => Some(device),
        _ => None,
    }
}

fn request(event: &Event) -> Option<(&str, &Request)> {
    match event {
        Event::Engine {
            device,
            action: EngineAction::Request(r),
        } => Some((device, r)),
        _ => None,
    }
}

/// Whether Hearth holds `hash` as a current file, a version or in the
/// trash: the places I1 names on Hearth's side.
pub fn on_hearth(server: &ServerView, hash: &str) -> bool {
    server.tree.holds(hash) || server.versions.contains(hash) || server.trash.contains(hash)
}

fn device_hashes(d: &DeviceView) -> BTreeSet<Hash> {
    let mut out: BTreeSet<Hash> = d.tree.hash_counts().into_keys().collect();
    out.extend(d.holding.iter().cloned());
    out
}

fn everywhere(s: &Snapshot) -> BTreeSet<Hash> {
    let mut out: BTreeSet<Hash> = s.server.tree.hash_counts().into_keys().collect();
    out.extend(s.server.versions.iter().cloned());
    out.extend(s.server.trash.iter().cloned());
    for d in s.devices.values() {
        out.extend(device_hashes(d));
    }
    out
}

/// The mount a server path is in: the longest mount path above it.
pub fn mount_of<'a>(
    server: &'a ServerView,
    path: &str,
) -> Option<(&'a String, &'a crate::trace::MountView)> {
    server
        .mounts
        .iter()
        .filter(|(m, _)| is_within(path, m))
        .max_by_key(|(m, _)| m.len())
}

/// I1 (sync.md 2): "No byte sequence a person saved is ever removed from a
/// device by the engine unless, at that moment, the same byte sequence
/// exists in at least one of: the same device under another name,
/// Hearth's current files, Hearth's versions, Hearth's trash, or the
/// engine's holding area on that device."
///
/// Checked at every engine step of every device. Also: no engine step
/// makes a content vanish from everywhere at once, except a delete the
/// engine sends on a mount without trash and a replace on a mount without
/// versions, which the contract makes final (sync-api 17, I1 row: "on
/// mounts without trash, treat a remote delete as final"). That a version the people kept is still
/// somewhere when the run settles is checked in [`crate::outcome`].
pub fn check_i1(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in trace.steps.iter().enumerate() {
        let Some(dev) = engine_device(&step.event) else {
            continue;
        };
        let before = trace.before(i);
        let after = &step.after;
        if let (Some(b), Some(a)) = (before.device(dev), after.device(dev)) {
            let now = device_hashes(a);
            for h in device_hashes(b).difference(&now) {
                if !on_hearth(&after.server, h) {
                    out.push(violation(
                        Guarantee::I1,
                        i,
                        Some(dev),
                        None,
                        format!("content {h} removed from the device to exist on the device, on Hearth (files, versions, trash) or in the holding area"),
                        "it is in none of them".into(),
                    ));
                }
            }
        }
        if is_final_write(&step.event, &before.server) {
            continue;
        }
        let gone: Vec<Hash> = everywhere(before)
            .difference(&everywhere(after))
            .cloned()
            .collect();
        for h in gone {
            out.push(violation(
                Guarantee::I1,
                i,
                Some(dev),
                None,
                format!("content {h} to exist somewhere after an engine step"),
                "the step removed its last copy everywhere".into(),
            ));
        }
    }
    out
}

/// Whether `event` is a write Hearth carried out that the contract makes
/// final: a delete on a mount without trash, or a replace on a mount
/// without versions (sync-api 4: `trash`, `versions`; 17, I1 row).
fn is_final_write(event: &Event, server: &ServerView) -> bool {
    let Some((_, r)) = request(event) else {
        return false;
    };
    let RequestKind::Write(w) = &r.kind else {
        return false;
    };
    if !matches!(r.outcome, Outcome::Done | Outcome::AnswerLost) {
        return false;
    }
    let Some((_, m)) = mount_of(server, &w.path) else {
        return false;
    };
    match w.op {
        WriteOp::DeleteFile | WriteOp::DeleteFolder => !m.trash,
        WriteOp::Replace => !m.versions,
        _ => false,
    }
}

/// I2 (sync.md 2): "The engine never overwrites a local file whose
/// current content it has not recorded as synced. Before a replace it
/// holds the replaced file, compares it with the synced tree, and keeps
/// it as a conflict copy if it differs."
///
/// Checked at every engine step: a file whose content changes or which
/// goes away, and whose old content is not what the engine's synced tree
/// records for that path, must keep that content visible on the device
/// (another name: the conflict copy) or on Hearth. The holding area is
/// not enough here: a version nobody has must stay in the person's sight
/// (3.7 step 5).
pub fn check_i2(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in trace.steps.iter().enumerate() {
        let Some(dev) = engine_device(&step.event) else {
            continue;
        };
        let (Some(b), Some(a)) = (trace.before(i).device(dev), step.after.device(dev)) else {
            continue;
        };
        for (path, kind) in &b.tree.entries {
            let Kind::File { hash: old, .. } = kind else {
                continue;
            };
            if a.tree.hash_at(path) == Some(old) {
                continue;
            }
            if b.synced.hash_at(&nfc_path(path)) == Some(old) {
                continue;
            }
            if a.tree.holds(old) || on_hearth(&step.after.server, old) {
                continue;
            }
            out.push(violation(
                Guarantee::I2,
                i,
                Some(dev),
                Some(path),
                format!("unsynced content {old} replaced or removed only after being kept as a conflict copy"),
                format!("now {:?}, and {old} is nowhere visible", a.tree.get(path)),
            ));
        }
    }
    out
}

/// I3 (sync.md 2, sync-api 8.1 and 17): "every remote write, move and
/// delete carries the etag it expects, and Hearth refuses on mismatch";
/// "no remote version is replaced that the replacing device had not
/// received".
///
/// Checked for every write Hearth carried out (answered or answer lost):
/// it carried the condition its operation requires (8.1 table); a file
/// condition named exactly the version Hearth held; and that version was
/// in the device's view of Hearth just before (it had received it; at any
/// path, since the device's own moves change where it is), or is content
/// the device itself sent earlier.
pub fn check_i3(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    // Contents each device sent itself: a hash condition on its own bytes
    // names a version it knows (sync-api 8.1, `ifHash`).
    let mut sent: BTreeMap<&str, BTreeSet<&Hash>> = BTreeMap::new();
    for (i, step) in trace.steps.iter().enumerate() {
        let Some((dev, r)) = request(&step.event) else {
            continue;
        };
        if let RequestKind::Upload { hash, .. } = &r.kind {
            sent.entry(dev).or_default().insert(hash);
        }
        let RequestKind::Write(w) = &r.kind else {
            continue;
        };
        if !matches!(r.outcome, Outcome::Done | Outcome::AnswerLost) {
            continue;
        }
        let before = trace.before(i);
        let own = sent.get(dev);
        if let Some(problem) = write_condition_problem(w, before.device(dev), |h| {
            own.is_some_and(|o| o.contains(h))
        }) {
            out.push(violation(
                Guarantee::I3,
                i,
                Some(dev),
                Some(&w.path),
                problem.0,
                problem.1,
            ));
        }
    }
    out
}

/// What is wrong with a carried-out write's condition, if anything.
fn write_condition_problem(
    w: &Write,
    device: Option<&DeviceView>,
    sent: impl Fn(&Hash) -> bool,
) -> Option<(String, String)> {
    let found = format!(
        "condition {:?} with Hearth holding {:?}",
        w.condition, w.before
    );
    let file_version = |held: &Hash| -> Option<(String, String)> {
        match &w.condition {
            Some(Condition::Version(v)) if v == held => {
                // At any path: the device's own moves in the same run
                // change where it is, not which version it is.
                let seen = device.is_some_and(|d| d.remote_view.holds(v)) || sent(v);
                (!seen).then(|| {
                    (
                        format!("the condition's version {v} to be one the device had received"),
                        "it is not in the device's view of Hearth".into(),
                    )
                })
            }
            _ => Some((
                format!("a condition on the version Hearth held ({held})"),
                found.clone(),
            )),
        }
    };
    match (&w.op, &w.before) {
        (WriteOp::CreateFile | WriteOp::Mkdir, None) => (w.condition != Some(Condition::Absent))
            .then(|| ("the create to carry If-None-Match: *".into(), found.clone())),
        (WriteOp::CreateFile | WriteOp::Mkdir, Some(held)) => Some((
            format!("no create over an existing file ({held})"),
            found.clone(),
        )),
        (WriteOp::Replace | WriteOp::DeleteFile, Some(held)) => file_version(held),
        (WriteOp::Move(_), Some(held)) => file_version(held),
        (WriteOp::Move(_), None) => (w.condition != Some(Condition::Seq))
            .then(|| ("a folder's move to carry its seq".into(), found.clone())),
        (WriteOp::DeleteFolder, _) => (w.condition != Some(Condition::UnchangedSince)).then(|| {
            (
                "a folder's delete to carry the unchanged-since condition".into(),
                found.clone(),
            )
        }),
        (WriteOp::Replace | WriteOp::DeleteFile, None) => Some((
            "a replace or delete to name a file Hearth held".into(),
            found.clone(),
        )),
    }
}

/// I4 and I5 (sync.md 2): "A file is never visible at its real name half
/// written, on disk or on Hearth"; "a download becomes visible only after
/// its SHA-256 equals the hash Hearth holds; an upload is committed by
/// Hearth only if the hash of what it received equals the hash the client
/// computed from disk".
///
/// Observable consequence: every file at a real name, on every disk and
/// on Hearth, at every step, holds a content some person saved (or that
/// was there at the start). A content nobody saved is a torn or altered
/// one. On Hearth that is I5 (it committed bytes it should have refused);
/// on a device it is I5 when an altered download was injected there
/// before, else I4.
pub fn check_i4_i5(trace: &Trace) -> Vec<Violation> {
    let mut known = everywhere(&trace.start);
    let mut corrupted: BTreeSet<&str> = BTreeSet::new();
    let mut out = Vec::new();
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Person {
                act,
                applied: true,
                side,
            } => {
                known.extend(act_content(
                    &trace.scenario.devices,
                    act,
                    side,
                    trace.before(i),
                ));
            }
            Event::Fault {
                device,
                fault: FaultOp::Corrupt(_),
            } => {
                corrupted.insert(device);
            }
            Event::Admin(AdminOp::ShareArrive { .. } | AdminOp::Restore) => {
                // A share's content and a restored backup were saved by
                // people before (they come from Hearth's own past).
                known.extend(everywhere(&step.after));
            }
            _ => {}
        }
        for (path, kind) in &step.after.server.tree.entries {
            if let Some(h) = kind.hash()
                && !known.contains(h)
            {
                out.push(violation(
                    Guarantee::I5,
                    i,
                    engine_device(&step.event),
                    Some(path),
                    "Hearth to hold only whole, verified contents".into(),
                    format!("content {h}, which nobody saved"),
                ));
            }
        }
        for (name, d) in &step.after.devices {
            for (path, kind) in &d.tree.entries {
                if let Some(h) = kind.hash()
                    && !known.contains(h)
                {
                    let g = if corrupted.contains(name.as_str()) {
                        Guarantee::I5
                    } else {
                        Guarantee::I4
                    };
                    out.push(violation(
                        g,
                        i,
                        Some(name),
                        Some(path),
                        "only whole, verified contents at real names".into(),
                        format!("content {h}, which nobody saved"),
                    ));
                }
            }
        }
    }
    out
}

/// The contents a person's act brings into the world.
pub fn act_content(
    devices: &[crate::scenario::DeviceSpec],
    act: &Act,
    side: &Side,
    before: &Snapshot,
) -> Vec<Hash> {
    let found =
        |path: &str| side_tree(devices, side, before).and_then(|(t, v)| find_on_side(t, v, path));
    match act {
        Act::Create { content, .. } | Act::Edit { content, .. } => vec![content.clone()],
        // A folder becoming a file makes a content; a file becoming a
        // folder makes none.
        Act::Retype { path, content } => match found(path) {
            Some(Kind::Folder) => vec![content.clone()],
            _ => Vec::new(),
        },
        Act::Copy { from, .. } => found(from)
            .and_then(|k| k.hash().cloned())
            .into_iter()
            .collect(),
        Act::Mkdir { .. } | Act::Rename { .. } | Act::Delete { .. } => Vec::new(),
    }
}

/// The tree of the side an act was made on, and how that side compares
/// names: a device by its volume's key, Hearth in NFC.
pub fn side_tree<'a>(
    trace_devices: &[crate::scenario::DeviceSpec],
    side: &Side,
    s: &'a Snapshot,
) -> Option<(&'a PathTree, Option<crate::name::Volume>)> {
    match side {
        Side::Web => Some((&s.server.tree, None)),
        Side::Device(d) => {
            let volume = trace_devices
                .iter()
                .find(|x| x.name == *d)
                .map(|x| x.volume);
            s.device(d).map(|t| (&t.tree, volume))
        }
    }
}

/// A path as a side compares it: each name by the volume's key, or in
/// NFC on Hearth.
pub fn side_key(volume: Option<crate::name::Volume>, path: &str) -> String {
    match volume {
        Some(v) => path
            .split('/')
            .map(|n| name_key(v, n))
            .collect::<Vec<_>>()
            .join("/"),
        None => nfc_path(path),
    }
}

/// The entry a person on that side means by `path`.
pub fn find_on_side<'a>(
    tree: &'a PathTree,
    volume: Option<crate::name::Volume>,
    path: &str,
) -> Option<&'a Kind> {
    let want = side_key(volume, path);
    tree.get(path).or_else(|| {
        tree.entries
            .iter()
            .find(|(p, _)| side_key(volume, p) == want)
            .map(|(_, k)| k)
    })
}

/// The entry at `path` in `tree`, comparing paths in NFC.
pub fn find_by_nfc<'a>(tree: &'a PathTree, path: &str) -> Option<&'a Kind> {
    let want = nfc_path(path);
    tree.get(path).or_else(|| {
        tree.entries
            .iter()
            .find(|(p, _)| nfc_path(p) == want)
            .map(|(_, k)| k)
    })
}

/// I6 (sync.md 2 and 3.11): "A missing, empty or unmounted sync root is
/// never read as 'the person deleted everything'"; and a restored server
/// (a changed epoch, sync-api 6.8) "means full resync by hashes, where
/// files missing on the server are uploaded again, not deleted locally".
///
/// Checked: while a device's root is missing or replaced, its engine
/// deletes and moves nothing on Hearth, and removes nothing from Hearth's
/// tree. Between a restore and the next settled point, no engine step
/// removes from a device a content that Hearth does not hold at all.
pub fn check_i6(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut after_restore = false;
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Admin(AdminOp::Restore) => after_restore = true,
            Event::Settled => after_restore = false,
            _ => {}
        }
        let Some(dev) = engine_device(&step.event) else {
            continue;
        };
        let before = trace.before(i);
        let Some(b) = before.device(dev) else {
            continue;
        };
        if b.root != RootState::Present {
            if let Some((_, r)) = request(&step.event)
                && let RequestKind::Write(w) = &r.kind
                && matches!(
                    w.op,
                    WriteOp::DeleteFile | WriteOp::DeleteFolder | WriteOp::Move(_)
                )
                && matches!(r.outcome, Outcome::Done | Outcome::AnswerLost)
            {
                out.push(violation(
                    Guarantee::I6,
                    i,
                    Some(dev),
                    Some(&w.path),
                    format!("no delete or move on Hearth while the root is {:?}", b.root),
                    format!("{:?}", w.op),
                ));
            }
            for path in before.server.tree.entries.keys() {
                if !step.after.server.tree.entries.contains_key(path) {
                    out.push(violation(
                        Guarantee::I6,
                        i,
                        Some(dev),
                        Some(path),
                        format!("nothing removed from Hearth while the root is {:?}", b.root),
                        "removed".into(),
                    ));
                }
            }
        }
        if after_restore && let Some(a) = step.after.device(dev) {
            for (path, kind) in &b.tree.entries {
                // A removal: the path is gone (a newer version from Hearth
                // taking its place is not a deletion).
                if let Some(h) = kind.hash()
                    && !a.tree.entries.contains_key(path)
                    && !a.tree.holds(h)
                    && !on_hearth(&step.after.server, h)
                {
                    out.push(violation(
                        Guarantee::I6,
                        i,
                        Some(dev),
                        Some(path),
                        "after a restore, files Hearth lacks are uploaded again, not deleted locally".into(),
                        format!("content {h} removed from the device"),
                    ));
                }
            }
        }
    }
    out
}

/// I7, quiescence and bounded steps (sync.md 2): "every device reaches
/// the same tree as Hearth in bounded steps, and then makes no further
/// requests except the feed's wait".
///
/// Checked: after each `Settled`, until the next thing that is not an
/// engine step, engines make only `/sync/wait` (and other bookkeeping
/// requests) and change nothing on disk; and the engine steps run since
/// the last outside event before `Settled` stay within the bound. A
/// device still paused at `Settled` is a runner fault (it must confirm
/// first, docs/model-needs.md).
pub fn check_i7_quiescence(trace: &Trace, cfg: &CheckConfig) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut since_outside = 0usize;
    let mut quiet = false;
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Settled => {
                if since_outside > cfg.settle_bound {
                    out.push(violation(
                        Guarantee::I7,
                        i,
                        None,
                        None,
                        format!("settled within {} engine steps", cfg.settle_bound),
                        format!("{since_outside} engine steps"),
                    ));
                }
                for (name, d) in &step.after.devices {
                    if let Some(why) = &d.paused {
                        out.push(violation(
                            Guarantee::Runner,
                            i,
                            Some(name),
                            None,
                            "no device paused at Settled (confirm first)".into(),
                            format!("paused: {why}"),
                        ));
                    }
                }
                quiet = true;
            }
            Event::Engine { device, action } => {
                since_outside += 1;
                if quiet {
                    let bad = match action {
                        EngineAction::Request(r) => {
                            !matches!(r.kind, RequestKind::Wait | RequestKind::Other(_))
                        }
                        EngineAction::Disk(_) => true,
                        EngineAction::Purge | EngineAction::Note(_) => false,
                    };
                    if bad {
                        out.push(violation(
                            Guarantee::I7,
                            i,
                            Some(device),
                            None,
                            "only the feed's wait once settled".into(),
                            format!("{action:?}"),
                        ));
                    }
                }
            }
            _ => {
                since_outside = 0;
                quiet = false;
            }
        }
    }
    out
}

/// The listed path covering `path` on a device (itself or a folder above
/// it), compared in NFC. A transient listing covers nothing.
fn listed_reason<'a>(d: &'a DeviceView, path: &str) -> Option<&'a String> {
    let p = nfc_path(path);
    d.listed
        .iter()
        .filter(|(_, r)| !r.starts_with("transient:"))
        .find(|(l, _)| is_within(&p, &nfc_path(l)))
        .map(|(_, r)| r)
}

/// I8 (sync.md 2): "After convergence, for every synced path, local
/// content hash, remote content hash and synced-tree hash are equal;
/// names are equal as byte strings in NFC."
///
/// Checked at every `Settled`: Hearth's names are NFC; each device's tree
/// in NFC equals Hearth's except for listed items (I10 checks the
/// listings are right); and the engine's synced tree agrees with both for
/// every path they share.
pub fn check_i8(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in trace.steps.iter().enumerate() {
        if !matches!(step.event, Event::Settled) {
            continue;
        }
        let server = &step.after.server.tree;
        for path in server.entries.keys() {
            if nfc_path(path) != *path {
                out.push(violation(
                    Guarantee::I8,
                    i,
                    None,
                    Some(path),
                    "names on Hearth in NFC".into(),
                    "a name not in NFC".into(),
                ));
            }
        }
        for (name, d) in &step.after.devices {
            if d.root != RootState::Present {
                continue;
            }
            let local = d.tree.nfc();
            for (path, kind) in &server.entries {
                let here = local.get(path);
                if here.is_some_and(|k| k.same_content(kind)) {
                    if !d.synced.get(path).is_some_and(|s| s.same_content(kind))
                        && listed_reason(d, path).is_none()
                    {
                        out.push(violation(
                            Guarantee::I8,
                            i,
                            Some(name),
                            Some(path),
                            format!("the synced tree to record {kind:?}"),
                            format!("{:?}", d.synced.get(path)),
                        ));
                    }
                    continue;
                }
                if listed_reason(d, path).is_none() {
                    out.push(violation(
                        Guarantee::I8,
                        i,
                        Some(name),
                        Some(path),
                        format!("the device to hold Hearth's {kind:?} (or list why not)"),
                        format!("{here:?}, not listed"),
                    ));
                }
            }
            for (path, kind) in &local.entries {
                if server.get(path).is_none() && listed_reason(d, path).is_none() {
                    out.push(violation(
                        Guarantee::I8,
                        i,
                        Some(name),
                        Some(path),
                        "nothing on the device that Hearth lacks (or listed)".into(),
                        format!("{kind:?}, not on Hearth, not listed"),
                    ));
                }
            }
        }
    }
    out
}

/// I9 (sync.md 2 and 3.5.1): "A conflict produces exactly one conflict
/// copy per losing version, with a name that says so, and conflict copies
/// themselves sync as ordinary files (no conflict copies of conflict
/// copies while nothing else changes)."
///
/// Checked at every `Settled`: no content is on Hearth more often than
/// people made it (a second copy of one losing version, or a copy of a
/// copy, breaks this), allowing for the files a resync without history
/// (restore, deleted database) must bring back as they are; and every
/// name the engine made that reads as a conflict copy is well formed and
/// names a device of the run. (That each losing version is there at all
/// is I1, checked in [`crate::outcome`].)
pub fn check_i9(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut allowance: BTreeMap<Hash, usize> = trace.start.server.tree.hash_counts();
    let mut person_names: BTreeSet<String> = BTreeSet::new();
    let labels: BTreeSet<String> = trace
        .scenario
        .devices
        .iter()
        .map(|d| crate::name::conflict_device_label(&d.name))
        .collect();
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Person {
                act,
                side,
                applied: true,
            } => {
                for h in act_content(&trace.scenario.devices, act, side, trace.before(i)) {
                    *allowance.entry(h).or_insert(0) += 1;
                }
                if let Act::Create { path, .. }
                | Act::Rename { to: path, .. }
                | Act::Copy { to: path, .. }
                | Act::Mkdir { path } = act
                {
                    person_names.insert(nfc(base_name(path)));
                }
            }
            Event::Admin(AdminOp::Restore | AdminOp::ShareArrive { .. }) => {
                for d in step.after.devices.values() {
                    for (h, n) in d.tree.hash_counts() {
                        *allowance.entry(h).or_insert(0) += n;
                    }
                }
                for (h, n) in step.after.server.tree.hash_counts() {
                    *allowance.entry(h).or_insert(0) += n;
                }
            }
            Event::Fault {
                device,
                fault: FaultOp::DbDeleted,
            } => {
                if let Some(d) = step.after.device(device) {
                    for (h, n) in d.tree.hash_counts() {
                        *allowance.entry(h).or_insert(0) += n;
                    }
                }
            }
            Event::Settled => {
                for (h, n) in step.after.server.tree.hash_counts() {
                    let allowed = allowance.get(&h).copied().unwrap_or(0);
                    if n > allowed {
                        out.push(violation(
                            Guarantee::I9,
                            i,
                            None,
                            None,
                            format!("content {h} on Hearth at most {allowed} times (once per version people made)"),
                            format!("{n} files hold it"),
                        ));
                    }
                }
                for path in step.after.server.tree.entries.keys() {
                    let name = base_name(path);
                    if !name.contains(" (conflict ") || person_names.contains(name) {
                        continue;
                    }
                    let ok = crate::name::parse_conflict_name(name)
                        .is_some_and(|c| labels.contains(&c.device));
                    if !ok {
                        out.push(violation(
                            Guarantee::I9,
                            i,
                            None,
                            Some(path),
                            "a conflict copy name of 3.5.1 naming a device of the run".into(),
                            "a malformed conflict copy name".into(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// I10 (sync.md 2): "Items the engine cannot sync ... are left untouched
/// on the side that has them, are listed with the reason, and never block
/// other items."
///
/// Checked: at every settled point, each listing's reason is one the
/// documents give and is true of that item by the model's own name and
/// rights rules (between settled points a listing may lag the round that
/// resolves it); and at every step, an engine never changes a listed item
/// on the device. That listed
/// items block nothing is the convergence of everything else, which I8
/// checks with only listed items excused.
pub fn check_i10(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut refused: BTreeSet<String> = BTreeSet::new();
    let mut denied: BTreeSet<(String, String)> = BTreeSet::new();
    let mut share_left = false;
    // Every name a person or the start ever gave, in NFC.
    let mut given: BTreeSet<String> = BTreeSet::new();
    for t in std::iter::once(&trace.start.server.tree)
        .chain(trace.start.devices.values().map(|d| &d.tree))
    {
        given.extend(t.entries.keys().flat_map(|p| p.split('/').map(nfc)));
    }
    for m in &trace.scenario.mounts {
        given.extend(m.path.split('/').map(nfc));
    }
    for (i, step) in trace.steps.iter().enumerate() {
        if let Event::Person { act, .. } = &step.event {
            let paths: Vec<&String> = match act {
                Act::Create { path, .. }
                | Act::Edit { path, .. }
                | Act::Mkdir { path }
                | Act::Delete { path }
                | Act::Retype { path, .. } => vec![path],
                Act::Rename { from, to } | Act::Copy { from, to } => vec![from, to],
            };
            given.extend(paths.into_iter().flat_map(|p| p.split('/').map(nfc)));
        }
        // The engine invents no names: everything on Hearth at a settled
        // point is named as a person named it, or is a conflict copy, or
        // the folder a departed share's changes are kept in (sync.md
        // 3.5.1, 3.8 "the engine never renames a file on Hearth because
        // one device cannot hold its name", 4.2).
        if matches!(step.event, Event::Settled) {
            for path in step.after.server.tree.entries.keys() {
                for name in path.split('/') {
                    if !given.contains(name)
                        && crate::name::parse_conflict_name(name).is_none()
                        && !name.ends_with(" (no longer shared)")
                    {
                        out.push(violation(
                            Guarantee::I10,
                            i,
                            None,
                            Some(path),
                            "only names people gave, or conflict copy names, on Hearth".into(),
                            format!("the name {name:?}, which nobody gave"),
                        ));
                    }
                }
            }
        }
        match &step.event {
            Event::Admin(AdminOp::Refuse { path, code }) => {
                refused.insert(format!("{}\u{0}{code}", nfc_path(path)));
            }
            Event::Admin(AdminOp::ShareLeave { .. }) => share_left = true,
            Event::Fault {
                device,
                fault: FaultOp::Deny(p),
            } => {
                denied.insert((device.clone(), nfc_path(p)));
            }
            _ => {}
        }
        // A listing may lag the round that resolves it by a few steps;
        // that it is true is judged where the run has settled.
        let settled = matches!(step.event, Event::Settled);
        for (name, d) in step.after.devices.iter().filter(|_| settled) {
            let volume = trace.scenario.device(name).map(|s| s.volume);
            for (path, reason) in &d.listed {
                let ok = listing_is_true(
                    path,
                    reason,
                    d,
                    &step.after.server,
                    volume,
                    &refused,
                    &denied,
                    name,
                    share_left,
                );
                if !ok {
                    out.push(violation(
                        Guarantee::I10,
                        i,
                        Some(name),
                        Some(path),
                        "a listing reason the documents give, true of this item".into(),
                        format!("listed as {reason:?}"),
                    ));
                }
            }
        }
        let Some(dev) = engine_device(&step.event) else {
            continue;
        };
        let (Some(b), Some(a)) = (trace.before(i).device(dev), step.after.device(dev)) else {
            continue;
        };
        // The listings at the end of this run of the device's engine: an
        // item whose reason ended (Hearth deleted it, the clash went) may
        // be acted on in the run that stops listing it.
        let mut end = i;
        while trace
            .steps
            .get(end + 1)
            .is_some_and(|s| engine_device(&s.event) == Some(dev))
        {
            end += 1;
        }
        let still = trace.steps[end].after.device(dev).map(|d| &d.listed);
        for path in b
            .listed
            .keys()
            .filter(|p| still.is_some_and(|l| l.contains_key(*p)))
        {
            for (p, kind) in b.tree.entries.iter().filter(|(p, _)| is_within(p, path)) {
                // Still there, or carried along by a folder that moved
                // (same name, same content, another folder).
                let kept = a.tree.get(p) == Some(kind)
                    || a.tree.entries.iter().any(|(q, k)| {
                        base_name(q) == base_name(p)
                            && k.same_content(kind)
                            && !b.tree.entries.contains_key(q)
                    });
                if !kept {
                    out.push(violation(
                        Guarantee::I10,
                        i,
                        Some(dev),
                        Some(p),
                        format!("listed item left untouched ({kind:?})"),
                        format!("{:?}", a.tree.get(p)),
                    ));
                }
            }
        }
    }
    out
}

/// Whether listing `path` with `reason` on device `d` is right.
#[allow(clippy::too_many_arguments)]
fn listing_is_true(
    path: &str,
    reason: &str,
    d: &DeviceView,
    server: &ServerView,
    volume: Option<crate::name::Volume>,
    refused: &BTreeSet<String>,
    denied: &BTreeSet<(String, String)>,
    device: &str,
    share_left: bool,
) -> bool {
    let name = base_name(path);
    let p = nfc_path(path);
    let lacks = |bit: u8| mount_of(server, &p).is_some_and(|(_, m)| m.perms & bit == 0);
    // A problem that passes on its own (a hash mismatch being retried, a
    // file still changing) may be listed at any step; at a settled point
    // it excuses nothing (I8, see listed_reason).
    if reason.starts_with("transient:") {
        return true;
    }
    if let Some(code) = reason.strip_prefix("name_refused:") {
        return hearth_refuses_local(name).is_some_and(|r| r.code() == code);
    }
    if let Some(code) = reason.strip_prefix("device_refused:") {
        return volume
            .and_then(|v| device_refuses(v, &nfc(name)))
            .is_some_and(|r| r.code() == code);
    }
    if let Some(code) = reason.strip_prefix("refused:") {
        return refused.iter().any(|r| {
            r.split_once('\u{0}')
                .is_some_and(|(rp, rc)| rc == code && is_within(&p, rp))
        });
    }
    match reason {
        "engine_name" => is_engine_name(name, parent_path(path).is_empty()),
        "name_clash" => {
            let Some(v) = volume else { return false };
            let key = name_key(v, name);
            let folder = parent_path(&p);
            let same_key = |t: &PathTree| {
                t.entries
                    .keys()
                    .filter(|q| {
                        parent_path(&nfc_path(q)) == folder && name_key(v, base_name(q)) == key
                    })
                    .map(|q| nfc_path(q))
                    .collect::<BTreeSet<_>>()
            };
            let mut names = same_key(&server.tree);
            names.extend(same_key(&d.tree));
            names.insert(p.clone());
            // Two names one device holds as one, or one name held with
            // different contents on the two sides (one side's item could
            // not be sent).
            let differs = match (
                server.tree.get(&p),
                d.tree.entries.iter().find(|(q, _)| nfc_path(q) == p),
            ) {
                (Some(a), Some((_, b))) => !a.same_content(b),
                _ => false,
            };
            names.len() > 1 || differs
        }
        "read_only_edit" => lacks(perms::UPDATE),
        "read_only_create" => lacks(perms::CREATE),
        "read_only_move" => lacks(perms::DELETE) || lacks(perms::CREATE),
        "denied" => denied
            .iter()
            .any(|(dv, dp)| dv == device && is_within(&p, dp)),
        "inside_listed" => d
            .listed
            .keys()
            .any(|l| *l != path && is_within(&p, &nfc_path(l))),
        // One item held under two names, because one side cannot take
        // the other's name (a remote rename to a name this device cannot
        // hold, Q10; a local rename to a name Hearth refuses, 3.8): the
        // side with the refused name is listed with its reason, the other
        // name with this code. True while exactly one side holds the path.
        "other_name" => {
            d.tree.entries.keys().any(|q| nfc_path(q) == p) != server.tree.get(&p).is_some()
        }
        "no_longer_shared" => {
            share_left && p.split('/').any(|n| n.ends_with(" (no longer shared)"))
        }
        _ => false,
    }
}

/// I11 (sync.md 2): "A crash at any instruction ... and any network
/// failure at any request leave the state database consistent and the
/// next start resumes without redoing completed transfers and without
/// violating I1 to I5." I1 to I5 are checked at every step anyway.
///
/// Checked here: a device never uploads again a content to a path where
/// an earlier upload of it was answered and Hearth still holds it, and
/// never downloads again a content to a path where an earlier download
/// of it was put in place and is still there. (An upload whose answer
/// was lost may be repeated: the device cannot know, sync-api 8.5.)
pub fn check_i11(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut uploaded: BTreeSet<(String, String, Hash)> = BTreeSet::new();
    let mut downloaded: BTreeSet<(String, String, Hash)> = BTreeSet::new();
    for (i, step) in trace.steps.iter().enumerate() {
        let Some((dev, r)) = request(&step.event) else {
            continue;
        };
        let before = trace.before(i);
        match &r.kind {
            RequestKind::Upload { path, hash } => {
                let key = (dev.to_string(), path.clone(), hash.clone());
                if uploaded.contains(&key) && before.server.tree.hash_at(path) == Some(hash) {
                    out.push(violation(
                        Guarantee::I11,
                        i,
                        Some(dev),
                        Some(path),
                        format!(
                            "no second upload of {hash}: the first was answered and Hearth holds it"
                        ),
                        "uploaded again".into(),
                    ));
                }
                if r.outcome == Outcome::Done {
                    uploaded.insert(key);
                }
            }
            RequestKind::Download { path, hash } => {
                let key = (dev.to_string(), path.clone(), hash.clone());
                let still_here = before
                    .device(dev)
                    .and_then(|d| find_by_nfc(&d.tree, path))
                    .and_then(Kind::hash)
                    == Some(hash);
                if downloaded.contains(&key) && still_here {
                    out.push(violation(
                        Guarantee::I11,
                        i,
                        Some(dev),
                        Some(path),
                        format!("no second download of {hash}: it is already in place"),
                        "downloaded again".into(),
                    ));
                }
                if r.outcome == Outcome::Done {
                    downloaded.insert(key);
                }
            }
            _ => {}
        }
    }
    out
}

/// I12 (sync.md 2): "Deleting it loses no data: the next start compares
/// disk and Hearth by content hash and resyncs (identical files are
/// adopted without transfer; differing ones become conflict copies)."
/// Losing no data is I1 and the settled outcome; checked here: after the
/// database is deleted and until the run settles, the device transfers
/// no content that both sides hold at the same path at that moment.
/// (A file the person deletes meanwhile is rightly fetched again: without
/// its history the device cannot tell a delete from a file it never had.)
pub fn check_i12(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut lost_db: BTreeSet<String> = BTreeSet::new();
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Fault {
                device,
                fault: FaultOp::DbDeleted,
            } => {
                lost_db.insert(device.clone());
            }
            Event::Settled => lost_db.clear(),
            _ => {}
        }
        let Some((dev, r)) = request(&step.event) else {
            continue;
        };
        if !lost_db.contains(dev) {
            continue;
        }
        let (path, hash) = match &r.kind {
            RequestKind::Download { path, hash } | RequestKind::Upload { path, hash } => {
                (path, hash)
            }
            _ => continue,
        };
        let before = trace.before(i);
        let local = before
            .device(dev)
            .and_then(|d| find_by_nfc(&d.tree, path))
            .and_then(Kind::hash);
        if local == Some(hash) && before.server.tree.hash_at(path) == Some(hash) {
            out.push(violation(
                Guarantee::I12,
                i,
                Some(dev),
                Some(path),
                format!("identical content {hash} adopted without transfer"),
                format!("{:?}", r.kind),
            ));
        }
    }
    out
}

/// I13 (sync.md 2): "The same seed gives the same run in the simulator,
/// byte for byte in its log." Compares the logs of two runs of one seed;
/// the first differing line is the counterexample.
pub fn check_i13(first: &[String], second: &[String]) -> Option<Violation> {
    let n = first.len().max(second.len());
    for i in 0..n {
        let (a, b) = (first.get(i), second.get(i));
        if a != b {
            return Some(Violation {
                guarantee: Guarantee::I13,
                step: Some(i),
                device: None,
                path: None,
                expected: format!("log line {i} {a:?}"),
                found: format!("{b:?}"),
            });
        }
    }
    None
}

/// The mass-change guard (sync.md 3.11): "a plan that would delete or
/// overwrite more than 50 files and more than a quarter of a mount on
/// either side pauses and asks".
///
/// A plan is not observable, so the checker counts what one run of a
/// device's engine does between two outside events that concern that
/// device (its person's act, a web act, an admin act, a fault on it):
/// files it removes from its disk and files that leave Hearth's tree by
/// its steps. Past both thresholds for one mount, without the person's
/// confirmation since the last settled point, is a violation.
/// docs/model.md states this reading.
pub fn check_mass_change(trace: &Trace, cfg: &CheckConfig) -> Vec<Violation> {
    let mut out = Vec::new();
    // device -> (snapshot index at burst start, files removed per mount)
    let mut bursts: BTreeMap<String, (usize, BTreeMap<String, BTreeSet<String>>)> = BTreeMap::new();
    let mut confirmed: BTreeSet<String> = BTreeSet::new();
    let mut flagged: BTreeSet<(String, usize)> = BTreeSet::new();
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Engine { device, .. } => {
                let before = trace.before(i);
                let entry = bursts
                    .entry(device.clone())
                    .or_insert_with(|| (i, BTreeMap::new()));
                let start = entry.0;
                let removed = removed_files(before, &step.after, device);
                for path in removed {
                    let mount = mount_of(&before.server, &path)
                        .map(|(m, _)| m.clone())
                        .unwrap_or_default();
                    entry.1.entry(mount).or_default().insert(path);
                }
                if confirmed.contains(device) {
                    continue;
                }
                let base = trace.before(start);
                for (mount, files) in &entry.1 {
                    let total = base
                        .server
                        .tree
                        .entries
                        .iter()
                        .filter(|(p, k)| !k.is_folder() && is_within(p, mount))
                        .count()
                        .max(1);
                    let n = files.len();
                    if n > cfg.mass_files
                        && n * cfg.mass_fraction.1 > total * cfg.mass_fraction.0
                        && flagged.insert((device.clone(), start))
                    {
                        out.push(violation(
                            Guarantee::MassChange,
                            i,
                            Some(device),
                            Some(mount),
                            format!("a pause before deleting more than {} files and a quarter of the mount ({total} files)", cfg.mass_files),
                            format!("{n} files deleted without confirmation"),
                        ));
                    }
                }
            }
            Event::Confirm { device } => {
                confirmed.insert(device.clone());
                bursts.remove(device);
            }
            Event::Settled => {
                confirmed.clear();
                bursts.clear();
            }
            Event::Fault { device, .. } => {
                bursts.remove(device);
            }
            Event::Person {
                side: Side::Device(device),
                ..
            } => {
                bursts.remove(device);
            }
            Event::Person {
                side: Side::Web, ..
            }
            | Event::Admin(_) => bursts.clear(),
        }
    }
    out
}

/// The files an engine step of `device` removed: from its disk (a path
/// gone whose content is nowhere else on the disk: not a move) and from
/// Hearth's tree (likewise). Paths are Hearth's (NFC).
fn removed_files(before: &Snapshot, after: &Snapshot, device: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let (Some(b), Some(a)) = (before.device(device), after.device(device)) {
        for (p, k) in &b.tree.entries {
            if let Some(h) = k.hash()
                && !a.tree.entries.contains_key(p)
                && !a.tree.holds(h)
            {
                out.insert(nfc_path(p));
            }
        }
    }
    for (p, k) in &before.server.tree.entries {
        if let Some(h) = k.hash()
            && !after.server.tree.entries.contains_key(p)
            && !after.server.tree.holds(h)
        {
            out.insert(p.clone());
        }
    }
    out
}
