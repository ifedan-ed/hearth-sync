//! One named test per cell of the decision table of docs/sync.md 3.5,
//! and per rule of its creation rows, permissions and names (3.8). Each
//! states the outcome the plan's text fixes for that cell, as trees.
//!
//! The base of every file case: folders `d` and `e`, and the file
//! `d/x.txt` holding `c0`. The local side edits to `c1` and moves to
//! `e/l.txt`; the remote side edits to `c2` and moves to `e/r.txt`.

use std::collections::BTreeSet;

use hearth_sync_model::name::Volume;
use hearth_sync_model::reconcile::{DeviceContext, Listing, MountRule, Settled, all_cells, perms, reconcile};
use hearth_sync_model::tree::{IdTree, Kind, NodeId, ROOT};

const D: NodeId = 1;
const E: NodeId = 2;
const X: NodeId = 10;
const COPY: &str = "(conflict Lap, 1970-01-01 00.00.00)";

fn file(h: &str) -> Kind {
    Kind::File {
        hash: h.into(),
        mtime: 0,
    }
}

fn ctx(volume: Volume, mounts: Vec<MountRule>) -> DeviceContext {
    DeviceContext {
        device: "Lap".into(),
        utc_offset_secs: 0,
        volume,
        mounts,
    }
}

fn base() -> IdTree {
    let mut t = IdTree::new();
    t.insert(D, ROOT, "d", Kind::Folder);
    t.insert(E, ROOT, "e", Kind::Folder);
    t.insert(X, D, "x.txt", file("c0"));
    t
}

/// A side's change of `x.txt`, by the plan's kind names.
#[derive(Clone, Copy, Debug)]
enum C {
    None,
    Edit,
    Move,
    Both,
    Delete,
}

fn changed(c: C, hash: &str, to: &str) -> IdTree {
    let mut t = base();
    match c {
        C::None => {}
        C::Edit => t.insert(X, D, "x.txt", file(hash)),
        C::Move => t.insert(X, E, to, file("c0")),
        C::Both => t.insert(X, E, to, file(hash)),
        C::Delete => {
            t.nodes.remove(&X);
        }
    }
    t
}

fn file_case(l: C, r: C) -> Settled {
    let local = changed(l, "c1", "l.txt");
    let remote = changed(r, "c2", "r.txt");
    reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("well formed")
}

/// The files of a tree, as (path, hash), sorted.
fn files(t: &IdTree) -> Vec<(String, String)> {
    t.to_paths()
        .entries
        .into_iter()
        .filter_map(|(p, k)| k.hash().map(|h| (p, h.clone())))
        .collect()
}

fn want(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = pairs.iter().map(|(p, h)| (p.to_string(), h.to_string())).collect();
    v.sort();
    v
}

/// Asserts both sides end with exactly these files.
fn ends_with(s: &Settled, pairs: &[(&str, &str)]) {
    assert_eq!(files(&s.server), want(pairs), "server");
    assert_eq!(files(&s.device), want(pairs), "device");
}

macro_rules! file_cell {
    ($name:ident, $l:ident, $r:ident, [$(($p:expr, $h:expr)),*]) => {
        #[test]
        fn $name() {
            let s = file_case(C::$l, C::$r);
            ends_with(&s, &[$(($p, $h)),*]);
        }
    };
}

