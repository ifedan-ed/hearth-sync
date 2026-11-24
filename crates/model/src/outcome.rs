//! The settled outcome: what must be true when a run has settled, beyond
//! the per-step guarantees.
//!
//! Two statements, one exact and one general:
//!
//! - The exact one. Between two settled points where at most one device
//!   changed anything, and that device was offline from the start of the
//!   window until every web change was made, the documents fix the
//!   outcome completely: it is [`reconcile`] of the synced tree, that
//!   device's tree and Hearth's tree (sync.md 3.5; 7.2 "every decision
//!   matches the reference model"). Every other device then holds Hearth's
//!   tree as its volume allows. The checker computes it and compares.
//! - The general one, for every window (several devices changing at
//!   once, faults, restores): the order in which devices reach Hearth is
//!   not fixed, so the outcome is a set, described by versions. Each
//!   content a person made is "live" until a person who saw it replaced
//!   or deleted it. At a settled point every live content is somewhere
//!   people can see it (on Hearth, or on a device as a listed item): that
//!   is I1. And Hearth holds no content that is not live: a delete or an
//!   edit a person made against what they saw is never undone (I7, "the
//!   settled outcome"). Which version keeps the name and which becomes
//!   the copy is free; I9 bounds the copies.
//!
//! The general statement is how the model "accepts the allowed set":
//! every interleaving the documents allow satisfies it, and losing,
//! duplicating or resurrecting a version breaks it.

use std::collections::{BTreeMap, BTreeSet};

use crate::apply::{Namespace, apply_act, lookup};
use crate::check::{Guarantee, Violation, act_content, find_by_nfc, mount_of};
use crate::reconcile::{DeviceContext, MountRule, Settled, perms, reconcile};
use crate::scenario::{Act, AdminOp, FaultOp, Side};
use crate::trace::{Event, RootState, Snapshot, Trace};
use crate::tree::{Hash, IdTree, Kind, NodeId, PathTree, is_within, nfc_path};

/// Runs the settled-outcome checks over a trace.
pub fn check_outcomes(trace: &Trace) -> Vec<Violation> {
    let mut out = check_versions(trace);
    out.extend(check_exact(trace));
    out
}

/// The live contents at the start: everything on Hearth or on a disk.
fn initial_live(s: &Snapshot) -> BTreeSet<Hash> {
    let mut live: BTreeSet<Hash> = s.server.tree.hash_counts().into_keys().collect();
    for d in s.devices.values() {
        live.extend(d.tree.hash_counts().into_keys());
    }
    live
}

/// Whether a person's act on `side` at `path` reaches Hearth at all. An
/// act Hearth will refuse (a read-only mount, a path the scenario made
/// Hearth refuse) does not replace anything there: the old version stays
/// on Hearth by the rules (3.5 Permissions; I10), so it is not
/// superseded.
fn reaches_hearth(side: &Side, path: &str, before: &Snapshot, refused: &BTreeSet<String>) -> bool {
    if matches!(side, Side::Web) {
        return true;
    }
    let p = nfc_path(path);
    if refused.iter().any(|r| is_within(&p, r)) {
        return false;
    }
    !mount_of(&before.server, &p).is_some_and(|(_, m)| m.perms != perms::ALL)
}

/// The contents a person's act replaces or deletes: what that side held
/// at the act's target just before (what the person saw).
fn superseded(act: &Act, side: &Side, before: &Snapshot) -> Vec<Hash> {
    let tree = match side {
        Side::Web => Some(&before.server.tree),
        Side::Device(d) => before.device(d).map(|d| &d.tree),
    };
    let Some(tree) = tree else { return Vec::new() };
    let under = |path: &str| -> Vec<Hash> {
        let p = nfc_path(path);
        tree.entries
            .iter()
            .filter(|(q, _)| is_within(&nfc_path(q), &p))
            .filter_map(|(_, k)| k.hash().cloned())
            .collect()
    };
    match act {
        Act::Edit { path, .. } => find_by_nfc(tree, path).and_then(Kind::hash).cloned().into_iter().collect(),
        Act::Delete { path } | Act::Retype { path, .. } => under(path),
        Act::Create { .. } | Act::Mkdir { .. } | Act::Rename { .. } | Act::Copy { .. } => Vec::new(),
    }
}

