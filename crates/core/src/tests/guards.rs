//! The guards against catastrophe (docs/sync.md 3.11), the holding area's
//! days (3.7), resync after cursor expiry (sync-api.md 6.8), selective
//! sync (3.13) and refusals that must block nothing else (I10).

use super::*;
use crate::engine::{Event, PauseReason};
use crate::testkit::Fault;

#[test]
fn a_missing_root_is_never_everything_deleted() {
    let (mut w, mut e) = synced(
        70,
        VolumeRules::LINUX,
        &[(&["a.txt"], b"a"), (&["b.txt"], b"b")],
    );
    w.fs.set_root_present(false);
    e.notify(Event::WatcherOverflow);
    let before = w.remote.log.len();
    let r = run_to_idle(&mut e, &mut w, BOUND).expect("no store failure");
    assert_eq!(r, Settled::Paused(PauseReason::RootUnavailable));
    let obs = e.observe().expect("ok");
    assert!(guarantees::i6_no_deletion_from_missing_root(&obs, false).is_empty());
    assert!(
        !w.remote
            .busy_requests_since(before)
            .iter()
            .any(|r| r.starts_with("delete")),
        "nothing deleted on Hearth"
    );
    assert_eq!(w.remote.files().len(), 2);
    // Plugged in again: carries on, nothing lost.
    w.fs.set_root_present(true);
    e.resume();
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(w.remote.files().len(), 2);
}

#[test]
fn an_emptied_root_with_its_marker_gone_pauses() {
    let (mut w, mut e) = synced(71, VolumeRules::LINUX, &[(&["a.txt"], b"a")]);
    // A folder replaced by another (a restored backup): the marker differs.
    w.fs.remove(&[".hearthsync"]);
    w.fs.remove(&["a.txt"]);
    e.notify(Event::WatcherOverflow);
    // The marker is checked before any local deletion and at every start;
    // a remote deletion derived from the empty folder needs the planner,
    // which runs only after the root guard of the next start.
    let mut e2 = Engine::new(e.into_store(), World::config(VolumeRules::LINUX)).expect("ok");
    let r = run_to_idle(&mut e2, &mut w, BOUND).expect("ok");
    assert_eq!(r, Settled::Paused(PauseReason::RootUnavailable));
    assert_eq!(w.remote.files().len(), 1);
}

#[test]
fn a_mass_deletion_pauses_and_goes_on_only_when_approved() {
    let names: Vec<String> = (0..80).map(|i| format!("f{i:03}.txt")).collect();
    let files: Vec<(Vec<&str>, Vec<u8>)> = names
        .iter()
        .map(|n| (vec![n.as_str()], n.as_bytes().to_vec()))
        .collect();
    let refs: Vec<(&[&str], &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_slice(), b.as_slice()))
        .collect();
    let (mut w, mut e) = synced(72, VolumeRules::LINUX, &refs);
    for n in &names {
        w.fs.remove(&[n.as_str()]);
    }
    e.notify(Event::WatcherOverflow);
    let r = run_to_idle(&mut e, &mut w, BOUND).expect("ok");
    let Settled::Paused(PauseReason::MassChange(v)) = r else {
        panic!("expected a mass-change pause, got {r:?}");
    };
    assert_eq!(
        w.remote.files().len(),
        80,
        "nothing deleted before approval"
    );
    assert!(v.items.iter().any(|i| i.count == 80));
    e.approve_mass_change(v.token);
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert!(w.remote.files().is_empty());
    assert_nothing_lost(&w, &[b"f000.txt", b"f079.txt"]);
}

#[test]
fn another_instance_pauses_and_loses_nothing_when_confirmed() {
    let (mut w, e) = synced(73, VolumeRules::LINUX, &[(&["a.txt"], b"a")]);
    w.fs.write(&["local-only.txt"], b"mine");
    // A rebuilt Hearth: new instance, empty.
    w.remote = crate::testkit::FakeRemote::new();
    w.remote.instance = "instance-2".into();
    let mut e2 = Engine::new(e.into_store(), World::config(VolumeRules::LINUX)).expect("ok");
    let r = run_to_idle(&mut e2, &mut w, BOUND).expect("ok");
    assert!(
        matches!(r, Settled::Paused(PauseReason::InstanceChanged { .. })),
        "{r:?}"
    );
    e2.confirm_new_instance().expect("ok");
    settle(&mut w, &mut e2);
    assert_converged(&w, &e2);
    // Everything on the device went up to the new instance; nothing local
    // was deleted.
    assert_eq!(w.remote.files().len(), 2);
    assert!(w.fs.exists(&["a.txt"]) && w.fs.exists(&["local-only.txt"]));
}

#[test]
fn a_restored_hearth_new_epoch_uploads_what_it_lacks_and_deletes_nothing() {
    let (mut w, mut e) = synced(74, VolumeRules::LINUX, &[(&["old.txt"], b"old")]);
    // Made after the backup: Hearth's restored state lacks it.
    w.fs.write(&["newer.txt"], b"newer");
    e.notify(Event::WatcherOverflow);
    settle(&mut w, &mut e);
    assert!(w.remote.read_path(&["newer.txt"]).is_some());
    w.remote.client_delete(&["newer.txt"]);
    w.remote.new_epoch("e2");
    e.notify(Event::RemoteHint);
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(
        w.remote.read_path(&["newer.txt"]).as_deref(),
        Some(&b"newer"[..]),
        "uploaded again, never deleted locally"
    );
}