// Row none.
file_cell!(file_none_none_nothing, None, None, [("d/x.txt", "c0")]);
file_cell!(file_none_edit_download, None, Edit, [("d/x.txt", "c2")]);
file_cell!(file_none_move_move_locally, None, Move, [("e/r.txt", "c0")]);
file_cell!(file_none_both_move_then_download, None, Both, [("e/r.txt", "c2")]);
file_cell!(file_none_delete_remove_locally, None, Delete, []);
// Row edit.
file_cell!(file_edit_none_upload, Edit, None, [("d/x.txt", "c1")]);
file_cell!(file_edit_edit_a_conflict_copy, Edit, Edit, [("d/x.txt", "c2"), (&format!("d/x {COPY}.txt"), "c1")]);
file_cell!(file_edit_move_upload_and_move_locally, Edit, Move, [("e/r.txt", "c1")]);
file_cell!(file_edit_both_a_at_remote_place, Edit, Both, [("e/r.txt", "c2"), (&format!("e/r {COPY}.txt"), "c1")]);
file_cell!(file_edit_delete_b_edit_wins, Edit, Delete, [("d/x.txt", "c1")]);
// Row move.
file_cell!(file_move_none_move_on_hearth, Move, None, [("e/l.txt", "c0")]);
file_cell!(file_move_edit_move_on_hearth_download, Move, Edit, [("e/l.txt", "c2")]);
file_cell!(file_move_move_c_hearth_wins, Move, Move, [("e/r.txt", "c0")]);
file_cell!(file_move_both_c_then_download, Move, Both, [("e/r.txt", "c2")]);
file_cell!(file_move_delete_d_delete_applied, Move, Delete, []);
// Row both.
file_cell!(file_both_none_move_and_upload, Both, None, [("e/l.txt", "c1")]);
file_cell!(file_both_edit_a_at_local_place, Both, Edit, [("e/l.txt", "c2"), (&format!("e/l {COPY}.txt"), "c1")]);
file_cell!(file_both_move_c_then_upload, Both, Move, [("e/r.txt", "c1")]);
file_cell!(file_both_both_c_and_a, Both, Both, [("e/r.txt", "c2"), (&format!("e/r {COPY}.txt"), "c1")]);
file_cell!(file_both_delete_b, Both, Delete, [("e/l.txt", "c1")]);
// Row delete.
file_cell!(file_delete_none_delete_on_hearth, Delete, None, []);
file_cell!(file_delete_edit_e_edit_wins, Delete, Edit, [("d/x.txt", "c2")]);
file_cell!(file_delete_move_delete_on_hearth, Delete, Move, []);
file_cell!(file_delete_both_e, Delete, Both, [("e/r.txt", "c2")]);
file_cell!(file_delete_delete_drop, Delete, Delete, []);

#[test]
fn file_edit_edit_equal_hashes_nothing_moves() {
    let mut local = base();
    local.insert(X, D, "x.txt", file("same"));
    let remote = local.clone();
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("d/x.txt", "same")]);
    assert!(s.copies.is_empty());
}

#[test]
fn file_move_move_c_records_the_move_back() {
    let s = file_case(C::Move, C::Move);
    assert!(s.moved_back.contains(&X));
}

/// Creations: one new file per side, same name.
fn creations(local_kind: Kind, remote_kind: Kind) -> Settled {
    let mut local = base();
    local.insert(20, ROOT, "n", local_kind);
    let mut remote = base();
    remote.insert(21, ROOT, "n", remote_kind);
    reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok")
}

#[test]
fn creation_file_file_equal_hash_adopted_as_one() {
    let s = creations(file("c5"), file("c5"));
    ends_with(&s, &[("d/x.txt", "c0"), ("n", "c5")]);
    assert_eq!(s.server.at_path(&["n".into()]), Some(21));
}

#[test]
fn creation_file_file_different_remote_keeps_name() {
    let s = creations(file("c5"), file("c6"));
    ends_with(&s, &[("d/x.txt", "c0"), ("n", "c6"), (&format!("n {COPY}"), "c5")]);
}

#[test]
fn creation_file_folder_the_folder_keeps_the_name() {
    let s = creations(file("c5"), Kind::Folder);
    ends_with(&s, &[("d/x.txt", "c0"), (&format!("n {COPY}"), "c5")]);
    assert!(s.server.to_paths().get("n").is_some_and(Kind::is_folder));
    // And the mirror: the remote file steps aside on its own side.
    let s = creations(Kind::Folder, file("c6"));
    ends_with(&s, &[("d/x.txt", "c0"), (&format!("n {COPY}"), "c6")]);
}

#[test]
fn creation_folder_folder_merged_recursively() {
    let mut local = base();
    local.insert(20, ROOT, "n", Kind::Folder);
    local.insert(22, 20, "a", file("c7"));
    local.insert(23, 20, "same", file("c9"));
    let mut remote = base();
    remote.insert(21, ROOT, "n", Kind::Folder);
    remote.insert(24, 21, "b", file("c8"));
    remote.insert(25, 21, "same", file("c10"));
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(
        &s,
        &[
            ("d/x.txt", "c0"),
            ("n/a", "c7"),
            ("n/b", "c8"),
            ("n/same", "c10"),
            (&format!("n/same {COPY}"), "c9"),
        ],
    );
}

