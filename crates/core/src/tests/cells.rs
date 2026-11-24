//! Every cell of the decision table (docs/sync.md 3.5), end to end, by
//! name: a synced file or folder, one change on each side, a run to idle,
//! then what both sides hold.

use super::*;

const BASE: &[u8] = b"base content";
const LOCAL: &[u8] = b"the local edit";
const REMOTE: &[u8] = b"the remote edit";
/// A local edit made together with a local move: same size as BASE and
/// the mtime put back, so the scanner can still tell the move by the
/// file id (3.4 rule 3) and the edit by the change time and hash (3.3).
const MOVED_EDIT: &[u8] = b"BASE CONTENT";

#[derive(Clone, Copy, Debug)]
enum Ch {
    None,
    Edit,
    Move,
    Both,
    Delete,
}

/// A synced `a/f.txt` and an empty synced folder `b`.
fn base_world(seed: u64) -> (World, Engine<MemStore>) {
    let (mut w, mut e) = synced(seed, VolumeRules::LINUX, &[(&["a", "f.txt"], BASE)]);
    w.remote.client_mkdir(&["b"]);
    settle(&mut w, &mut e);
    assert!(w.fs.exists(&["b"]));
    (w, e)
}

/// The local side moves to `b/f.txt`; Hearth's side to `b/g.txt`, so the
/// two moves have different destinations (C).
fn local(w: &mut World, c: Ch) {
    match c {
        Ch::None => {}
        Ch::Edit => w.fs.write(&["a", "f.txt"], LOCAL),
        Ch::Move => w.fs.rename(&["a", "f.txt"], &["b", "f.txt"]),
        Ch::Both => {
            w.fs.rename(&["a", "f.txt"], &["b", "f.txt"]);
            w.fs.write_keeping_mtime(&["b", "f.txt"], MOVED_EDIT);
        }
        Ch::Delete => w.fs.remove(&["a", "f.txt"]),
    }
}

/// The contents a change saves (I1 is checked for exactly these).
fn saved_by(l: Ch, r: Ch) -> Vec<&'static [u8]> {
    let mut v: Vec<&[u8]> = vec![BASE];
    match l {
        Ch::Edit => v.push(LOCAL),
        Ch::Both => v.push(MOVED_EDIT),
        _ => {}
    }
    if matches!(r, Ch::Edit | Ch::Both) {
        v.push(REMOTE);
    }
    v
}

fn remote(w: &mut World, c: Ch) {
    match c {
        Ch::None => {}
        Ch::Edit => w.remote.client_write(&["a", "f.txt"], REMOTE),
        Ch::Move => w.remote.client_move(&["a", "f.txt"], &["b", "g.txt"]),
        Ch::Both => {
            w.remote.client_move(&["a", "f.txt"], &["b", "g.txt"]);
            w.remote.client_write(&["b", "g.txt"], REMOTE);
        }
        Ch::Delete => w.remote.client_delete(&["a", "f.txt"]),
    }
}

/// Runs one cell and returns the converged world.
fn cell(l: Ch, r: Ch) -> World {
    let (mut w, mut e) = base_world(11);
    local(&mut w, l);
    remote(&mut w, r);
    // A watcher would have said which folders changed; a full scan stands
    // in for it here.
    e.notify(crate::engine::Event::WatcherOverflow);
    e.notify(crate::engine::Event::RemoteHint);
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_nothing_lost(&w, &saved_by(l, r));
    let copies = e.conflict_copies().expect("readable");
    let v = guarantees::i9_conflict_copies(&e.observe().expect("observable"), &copies);
    assert!(v.is_empty(), "I9 in {l:?}/{r:?}: {v:#?}");
    w
}

/// The user files of a world without its conflict copies.
fn plain(w: &World) -> BTreeMap<String, Vec<u8>> {
    w.fs.user_files()
        .into_iter()
        .filter(|(k, _)| !k.contains(" (conflict "))
        .collect()
}

fn files(pairs: &[(&str, &[u8])]) -> BTreeMap<String, Vec<u8>> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), v.to_vec()))
        .collect()
}

/// One conflict copy, holding `content`, in folder `dir`.
fn one_copy(w: &World, dir: &str, content: &[u8]) {
    let c = conflict_copies(w);
    assert_eq!(c.len(), 1, "exactly one conflict copy: {c:#?}");
    let (name, body) = c.into_iter().next().expect("one");
    assert!(
        name.starts_with(&format!("{dir}/f (conflict Laptop, ")),
        "{name}"
    );
    assert!(name.ends_with(").txt"), "{name}");
    assert_eq!(body, content);
}

