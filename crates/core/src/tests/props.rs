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
    rules: VolumeRules,
) -> Result<Engine<MemStore>, TestCaseError> {
    let mut steps = 0;
    let mut idle = 0;
    loop {
        steps += 1;
        prop_assert!(steps < BOUND, "no bound: a sync loop (I7)");
        if Some(steps) == crash_at {
            let store = e.into_store();
            e = Engine::new(store, World::config(rules)).expect("ok");
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

/// One random case. On a case-sensitive volume everything must converge
/// with nothing listed; on an insensitive one, what is not converged must
/// be listed (two Hearth names that are one name here).
fn random_case(
    rules: VolumeRules,
    seed: u64,
    locals: &[Change],
    remotes: &[Change],
    crash: Option<usize>,
) -> Result<(), TestCaseError> {
    let strict = !rules.case_insensitive;
    let mut w = World::new(seed, rules);
    let mut saved = initial(&mut w);
    let e = Engine::new(MemStore::new(), World::config(rules)).expect("ok");
    let mut e = run_checked(&mut w, e, None, rules)?;
    prop_assert_eq!(w.fs.user_files(), w.remote.files());
    let mut written = Vec::new();
    for c in locals {
        apply_local(&mut w, c, &mut written);
    }
    // Saved means seen at rest: what the person wrote and still has when
    // sync looks (a file written and deleted before is not).
    saved.extend(w.fs.user_files().into_values());
    for c in remotes {
        apply_remote(&mut w, c, &mut saved);
    }
    e.notify(Event::WatcherOverflow);
    e.notify(Event::RemoteHint);
    let e = run_checked(&mut w, e, crash, rules)?;
    let obs = e.observe().expect("ok");
    if strict {
        let problems: Vec<_> = obs
            .problems
            .iter()
            .map(|p| (p.kind, p.path.clone()))
            .collect();
        prop_assert!(problems.is_empty(), "problems: {:?}", problems);
        prop_assert_eq!(w.fs.user_files(), w.remote.files(), "seed {}", seed);
        let v = guarantees::i8_converged(&obs);
        prop_assert!(v.is_empty(), "I8: {:?}", v);
    } else {
        let v = guarantees::i10_problems_listed(&obs);
        prop_assert!(v.is_empty(), "I10: {:?}", v);
        let stuck: Vec<_> = obs
            .problems
            .iter()
            .filter(|p| p.kind == crate::tree::ProblemKind::Stuck)
            .collect();
        prop_assert!(stuck.is_empty(), "stuck: {:?}\n{:#?}", stuck, obs.nodes);
    }
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
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    /// Random changes on both sides converge, lose nothing and never write
    /// without a condition; if a crash is thrown in anywhere, the same.
    /// On a case-sensitive volume.
    #[test]
    fn random_changes_converge_and_lose_nothing(
        seed in any::<u64>(),
        locals in proptest::collection::vec(change(), 0..6),
        remotes in proptest::collection::vec(change(), 0..6),
        crash in proptest::option::of(1usize..200),
    ) {
        random_case(VolumeRules::LINUX, seed, &locals, &remotes, crash)?;
    }

    /// The same on a case-insensitive volume, where `f.txt` and `F.txt`
    /// are one name: what cannot sync must be listed (I10), nothing lost.
    #[test]
    fn random_changes_on_a_case_insensitive_volume(
        seed in any::<u64>(),
        locals in proptest::collection::vec(change(), 0..6),
        remotes in proptest::collection::vec(change(), 0..6),
        crash in proptest::option::of(1usize..200),
    ) {
        random_case(VolumeRules::WINDOWS, seed, &locals, &remotes, crash)?;
    }

    /// Applying the same feed twice changes nothing (F2): a converged
    /// engine whose cursor is set back reads the same changes again and
    /// stays converged without a request beyond the reads.
    #[test]
    fn a_replayed_feed_is_harmless(seed in any::<u64>()) {
        let mut w = World::new(seed, VolumeRules::LINUX);
        initial(&mut w);
        let e = Engine::new(MemStore::new(), World::config(VolumeRules::LINUX)).expect("ok");
        let mut e = run_checked(&mut w, e, None, VolumeRules::LINUX)?;
        let before_dump = crate::store::conformance::dump(e.store());
        let mounts = e.store().mounts().expect("ok");
        let mut m = mounts[0].clone();
        m.cursor = Some(crate::ids::Cursor(format!("c{}.1", w.remote.epoch)));
        m.caught_up = false;
        e.store_mut().apply(vec![crate::store::Write::PutMount(m)]).expect("ok");
        let before = w.remote.log.len();
        let e = run_checked(&mut w, e, None, VolumeRules::LINUX)?;
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

/// Regression, from the build copy's CI (2026-11-24): two local moves on a
/// case-insensitive volume, the second onto a name the first one left
/// (`b/F.txt` is `b/f.txt` there). Both nodes changed names in one read
/// of the folder, and the store refused the batch (a name key held twice).
#[test]
fn regression_two_moves_pass_a_name_in_one_read() {
    let r = random_case(
        VolumeRules::WINDOWS,
        0,
        &[Change::Move(0, 0, 0, 1), Change::Move(1, 2, 0, 2)],
        &[],
        None,
    );
    assert!(r.is_ok(), "{r:?}");
}

/// Regression, from the build copy's CI (2026-11-24): on a
/// case-insensitive volume Hearth moves `c/f.txt` to `a/F.txt` (one name
/// with `a/f.txt` here, so it is listed and stays in `c`) and then deletes
/// `c`. The folder waited for a child that will never leave; it now stays
/// with it (3.5 F: a folder survives as far as what is in it does).
#[test]
fn regression_folder_deleted_around_a_listed_item() {
    let r = random_case(
        VolumeRules::WINDOWS,
        0,
        &[],
        &[Change::Move(2, 0, 0, 2), Change::DeleteFolder(2)],
        None,
    );
    assert!(r.is_ok(), "{r:?}");
}

/// Regression, found by the local run of 20,000 cases (2026-11-24): the
/// folder `c` is deleted on disk while Hearth moves `a` into `c`, makes a
/// new `a`, and moves `c` into it. The old `a` waited for `c` to be made,
/// `c` for the new `a`, the new `a` for the old one to free its name: a
/// circle, now broken by the old `a` stepping to a parking name.
#[test]
fn regression_a_circle_of_folder_waits() {
    let r = random_case(
        VolumeRules::WINDOWS,
        0,
        &[Change::DeleteFolder(2)],
        &[
            Change::MoveFolder(0, 2),
            Change::Write(0, 0, 0),
            Change::MoveFolder(2, 0),
        ],
        None,
    );
    assert!(r.is_ok(), "{r:?}");
    let r = random_case(
        VolumeRules::LINUX,
        0,
        &[Change::DeleteFolder(2)],
        &[
            Change::MoveFolder(0, 2),
            Change::Write(0, 0, 0),
            Change::MoveFolder(2, 0),
        ],
        None,
    );
    assert!(r.is_ok(), "{r:?}");
}

/// Regression, found by the local run of 20,000 cases (2026-11-24): the
/// disk deletes `a` and moves `c` to `b/moved`, while Hearth edits inside
/// `a` and moves `a` to `b/moved`. Hearth's `a` comes back to the disk
/// at its place (it holds an edit), so it keeps the name and the local
/// `c` steps aside, instead of each waiting for the other.
#[test]
fn regression_a_deleted_folder_hearth_moved_keeps_its_name() {
    for rules in [VolumeRules::LINUX, VolumeRules::WINDOWS] {
        let r = random_case(
            rules,
            0,
            &[Change::DeleteFolder(0), Change::MoveFolder(2, 1)],
            &[Change::Write(0, 0, 0), Change::MoveFolder(0, 1)],
            None,
        );
        assert!(r.is_ok(), "{rules:?}: {r:?}");
    }
}

/// Regression, found by the local run of 20,000 cases (2026-11-24): the
/// disk deletes `a` while Hearth moves `a/f.txt` to `c/F.txt` (one name
/// with `c/f.txt` on this volume) and edits it.
#[test]
fn regression_a_moved_out_edit_onto_a_case_twin() {
    let r = random_case(
        VolumeRules::WINDOWS,
        0,
        &[Change::DeleteFolder(0)],
        &[Change::Move(0, 0, 2, 2), Change::Write(2, 2, 0)],
        None,
    );
    assert!(r.is_ok(), "{r:?}");
}

/// Regression, found by the local run of 20,000 cases after the rulings
/// (2026-11-24): on a case-insensitive volume the disk moves `c/f.txt` to
/// `a/g.txt` and deletes `c`, while Hearth moves it to `a/F.txt` (one name
/// with `a/f.txt` here, so the move back is listed). The deleted folder
/// `c` waited for ever on that file's base.
#[test]
fn regression_a_folder_kept_by_the_base_of_a_listed_move() {
    let r = random_case(
        VolumeRules::WINDOWS,
        0,
        &[Change::Move(2, 2, 0, 1), Change::DeleteFolder(2)],
        &[Change::Move(2, 0, 0, 2)],
        None,
    );
    assert!(r.is_ok(), "{r:?}");
}
