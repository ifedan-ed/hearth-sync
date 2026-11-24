//! The guarantees of docs/sync.md 2 as functions over what can be observed
//! (docs/observation.md 4). Each returns the violations it finds; an empty
//! list means the guarantee holds for what it was given.
//!
//! These judge single moments. What needs a whole run (I1's history of
//! saved bytes, I7's bound and quiescence, I12's deleted database, I13's
//! identical logs) is the model's and the simulator's: they keep the
//! history and call these at every step. The functions say nothing about
//! how the engine decided; they compare outcomes with the promise.

use std::collections::{BTreeMap, BTreeSet};

use crate::ids::{Etag, FileId, Hash, Kind, NodeId, RemoteId};
use crate::names::{LocalPath, TEMP_SUFFIX, to_nfc};
use crate::observe::{Observation, Phase};
use crate::store::{Store, StoreResult, check_consistency};
use crate::tree::{Condition, ConflictRow, OpKind};

/// One broken promise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// Which guarantee, `I1` to `I13`.
    pub guarantee: &'static str,
    /// What was found.
    pub detail: String,
}

fn v(guarantee: &'static str, detail: String) -> Violation {
    Violation { guarantee, detail }
}

/// One file on the simulated disk at a moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFile {
    /// Its path under the sync root.
    pub path: LocalPath,
    /// Its content's hash.
    pub hash: Hash,
    /// Its identity.
    pub file_id: FileId,
    /// Its size.
    pub size: u64,
    /// Its modification time.
    pub mtime_ns: i64,
}

/// Every content hash present in the places I1 names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldHashes {
    /// On the device: any file under the sync root, the holding area
    /// included.
    pub device: BTreeSet<Hash>,
    /// On Hearth: current files, versions and trash.
    pub hearth: BTreeSet<Hash>,
}

/// I1: every saved content still exists somewhere it may be (the device,
/// Hearth's files, versions or trash, the holding area).
pub fn i1_saved_bytes_present(saved: &BTreeSet<Hash>, world: &WorldHashes) -> Vec<Violation> {
    saved
        .iter()
        .filter(|h| !world.device.contains(h) && !world.hearth.contains(h))
        .map(|h| v("I1", format!("saved content {h} exists nowhere")))
        .collect()
}

/// I2: across one step, every content that was on the disk and was not
/// recorded as synced is still on the disk (under any name).
pub fn i2_no_unsynced_overwrite(
    before_obs: &Observation,
    before: &[DiskFile],
    after: &[DiskFile],
) -> Vec<Violation> {
    let after_hashes: BTreeSet<Hash> = after.iter().map(|f| f.hash).collect();
    let mut synced: BTreeSet<Hash> = BTreeSet::new();
    for n in &before_obs.nodes {
        if let (Some(l), Some(s)) = (&n.local, &n.synced) {
            if l.hash.is_some() && l.hash == s.hash {
                synced.extend(l.hash);
            }
        }
    }
    before
        .iter()
        .filter(|f| !synced.contains(&f.hash) && !after_hashes.contains(&f.hash))
        .map(|f| {
            v(
                "I2",
                format!("unsynced content {} at {:?} is gone", f.hash, f.path),
            )
        })
        .collect()
}

/// I3: a remote write's condition names a state the engine had received
/// from Hearth: an etag seen for that entry, a hash seen for it, or (for a
/// create) no entry at all. `seen` is every (entry, etag) and every
/// (entry, hash) the engine was told before the write.
pub fn i3_condition_seen(
    target: Option<&RemoteId>,
    condition: &Condition,
    seen_etags: &BTreeSet<(RemoteId, Etag)>,
    seen_hashes: &BTreeSet<(RemoteId, Hash)>,
) -> Vec<Violation> {
    match (condition, target) {
        (Condition::IfNoneMatch, _) => Vec::new(),
        (Condition::IfMatch(e), Some(id)) if seen_etags.contains(&(id.clone(), e.clone())) => {
            Vec::new()
        }
        (Condition::IfHash(h), Some(id)) if seen_hashes.contains(&(id.clone(), *h)) => Vec::new(),
        (Condition::IfSeq(_) | Condition::UnchangedSince(_), Some(_)) => Vec::new(),
        (c, t) => vec![v(
            "I3",
            format!("write to {t:?} conditioned on {c:?}, a state never received"),
        )],
    }
}

/// I4: no file at a real name is half written: every file outside the
/// engine's own places holds a version that was whole somewhere (saved
/// locally, or committed on Hearth). `known` is every such content.
pub fn i4_nothing_half_written(disk: &[DiskFile], known: &BTreeSet<Hash>) -> Vec<Violation> {
    disk.iter()
        .filter(|f| !is_engine_path(&f.path))
        .filter(|f| !known.contains(&f.hash))
        .map(|f| {
            v(
                "I4",
                format!("{:?} holds content never whole: {}", f.path, f.hash),
            )
        })
        .collect()
}

/// Whether a path is the engine's own (its folder at the root, or a temp
/// name beside a target).
pub fn is_engine_path(p: &LocalPath) -> bool {
    p.0.first()
        .is_some_and(|n| n.0 == crate::names::ENGINE_DIR.as_bytes())
        || p.file_name()
            .is_some_and(|n| n.0.ends_with(TEMP_SUFFIX.as_bytes()))
}