/// The target path of an act, for [`reaches_hearth`].
fn act_path(act: &Act) -> &str {
    match act {
        Act::Create { path, .. }
        | Act::Edit { path, .. }
        | Act::Mkdir { path }
        | Act::Delete { path }
        | Act::Retype { path, .. } => path,
        Act::Rename { from, .. } | Act::Copy { from, .. } => from,
    }
}

/// The general statement (see the module comment): at every `Settled`,
/// every live content is visible somewhere (I1), and Hearth holds no
/// content a person replaced or deleted (I7).
pub fn check_versions(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut live = initial_live(&trace.start);
    let mut refused: BTreeSet<String> = BTreeSet::new();
    for (i, step) in trace.steps.iter().enumerate() {
        let before = trace.before(i);
        match &step.event {
            Event::Person { side, act, applied: true } => {
                if reaches_hearth(side, act_path(act), before, &refused) {
                    for h in superseded(act, side, before) {
                        live.remove(&h);
                    }
                }
                live.extend(act_content(act, side, before));
            }
            Event::Admin(AdminOp::Refuse { path, .. }) => {
                refused.insert(nfc_path(path));
            }
            Event::Admin(AdminOp::ShareArrive { path }) => {
                for (p, k) in &step.after.server.tree.entries {
                    if is_within(p, &nfc_path(path))
                        && let Some(h) = k.hash()
                    {
                        live.insert(h.clone());
                    }
                }
            }
            Event::Admin(AdminOp::ShareLeave { path }) => {
                // Its files left the person's view; they are the owner's
                // and still on the owner's Hearth (sync-api 4).
                for (p, k) in &before.server.tree.entries {
                    if is_within(p, &nfc_path(path))
                        && let Some(h) = k.hash()
                    {
                        live.remove(h);
                    }
                }
            }
            Event::Admin(AdminOp::Restore) => {
                // What was only on Hearth after the backup is gone by the
                // operator's act; everything on a disk comes back by the
                // resync by hashes (sync.md 3.11), and so does the backup.
                let mut present: BTreeSet<Hash> = step.after.server.tree.hash_counts().into_keys().collect();
                for d in step.after.devices.values() {
                    present.extend(d.tree.hash_counts().into_keys());
                }
                live = present;
            }
            Event::Fault {
                device,
                fault: FaultOp::DbDeleted,
            } => {
                // Without history every differing local file is kept as a
                // conflict copy (I12): everything on the disk is live.
                if let Some(d) = before.device(device) {
                    live.extend(d.tree.hash_counts().into_keys());
                }
            }
            Event::Settled => {
                let after = &step.after;
                let mut visible: BTreeSet<Hash> = after.server.tree.hash_counts().into_keys().collect();
                for d in after.devices.values() {
                    for (p, k) in &d.tree.entries {
                        let listed = d.listed.keys().any(|l| is_within(&nfc_path(p), &nfc_path(l)));
                        if listed && let Some(h) = k.hash() {
                            visible.insert(h.clone());
                        }
                    }
                }
                for h in live.difference(&visible) {
                    out.push(Violation {
                        guarantee: Guarantee::I1,
                        step: Some(i),
                        device: None,
                        path: None,
                        expected: format!("content {h}, which no person replaced or deleted, on Hearth or listed on a device"),
                        found: "it is in neither place".into(),
                    });
                }
                for (p, k) in &after.server.tree.entries {
                    if let Some(h) = k.hash()
                        && !live.contains(h)
                    {
                        out.push(Violation {
                            guarantee: Guarantee::I7,
                            step: Some(i),
                            device: None,
                            path: Some(p.clone()),
                            expected: format!("content {h} gone: a person replaced or deleted it after seeing it"),
                            found: "it is on Hearth again".into(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The window between two settled points, followed act by act on trees
/// by identity, for the exact statement.
struct Window {
    /// Whether the exact outcome is fixed for this window so far.
    clean: bool,
    base: IdTree,
    remote: IdTree,
    local: BTreeMap<String, IdTree>,
    next_id: NodeId,
    /// Devices whose person acted in this window.
    changed: BTreeSet<String>,
    /// Devices that went offline before any web act of the window.
    offline_early: BTreeSet<String>,
    /// Devices that came online again during the window.
    came_online: BTreeSet<String>,
    web_acted: bool,
}

impl Window {
    /// A window starting from an observed settled state. It is clean only
    /// when every device holds exactly Hearth's tree and lists nothing.
    fn from(snapshot: &Snapshot) -> Window {
        let base = snapshot.server.tree.to_ids(1);
        let next_id = base.next_id();
        let mut clean = true;
        let mut local = BTreeMap::new();
        for (name, d) in &snapshot.devices {
            if d.tree.nfc() != snapshot.server.tree || !d.listed.is_empty() || d.root != RootState::Present || d.paused.is_some() {
                clean = false;
            }
            local.insert(name.clone(), base.clone());
        }
        Window {
            clean,
            remote: base.clone(),
            base,
            local,
            next_id,
            changed: BTreeSet::new(),
            offline_early: BTreeSet::new(),
            came_online: BTreeSet::new(),
            web_acted: false,
        }
    }
}

/// The exact statement (see the module comment), at every `Settled`
/// that ends a window where it applies.
pub fn check_exact(trace: &Trace) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut w = Window::from(&trace.start);
    for (i, step) in trace.steps.iter().enumerate() {
        match &step.event {
            Event::Person { side, act, applied } => {
                if !*applied {
                    continue;
                }
                match side {
                    Side::Web => {
                        if w.changed.iter().any(|d| w.came_online.contains(d)) {
                            w.clean = false;
                        }
                        w.web_acted = true;
                        let mtime = written_mtime(act, &step.after.server.tree);
                        let ok = apply_act(&mut w.remote, Namespace::Hearth, act, mtime, &mut w.next_id);
                        if !ok {
                            w.clean = false;
                        }
                    }
                    Side::Device(d) => {
                        if !w.offline_early.contains(d) || w.came_online.contains(d) {
                            w.clean = false;
                        }
                        w.changed.insert(d.clone());
                        let (Some(spec), Some(after)) = (trace.scenario.device(d), step.after.device(d)) else {
                            w.clean = false;
                            continue;
                        };
                        let mtime = written_mtime(act, &after.tree);
                        let ok = w
                            .local
                            .get_mut(d)
                            .is_some_and(|t| apply_act(t, Namespace::Device(spec.volume), act, mtime, &mut w.next_id));
                        if !ok {
                            w.clean = false;
                        }
                    }
                }
            }
            Event::Fault { device, fault } => match fault {
                FaultOp::Offline => {
                    if !w.web_acted && !w.changed.contains(device) {
                        w.offline_early.insert(device.clone());
                    }
                }
                FaultOp::Online => {
                    w.came_online.insert(device.clone());
                }
                FaultOp::Drop(_)
                | FaultOp::Stall(_)
                | FaultOp::LoseAnswer(_)
                | FaultOp::Crash(_)
                | FaultOp::DiskFull(_)
                | FaultOp::Skew(_)
                | FaultOp::Corrupt(_) => {}
                FaultOp::Deny(_) | FaultOp::RootVanish | FaultOp::RootBack | FaultOp::RootReplaced | FaultOp::DbDeleted => {
                    w.clean = false;
                }
            },
            Event::Admin(a) => {
                if !matches!(a, AdminOp::Restart | AdminOp::CursorExpire | AdminOp::EmptyTrash) {
                    w.clean = false;
                }
            }
            Event::Settled => {
                if w.clean && w.changed.len() <= 1 {
                    out.extend(compare_exact(trace, &w, i));
                }
                w = Window::from(&step.after);
            }
            Event::Confirm { .. } | Event::Engine { .. } => {}
        }
    }
    out
}

/// The time written on the content an act made, read from the side's
/// tree just after it (the model does not decide times; it only needs
/// them to name conflict copies).
fn written_mtime(act: &Act, tree: &PathTree) -> i64 {
    let path = match act {
        Act::Create { path, .. } | Act::Edit { path, .. } | Act::Retype { path, .. } => path,
        Act::Copy { to, .. } => to,
        _ => return 0,
    };
    match find_by_nfc(tree, path) {
        Some(Kind::File { mtime, .. }) => *mtime,
        _ => 0,
    }
}

/// The merge context of device `name` at a snapshot: its spec and the
/// mounts with their rights, by node in `remote`.
fn context(trace: &Trace, name: &str, snapshot: &Snapshot, remote: &IdTree) -> Option<DeviceContext> {
    let spec = trace.scenario.device(name)?;
    let mounts = snapshot
        .server
        .mounts
        .iter()
        .filter(|(p, _)| !p.is_empty())
        .filter_map(|(p, m)| {
            lookup(remote, Namespace::Hearth, p).map(|root| MountRule {
                root,
                perms: m.perms,
            })
        })
        .collect();
    Some(DeviceContext {
        device: spec.name.clone(),
        utc_offset_secs: spec.utc_offset_secs,
        volume: spec.volume,
        mounts,
    })
}

/// Compares the observed settled state at step `i` with the exact
/// outcome of window `w`.
fn compare_exact(trace: &Trace, w: &Window, i: usize) -> Vec<Violation> {
    let after = &trace.steps[i].after;
    let mut out = Vec::new();
    let mut next_id = w.next_id;
    let changed = w.changed.iter().next();
    let (server, device_outcomes): (IdTree, BTreeMap<String, Settled>) = match changed {
        Some(d) => {
            let (Some(ctx), Some(local)) = (context(trace, d, after, &w.remote), w.local.get(d)) else {
                return out;
            };
            match reconcile(&w.base, local, &w.remote, &ctx, next_id) {
                Ok(s) => {
                    next_id = next_id.max(s.server.next_id()).max(s.device.next_id());
                    (s.server.clone(), BTreeMap::from([(d.clone(), s)]))
                }
                Err(e) => {
                    out.push(model_error(i, d, &e));
                    return out;
                }
            }
        }
        None => (w.remote.clone(), BTreeMap::new()),
    };
    let mut devices = device_outcomes;
    for name in after.devices.keys() {
        if devices.contains_key(name) {
            continue;
        }
        let Some(ctx) = context(trace, name, after, &server) else { continue };
        match reconcile(&w.base, &w.base, &server, &ctx, next_id) {
            Ok(s) => {
                devices.insert(name.clone(), s);
            }
            Err(e) => out.push(model_error(i, name, &e)),
        }
    }
    let expected_server = server.to_paths();
    out.extend(tree_diff(i, None, &expected_server, &after.server.tree));
    for (name, s) in &devices {
        let Some(observed) = after.device(name) else { continue };
        let observed = observed.tree.nfc();
        let candidates = device_candidates(s);
        if !candidates.iter().any(|c| *c == observed) {
            out.extend(tree_diff(i, Some(name), &candidates[0], &observed));
        }
    }
    out
}

fn model_error(i: usize, device: &str, e: &crate::reconcile::ModelError) -> Violation {
    Violation {
        guarantee: Guarantee::Runner,
        step: Some(i),
        device: Some(device.to_string()),
        path: None,
        expected: "trees the model can read".into(),
        found: format!("{e:?}"),
    }
}

/// Every device tree the outcome allows: the default, and one per choice
/// of which clashing item arrived first (3.8), at most 16.
fn device_candidates(s: &Settled) -> Vec<PathTree> {
    let mut trees = vec![s.device.clone()];
    for group in &s.device_alternatives {
        let mut more = Vec::new();
        for t in &trees {
            let Some(shown) = group.iter().find(|id| t.has(**id)) else { continue };
            for other in group.iter().filter(|id| *id != shown) {
                let mut alt = t.clone();
                for gone in t.subtree(*shown) {
                    alt.nodes.remove(&gone);
                }
                for id in s.server.subtree(*other) {
                    if let Some(n) = s.server.get(id) {
                        alt.nodes.insert(id, n.clone());
                    }
                }
                more.push(alt);
            }
        }
        trees.extend(more);
        trees.truncate(16);
    }
    trees.iter().map(IdTree::to_paths).collect()
}

/// The differences between an expected and an observed tree, as
/// violations of I7 (the settled outcome), one per path.
fn tree_diff(i: usize, device: Option<&String>, expected: &PathTree, observed: &PathTree) -> Vec<Violation> {
    let mut out = Vec::new();
    let paths: BTreeSet<&String> = expected.entries.keys().chain(observed.entries.keys()).collect();
    for p in paths {
        let (e, o) = (expected.get(p), observed.get(p));
        let same = match (e, o) {
            (Some(a), Some(b)) => a.same_content(b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            out.push(Violation {
                guarantee: Guarantee::I7,
                step: Some(i),
                device: device.cloned(),
                path: Some(p.clone()),
                expected: format!("the model's settled outcome: {}", show(e)),
                found: show(o),
            });
        }
    }
    out
}

fn show(k: Option<&Kind>) -> String {
    match k {
        None => "nothing".into(),
        Some(Kind::Folder) => "a folder".into(),
        Some(Kind::File { hash, .. }) => format!("file {hash}"),
    }
}
