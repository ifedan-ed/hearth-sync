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
use crate::check::{
    Guarantee, Violation, act_content, find_on_side, mount_of, side_key, side_tree,
};
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

/// Live files by content: how many files people made with each content
/// that no person has since replaced or deleted. A count, not a set: a
/// copy is a second live file with the same content, and editing one of
/// the two retires only that one.
type Live = BTreeMap<Hash, usize>;

/// Retires one live file with content `h`.
fn retire(live: &mut Live, h: &Hash) {
    if let Some(n) = live.get_mut(h) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            live.remove(h);
        }
    }
}

/// The live files at the start: everything on Hearth or on a disk, each
/// content counted as often as the most any one side holds it (the same
/// file seen on Hearth and on a device is one file).
fn initial_live(s: &Snapshot) -> Live {
    let mut live = s.server.tree.hash_counts();
    for d in s.devices.values() {
        for (h, n) in d.tree.hash_counts() {
            let e = live.entry(h).or_insert(0);
            *e = (*e).max(n);
        }
    }
    live
}

/// Whether a person's act on `side` at `path` reaches Hearth at all. An
/// act Hearth will refuse (a read-only mount, a path the scenario made
/// Hearth refuse) does not replace anything there: the old version may
/// stay on Hearth by the rules (3.5 Permissions; I10), so its presence
/// there is not a resurrection.
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
/// at the act's target just before (what the person saw), the target
/// found the way that side compares names.
fn superseded(
    devices: &[crate::scenario::DeviceSpec],
    act: &Act,
    side: &Side,
    before: &Snapshot,
) -> Vec<(Hash, String)> {
    let Some((tree, volume)) = side_tree(devices, side, before) else {
        return Vec::new();
    };
    let under = |path: &str| -> Vec<(Hash, String)> {
        let p = side_key(volume, path);
        tree.entries
            .iter()
            .filter(|(q, _)| is_within(&side_key(volume, q), &p))
            .filter_map(|(q, k)| k.hash().map(|h| (h.clone(), nfc_path(q))))
            .collect()
    };
    match act {
        Act::Edit { path, .. } => find_on_side(tree, volume, path)
            .and_then(Kind::hash)
            .map(|h| (h.clone(), nfc_path(path)))
            .into_iter()
            .collect(),
        Act::Delete { path } | Act::Retype { path, .. } => under(path),
        Act::Create { .. } | Act::Mkdir { .. } | Act::Rename { .. } | Act::Copy { .. } => {
            Vec::new()
        }
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
    let mut denied: BTreeSet<(String, String)> = BTreeSet::new();
    // What each departed share held that was live, for its return.
    let mut away: BTreeMap<String, Live> = BTreeMap::new();
    // (content, path) pairs retired since the last settled point.
    let mut retired: BTreeSet<(Hash, String)> = BTreeSet::new();
    // Contents no longer required anywhere that may still stay on Hearth:
    // those a person replaced or deleted by an act Hearth refuses, and
    // those on Hearth when a device lost its database (it cannot know of
    // a delete it had not yet sent, so the files rightly come back, I12).
    let mut may_stay: BTreeSet<Hash> = BTreeSet::new();
    for (i, step) in trace.steps.iter().enumerate() {
        let before = trace.before(i);
        match &step.event {
            Event::Person {
                side,
                act,
                applied: true,
            } => {
                let denied_here = match side {
                    Side::Device(d) => denied
                        .iter()
                        .any(|(dv, p)| dv == d && is_within(&nfc_path(act_path(act)), p)),
                    Side::Web => false,
                };
                let reaches = !denied_here && reaches_hearth(side, act_path(act), before, &refused);
                for (h, file) in superseded(&trace.scenario.devices, act, side, before) {
                    // Two people replacing the same file (one of them
                    // stale) retire it once.
                    if retired.insert((h.clone(), file)) {
                        retire(&mut live, &h);
                    }
                    if !reaches {
                        may_stay.insert(h);
                    }
                }
                for h in act_content(&trace.scenario.devices, act, side, before) {
                    *live.entry(h).or_insert(0) += 1;
                }
            }
            Event::Admin(AdminOp::Refuse { path, .. }) => {
                refused.insert(nfc_path(path));
            }
            Event::Fault {
                device,
                fault: FaultOp::Deny(path),
            } => {
                denied.insert((device.clone(), nfc_path(path)));
            }
            Event::Fault {
                fault: FaultOp::RootReplaced,
                ..
            } => {
                // Files only the old root held went with it, by an act
                // outside the engine (an unplugged disk, a folder
                // replaced): no longer required.
                let mut anywhere: BTreeSet<Hash> =
                    step.after.server.tree.hash_counts().into_keys().collect();
                for d in step.after.devices.values() {
                    anywhere.extend(d.tree.hash_counts().into_keys());
                    anywhere.extend(d.holding.iter().cloned());
                }
                live.retain(|h, _| anywhere.contains(h));
                // Confirmed, the new root starts without history: deletes
                // made on the old one cannot be known, so what Hearth holds
                // comes back (contract-questions M5).
                may_stay.extend(step.after.server.tree.hash_counts().into_keys());
            }
            Event::Admin(AdminOp::ShareArrive { path })
                if !before.server.mounts.contains_key(path) =>
            {
                // Back as it was live when it left; a first arrival brings
                // what the share holds.
                match away.remove(path) {
                    Some(kept) => {
                        for (h, n) in kept {
                            *live.entry(h).or_insert(0) += n;
                        }
                    }
                    None => {
                        for (p, k) in &step.after.server.tree.entries {
                            if is_within(p, &nfc_path(path))
                                && let Some(h) = k.hash()
                            {
                                *live.entry(h.clone()).or_insert(0) += 1;
                            }
                        }
                    }
                }
            }
            Event::Admin(AdminOp::ShareLeave { path })
                if before.server.mounts.contains_key(path) =>
            {
                // Its files left the person's view; they are the owner's
                // and still on the owner's Hearth (sync-api 4).
                let mut kept: Live = Live::new();
                for (p, k) in &before.server.tree.entries {
                    if is_within(p, &nfc_path(path))
                        && let Some(h) = k.hash()
                        && live.contains_key(h)
                    {
                        *kept.entry(h.clone()).or_insert(0) += 1;
                        retire(&mut live, h);
                    }
                }
                away.insert(path.clone(), kept);
            }
            Event::Admin(AdminOp::Restore) => {
                // What was only on Hearth after the backup is gone by the
                // operator's act; everything on a disk comes back by the
                // resync by hashes (sync.md 3.11), and so does the backup.
                may_stay.extend(live.keys().cloned());
                retired.clear();
                live = initial_live(&step.after);
                // The resync after a restore has no history either: an
                // edit made then is not known as one, so what was there
                // may stay beside it (sync.md 3.11, as I12).
                may_stay.extend(live.keys().cloned());
            }
            Event::Fault {
                device,
                fault: FaultOp::DbDeleted,
            } => {
                // Without history every differing local file is kept as a
                // conflict copy (I12): everything on the disk is live.
                if let Some(d) = before.device(device) {
                    for (h, n) in d.tree.hash_counts() {
                        let e = live.entry(h).or_insert(0);
                        *e = (*e).max(n);
                    }
                }
                may_stay.extend(before.server.tree.hash_counts().into_keys());
                retired.clear();
            }
            Event::Settled => {
                let after = &step.after;
                let mut visible: BTreeSet<Hash> =
                    after.server.tree.hash_counts().into_keys().collect();
                for d in after.devices.values() {
                    for (p, k) in &d.tree.entries {
                        let listed = d
                            .listed
                            .keys()
                            .any(|l| is_within(&nfc_path(p), &nfc_path(l)));
                        if listed && let Some(h) = k.hash() {
                            visible.insert(h.clone());
                        }
                    }
                }
                for h in live.keys().filter(|h| !visible.contains(*h)) {
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
                        && !live.contains_key(h)
                        && !may_stay.contains(h)
                    {
                        out.push(Violation {
                            guarantee: Guarantee::I7,
                            step: Some(i),
                            device: None,
                            path: Some(p.clone()),
                            expected: format!(
                                "content {h} gone: a person replaced or deleted it after seeing it"
                            ),
                            found: "it is on Hearth again".into(),
                        });
                    }
                }
                // Two equal files people made separately may have become
                // one (the creation row "equal hash: adopt as one node"):
                // from here on each content is live as often as it is
                // visible, so deleting that one file retires it.
                let mut seen = after.server.tree.hash_counts();
                for d in after.devices.values() {
                    for (p, k) in &d.tree.entries {
                        let listed = d
                            .listed
                            .keys()
                            .any(|l| is_within(&nfc_path(p), &nfc_path(l)));
                        if listed && let Some(h) = k.hash() {
                            *seen.entry(h.clone()).or_insert(0) += 1;
                        }
                    }
                }
                for (h, n) in live.iter_mut() {
                    *n = (*n).min(seen.get(h).copied().unwrap_or(0)).max(1);
                }
                retired.clear();
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
            if d.tree.nfc() != snapshot.server.tree
                || !d.listed.is_empty()
                || d.root != RootState::Present
                || d.paused.is_some()
            {
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
    exact_windows(trace).0
}

/// The exact statement's violations, and how many settled points it
/// applied to (the rest are left to the general statement): for the
/// coverage report.
pub fn exact_windows(trace: &Trace) -> (Vec<Violation>, usize) {
    let mut compared = 0usize;
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
                        let mtime = written_mtime(act, &step.after.server.tree, None);
                        let ok =
                            apply_act(&mut w.remote, Namespace::Hearth, act, mtime, &mut w.next_id);
                        if !ok {
                            w.clean = false;
                        }
                    }
                    Side::Device(d) => {
                        if !w.offline_early.contains(d) || w.came_online.contains(d) {
                            w.clean = false;
                        }
                        w.changed.insert(d.clone());
                        let (Some(spec), Some(after)) =
                            (trace.scenario.device(d), step.after.device(d))
                        else {
                            w.clean = false;
                            continue;
                        };
                        let mtime = written_mtime(act, &after.tree, Some(spec.volume));
                        let ok = w.local.get_mut(d).is_some_and(|t| {
                            apply_act(
                                t,
                                Namespace::Device(spec.volume),
                                act,
                                mtime,
                                &mut w.next_id,
                            )
                        });
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
                FaultOp::Deny(_)
                | FaultOp::RootVanish
                | FaultOp::RootBack
                | FaultOp::RootReplaced
                | FaultOp::DbDeleted => {
                    w.clean = false;
                }
            },
            Event::Admin(a) => {
                if !matches!(
                    a,
                    AdminOp::Restart | AdminOp::CursorExpire | AdminOp::EmptyTrash
                ) {
                    w.clean = false;
                }
            }
            Event::Settled => {
                if w.clean && w.changed.len() <= 1 {
                    compared += 1;
                    out.extend(compare_exact(trace, &w, i));
                }
                w = Window::from(&step.after);
            }
            Event::Confirm { .. } | Event::Engine { .. } => {}
        }
    }
    (out, compared)
}

/// The time written on the content an act made, read from the side's
/// tree just after it (the model does not decide times; it only needs
/// them to name conflict copies).
fn written_mtime(act: &Act, tree: &PathTree, volume: Option<crate::name::Volume>) -> i64 {
    let path = match act {
        Act::Create { path, .. } | Act::Edit { path, .. } | Act::Retype { path, .. } => path,
        Act::Copy { to, .. } => to,
        _ => return 0,
    };
    match find_on_side(tree, volume, path) {
        Some(Kind::File { mtime, .. }) => *mtime,
        _ => 0,
    }
}

/// The merge context of device `name` at a snapshot: its spec and the
/// mounts with their rights, by node in `remote`.
fn context(
    trace: &Trace,
    name: &str,
    snapshot: &Snapshot,
    remote: &IdTree,
) -> Option<DeviceContext> {
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
            let (Some(ctx), Some(local)) = (context(trace, d, after, &w.remote), w.local.get(d))
            else {
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
        let Some(ctx) = context(trace, name, after, &server) else {
            continue;
        };
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
        // A device that was online while the web changed things saw the
        // steps in between; where its volume refuses or clashes a name,
        // which of those steps it kept depends on timing, so its tree is
        // left to the general checks.
        let online_through = changed != Some(name);
        if online_through && (!s.listed.is_empty() || !s.device_alternatives.is_empty()) {
            continue;
        }
        let Some(observed) = after.device(name) else {
            continue;
        };
        let observed = observed.tree.nfc();
        let candidates = device_candidates(s);
        if !candidates.contains(&observed) {
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
            let Some(shown) = group.iter().find(|id| t.has(**id)) else {
                continue;
            };
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
fn tree_diff(
    i: usize,
    device: Option<&String>,
    expected: &PathTree,
    observed: &PathTree,
) -> Vec<Violation> {
    let mut out = Vec::new();
    let paths: BTreeSet<&String> = expected
        .entries
        .keys()
        .chain(observed.entries.keys())
        .collect();
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