#[test]
fn creation_a_name_the_other_side_moved_a_node_to() {
    let mut local = base();
    local.insert(20, ROOT, "r.txt", file("c5"));
    let mut remote = base();
    remote.insert(X, ROOT, "r.txt", file("c0"));
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("r.txt", "c0"), (&format!("r {COPY}.txt"), "c5")]);
}

#[test]
fn creation_same_id_both_sides_without_base_is_a() {
    // A node both sides hold with no base (state database lost, I12).
    let mut local = IdTree::new();
    local.insert(X, ROOT, "x.txt", file("c1"));
    let mut remote = IdTree::new();
    remote.insert(X, ROOT, "x.txt", file("c2"));
    let s = reconcile(&IdTree::new(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("x.txt", "c2"), (&format!("x {COPY}.txt"), "c1")]);
    let s = reconcile(&IdTree::new(), &remote, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("x.txt", "c2")]);
    assert!(s.copies.is_empty());
}

/// Folder cases: the folder `d` (holding `x.txt`, unchanged) moved to
/// `e/dl` locally or `e/dr` remotely, or deleted with what it holds.
fn folder_case(l: C, r: C) -> Settled {
    let side = |c: C, to: &str| {
        let mut t = base();
        match c {
            C::Move => t.insert(D, E, to, Kind::Folder),
            C::Delete => {
                t.nodes.remove(&D);
                t.nodes.remove(&X);
            }
            _ => {}
        }
        t
    };
    reconcile(&base(), &side(l, "dl"), &side(r, "dr"), &ctx(Volume::Ext4, vec![]), 100).expect("ok")
}

macro_rules! folder_cell {
    ($name:ident, $l:ident, $r:ident, [$(($p:expr, $h:expr)),*]) => {
        #[test]
        fn $name() {
            let s = folder_case(C::$l, C::$r);
            ends_with(&s, &[$(($p, $h)),*]);
        }
    };
}

folder_cell!(folder_none_none_nothing, None, None, [("d/x.txt", "c0")]);
folder_cell!(folder_none_move_move_locally, None, Move, [("e/dr/x.txt", "c0")]);
folder_cell!(folder_none_delete_f_remove_locally, None, Delete, []);
folder_cell!(folder_move_none_move_on_hearth, Move, None, [("e/dl/x.txt", "c0")]);
folder_cell!(folder_move_move_c, Move, Move, [("e/dr/x.txt", "c0")]);
folder_cell!(folder_move_delete_g, Move, Delete, []);
folder_cell!(folder_delete_none_f_delete_on_hearth, Delete, None, []);
folder_cell!(folder_delete_move_g, Delete, Move, []);
folder_cell!(folder_delete_delete_nothing, Delete, Delete, []);

#[test]
fn folder_f_an_edited_child_keeps_its_folders() {
    let mut local = base();
    local.insert(X, D, "x.txt", file("c1"));
    let mut remote = base();
    remote.nodes.remove(&D);
    remote.nodes.remove(&X);
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("d/x.txt", "c1")]);
}

#[test]
fn folder_g_survivors_stay_at_the_new_place() {
    let mut local = base();
    local.nodes.remove(&D);
    local.nodes.remove(&X);
    let mut remote = base();
    remote.insert(D, E, "dr", Kind::Folder);
    remote.insert(X, D, "x.txt", file("c2"));
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("e/dr/x.txt", "c2")]);
}

#[test]
fn folder_created_in_a_folder_the_other_side_deleted_survives() {
    let mut local = base();
    local.insert(30, D, "new.txt", file("c3"));
    let mut remote = base();
    remote.nodes.remove(&D);
    remote.nodes.remove(&X);
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("d/new.txt", "c3")]);
}

#[test]
fn folder_moves_into_each_other_hearth_wins() {
    let mut local = base();
    local.insert(D, E, "d", Kind::Folder);
    let mut remote = base();
    remote.insert(E, D, "e", Kind::Folder);
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[("d/x.txt", "c0")]);
    assert!(s.server.to_paths().get("d/e").is_some_and(Kind::is_folder));
    assert!(s.moved_back.contains(&D));
}

