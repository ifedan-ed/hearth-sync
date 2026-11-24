//! The rulings of docs/sync.md 3.17 that change the engine, one test each
//! (and the regressions they came with).

use super::*;
use crate::engine::{Event, PauseReason};
use crate::testkit::Fault;
use crate::tree::ProblemKind;

fn kinds(e: &Engine<MemStore>) -> Vec<ProblemKind> {
    e.observe()
        .expect("ok")
        .problems
        .iter()
        .map(|p| p.kind)
        .collect()
}

fn run(w: &mut World, e: &mut Engine<MemStore>) {
    e.notify(Event::WatcherOverflow);
    e.notify(Event::RemoteHint);
    settle(w, e);
}

/// R18: a local move inside a read-only mount is undone on the device,
/// and nothing stays listed.
#[test]
fn r18_a_read_only_move_is_undone_and_not_listed() {
    let (mut w, mut e) = synced(100, VolumeRules::LINUX, &[(&["a", "f.txt"], b"f")]);
    w.remote.client_mkdir(&["b"]);
    w.remote.perms = 1;
    run(&mut w, &mut e);
    w.fs.rename(&["a", "f.txt"], &["b", "f.txt"]);
    run(&mut w, &mut e);
    assert!(w.fs.exists(&["a", "f.txt"]) && !w.fs.exists(&["b", "f.txt"]));
    assert_converged(&w, &e);
    assert!(kinds(&e).is_empty(), "{:?}", kinds(&e));
}

/// R18: a 403 that comes late (the rights changed and the device has not
/// read the mounts yet) also undoes the move.
#[test]
fn r18_a_late_refusal_undoes_the_move() {
    let (mut w, mut e) = synced(101, VolumeRules::LINUX, &[(&["a", "f.txt"], b"f")]);
    w.remote.client_mkdir(&["b"]);
    run(&mut w, &mut e);
    w.fs.rename(&["a", "f.txt"], &["b", "f.txt"]);
    // The move's request meets the new rights first.
    w.remote.faults.push_back(Fault::RefuseRequest(
        "move".into(),
        403,
        "no_permission".into(),
    ));
    run(&mut w, &mut e);
    assert!(w.fs.exists(&["a", "f.txt"]) && !w.fs.exists(&["b", "f.txt"]));
    assert_converged(&w, &e);
}

/// R19: Hearth not knowing a file's hash is not a difference; when the
/// hash arrives and differs, the file is downloaded again, never uploaded.
#[test]
fn r19_an_unknown_hash_converges_and_a_wrong_one_is_downloaded() {
    let (mut w, mut e) = synced(102, VolumeRules::LINUX, &[(&["f.txt"], b"bytes")]);
    w.remote.client_set_hash(&["f.txt"], false, false);
    run(&mut w, &mut e);
    let obs = e.observe().expect("ok");
    assert!(guarantees::i8_converged(&obs).is_empty());
    assert!(kinds(&e).is_empty());
    let before = w.remote.log.len();
    w.remote.client_set_hash(&["f.txt"], true, true);
    run(&mut w, &mut e);
    let log = w.remote.busy_requests_since(before);
    assert!(
        !log.iter()
            .any(|r| r == "replace_content" || r == "create_file"),
        "{log:?}"
    );
    assert!(log.iter().any(|r| r == "read"), "downloaded again: {log:?}");
}

