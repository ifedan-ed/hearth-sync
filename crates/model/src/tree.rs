//! The model's trees: plain in-memory maps, nothing clever.
//!
//! Two shapes of the same thing:
//!
//! - [`IdTree`]: nodes by identity. The three-way merge needs it, because
//!   a rename is a change of one node, never a delete and a create
//!   (sync.md 1 and 3.4: "Files are known by identity, not by path").
//! - [`PathTree`]: entries by path. What an observer sees on a disk or on
//!   Hearth, and what the checkers compare.

use std::collections::{BTreeMap, BTreeSet};

/// A content hash, opaque to the model: in the simulator it is the real
/// `sha256:<hex>` of sync-api 1.5, in a scenario an abstract token such as
/// `c17`. Equal hashes mean equal bytes; nothing else is read from it.
pub type Hash = String;

/// A node's identity. One number space for every tree of a run: a node
/// that both sides hold has the same id on both (the model's stand-in for
/// "Hearth's file id" and "the local file id matched to it", sync.md 3.4).
pub type NodeId = u64;

/// The id of the sync root (or a mount's top), never stored as a node.
pub const ROOT: NodeId = 0;

/// What a node is. A node never changes kind: "type ... never changes for
/// an id" (sync-api 5), so a type change is a delete and a create
/// (sync.md 3.5, "retype").
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A file with its content and last change time.
    File {
        /// The content.
        hash: Hash,
        /// Last change, microseconds since 1970 UTC. Never decides
        /// anything (sync.md 3.15); used only to name conflict copies.
        mtime: i64,
    },
    /// A folder. Folders carry no content (sync-api 5.1).
    Folder,
}

impl Kind {
    /// The content hash of a file, `None` for a folder.
    pub fn hash(&self) -> Option<&Hash> {
        match self {
            Kind::File { hash, .. } => Some(hash),
            Kind::Folder => None,
        }
    }

    /// Whether this is a folder.
    pub fn is_folder(&self) -> bool {
        matches!(self, Kind::Folder)
    }

    /// Equal as content: same kind and, for files, same hash. Times are
    /// not content (sync.md 3.5.1: "Never for a difference in times").
    pub fn same_content(&self, other: &Kind) -> bool {
        match (self, other) {
            (Kind::Folder, Kind::Folder) => true,
            (Kind::File { hash: a, .. }, Kind::File { hash: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// One node of an [`IdTree`]: where it is and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The folder holding it ([`ROOT`] at the top).
    pub parent: NodeId,
    /// Its name in that folder, as that side spells it.
    pub name: String,
    /// File or folder.
    pub kind: Kind,
}

/// A tree by identity: every node with its parent and name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdTree {
    /// The nodes, by id. Ordered, so that every walk of the tree is the
    /// same on every run (I13).
    pub nodes: BTreeMap<NodeId, Node>,
}

impl IdTree {
    /// An empty tree (just the root).
    pub fn new() -> Self {
        IdTree::default()
    }

    /// The node `id`, if this tree holds it.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// Whether this tree holds `id` (the root always counts as held).
    pub fn has(&self, id: NodeId) -> bool {
        id == ROOT || self.nodes.contains_key(&id)
    }

    /// Adds or replaces a node.
    pub fn insert(&mut self, id: NodeId, parent: NodeId, name: &str, kind: Kind) {
        self.nodes.insert(
            id,
            Node {
                parent,
                name: name.to_string(),
                kind,
            },
        );
    }

    /// The ids of the nodes directly in `folder`, in id order.
    pub fn children(&self, folder: NodeId) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.parent == folder)
            .map(|(id, _)| *id)
            .collect()
    }

    /// `id` and every node below it.
    pub fn subtree(&self, id: NodeId) -> BTreeSet<NodeId> {
        let mut out = BTreeSet::new();
        let mut todo = vec![id];
        while let Some(next) = todo.pop() {
            if out.insert(next) {
                todo.extend(self.children(next));
            }
        }
        out
    }

    /// The node directly in `folder` whose name is exactly `name`.
    pub fn child_named(&self, folder: NodeId, name: &str) -> Option<NodeId> {
        self.nodes
            .iter()
            .find(|(_, n)| n.parent == folder && n.name == name)
            .map(|(id, _)| *id)
    }

    /// The names from the root down to `id`, or `None` when a parent is
    /// missing or the parents loop (a malformed tree; the model's own
    /// trees never are, and an observed one is reported, not trusted).
    pub fn path(&self, id: NodeId) -> Option<Vec<String>> {
        let mut names = Vec::new();
        let mut at = id;
        while at != ROOT {
            let node = self.nodes.get(&at)?;
            names.push(node.name.clone());
            at = node.parent;
            if names.len() > self.nodes.len() {
                return None;
            }
        }
        names.reverse();
        Some(names)
    }

    /// The node at `path` (names from the root), if any.
    pub fn at_path(&self, path: &[String]) -> Option<NodeId> {
        let mut at = ROOT;
        for name in path {
            at = self.child_named(at, name)?;
        }
        Some(at)
    }

    /// The same tree by path. Nodes whose path cannot be formed (see
    /// [`IdTree::path`]) are left out.
    pub fn to_paths(&self) -> PathTree {
        let mut out = PathTree::default();
        for (id, node) in &self.nodes {
            if let Some(path) = self.path(*id) {
                out.entries.insert(path.join("/"), node.kind.clone());
            }
        }
        out
    }

    /// One more than the largest id in use: a fresh id for this tree.
    pub fn next_id(&self) -> NodeId {
        self.nodes.keys().next_back().map_or(1, |id| id + 1)
    }
}

/// A tree by path: `a/b/c` -> what is there. Paths use `/` between names
/// (a name never holds `/` on Hearth, sync-api 13; on a device the model
/// never makes one).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PathTree {
    /// The entries, by path, ordered.
    pub entries: BTreeMap<String, Kind>,
}

