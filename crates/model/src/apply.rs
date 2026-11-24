//! What a person's act does to one tree: a device's folder or Hearth's
//! web view.
//!
//! The rules are the ordinary ones of a file system, stated once: a name
//! is looked up the way that side compares names (a device by its
//! volume's key, sync.md 3.8; Hearth exactly in NFC, sync-api 13), an act
//! whose target is missing or whose name is taken does nothing, and a
//! rename may change only the case of a name on a case-insensitive volume
//! (3.8). The model uses it to follow what people did between two settled
//! points; the model's own tests use it to play a scenario.

use crate::name::{Volume, device_refuses, hearth_refuses, name_key, nfc, nfd};
use crate::scenario::Act;
use crate::tree::{IdTree, Kind, NodeId, ROOT};

/// How a side spells and compares names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    /// A device's volume: compared by key, kept as typed (HFS+ keeps the
    /// decomposed form, sync.md 3.8), refused per the volume.
    Device(Volume),
    /// Hearth through the web app: names normalised to NFC (sync-api 16:
    /// "WebDAV and the web API keep normalising silently") and compared
    /// exactly; refused per sync-api 13.
    Hearth,
}

impl Namespace {
    fn key(self, name: &str) -> String {
        match self {
            Namespace::Device(v) => name_key(v, name),
            Namespace::Hearth => nfc(name),
        }
    }

    /// The name as this side stores it, or `None` when it refuses it.
    fn stored(self, name: &str) -> Option<String> {
        match self {
            Namespace::Device(v) => {
                if name.is_empty()
                    || name.contains('/')
                    || name == "."
                    || name == ".."
                    || name.contains('\0')
                {
                    return None;
                }
                if device_refuses(v, &nfc(name)).is_some() {
                    return None;
                }
                Some(if v == Volume::HfsPlus {
                    nfd(name)
                } else {
                    name.to_string()
                })
            }
            Namespace::Hearth => {
                let n = nfc(name);
                hearth_refuses(&n).is_none().then_some(n)
            }
        }
    }
}

/// The node at `path` in `tree`, names compared the way `ns` does.
pub fn lookup(tree: &IdTree, ns: Namespace, path: &str) -> Option<NodeId> {
    if path.is_empty() {
        return Some(ROOT);
    }
    let mut at = ROOT;
    for name in path.split('/') {
        let key = ns.key(name);
        at = tree
            .nodes
            .iter()
            .find(|(_, n)| n.parent == at && ns.key(&n.name) == key)
            .map(|(id, _)| *id)?;
    }
    Some(at)
}

/// The folder that would hold `path` and the last name, if that folder
/// exists and is a folder.
fn parent_of(tree: &IdTree, ns: Namespace, path: &str) -> Option<(NodeId, String)> {
    let (parent, name) = match path.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", path),
    };
    let pid = lookup(tree, ns, parent)?;
    if pid != ROOT && !tree.get(pid)?.kind.is_folder() {
        return None;
    }
    Some((pid, name.to_string()))
}

/// Whether `folder` has a child whose name is `name` by `ns`'s rule,
/// other than `except`.
fn taken(tree: &IdTree, ns: Namespace, folder: NodeId, name: &str, except: Option<NodeId>) -> bool {
    let key = ns.key(name);
    tree.nodes
        .iter()
        .any(|(id, n)| n.parent == folder && Some(*id) != except && ns.key(&n.name) == key)
}