/// R8: moved out of a folder the other side then deleted: it survives at
/// its new place, both ways round.
#[test]
fn r8_moved_out_of_a_deleted_folder_survives() {
    let (mut w, mut e) = synced(
        103,
        VolumeRules::LINUX,
        &[
            (&["d", "x"], b"x"),
            (&["d", "y"], b"y"),
            (&["e", "keep"], b"k"),
        ],
    );
    w.fs.rename(&["d", "x"], &["e", "x"]);
    w.remote.client_delete(&["d"]);
    run(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(w.remote.read_path(&["e", "x"]).as_deref(), Some(&b"x"[..]));
    assert!(!w.fs.exists(&["d", "y"]), "the rest of the folder goes (F)");

    let (mut w, mut e) = synced(
        104,
        VolumeRules::LINUX,
        &[
            (&["d", "x"], b"x"),
            (&["d", "y"], b"y"),
            (&["e", "keep"], b"k"),
        ],
    );
    w.remote.client_move(&["d", "x"], &["e", "x"]);
    w.fs.remove(&["d"]);
    run(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(w.fs.read_file(&["e", "x"]).as_deref(), Some(&b"x"[..]));
    assert!(w.remote.read_path(&["d", "y"]).is_none());
}

/// R8: a direct delete against a move stays cell D.
#[test]
fn r8_a_direct_delete_against_a_move_is_applied() {
    let (mut w, mut e) = synced(
        105,
        VolumeRules::LINUX,
        &[(&["d", "x"], b"x"), (&["e", "k"], b"k")],
    );
    w.fs.rename(&["d", "x"], &["e", "x"]);
    w.remote.client_delete(&["d", "x"]);
    run(&mut w, &mut e);
    assert_converged(&w, &e);
    assert!(!w.fs.exists(&["e", "x"]));
}

/// R10: Hearth renames a file to a name this device cannot hold: only the
/// name stays behind; the content keeps syncing both ways.
#[test]
fn r10_only_the_name_stays_behind() {
    let (mut w, mut e) = synced(106, VolumeRules::WINDOWS, &[(&["d", "f.txt"], b"one")]);
    w.remote.client_move(&["d"], &["d:2"]);
    w.remote.client_move(&["d:2", "f.txt"], &["d:2", "f?.txt"]);
    run(&mut w, &mut e);
    assert!(w.fs.exists(&["d", "f.txt"]), "kept under its old names");
    assert!(kinds(&e).contains(&ProblemKind::NameNotHoldable));
    // An edit on the device reaches Hearth under Hearth's names.
    w.fs.write(&["d", "f.txt"], b"two");
    run(&mut w, &mut e);
    assert_eq!(
        w.remote.read_path(&["d:2", "f?.txt"]).as_deref(),
        Some(&b"two"[..])
    );
    // An edit on Hearth reaches the device.
    w.remote.client_write(&["d:2", "f?.txt"], b"three");
    run(&mut w, &mut e);
    assert_eq!(
        w.fs.read_file(&["d", "f.txt"]).as_deref(),
        Some(&b"three"[..])
    );
    // A new file inside the folder syncs too.
    w.fs.write(&["d", "new.txt"], b"n");
    run(&mut w, &mut e);
    assert_eq!(
        w.remote.read_path(&["d:2", "new.txt"]).as_deref(),
        Some(&b"n"[..])
    );
}

/// R4: two local names that are one in NFC: the one already NFC on disk
/// syncs, the other is listed `name_clash`, whatever their byte order.
#[test]
fn r4_the_nfc_name_wins() {
    let (mut w, mut e) = setup(107, VolumeRules::LINUX);
    // The decomposed form sorts first as bytes ("e" < "\u{e9}").
    w.fs.write(&["cafe\u{301}"], b"decomposed");
    w.fs.write(&["caf\u{e9}"], b"composed");
    run(&mut w, &mut e);
    assert_eq!(
        w.remote.read_path(&["caf\u{e9}"]).as_deref(),
        Some(&b"composed"[..])
    );
    assert!(kinds(&e).contains(&ProblemKind::NameClash));
}

/// R16: ignored names are not problems; they are counted.
#[test]
fn r16_ignored_names_are_counted_not_listed() {
    let (mut w, mut e) = setup(108, VolumeRules::LINUX);
    w.fs.write(&["~$Budget.xlsx"], b"lock");
    w.fs.write(&["a", "x.tmp"], b"t");
    w.fs.write(&["Budget.xlsx"], b"b");
    run(&mut w, &mut e);
    let obs = e.observe().expect("ok");
    assert!(obs.problems.is_empty(), "{:?}", obs.problems);
    assert_eq!(obs.ignored, 2);
}

/// R14: a `.hearthtmp` name on Hearth is the engine's: listed, never
/// downloaded; and one on disk is never uploaded.
#[test]
fn r14_engine_names_are_never_synced() {
    let (mut w, mut e) = setup(109, VolumeRules::MACOS);
    w.remote.client_write(&["d", "x.HEARTHTMP"], b"r");
    w.fs.write(&["e", "y.hearthtmp"], b"l");
    run(&mut w, &mut e);
    assert!(!w.fs.exists(&["d", "x.HEARTHTMP"]));
    assert!(w.remote.read_path(&["e", "y.hearthtmp"]).is_none());
    assert!(kinds(&e).contains(&ProblemKind::NameNotHoldable));
}

/// R14, R17: a file in the engine's temp folder that no operation names is
/// moved to the holding area at start, never unlinked, and listed once.
#[test]
fn r17_an_orphan_temp_file_is_held_not_removed() {
    let (mut w, e) = synced(110, VolumeRules::LINUX, &[(&["f.txt"], b"f")]);
    w.fs.write(&[".hearthsync", "tmp", "op999"], b"a replaced file");
    let mut e = Engine::new(e.into_store(), World::config(VolumeRules::LINUX)).expect("ok");
    settle(&mut w, &mut e);
    assert!(!w.fs.exists(&[".hearthsync", "tmp", "op999"]));
    let obs = e.observe().expect("ok");
    assert_eq!(obs.held.len(), 1);
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == ProblemKind::Recovered)
    );
    assert_nothing_lost(&w, &[b"a replaced file"]);
}

/// R5: "go on" for a replaced root syncs the folder found there as after a
/// lost database, deleting nothing.
#[test]
fn r5_a_confirmed_replaced_root_deletes_nothing() {
    let (mut w, mut e) = synced(
        111,
        VolumeRules::LINUX,
        &[(&["a.txt"], b"a"), (&["b.txt"], b"b")],
    );
    // Another folder at the root's place: no marker, other files.
    w.fs = crate::testkit::FakeFs::new(VolumeRules::LINUX);
    w.fs.write(&["a.txt"], b"a");
    w.fs.write(&["c.txt"], b"c");
    e.notify(Event::WatcherOverflow);
    let mut e = Engine::new(e.into_store(), World::config(VolumeRules::LINUX)).expect("ok");
    let r = run_to_idle(&mut e, &mut w, BOUND).expect("ok");
    assert_eq!(r, Settled::Paused(PauseReason::RootUnavailable));
    e.confirm_replaced_root(&mut w.env()).expect("ok");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(w.remote.files().len(), 3, "{:?}", w.remote.files().keys());
    assert!(conflict_copies(&w).is_empty());
}

/// R12: short of space, a proven holding-area entry is emptied before its
/// days; an unproven one never.
#[test]
fn r12_low_space_empties_only_proven_entries() {
    let (mut w, mut e) = synced(112, VolumeRules::LINUX, &[(&["a.txt"], b"a")]);
    w.remote.client_delete(&["a.txt"]);
    run(&mut w, &mut e);
    assert_eq!(e.observe().expect("ok").held.len(), 1);
    w.fs.free = 1;
    w.clock.advance(120 * 1_000_000_000);
    settle(&mut w, &mut e);
    assert_eq!(e.observe().expect("ok").held.len(), 0);
}