impl PathTree {
    /// An empty tree.
    pub fn new() -> Self {
        PathTree::default()
    }

    /// The entry at `path`.
    pub fn get(&self, path: &str) -> Option<&Kind> {
        self.entries.get(path)
    }

    /// The content hash of the file at `path`.
    pub fn hash_at(&self, path: &str) -> Option<&Hash> {
        self.entries.get(path).and_then(Kind::hash)
    }

    /// Every file's hash, with how many files hold it.
    pub fn hash_counts(&self) -> BTreeMap<Hash, usize> {
        let mut out = BTreeMap::new();
        for kind in self.entries.values() {
            if let Some(h) = kind.hash() {
                *out.entry(h.clone()).or_insert(0) += 1;
            }
        }
        out
    }

    /// Whether any file here holds `hash`.
    pub fn holds(&self, hash: &str) -> bool {
        self.entries
            .values()
            .any(|k| k.hash().is_some_and(|h| h == hash))
    }

    /// The same tree with every name in NFC: how a device's tree is
    /// compared with Hearth's (I8: "names are equal as byte strings in
    /// NFC").
    pub fn nfc(&self) -> PathTree {
        PathTree {
            entries: self
                .entries
                .iter()
                .map(|(p, k)| (nfc_path(p), k.clone()))
                .collect(),
        }
    }

    /// The tree as id nodes, numbering from `first_id` in path order (so
    /// the same paths always get the same ids).
    pub fn to_ids(&self, first_id: NodeId) -> IdTree {
        let mut tree = IdTree::new();
        let mut ids: BTreeMap<&str, NodeId> = BTreeMap::new();
        // BTreeMap order puts "a" before "a/b", so a parent is numbered
        // before its children.
        for (next, (path, kind)) in (first_id..).zip(self.entries.iter()) {
            let (parent, name) = match path.rsplit_once('/') {
                Some((p, n)) => (ids.get(p).copied().unwrap_or(ROOT), n),
                None => (ROOT, path.as_str()),
            };
            tree.insert(next, parent, name, kind.clone());
            ids.insert(path, next);
        }
        tree
    }
}

/// `path` with each name in NFC.
pub fn nfc_path(path: &str) -> String {
    path.split('/')
        .map(crate::name::nfc)
        .collect::<Vec<_>>()
        .join("/")
}

/// The folder part of a path ("" at the top).
pub fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}

/// The last name of a path.
pub fn base_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, n)| n)
}

/// `parent/name`, or `name` at the top.
pub fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Whether `path` is `ancestor` or lies below it.
pub fn is_within(path: &str, ancestor: &str) -> bool {
    ancestor.is_empty()
        || path == ancestor
        || (path.len() > ancestor.len()
            && path.starts_with(ancestor)
            && path.as_bytes()[ancestor.len()] == b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(h: &str) -> Kind {
        Kind::File {
            hash: h.into(),
            mtime: 0,
        }
    }

    #[test]
    fn paths_and_ids_agree() {
        let mut t = IdTree::new();
        t.insert(1, ROOT, "a", Kind::Folder);
        t.insert(2, 1, "b.txt", file("c1"));
        let p = t.to_paths();
        assert_eq!(p.hash_at("a/b.txt"), Some(&"c1".to_string()));
        assert_eq!(t.at_path(&["a".into(), "b.txt".into()]), Some(2));
        assert_eq!(p.to_ids(1).to_paths(), p);
        assert_eq!(t.subtree(1).len(), 2);
    }

    #[test]
    fn loops_have_no_path() {
        let mut t = IdTree::new();
        t.insert(1, 2, "a", Kind::Folder);
        t.insert(2, 1, "b", Kind::Folder);
        assert_eq!(t.path(1), None);
        assert!(t.to_paths().entries.is_empty());
    }

    #[test]
    fn within() {
        assert!(is_within("a/b", "a"));
        assert!(!is_within("ab", "a"));
        assert!(is_within("x", ""));
    }
}