fn no_copies(w: &World) {
    assert!(conflict_copies(w).is_empty(), "{:#?}", conflict_copies(w));
}

#[test]
fn file_none_none_nothing() {
    let w = cell(Ch::None, Ch::None);
    assert_eq!(plain(&w), files(&[("a/f.txt", BASE)]));
    no_copies(&w);
}

#[test]
fn file_none_edit_download() {
    let w = cell(Ch::None, Ch::Edit);
    assert_eq!(plain(&w), files(&[("a/f.txt", REMOTE)]));
    no_copies(&w);
}

#[test]
fn file_none_move_moves_locally() {
    let w = cell(Ch::None, Ch::Move);
    assert_eq!(plain(&w), files(&[("b/g.txt", BASE)]));
    no_copies(&w);
}

#[test]
fn file_none_both_moves_then_downloads() {
    let w = cell(Ch::None, Ch::Both);
    assert_eq!(plain(&w), files(&[("b/g.txt", REMOTE)]));
    no_copies(&w);
}

#[test]
fn file_none_delete_removes_locally_through_the_holding_area() {
    let w = cell(Ch::None, Ch::Delete);
    assert_eq!(plain(&w), files(&[]));
    let held: Vec<_> =
        w.fs.files()
            .into_iter()
            .filter(|f| f.path.display().starts_with(".hearthsync/held/"))
            .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].hash, Hash::of(BASE));
}

#[test]
fn file_edit_none_upload() {
    let w = cell(Ch::Edit, Ch::None);
    assert_eq!(plain(&w), files(&[("a/f.txt", LOCAL)]));
    no_copies(&w);
}

#[test]
fn file_edit_edit_a_conflict_copy_of_the_local_version() {
    let w = cell(Ch::Edit, Ch::Edit);
    assert_eq!(plain(&w), files(&[("a/f.txt", REMOTE)]));
    one_copy(&w, "a", LOCAL);
}

#[test]
fn file_edit_edit_equal_content_adopts() {
    let (mut w, mut e) = base_world(12);
    w.fs.write(&["a", "f.txt"], LOCAL);
    w.remote.client_write(&["a", "f.txt"], LOCAL);
    e.notify(crate::engine::Event::WatcherOverflow);
    e.notify(crate::engine::Event::RemoteHint);
    let before = w.remote.log.len();
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    no_copies(&w);
    let moved: Vec<_> = w.remote.log[before..]
        .iter()
        .filter(|r| r.as_str() == "read" || r.as_str() == "replace_content")
        .collect();
    assert!(moved.is_empty(), "equal hashes move no bytes: {moved:?}");
}

#[test]
fn file_edit_move_uploads_to_the_id_and_moves_locally() {
    let w = cell(Ch::Edit, Ch::Move);
    assert_eq!(plain(&w), files(&[("b/g.txt", LOCAL)]));
    no_copies(&w);
}

#[test]
fn file_edit_both_a_at_hearths_new_place() {
    let w = cell(Ch::Edit, Ch::Both);
    assert_eq!(plain(&w), files(&[("b/g.txt", REMOTE)]));
    let c = conflict_copies(&w);
    assert_eq!(c.len(), 1, "{c:#?}");
    let (name, body) = c.into_iter().next().expect("one");
    assert!(name.starts_with("b/g (conflict Laptop, "), "{name}");
    assert_eq!(body, LOCAL);
}

#[test]
fn file_edit_delete_b_the_edit_is_uploaded_again() {
    let w = cell(Ch::Edit, Ch::Delete);
    assert_eq!(plain(&w), files(&[("a/f.txt", LOCAL)]));
    no_copies(&w);
}

#[test]
fn file_move_none_moves_on_hearth() {
    let w = cell(Ch::Move, Ch::None);
    assert_eq!(plain(&w), files(&[("b/f.txt", BASE)]));
    no_copies(&w);
}

#[test]
fn file_move_edit_moves_on_hearth_and_downloads() {
    let w = cell(Ch::Move, Ch::Edit);
    assert_eq!(plain(&w), files(&[("b/f.txt", REMOTE)]));
    no_copies(&w);
}

#[test]
fn file_move_move_c_hearths_place_wins() {
    let w = cell(Ch::Move, Ch::Move);
    assert_eq!(plain(&w), files(&[("b/g.txt", BASE)]));
    no_copies(&w);
}

