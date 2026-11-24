//! Property tests over random runs (docs/sync.md 7.2, at the core's own
//! scale): from a synced tree, random changes on both sides, a run to idle,
//! and the guarantees checked. The number of cases is proptest's
//! `PROPTEST_CASES` (a modest number locally; the nightly runs with
//! millions belong to CI). A failing case is shrunk and printed with its
//! seed, and replays exactly.

use proptest::prelude::*;

use super::*;
use crate::engine::{Event, StepOutcome};
use crate::env::Clock;
use crate::store::Store;
use crate::tree::OpKind;

/// One change by the person or by another device.
#[derive(Clone, Debug)]
enum Change {
    Write(usize, usize, u8),
    Delete(usize, usize),
    Move(usize, usize, usize, usize),
    DeleteFolder(usize),
    MoveFolder(usize, usize),
}

const FOLDERS: [&str; 3] = ["a", "b", "c"];
const NAMES: [&str; 4] = ["f.txt", "g.txt", "F.txt", "h"];

fn change() -> impl Strategy<Value = Change> {
    prop_oneof![
        4 => (0..3usize, 0..4usize, any::<u8>()).prop_map(|(d, n, b)| Change::Write(d, n, b)),
        2 => (0..3usize, 0..4usize).prop_map(|(d, n)| Change::Delete(d, n)),
        2 => (0..3usize, 0..4usize, 0..3usize, 0..4usize).prop_map(|(a, b, c, d)| Change::Move(a, b, c, d)),
        1 => (0..3usize).prop_map(Change::DeleteFolder),
        1 => (0..3usize, 0..3usize).prop_map(|(a, b)| Change::MoveFolder(a, b)),
    ]
}

fn content(b: u8, side: &str) -> Vec<u8> {
    format!("{side} {b}").into_bytes()
}

fn apply_local(w: &mut World, c: &Change, saved: &mut Vec<Vec<u8>>) {
    match c {
        Change::Write(d, n, b) => {
            let body = content(*b, "local");
            saved.push(body.clone());
            w.fs.write(&[FOLDERS[*d], NAMES[*n]], &body);
        }
        Change::Delete(d, n) => w.fs.remove(&[FOLDERS[*d], NAMES[*n]]),
        Change::Move(a, b, c, d) => {
            if w.fs.exists(&[FOLDERS[*a], NAMES[*b]]) && w.fs.exists(&[FOLDERS[*c]]) {
                w.fs.rename(&[FOLDERS[*a], NAMES[*b]], &[FOLDERS[*c], NAMES[*d]]);
            }
        }
        Change::DeleteFolder(d) => w.fs.remove(&[FOLDERS[*d]]),
        Change::MoveFolder(a, b) => {
            if a != b && w.fs.exists(&[FOLDERS[*a]]) && w.fs.exists(&[FOLDERS[*b]]) {
                w.fs.rename(&[FOLDERS[*a]], &[FOLDERS[*b], "moved"]);
            }
        }
    }
}

fn apply_remote(w: &mut World, c: &Change, saved: &mut Vec<Vec<u8>>) {
    match c {
        Change::Write(d, n, b) => {
            let body = content(*b, "remote");
            saved.push(body.clone());
            w.remote.client_write(&[FOLDERS[*d], NAMES[*n]], &body);
        }
        Change::Delete(d, n) => w.remote.client_delete(&[FOLDERS[*d], NAMES[*n]]),
        Change::Move(a, b, c, d) => {
            if w.remote.id_at(&[FOLDERS[*c]]).is_some() {
                w.remote
                    .client_move(&[FOLDERS[*a], NAMES[*b]], &[FOLDERS[*c], NAMES[*d]]);
            }
        }
        Change::DeleteFolder(d) => w.remote.client_delete(&[FOLDERS[*d]]),
        Change::MoveFolder(a, b) => {
            if a != b && w.remote.id_at(&[FOLDERS[*b]]).is_some() {
                w.remote
                    .client_move(&[FOLDERS[*a]], &[FOLDERS[*b], "moved"]);
            }
        }
    }
}

/// The kinds that write to Hearth: each must carry a condition (I3).
fn writes_remotely(k: OpKind) -> bool {
    matches!(
        k,
        OpKind::Upload
            | OpKind::UploadNew
            | OpKind::MkdirRemote
            | OpKind::MoveRemote
            | OpKind::DeleteRemote
            | OpKind::DeleteRemoteFolder
            | OpKind::ConflictAsideRemote
    )
}

/// Steps to idle, checking after every step that no operation that writes
/// to Hearth lacks a condition, and that the store stays consistent.
/// Optionally crashes (drops and rebuilds the engine) at `crash_at`.
fn run_checked(
    w: &mut World,
    mut e: Engine<MemStore>,
    crash_at: Option<usize>,
) -> Result<Engine<MemStore>, TestCaseError> {
    let mut steps = 0;
    let mut idle = 0;
    loop {
        steps += 1;
        prop_assert!(steps < BOUND, "no bound: a sync loop (I7)");
        if Some(steps) == crash_at {
            let store = e.into_store();
            e = Engine::new(store, World::config(VolumeRules::LINUX)).expect("ok");
        }
        let out = e.step(&mut w.env()).expect("no store failure");
        let obs = e.observe().expect("ok");
        for op in &obs.ops {
            prop_assert!(
                !writes_remotely(op.kind) || op.condition.is_some(),
                "an unconditional write planned: {op:?}"
            );
        }
        match out {
            StepOutcome::Worked => idle = 0,
            StepOutcome::Paused(p) => {
                return Err(TestCaseError::fail(format!("paused: {p:?}")));
            }
            StepOutcome::Idle { wake_at } => {
                e.wait_remote(&mut w.env(), 0).expect("ok");
                match e.step(&mut w.env()).expect("ok") {
                    StepOutcome::Worked => idle = 0,
                    StepOutcome::Paused(p) => {
                        return Err(TestCaseError::fail(format!("paused: {p:?}")));
                    }
                    StepOutcome::Idle { wake_at: w2 } => match w2.or(wake_at) {
                        Some(t) if t - w.clock.now_ns() < 3_600_000_000_000 => {
                            w.clock.set(t.max(w.clock.now_ns()));
                            idle += 1;
                            prop_assert!(idle < 50, "never idle");
                        }
                        _ => break,
                    },
                }
            }
        }
    }
    let c = check_consistency(e.store()).expect("ok");
    prop_assert!(c.is_empty(), "I11: {c:?}");
    Ok(e)
}