#[test]
fn retype_is_delete_and_create() {
    // Locally x.txt became a folder (new id); remotely untouched: the
    // file is deleted on Hearth and the folder made.
    let mut local = base();
    local.nodes.remove(&X);
    local.insert(31, D, "x.txt", Kind::Folder);
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[]);
    assert!(s.server.to_paths().get("d/x.txt").is_some_and(Kind::is_folder));
    // Remotely edited meanwhile: E keeps the file, the folder takes the
    // name, the file steps aside as a copy on its own side.
    let mut remote = base();
    remote.insert(X, D, "x.txt", file("c2"));
    let s = reconcile(&base(), &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    ends_with(&s, &[(&format!("d/x {COPY}.txt"), "c2")]);
}

fn read_only() -> Vec<MountRule> {
    vec![MountRule {
        root: D,
        perms: perms::READ,
    }]
}

#[test]
fn permissions_read_only_edit_restored_and_copy_kept_locally() {
    let mut local = base();
    local.insert(X, D, "x.txt", file("c1"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, read_only()), 100).expect("ok");
    assert_eq!(files(&s.server), want(&[("d/x.txt", "c0")]));
    assert_eq!(files(&s.device), want(&[("d/x.txt", "c0"), (&format!("d/x {COPY}.txt"), "c1")]));
    assert!(s.listed.values().any(|l| *l == Listing::ReadOnlyEdit));
}

#[test]
fn permissions_read_only_delete_downloaded_again() {
    let mut local = base();
    local.nodes.remove(&X);
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, read_only()), 100).expect("ok");
    ends_with(&s, &[("d/x.txt", "c0")]);
}

#[test]
fn permissions_read_only_create_stays_local() {
    let mut local = base();
    local.insert(30, D, "new.txt", file("c3"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, read_only()), 100).expect("ok");
    assert_eq!(files(&s.server), want(&[("d/x.txt", "c0")]));
    assert_eq!(files(&s.device), want(&[("d/x.txt", "c0"), ("d/new.txt", "c3")]));
    assert_eq!(s.listed.get(&30), Some(&Listing::ReadOnlyCreate));
}

#[test]
fn permissions_read_only_move_out_undone() {
    let mut local = base();
    local.insert(X, E, "x.txt", file("c0"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, read_only()), 100).expect("ok");
    ends_with(&s, &[("d/x.txt", "c0")]);
    assert_eq!(s.listed.get(&X), Some(&Listing::ReadOnlyMove));
}