#[test]
fn file_move_move_same_destination_nothing() {
    let (mut w, mut e) = base_world(13);
    w.fs.rename(&["a", "f.txt"], &["b", "g.txt"]);
    w.remote.client_move(&["a", "f.txt"], &["b", "g.txt"]);
    e.notify(crate::engine::Event::WatcherOverflow);
    e.notify(crate::engine::Event::RemoteHint);
    let before = w.remote.log.len();
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(plain(&w), files(&[("b/g.txt", BASE)]));
    assert!(!w.remote.log[before..].iter().any(|r| r == "move"));
}

#[test]
fn file_move_both_c_then_download() {
    let w = cell(Ch::Move, Ch::Both);
    assert_eq!(plain(&w), files(&[("b/g.txt", REMOTE)]));
    no_copies(&w);
}

#[test]
fn file_move_delete_d_the_delete_is_applied_locally() {
    let w = cell(Ch::Move, Ch::Delete);
    assert_eq!(plain(&w), files(&[]));
}

#[test]
fn file_both_none_moves_and_uploads() {
    let w = cell(Ch::Both, Ch::None);
    assert_eq!(plain(&w), files(&[("b/f.txt", MOVED_EDIT)]));
    no_copies(&w);
}

#[test]
fn file_both_edit_a_at_the_local_new_place() {
    let w = cell(Ch::Both, Ch::Edit);
    assert_eq!(plain(&w), files(&[("b/f.txt", REMOTE)]));
    one_copy(&w, "b", MOVED_EDIT);
}

#[test]
fn file_both_move_c_then_upload() {
    let w = cell(Ch::Both, Ch::Move);
    assert_eq!(plain(&w), files(&[("b/g.txt", MOVED_EDIT)]));
    no_copies(&w);
}

#[test]
fn file_both_both_c_for_the_place_a_for_the_content() {
    let w = cell(Ch::Both, Ch::Both);
    assert_eq!(plain(&w), files(&[("b/g.txt", REMOTE)]));
    let c = conflict_copies(&w);
    assert_eq!(c.len(), 1, "{c:#?}");
    assert_eq!(c.values().next().expect("one").as_slice(), MOVED_EDIT);
}

#[test]
fn file_both_delete_b_uploaded_as_new_at_the_local_place() {
    let w = cell(Ch::Both, Ch::Delete);
    assert_eq!(plain(&w), files(&[("b/f.txt", MOVED_EDIT)]));
    no_copies(&w);
}

#[test]
fn file_delete_none_deletes_on_hearth() {
    let w = cell(Ch::Delete, Ch::None);
    assert_eq!(plain(&w), files(&[]));
}

#[test]
fn file_delete_edit_e_the_edit_comes_back() {
    let w = cell(Ch::Delete, Ch::Edit);
    assert_eq!(plain(&w), files(&[("a/f.txt", REMOTE)]));
}

#[test]
fn file_delete_move_deletes_on_hearth() {
    let w = cell(Ch::Delete, Ch::Move);
    assert_eq!(plain(&w), files(&[]));
}

#[test]
fn file_delete_both_e_downloaded_at_hearths_place() {
    let w = cell(Ch::Delete, Ch::Both);
    assert_eq!(plain(&w), files(&[("b/g.txt", REMOTE)]));
}

#[test]
fn file_delete_delete_drops_the_node() {
    let w = cell(Ch::Delete, Ch::Delete);
    assert_eq!(plain(&w), files(&[]));
}

// ----- folders -----

/// A synced folder `d` with `d/x` and `d/sub/y`, and a synced folder `e`.
fn folder_world(seed: u64) -> (World, Engine<MemStore>) {
    let (mut w, mut e) = synced(
        seed,
        VolumeRules::LINUX,
        &[(&["d", "x"], b"x"), (&["d", "sub", "y"], b"y")],
    );
    w.remote.client_mkdir(&["e"]);
    settle(&mut w, &mut e);
    (w, e)
}

fn run(w: &mut World, e: &mut Engine<MemStore>) {
    e.notify(crate::engine::Event::WatcherOverflow);
    e.notify(crate::engine::Event::RemoteHint);
    settle(w, e);
    assert_converged(w, e);
}

#[test]
fn folder_none_move_moves_locally() {
    let (mut w, mut e) = folder_world(20);
    w.remote.client_move(&["d"], &["e", "d3"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[("e/d3/sub/y", b"y"), ("e/d3/x", b"x")]));
}