/// I5: every file the engine records as synced, and that has not changed
/// on disk since it was recorded, holds exactly the recorded content.
pub fn i5_hashes_verified(obs: &Observation, disk: &[DiskFile]) -> Vec<Violation> {
    let by_path: BTreeMap<&LocalPath, &DiskFile> = disk.iter().map(|f| (&f.path, f)).collect();
    let mut out = Vec::new();
    for n in &obs.nodes {
        let (Some(l), Some(s), Some(p)) = (&n.local, &n.synced, &n.local_path) else {
            continue;
        };
        if l.kind != Kind::File || l.hash.is_none() || l.hash != s.hash {
            continue;
        }
        let Some(f) = by_path.get(p) else { continue };
        let unchanged = f.file_id == l.file_id && f.size == l.size && f.mtime_ns == l.mtime_ns;
        if unchanged && Some(f.hash) != l.hash {
            out.push(v(
                "I5",
                format!("{p:?} recorded as {:?} but holds {}", l.hash, f.hash),
            ));
        }
    }
    out
}

/// I6: while the sync root is missing, nothing is being deleted because of
/// it: the engine is paused, or no deletion on Hearth is in flight.
pub fn i6_no_deletion_from_missing_root(obs: &Observation, root_present: bool) -> Vec<Violation> {
    if root_present || matches!(obs.phase, Phase::Paused(_)) {
        return Vec::new();
    }
    obs.ops
        .iter()
        .filter(|o| matches!(o.kind, OpKind::DeleteRemote | OpKind::DeleteRemoteFolder))
        .map(|o| {
            v(
                "I6",
                format!("{:?} in flight while the root is missing", o.kind),
            )
        })
        .collect()
}

/// I8: when idle, every node not listed as a problem and in a synced mount
/// has its three pictures equal: content hashes, kinds and NFC names and
/// places; or none of them.
pub fn i8_converged(obs: &Observation) -> Vec<Violation> {
    let selected: BTreeSet<_> = obs
        .mounts
        .iter()
        .filter(|m| m.selected)
        .map(|m| m.id.clone())
        .collect();
    let problems: BTreeSet<NodeId> = obs.problems.iter().filter_map(|p| p.node).collect();
    let mut out = Vec::new();
    for n in &obs.nodes {
        if !selected.contains(&n.mount) || problems.contains(&n.node) || n.excluded {
            continue;
        }
        match (&n.local, &n.remote, &n.synced) {
            (None, None, None) => {}
            (Some(l), Some(r), Some(s)) => {
                let same_kind = l.kind == r.kind && r.kind == s.kind;
                let same_hash = l.kind == Kind::Folder || (l.hash == r.hash && r.hash == s.hash);
                let lname = l.name.as_str().map(to_nfc);
                let same_name = r.parent.is_none() || lname.as_deref() == Some(r.name.as_str());
                if !same_kind || !same_hash || !same_name {
                    out.push(v(
                        "I8",
                        format!(
                            "{} not converged: local {lname:?} {:?}, remote {:?} {:?}, base {:?}",
                            n.node, l.hash, r.name, r.hash, s.hash
                        ),
                    ));
                }
            }
            _ => out.push(v(
                "I8",
                format!(
                    "{} not converged: local {}, remote {}, base {}",
                    n.node,
                    n.local.is_some(),
                    n.remote.is_some(),
                    n.synced.is_some()
                ),
            )),
        }
    }
    out
}

/// I9: conflict copies are named as the plan says, each name once, and
/// never made where the two contents were equal.
pub fn i9_conflict_copies(obs: &Observation, copies: &[ConflictRow]) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut names = BTreeSet::new();
    for c in copies {
        if !crate::names::looks_like_conflict_copy(&c.name) {
            out.push(v("I9", format!("conflict copy {:?} misnamed", c.name)));
        }
        if !names.insert((c.node, c.name.clone())) {
            out.push(v("I9", format!("conflict copy {:?} made twice", c.name)));
        }
        if let Some(winner) = obs.node(c.node) {
            let win = winner.remote.as_ref().and_then(|r| r.hash);
            if c.copy != c.node && c.hash.is_some() && c.hash == win {
                out.push(v(
                    "I9",
                    format!("conflict copy {:?} of equal content", c.name),
                ));
            }
        }
    }
    out
}

/// I10: when idle, every node that is not converged is listed with a
/// reason (or waits in an operation in flight).
pub fn i10_problems_listed(obs: &Observation) -> Vec<Violation> {
    let busy: BTreeSet<NodeId> = obs.ops.iter().map(|o| o.node).collect();
    i8_converged(obs)
        .into_iter()
        .filter(|x| {
            // The node id is the first word of I8's detail.
            let id = x
                .detail
                .split_whitespace()
                .next()
                .and_then(|w| w.strip_prefix('n'))
                .and_then(|w| w.parse::<u64>().ok());
            id.is_none_or(|i| !busy.contains(&NodeId(i)))
        })
        .map(|x| v("I10", format!("unlisted: {}", x.detail)))
        .collect()
}

/// I11: the state database keeps the schema's promises after a crash.
pub fn i11_store_consistent(store: &dyn Store) -> StoreResult<Vec<Violation>> {
    Ok(check_consistency(store)?
        .into_iter()
        .map(|d| v("I11", d))
        .collect())
}