/// Initial files: one per folder.
fn initial(w: &mut World) -> Vec<Vec<u8>> {
    let mut saved = Vec::new();
    for (i, d) in FOLDERS.iter().enumerate() {
        let body = format!("initial {i}").into_bytes();
        w.remote.client_write(&[d, "f.txt"], &body);
        saved.push(body);
    }
    saved
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    /// Random changes on both sides converge, lose nothing and never write
    /// without a condition; if a crash is thrown in anywhere, the same.
    #[test]
    fn random_changes_converge_and_lose_nothing(
        seed in any::<u64>(),
        locals in proptest::collection::vec(change(), 0..6),
        remotes in proptest::collection::vec(change(), 0..6),
        crash in proptest::option::of(1usize..200),
    ) {
        let mut w = World::new(seed, VolumeRules::LINUX);
        let mut saved = initial(&mut w);
        let e = Engine::new(MemStore::new(), World::config(VolumeRules::LINUX)).expect("ok");
        let mut e = run_checked(&mut w, e, None)?;
        prop_assert_eq!(w.fs.user_files(), w.remote.files());
        let mut written = Vec::new();
        for c in &locals {
            apply_local(&mut w, c, &mut written);
        }
        // Saved means seen at rest: what the person wrote and still has
        // when sync looks (a file written and deleted before is not).
        saved.extend(w.fs.user_files().into_values());
        for c in &remotes {
            apply_remote(&mut w, c, &mut saved);
        }
        e.notify(Event::WatcherOverflow);
        e.notify(Event::RemoteHint);
        let e = run_checked(&mut w, e, crash)?;
        let obs = e.observe().expect("ok");
        let problems: Vec<_> = obs.problems.iter().map(|p| (p.kind, p.path.clone())).collect();
        prop_assert!(problems.is_empty(), "problems: {:?}", problems);
        prop_assert_eq!(w.fs.user_files(), w.remote.files(), "seed {}", seed);
        let v = guarantees::i8_converged(&obs);
        prop_assert!(v.is_empty(), "I8: {:?}", v);
        let world = guarantees::WorldHashes {
            device: w.fs.files().into_iter().map(|f| f.hash).collect(),
            hearth: w.remote.all_hashes(),
        };
        let saved: BTreeSet<Hash> = saved.iter().map(|b| Hash::of(b)).collect();
        let v = guarantees::i1_saved_bytes_present(&saved, &world);
        prop_assert!(v.is_empty(), "I1: {:?}", v);
        // Quiescence (I7): one more round asks Hearth nothing but the wait.
        let before = w.remote.log.len();
        let mut e = e;
        let _ = e.step(&mut w.env());
        e.wait_remote(&mut w.env(), 0).expect("ok");
        let _ = e.step(&mut w.env());
        let busy = w.remote.busy_requests_since(before);
        prop_assert!(busy.is_empty(), "requests after convergence: {:?}", busy);
    }

    /// Applying the same feed twice changes nothing (F2): a converged
    /// engine whose cursor is set back reads the same changes again and
    /// stays converged without a request beyond the reads.
    #[test]
    fn a_replayed_feed_is_harmless(seed in any::<u64>()) {
        let mut w = World::new(seed, VolumeRules::LINUX);
        initial(&mut w);
        let e = Engine::new(MemStore::new(), World::config(VolumeRules::LINUX)).expect("ok");
        let mut e = run_checked(&mut w, e, None)?;
        let before_dump = crate::store::conformance::dump(e.store());
        let mounts = e.store().mounts().expect("ok");
        let mut m = mounts[0].clone();
        m.cursor = Some(crate::ids::Cursor(format!("c{}.1", w.remote.epoch)));
        m.caught_up = false;
        e.store_mut().apply(vec![crate::store::Write::PutMount(m)]).expect("ok");
        let before = w.remote.log.len();
        let e = run_checked(&mut w, e, None)?;
        let busy: Vec<_> = w
            .remote
            .busy_requests_since(before)
            .into_iter()
            .filter(|r| r != "changes")
            .collect();
        prop_assert!(busy.is_empty(), "{:?}", busy);
        prop_assert_eq!(w.fs.user_files(), w.remote.files());
        let after_dump = crate::store::conformance::dump(e.store());
        // The trees are the same; only the mount's round counters moved.
        let strip = |s: &str| s.lines().filter(|l| !l.contains("MountRow") && !l.starts_with("meta feed_round")).collect::<Vec<_>>().join("\n");
        prop_assert_eq!(strip(&before_dump), strip(&after_dump));
    }
}