#[test]
fn folder_move_none_moves_on_hearth_in_one_request() {
    let (mut w, mut e) = folder_world(21);
    w.fs.rename(&["d"], &["e", "d2"]);
    let before = w.remote.log.len();
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[("e/d2/sub/y", b"y"), ("e/d2/x", b"x")]));
    let writes: Vec<_> = w.remote.busy_requests_since(before);
    assert_eq!(
        writes.iter().filter(|r| r.as_str() == "move").count(),
        1,
        "a folder rename is one request: {writes:?}"
    );
}

#[test]
fn folder_move_move_c_hearths_place_wins() {
    let (mut w, mut e) = folder_world(22);
    w.fs.rename(&["d"], &["e", "d2"]);
    w.remote.client_move(&["d"], &["e", "d3"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[("e/d3/sub/y", b"y"), ("e/d3/x", b"x")]));
}

#[test]
fn folder_none_delete_f_removed_locally_child_by_child() {
    let (mut w, mut e) = folder_world(23);
    w.remote.client_delete(&["d"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[]));
    assert!(!w.fs.exists(&["d"]));
}

#[test]
fn folder_delete_none_f_deleted_on_hearth_in_one_request() {
    let (mut w, mut e) = folder_world(24);
    w.fs.remove(&["d"]);
    let before = w.remote.log.len();
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[]));
    let writes = w.remote.busy_requests_since(before);
    assert_eq!(
        writes.iter().filter(|r| r.starts_with("delete")).count(),
        1,
        "one request for the whole folder: {writes:?}"
    );
}

#[test]
fn folder_move_delete_g_applied_at_the_new_place() {
    let (mut w, mut e) = folder_world(25);
    w.fs.rename(&["d"], &["e", "d2"]);
    w.remote.client_delete(&["d"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[]));
}

#[test]
fn folder_delete_move_g_deleted_on_hearth_at_its_new_place() {
    let (mut w, mut e) = folder_world(26);
    w.fs.remove(&["d"]);
    w.remote.client_move(&["d"], &["e", "d3"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[]));
}

#[test]
fn folder_delete_delete_drops() {
    let (mut w, mut e) = folder_world(27);
    w.fs.remove(&["d"]);
    w.remote.client_delete(&["d"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[]));
    let obs = e.observe().expect("ok");
    assert!(
        obs.nodes.len() <= 3,
        "the dropped nodes are forgotten: {obs:#?}"
    );
}

#[test]
fn folder_deleted_locally_survives_where_hearth_edited_inside() {
    let (mut w, mut e) = folder_world(28);
    w.fs.remove(&["d"]);
    w.remote.client_write(&["d", "sub", "y"], b"y2");
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    // F: the edited file and the folders above it survive; x is deleted.
    assert_eq!(plain(&w), files(&[("d/sub/y", b"y2")]));
}

#[test]
fn folder_deleted_on_hearth_survives_where_something_new_is_inside() {
    let (mut w, mut e) = folder_world(29);
    w.remote.client_delete(&["d"]);
    w.fs.write(&["d", "sub", "new"], b"new");
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[("d/sub/new", b"new")]));
    assert_nothing_lost(&w, &[b"new"]);
}

#[test]
fn folder_moved_into_a_folder_the_other_side_deleted_survives() {
    let (mut w, mut e) = folder_world(30);
    w.fs.rename(&["d", "x"], &["e", "x"]);
    w.remote.client_delete(&["e"]);
    run(&mut w, &mut e);
    assert_nothing_lost(&w, &[b"x", b"y"]);
    assert_eq!(plain(&w), files(&[("d/sub/y", b"y"), ("e/x", b"x")]));
}

#[test]
fn folder_cycle_hearths_moves_win() {
    let (mut w, mut e) = synced(
        31,
        VolumeRules::LINUX,
        &[(&["A", "a"], b"a"), (&["B", "b"], b"b")],
    );
    w.fs.rename(&["A"], &["B", "A"]);
    w.remote.client_move(&["B"], &["A", "B"]);
    run(&mut w, &mut e);
    assert_eq!(plain(&w), files(&[("A/B/b", b"b"), ("A/a", b"a")]));
    assert_nothing_lost(&w, &[b"a", b"b"]);
}

// ----- creations (no base) -----

#[test]
fn create_both_equal_files_adopted_without_transfer() {
    let (mut w, mut e) = setup(40, VolumeRules::LINUX);
    w.fs.write(&["n.txt"], b"same");
    w.remote.client_write(&["n.txt"], b"same");
    let before = w.remote.log.len();
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    let log = w.remote.busy_requests_since(before);
    assert!(
        !log.iter().any(|r| r == "read" || r == "create_file"),
        "{log:?}"
    );
    no_copies(&w);
}