#[test]
fn names_hearth_refuses_stay_local() {
    let mut local = base();
    local.insert(30, ROOT, "a\\b", file("c3"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    assert_eq!(files(&s.server), want(&[("d/x.txt", "c0")]));
    assert!(files(&s.device).contains(&("a\\b".to_string(), "c3".to_string())));
    assert!(matches!(s.listed.get(&30), Some(Listing::HearthRefused(_))));
    // A rename to a refused name: Hearth keeps the old name.
    let mut local = base();
    local.insert(X, D, ".htaccess", file("c0"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    assert_eq!(files(&s.server), want(&[("d/x.txt", "c0")]));
    assert_eq!(files(&s.device), want(&[("d/.htaccess", "c0")]));
    // Decided the same way again next time (never retried).
    let again = reconcile(&s.base, &local, &s.server, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    assert_eq!(files(&again.server), files(&s.server));
    assert_eq!(files(&again.device), files(&s.device));
}

#[test]
fn names_a_device_cannot_hold_stay_on_hearth() {
    let mut remote = base();
    remote.insert(30, ROOT, "what?", file("c3"));
    remote.insert(31, ROOT, "con.txt", file("c4"));
    remote.insert(32, ROOT, "trail.", file("c5"));
    let s = reconcile(&base(), &base(), &remote, &ctx(Volume::Ntfs, vec![]), 100).expect("ok");
    assert_eq!(files(&s.server).len(), 4);
    assert_eq!(files(&s.device), want(&[("d/x.txt", "c0")]));
    assert_eq!(s.listed.len(), 3);
    // An ext4 device holds all of them.
    let s = reconcile(&base(), &base(), &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    assert_eq!(files(&s.device).len(), 4);
}

#[test]
fn names_case_clash_on_an_insensitive_device_one_arrives() {
    let mut remote = base();
    remote.insert(30, ROOT, "Report.pdf", file("c3"));
    remote.insert(31, ROOT, "report.pdf", file("c4"));
    let s = reconcile(&base(), &base(), &remote, &ctx(Volume::Ntfs, vec![]), 100).expect("ok");
    assert_eq!(files(&s.server).len(), 3);
    assert_eq!(files(&s.device).len(), 2);
    assert_eq!(s.device_alternatives, vec![BTreeSet::from([30, 31])]);
    // The one already on the device keeps its place.
    let mut local = base();
    local.insert(31, ROOT, "report.pdf", file("c4"));
    let mut b = base();
    b.insert(31, ROOT, "report.pdf", file("c4"));
    let s = reconcile(&b, &local, &remote, &ctx(Volume::Ntfs, vec![]), 100).expect("ok");
    assert!(files(&s.device).contains(&("report.pdf".to_string(), "c4".to_string())));
    assert!(s.device_alternatives.is_empty());
}

#[test]
fn names_nfd_local_file_uploaded_under_its_nfc_name() {
    let mut local = base();
    local.insert(30, ROOT, "Cafe\u{301}", file("c3"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::HfsPlus, vec![]), 100).expect("ok");
    assert!(files(&s.server).contains(&("Caf\u{E9}".to_string(), "c3".to_string())));
    // Not a rename next time: same name in another form.
    let again = reconcile(&s.base, &local, &s.server, &ctx(Volume::HfsPlus, vec![]), 100).expect("ok");
    assert_eq!(files(&again.server), files(&s.server));
    assert!(again.copies.is_empty());
}

#[test]
fn names_case_only_rename_is_a_rename() {
    let mut local = base();
    local.insert(X, D, "X.TXT", file("c0"));
    let s = reconcile(&base(), &local, &base(), &ctx(Volume::Ntfs, vec![]), 100).expect("ok");
    ends_with(&s, &[("d/X.TXT", "c0")]);
}

#[test]
fn names_engine_names_at_the_root_are_not_synced() {
    let mut remote = base();
    remote.insert(30, ROOT, ".hearthsync", Kind::Folder);
    remote.insert(31, ROOT, "a.hearthtmp", file("c3"));
    let s = reconcile(&base(), &base(), &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    assert_eq!(s.listed.get(&30), Some(&Listing::EngineName));
    assert_eq!(s.listed.get(&31), Some(&Listing::EngineName));
    assert_eq!(files(&s.device), want(&[("d/x.txt", "c0")]));
}

#[test]
fn conflict_copy_numbering_when_the_name_is_taken() {
    let mut b = base();
    b.insert(40, D, &format!("x {COPY}.txt"), file("old"));
    let mut local = b.clone();
    local.insert(X, D, "x.txt", file("c1"));
    let mut remote = b.clone();
    remote.insert(X, D, "x.txt", file("c2"));
    let s = reconcile(&b, &local, &remote, &ctx(Volume::Ext4, vec![]), 100).expect("ok");
    let names: Vec<String> = files(&s.server).into_iter().map(|(p, _)| p).collect();
    assert!(names.contains(&"d/x (conflict Lap, 1970-01-01 00.00.00 2).txt".to_string()), "{names:?}");
}

#[test]
fn every_cell_is_reached() {
    let kinds = [C::None, C::Edit, C::Move, C::Both, C::Delete];
    let mut reached = BTreeSet::new();
    for l in kinds {
        for r in kinds {
            reached.extend(file_case(l, r).cells);
        }
    }
    let folder_kinds = [C::None, C::Move, C::Delete];
    for l in folder_kinds {
        for r in folder_kinds {
            reached.extend(folder_case(l, r).cells);
        }
    }
    reached.extend(creations(file("a"), file("b")).cells);
    reached.extend(creations(Kind::Folder, Kind::Folder).cells);
    let mut both = IdTree::new();
    both.insert(50, ROOT, "f", Kind::Folder);
    both.insert(51, ROOT, "g", file("c"));
    let ctx = ctx(Volume::Ext4, vec![]);
    reached.extend(reconcile(&IdTree::new(), &both, &both, &ctx, 100).expect("ok").cells);
    let missing: Vec<String> = all_cells().into_iter().filter(|c| !reached.contains(c)).collect();
    assert!(missing.is_empty(), "cells never reached: {missing:?}");
}
