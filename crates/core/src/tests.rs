//! End-to-end tests of the engine against the test kit: every cell of the
//! decision table (docs/sync.md 3.5) by name, the creations, the folder
//! rules, rights, names, the guards, the holding area, recovery after a
//! crash at every step, and random runs as property tests.
//!
//! Each test builds a world from a seed, so a failure replays exactly; the
//! random ones print their seed and case.

mod cells;
mod guards;
mod props;
mod recovery;
mod rulings;

use std::collections::{BTreeMap, BTreeSet};

use crate::engine::Engine;
use crate::guarantees;
use crate::ids::Hash;
use crate::memstore::MemStore;
use crate::names::VolumeRules;
use crate::store::check_consistency;
use crate::testkit::{Settled, World, run_to_idle};

/// The step bound of a run (I7: a sync loop fails the test).
const BOUND: usize = 20_000;

/// A world and an engine over an empty in-memory store.
fn setup(seed: u64, rules: VolumeRules) -> (World, Engine<MemStore>) {
    let world = World::new(seed, rules);
    let engine = Engine::new(MemStore::new(), World::config(rules)).expect("valid settings");
    (world, engine)
}

/// Runs to idle and asserts it settled (not paused, not looping).
fn settle(world: &mut World, engine: &mut Engine<MemStore>) {
    match run_to_idle(engine, world, BOUND).expect("no store failure") {
        Settled::Idle { .. } => {}
        other => panic!("did not settle: {other:?}\n{:#?}", engine.observe()),
    }
}

/// A world where Hearth and the disk hold the same files, synced.
fn synced(seed: u64, rules: VolumeRules, files: &[(&[&str], &[u8])]) -> (World, Engine<MemStore>) {
    let (mut w, mut e) = setup(seed, rules);
    for (p, b) in files {
        w.remote.client_write(p, b);
    }
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    (w, e)
}

/// The disk's files (outside the engine's places) and Hearth's are the
/// same, and the engine's guarantees hold for what it observes.
fn assert_converged(w: &World, e: &Engine<MemStore>) {
    let local = w.fs.user_files();
    let remote = w.remote.files();
    assert_eq!(local, remote, "disk and Hearth differ");
    let obs = e.observe().expect("observable");
    let v = guarantees::i8_converged(&obs);
    assert!(v.is_empty(), "I8: {v:#?}\n{obs:#?}");
    let c = check_consistency(e.store()).expect("readable");
    assert!(c.is_empty(), "I11: {c:#?}");
    let v = guarantees::i5_hashes_verified(&obs, &w.fs.files());
    assert!(v.is_empty(), "I5: {v:#?}");
}

/// I1: every content in `saved` still exists on the device (the holding
/// area included) or on Hearth (files, versions, trash).
fn assert_nothing_lost(w: &World, saved: &[&[u8]]) {
    let world = guarantees::WorldHashes {
        device: w.fs.files().into_iter().map(|f| f.hash).collect(),
        hearth: w.remote.all_hashes(),
    };
    let saved: BTreeSet<Hash> = saved.iter().map(|b| Hash::of(b)).collect();
    let v = guarantees::i1_saved_bytes_present(&saved, &world);
    assert!(v.is_empty(), "I1: {v:#?}");
}

/// The user files on disk whose names mark them as conflict copies.
fn conflict_copies(w: &World) -> BTreeMap<String, Vec<u8>> {
    w.fs.user_files()
        .into_iter()
        .filter(|(k, _)| k.contains(" (conflict "))
        .collect()
}
