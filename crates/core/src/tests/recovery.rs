//! Surviving failure (I11, I12): a crash between any two steps, a store
//! write failing at any point, a deleted database. After each, a new
//! engine on what survived must converge without losing or duplicating
//! anything.

use super::*;
use crate::config::Config;
use crate::engine::StepOutcome;
use crate::store::Store;

/// Settings that make even small test files go through upload sessions
/// and ranged downloads, so their resumable states are crossed too.
fn small_pieces() -> Config {
    let mut c = World::config(VolumeRules::LINUX);
    c.small_file_max = 8;
    c.chunk_size = 4;
    c.range_size = 4;
    c
}

/// A change on both sides worth a few dozen steps.
fn scenario(w: &mut World) {
    w.remote.limits.chunk_min = 1;
    w.remote
        .client_write(&["r", "one.txt"], b"remote one, long enough");
    w.remote.client_write(&["r", "two.txt"], b"r2");
    w.fs.write(&["l", "big.txt"], b"a local file in several chunks");
    w.fs.write(&["l", "s.txt"], b"ls");
}

fn engine_on(store: MemStore) -> Engine<MemStore> {
    Engine::new(store, small_pieces()).expect("valid settings")
}

/// How many steps the scenario takes without faults.
fn reference_steps(seed: u64) -> usize {
    let mut w = World::new(seed, VolumeRules::LINUX);
    scenario(&mut w);
    let mut e = engine_on(MemStore::new());
    let mut n = 0;
    loop {
        n += 1;
        match e.step(&mut w.env()).expect("ok") {
            StepOutcome::Worked => {}
            _ => return n,
        }
        assert!(n < BOUND, "a loop");
    }
}

#[test]
fn a_crash_between_any_two_steps_recovers_and_converges() {
    let total = reference_steps(90);
    assert!(
        total > 20,
        "the scenario crosses many durable points: {total}"
    );
    for k in 0..total {
        let mut w = World::new(90, VolumeRules::LINUX);
        scenario(&mut w);
        let mut e = engine_on(MemStore::new());
        for _ in 0..k {
            let _ = e.step(&mut w.env()).expect("ok");
        }
        // The crash: everything in memory is gone; the store stays.
        let store = e.into_store();
        let c = check_consistency(&store).expect("ok");
        assert!(c.is_empty(), "I11 after a crash at step {k}: {c:#?}");
        let mut e = engine_on(store);
        settle(&mut w, &mut e);
        assert_converged(&w, &e);
        assert_eq!(
            w.remote.files().len(),
            4,
            "crash at step {k}: {:?}",
            w.remote.files().keys()
        );
        assert!(conflict_copies(&w).is_empty(), "crash at step {k}");
        assert_nothing_lost(
            &w,
            &[
                b"remote one, long enough",
                b"r2",
                b"a local file in several chunks",
                b"ls",
            ],
        );
    }
}

#[test]
fn a_store_write_failing_at_any_point_recovers_and_converges() {
    for fail_at in 0..60 {
        let mut w = World::new(91, VolumeRules::LINUX);
        scenario(&mut w);
        let mut e = engine_on(MemStore::new());
        e.store_mut().fail_writes_after(Some(fail_at));
        let mut failed = false;
        for _ in 0..BOUND {
            match e.step(&mut w.env()) {
                Err(_) => {
                    failed = true;
                    break;
                }
                Ok(StepOutcome::Worked) => {}
                Ok(_) => break,
            }
        }
        let mut store = e.into_store();
        store.fail_writes_after(None);
        let c = check_consistency(&store).expect("ok");
        assert!(
            c.is_empty(),
            "I11 after a store failure at write {fail_at}: {c:#?}"
        );
        let mut e = engine_on(store);
        settle(&mut w, &mut e);
        assert_converged(&w, &e);
        assert_eq!(
            w.remote.files().len(),
            4,
            "failure at write {fail_at} (failed: {failed})"
        );
        assert!(conflict_copies(&w).is_empty());
    }
}

#[test]
fn a_deleted_database_loses_nothing_and_moves_no_equal_bytes() {
    let (mut w, e) = synced(
        92,
        VolumeRules::LINUX,
        &[(&["same.txt"], b"same"), (&["d", "edited.txt"], b"v1")],
    );
    drop(e);
    // While the database was gone: an edit on each side of different files.
    w.fs.write(&["d", "edited.txt"], b"v2 local");
    w.remote.client_write(&["only-remote.txt"], b"r");
    w.fs.write(&["only-local.txt"], b"l");
    let before = w.remote.log.len();
    let mut e = Engine::new(MemStore::new(), World::config(VolumeRules::LINUX)).expect("ok");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    // I12: equal files are adopted without transfer; the differing one
    // becomes a conflict copy; nothing is lost.
    let log = w.remote.busy_requests_since(before);
    assert!(
        !log.iter().any(|r| r == "replace_content"),
        "no blind overwrite: {log:?}"
    );
    assert_nothing_lost(&w, &[b"same", b"v1", b"v2 local", b"r", b"l"]);
    assert_eq!(conflict_copies(&w).len(), 1);
}

#[test]
fn the_same_seed_gives_the_same_run() {
    let run = |seed: u64| {
        let mut w = World::new(seed, VolumeRules::LINUX);
        scenario(&mut w);
        let mut e = engine_on(MemStore::new());
        settle(&mut w, &mut e);
        (
            w.remote.log.clone(),
            crate::store::conformance::dump(e.store()),
            w.fs.user_files(),
        )
    };
    assert_eq!(run(93), run(93), "I13");
}