#[test]
fn create_both_different_files_hearth_keeps_the_name() {
    let (mut w, mut e) = setup(41, VolumeRules::LINUX);
    w.fs.write(&["n.txt"], b"local");
    w.remote.client_write(&["n.txt"], b"remote");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(w.fs.read_file(&["n.txt"]).as_deref(), Some(&b"remote"[..]));
    let c = conflict_copies(&w);
    assert_eq!(c.len(), 1);
    assert_eq!(c.values().next().expect("one").as_slice(), b"local");
}

#[test]
fn create_both_folders_merge() {
    let (mut w, mut e) = setup(42, VolumeRules::LINUX);
    w.fs.write(&["f", "l.txt"], b"l");
    w.remote.client_write(&["f", "r.txt"], b"r");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(plain(&w), files(&[("f/l.txt", b"l"), ("f/r.txt", b"r")]));
}

#[test]
fn create_local_file_against_remote_folder_the_folder_keeps_the_name() {
    let (mut w, mut e) = setup(43, VolumeRules::LINUX);
    w.fs.write(&["n"], b"file");
    w.remote.client_write(&["n", "inside"], b"i");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    let c = conflict_copies(&w);
    assert_eq!(c.len(), 1, "{c:#?}");
    assert!(w.fs.exists(&["n", "inside"]));
}

#[test]
fn create_local_folder_against_remote_file_the_folder_keeps_the_name() {
    let (mut w, mut e) = setup(44, VolumeRules::LINUX);
    w.fs.write(&["n", "inside"], b"i");
    w.remote.client_write(&["n"], b"file");
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    let c = conflict_copies(&w);
    assert_eq!(c.len(), 1, "{c:#?}");
    assert_eq!(c.values().next().expect("one").as_slice(), b"file");
    assert!(w.fs.exists(&["n", "inside"]));
}

// ----- rights (3.5, the permission rows) -----

fn read_only_world(seed: u64) -> (World, Engine<MemStore>) {
    let (mut w, e) = synced(seed, VolumeRules::LINUX, &[(&["r.txt"], b"orig")]);
    w.remote.perms = 1;
    (w, e)
}

fn mounts_changed(e: &mut Engine<MemStore>) {
    // The rights change shows as a new mounts tag; the tests trigger the
    // read of the mounts the same way.
    e.notify(crate::engine::Event::WatcherOverflow);
    e.notify(crate::engine::Event::RemoteHint);
}

#[test]
fn read_only_local_edit_restored_and_kept_as_a_conflict_copy() {
    let (mut w, mut e) = read_only_world(50);
    w.fs.write(&["r.txt"], b"edited");
    mounts_changed(&mut e);
    settle(&mut w, &mut e);
    assert_eq!(w.fs.read_file(&["r.txt"]).as_deref(), Some(&b"orig"[..]));
    let c = conflict_copies(&w);
    assert_eq!(c.len(), 1);
    assert_eq!(c.values().next().expect("one").as_slice(), b"edited");
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::ReadOnly),
        "listed: {:#?}",
        obs.problems
    );
    assert_nothing_lost(&w, &[b"orig", b"edited"]);
}

#[test]
fn read_only_local_delete_downloaded_again() {
    let (mut w, mut e) = read_only_world(51);
    w.fs.remove(&["r.txt"]);
    mounts_changed(&mut e);
    settle(&mut w, &mut e);
    assert_eq!(w.fs.read_file(&["r.txt"]).as_deref(), Some(&b"orig"[..]));
}

#[test]
fn read_only_local_create_stays_local_and_is_listed() {
    let (mut w, mut e) = read_only_world(52);
    w.fs.write(&["new.txt"], b"n");
    mounts_changed(&mut e);
    settle(&mut w, &mut e);
    assert!(w.fs.exists(&["new.txt"]));
    assert!(w.remote.read_path(&["new.txt"]).is_none());
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::ReadOnly)
    );
}

#[test]
fn read_only_local_move_undone() {
    let (mut w, mut e) = read_only_world(53);
    w.fs.rename(&["r.txt"], &["moved.txt"]);
    mounts_changed(&mut e);
    settle(&mut w, &mut e);
    assert!(w.fs.exists(&["r.txt"]));
    assert!(!w.fs.exists(&["moved.txt"]));
}