/// Applies `act` to `tree`. `mtime` is the time written on a new content,
/// `next_id` the source of new ids. Returns whether the act happened.
pub fn apply_act(
    tree: &mut IdTree,
    ns: Namespace,
    act: &Act,
    mtime: i64,
    next_id: &mut NodeId,
) -> bool {
    let mut fresh = || {
        let id = *next_id;
        *next_id += 1;
        id
    };
    let file = |content: &str| Kind::File {
        hash: content.to_string(),
        mtime,
    };
    match act {
        Act::Create { path, content } => {
            let Some((parent, name)) = parent_of(tree, ns, path) else {
                return false;
            };
            let Some(stored) = ns.stored(&name) else {
                return false;
            };
            if taken(tree, ns, parent, &stored, None) {
                return false;
            }
            tree.insert(fresh(), parent, &stored, file(content));
            true
        }
        Act::Mkdir { path } => {
            let Some((parent, name)) = parent_of(tree, ns, path) else {
                return false;
            };
            let Some(stored) = ns.stored(&name) else {
                return false;
            };
            if taken(tree, ns, parent, &stored, None) {
                return false;
            }
            tree.insert(fresh(), parent, &stored, Kind::Folder);
            true
        }
        Act::Edit { path, content } => {
            let Some(id) = lookup(tree, ns, path) else {
                return false;
            };
            match tree.nodes.get_mut(&id) {
                Some(n) if !n.kind.is_folder() => {
                    n.kind = file(content);
                    true
                }
                _ => false,
            }
        }
        Act::Rename { from, to } => {
            let Some(id) = lookup(tree, ns, from) else {
                return false;
            };
            if id == ROOT {
                return false;
            }
            let Some((parent, name)) = parent_of(tree, ns, to) else {
                return false;
            };
            let Some(stored) = ns.stored(&name) else {
                return false;
            };
            // Not into itself or below itself.
            if tree.subtree(id).contains(&parent) {
                return false;
            }
            if taken(tree, ns, parent, &stored, Some(id)) {
                return false;
            }
            match tree.nodes.get_mut(&id) {
                Some(n) if n.parent != parent || n.name != stored => {
                    n.parent = parent;
                    n.name = stored;
                    true
                }
                _ => false,
            }
        }
        Act::Delete { path } => {
            let Some(id) = lookup(tree, ns, path) else {
                return false;
            };
            if id == ROOT {
                return false;
            }
            for gone in tree.subtree(id) {
                tree.nodes.remove(&gone);
            }
            true
        }
        Act::Copy { from, to } => {
            let Some(src) = lookup(tree, ns, from) else {
                return false;
            };
            let Some(hash) = tree.get(src).and_then(|n| n.kind.hash().cloned()) else {
                return false;
            };
            let Some((parent, name)) = parent_of(tree, ns, to) else {
                return false;
            };
            let Some(stored) = ns.stored(&name) else {
                return false;
            };
            if taken(tree, ns, parent, &stored, None) {
                return false;
            }
            tree.insert(fresh(), parent, &stored, file(&hash));
            true
        }
        Act::Retype { path, content } => {
            let Some(id) = lookup(tree, ns, path) else {
                return false;
            };
            let Some(node) = tree.get(id).cloned() else {
                return false;
            };
            for gone in tree.subtree(id) {
                tree.nodes.remove(&gone);
            }
            let kind = if node.kind.is_folder() {
                file(content)
            } else {
                Kind::Folder
            };
            tree.insert(fresh(), node.parent, &node.name, kind);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_compares_by_key_and_hearth_exactly() {
        let mut t = IdTree::new();
        let mut next = 1;
        let ntfs = Namespace::Device(Volume::Ntfs);
        let create = |p: &str| Act::Create {
            path: p.into(),
            content: "c1".into(),
        };
        assert!(apply_act(&mut t, ntfs, &create("Report.pdf"), 0, &mut next));
        assert!(!apply_act(
            &mut t,
            ntfs,
            &create("report.pdf"),
            0,
            &mut next
        ));
        assert!(!apply_act(&mut t, ntfs, &create("con.txt"), 0, &mut next));
        // A case-only rename is a rename.
        let rename = Act::Rename {
            from: "report.PDF".into(),
            to: "REPORT.pdf".into(),
        };
        assert!(apply_act(&mut t, ntfs, &rename, 0, &mut next));
        assert_eq!(t.get(1).map(|n| n.name.as_str()), Some("REPORT.pdf"));

        let mut h = IdTree::new();
        assert!(apply_act(
            &mut h,
            Namespace::Hearth,
            &create("Report.pdf"),
            0,
            &mut next
        ));
        assert!(apply_act(
            &mut h,
            Namespace::Hearth,
            &create("report.pdf"),
            0,
            &mut next
        ));
        assert!(apply_act(
            &mut h,
            Namespace::Hearth,
            &create("con.txt"),
            0,
            &mut next
        ));
        assert!(!apply_act(
            &mut h,
            Namespace::Hearth,
            &create(".htaccess"),
            0,
            &mut next
        ));
    }

    #[test]
    fn hfs_keeps_decomposed_names() {
        let mut t = IdTree::new();
        let mut next = 1;
        let act = Act::Create {
            path: "Caf\u{E9}".into(),
            content: "c".into(),
        };
        assert!(apply_act(
            &mut t,
            Namespace::Device(Volume::HfsPlus),
            &act,
            0,
            &mut next
        ));
        assert_eq!(
            t.get(1).map(|n| n.name.clone()),
            Some("Cafe\u{301}".to_string())
        );
        assert_eq!(
            lookup(&t, Namespace::Device(Volume::HfsPlus), "caf\u{E9}"),
            Some(1)
        );
    }

    #[test]
    fn no_move_into_itself() {
        let mut t = IdTree::new();
        let mut next = 1;
        let ns = Namespace::Hearth;
        assert!(apply_act(
            &mut t,
            ns,
            &Act::Mkdir { path: "a".into() },
            0,
            &mut next
        ));
        assert!(apply_act(
            &mut t,
            ns,
            &Act::Mkdir { path: "a/b".into() },
            0,
            &mut next
        ));
        let into = Act::Rename {
            from: "a".into(),
            to: "a/b/a".into(),
        };
        assert!(!apply_act(&mut t, ns, &into, 0, &mut next));
    }
}