#[test]
fn cursor_retention_expiry_resyncs_by_listing() {
    let (mut w, mut e) = synced(
        75,
        VolumeRules::LINUX,
        &[(&["a.txt"], b"a"), (&["b.txt"], b"b")],
    );
    w.remote.client_delete(&["a.txt"]);
    w.remote.client_write(&["c.txt"], b"c");
    w.remote.expire_markers();
    e.notify(Event::RemoteHint);
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(
        w.fs.user_files().into_keys().collect::<Vec<_>>(),
        vec!["b.txt", "c.txt"]
    );
}

#[test]
fn the_holding_area_is_emptied_after_its_days_only_with_proof() {
    let (mut w, mut e) = synced(76, VolumeRules::LINUX, &[(&["a.txt"], b"a")]);
    w.remote.client_delete(&["a.txt"]);
    e.notify(Event::RemoteHint);
    settle(&mut w, &mut e);
    assert_eq!(e.observe().expect("ok").held.len(), 1);
    // Six days later: still held.
    w.clock.advance(6 * 86_400 * 1_000_000_000);
    settle(&mut w, &mut e);
    assert_eq!(e.observe().expect("ok").held.len(), 1);
    // Eight days: emptied (the delete went to Hearth's trash).
    w.clock.advance(2 * 86_400 * 1_000_000_000);
    settle(&mut w, &mut e);
    assert_eq!(e.observe().expect("ok").held.len(), 0);
    assert!(
        !w.fs
            .files()
            .iter()
            .any(|f| f.path.display().starts_with(".hearthsync/held/") && f.hash == Hash::of(b"a"))
    );
}

#[test]
fn a_locked_file_is_listed_and_blocks_nothing_else() {
    let (mut w, mut e) = synced(
        77,
        VolumeRules::LINUX,
        &[(&["a.txt"], b"a"), (&["b.txt"], b"b")],
    );
    w.fs.write(&["a.txt"], b"a2");
    w.fs.write(&["b.txt"], b"b2");
    e.notify(Event::WatcherOverflow);
    // Scans and hashing first; the first upload meets a lock.
    w.remote
        .faults
        .extend(std::iter::repeat_n(Fault::Refuse(423, "locked".into()), 1));
    settle(&mut w, &mut e);
    let obs = e.observe().expect("ok");
    let locked: Vec<_> = obs
        .problems
        .iter()
        .filter(|p| p.kind == crate::tree::ProblemKind::Locked)
        .collect();
    // Whichever request met the lock, the other file went up.
    let up = w.remote.files();
    let both = up.get("a.txt").map(Vec::as_slice) == Some(b"a2")
        && up.get("b.txt").map(Vec::as_slice) == Some(b"b2");
    assert!(both || locked.len() == 1, "{locked:?} {up:?}");
}

#[test]
fn lost_answers_never_duplicate_a_file() {
    let (mut w, mut e) = setup(78, VolumeRules::LINUX);
    w.fs.write(&["a.txt"], b"a");
    w.fs.write(&["d", "b.txt"], b"b");
    // Every write's first answer is lost after it took effect.
    for _ in 0..40 {
        w.remote.faults.push_back(Fault::LostAfter);
    }
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(w.remote.files().len(), 2);
    assert!(conflict_copies(&w).is_empty());
}

#[test]
fn unselecting_a_folder_takes_it_off_the_disk_through_the_holding_area() {
    let (mut w, mut e) = synced(
        79,
        VolumeRules::LINUX,
        &[(&["big", "x"], b"x"), (&["keep"], b"k")],
    );
    let obs = e.observe().expect("ok");
    let big = obs
        .nodes
        .iter()
        .find(|n| n.remote.as_ref().is_some_and(|r| r.name == "big"))
        .expect("the folder")
        .node;
    e.unselect(big).expect("ok");
    settle(&mut w, &mut e);
    assert!(!w.fs.exists(&["big", "x"]));
    assert!(w.fs.exists(&["keep"]));
    assert!(
        w.remote.read_path(&["big", "x"]).is_some(),
        "nothing deleted on Hearth"
    );
    assert_nothing_lost(&w, &[b"x"]);
    e.reselect(big).expect("ok");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
}

#[test]
fn a_corrupted_download_is_retried_from_zero_then_listed_never_placed() {
    // Once corrupted: the retry from zero gets it whole.
    let (mut w, mut e) = setup(80, VolumeRules::LINUX);
    w.remote.client_write(&["a.txt"], b"aaaa");
    w.remote.corrupt_reads = 1;
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    // Always corrupted: never made visible, listed, and nothing else
    // blocked (I5, I10).
    let (mut w, mut e) = setup(81, VolumeRules::LINUX);
    w.remote.client_write(&["bad.txt"], b"bbbb");
    w.remote.corrupt_reads = 1000;
    settle(&mut w, &mut e);
    assert!(!w.fs.exists(&["bad.txt"]));
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::HashMismatch)
    );
    w.remote.corrupt_reads = 0;
    w.remote.client_write(&["good.txt"], b"g");
    e.notify(Event::RemoteHint);
    settle(&mut w, &mut e);
    assert!(w.fs.exists(&["good.txt"]));
}
