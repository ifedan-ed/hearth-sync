//! Reading the three pictures: places, paths and mounts of nodes, computed
//! from the store's rows. Shared by the planner, the executor and the
//! observation, so all three agree on what "the local place" or "Hearth's
//! place" of a node is.

use std::collections::{BTreeMap, BTreeSet};

use crate::ids::{Kind, MountId, NodeId};
use crate::ignore::IgnoreRules;
use crate::names::{LocalName, LocalPath, VolumeRules, key_of_str, to_nfc};
use crate::store::{Store, StoreResult};
use crate::tree::{Crypt, MountRow, NodeRecord};

/// Where an item is or should be: its folder's node and its NFC name. The
/// sync root has no folder and an empty name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Place {
    /// The folder's node; none for the sync root.
    pub parent: Option<NodeId>,
    /// The name, NFC.
    pub name: String,
}

/// The node of the folder at a path of text components under the sync
/// root, by the local tree, if every component is held locally.
pub fn local_node_at(
    store: &dyn Store,
    root: NodeId,
    parts: &[String],
    rules: &VolumeRules,
) -> StoreResult<Option<NodeId>> {
    let mut cur = root;
    for p in parts {
        match store.local_child_by_key(cur, &key_of_str(p, rules))? {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// The local place of a node, from its local picture.
pub fn local_place(rec: &NodeRecord) -> Option<Place> {
    let l = rec.local.as_ref()?;
    Some(Place {
        parent: l.parent,
        name: to_nfc(l.name.as_str()?),
    })
}

/// The base's place of a node.
pub fn base_place(rec: &NodeRecord) -> Option<Place> {
    let s = rec.synced.as_ref()?;
    Some(Place {
        parent: s.parent,
        name: s.name.clone(),
    })
}

/// Hearth's place of a node, translated into nodes: the node of its remote
/// parent, and its name. A mount's root sits where the mount's path says
/// (its last part is the name the person sees). None when the parent is
/// not known yet, or the mount's folder above is not on disk yet.
pub fn remote_place(
    store: &dyn Store,
    mounts: &BTreeMap<MountId, MountRow>,
    root: NodeId,
    rules: &VolumeRules,
    rec: &NodeRecord,
) -> StoreResult<Option<Place>> {
    let Some(r) = rec.remote.as_ref() else {
        return Ok(None);
    };
    match &r.parent {
        Some(p) => Ok(store.node_by_remote(p)?.map(|pn| Place {
            parent: Some(pn),
            name: r.name.clone(),
        })),
        None => {
            let Some(m) = mounts.get(&r.mount) else {
                return Ok(None);
            };
            match m.local_path.split_last() {
                None => Ok(Some(Place {
                    parent: None,
                    name: String::new(),
                })),
                Some((last, above)) => {
                    Ok(local_node_at(store, root, above, rules)?.map(|pn| Place {
                        parent: Some(pn),
                        name: to_nfc(last),
                    }))
                }
            }
        }
    }
}

/// The path of a node under the sync root, by its local picture's parents.
/// None if the node or a folder above has no local picture.
pub fn local_path(store: &dyn Store, node: NodeId) -> StoreResult<Option<LocalPath>> {
    let mut parts: Vec<LocalName> = Vec::new();
    let mut cur = Some(node);
    let mut guard = 0usize;
    while let Some(n) = cur {
        guard += 1;
        if guard > 100_000 {
            // A cycle; the consistency checker reports it. Never loop.
            return Ok(None);
        }
        let Some(rec) = store.node(n)? else {
            return Ok(None);
        };
        let Some(l) = rec.local else {
            return Ok(None);
        };
        if l.parent.is_some() {
            parts.push(l.name.clone());
        }
        cur = l.parent;
    }
    parts.reverse();
    Ok(Some(LocalPath(parts)))
}

/// The path, as text components, where Hearth has a node under the sync
/// root (the mount's path, then the names below its root). None while a
/// parent is unknown.
pub fn remote_path(
    store: &dyn Store,
    mounts: &BTreeMap<MountId, MountRow>,
    node: NodeId,
) -> StoreResult<Option<Vec<String>>> {
    let mut parts = Vec::new();
    let mut cur = store.node(node)?;
    let mut guard = 0usize;
    while let Some(rec) = cur {
        guard += 1;
        if guard > 100_000 {
            return Ok(None);
        }
        let Some(r) = rec.remote else {
            return Ok(None);
        };
        match r.parent {
            Some(p) => {
                parts.push(r.name.clone());
                cur = match store.node_by_remote(&p)? {
                    Some(pn) => store.node(pn)?,
                    None => return Ok(None),
                };
            }
            None => {
                let Some(m) = mounts.get(&r.mount) else {
                    return Ok(None);
                };
                for c in m.local_path.iter().rev() {
                    parts.push(c.clone());
                }
                break;
            }
        }
    }
    parts.reverse();
    Ok(Some(parts))
}

/// The text components of a local path (none if a name is not UTF-8).
pub fn path_strs(path: &LocalPath) -> Option<Vec<&str>> {
    path.0.iter().map(LocalName::as_str).collect()
}

/// What keeps a node out of sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exclusion {
    /// Nothing: it syncs.
    None,
    /// It is, or is below, a folder the person chose not to sync (3.13).
    Unselected,
    /// It is below the top of an end-to-end encrypted folder, which this
    /// version does not sync into (sync-api.md 15.2).
    E2ee,
}

/// Whether a node is out of sync by selection or encryption, by either
/// side's parents.
pub fn exclusion(
    store: &dyn Store,
    unselected: &BTreeSet<NodeId>,
    rec: &NodeRecord,
) -> StoreResult<Exclusion> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![rec.node];
    let mut first = true;
    while let Some(n) = stack.pop() {
        if !seen.insert(n) || seen.len() > 10_000 {
            continue;
        }
        if unselected.contains(&n) {
            return Ok(Exclusion::Unselected);
        }
        let Some(r) = store.node(n)? else {
            continue;
        };
        // The top of an end-to-end folder is itself synced; what is below
        // it is not.
        if !first && r.remote.as_ref().is_some_and(|x| x.crypt == Crypt::E2ee) {
            return Ok(Exclusion::E2ee);
        }
        first = false;
        if let Some(p) = r.local.as_ref().and_then(|l| l.parent) {
            stack.push(p);
        }
        if let Some(p) = r.remote.as_ref().and_then(|x| x.parent.clone()) {
            if let Some(pn) = store.node_by_remote(&p)? {
                stack.push(pn);
            }
        }
    }
    Ok(Exclusion::None)
}

/// Whether Hearth's item is matched by the ignore rules at its path
/// (ignored items are never downloaded: 3.14).
pub fn remote_ignored(
    store: &dyn Store,
    mounts: &BTreeMap<MountId, MountRow>,
    ignore: &IgnoreRules,
    rec: &NodeRecord,
) -> StoreResult<bool> {
    let Some(parts) = remote_path(store, mounts, rec.node)? else {
        return Ok(false);
    };
    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let is_dir = rec.remote.as_ref().is_some_and(|r| r.kind == Kind::Folder);
    Ok(!refs.is_empty() && ignore.is_ignored(&refs, is_dir))
}