// ----- names (3.8) -----

#[test]
fn a_remote_name_this_device_cannot_hold_is_listed_and_blocks_nothing() {
    let (mut w, mut e) = setup(60, VolumeRules::WINDOWS);
    w.remote.client_write(&["a:b.txt"], b"colon");
    w.remote.client_write(&["ok.txt"], b"ok");
    settle(&mut w, &mut e);
    assert!(w.fs.exists(&["ok.txt"]));
    assert!(!w.fs.exists(&["a:b.txt"]));
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::NameNotHoldable)
    );
}

#[test]
fn two_remote_names_one_on_this_device_the_first_syncs_the_second_is_listed() {
    let (mut w, mut e) = setup(61, VolumeRules::MACOS);
    w.remote.client_write(&["Report.pdf"], b"one");
    w.remote.client_write(&["report.pdf"], b"two");
    settle(&mut w, &mut e);
    let present: Vec<_> = w.fs.user_files().into_keys().collect();
    assert_eq!(present.len(), 1, "{present:?}");
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::NameClash),
        "{:#?}",
        obs.problems
    );
    // Nothing was overwritten on Hearth.
    assert_eq!(w.remote.files().len(), 2);
}

#[test]
fn a_case_only_rename_on_an_insensitive_volume_is_a_rename() {
    let (mut w, mut e) = synced(62, VolumeRules::MACOS, &[(&["readme"], b"r")]);
    w.fs.rename(&["readme"], &["README"]);
    e.notify(crate::engine::Event::WatcherOverflow);
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(
        w.remote.files().keys().cloned().collect::<Vec<_>>(),
        vec!["README"]
    );
}

#[test]
fn a_remote_case_only_rename_is_done_locally_through_a_temp_name() {
    let (mut w, mut e) = synced(63, VolumeRules::MACOS, &[(&["readme"], b"r")]);
    w.remote.client_move(&["readme"], &["README"]);
    e.notify(crate::engine::Event::RemoteHint);
    settle(&mut w, &mut e);
    assert_converged(&w, &e);
    assert_eq!(
        w.fs.user_files().keys().cloned().collect::<Vec<_>>(),
        vec!["README"]
    );
}

#[test]
fn a_local_name_hearth_refuses_stays_and_is_listed() {
    let (mut w, mut e) = setup(64, VolumeRules::LINUX);
    w.fs.write(&[".htaccess"], b"deny");
    w.fs.write(&["fine.txt"], b"fine");
    settle(&mut w, &mut e);
    assert!(w.remote.read_path(&["fine.txt"]).is_some());
    assert!(w.remote.read_path(&[".htaccess"]).is_none());
    assert!(w.fs.exists(&[".htaccess"]));
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::NameRefused)
    );
}

#[test]
fn a_decomposed_local_name_is_uploaded_in_nfc_and_not_renamed_locally() {
    let (mut w, mut e) = setup(65, VolumeRules::LINUX);
    let nfd = "cafe\u{301}.txt";
    w.fs.write(&[nfd], b"coffee");
    settle(&mut w, &mut e);
    assert!(w.remote.read_path(&["caf\u{e9}.txt"]).is_some());
    assert!(w.fs.exists(&[nfd]));
}

#[test]
fn symbolic_links_are_listed_not_followed() {
    let (mut w, mut e) = setup(66, VolumeRules::LINUX);
    w.fs.symlink(&["link"], "/etc/passwd");
    w.fs.write(&["a.txt"], b"a");
    settle(&mut w, &mut e);
    assert!(w.remote.read_path(&["link"]).is_none());
    assert!(w.remote.read_path(&["a.txt"]).is_some());
    let obs = e.observe().expect("ok");
    assert!(
        obs.problems
            .iter()
            .any(|p| p.kind == crate::tree::ProblemKind::Symlink)
    );
}

#[test]
fn ignored_files_are_never_uploaded_nor_downloaded() {
    let (mut w, mut e) = setup(67, VolumeRules::LINUX);
    w.fs.write(&["~$Budget.xlsx"], b"lock");
    w.remote.client_write(&[".DS_Store"], b"litter");
    w.fs.write(&["Budget.xlsx"], b"real");
    settle(&mut w, &mut e);
    assert!(w.remote.read_path(&["~$Budget.xlsx"]).is_none());
    assert!(!w.fs.exists(&[".DS_Store"]));
    assert!(w.remote.read_path(&["Budget.xlsx"]).is_some());
}
