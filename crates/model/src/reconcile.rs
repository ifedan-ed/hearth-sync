//! The reference three-way merge: from the synced tree (base), one
//! device's tree (local) and Hearth's tree (remote), the trees that must
//! result once that device and Hearth have settled.
//!
//! Written from docs/sync.md 3.5 (the decision table), 3.5.1 (conflict
//! copies), 3.8 (names) and docs/sync-api.md 13, as directly as the
//! author could, with no thought for speed. It does not plan operations;
//! it states the end. The engine's planner reaching another end is a bug
//! in one of the two, settled against the plan (sync.md 7.2).
//!
//! The stages, in order, each one a paragraph of the plan:
//!
//! 1. classify each side's change of every node (3.5, the list of kinds);
//! 2. decide each node by the table (files, folders, creations);
//! 3. permissions of the remote side (3.5, "Permissions");
//! 4. names Hearth refuses stay local (3.8, last rule);
//! 5. moves that would make a cycle: Hearth's wins (3.5, C);
//! 6. a folder survives when anything placed in it survives (3.5, F, G);
//! 7. two nodes wanting one name on Hearth (3.5, "Creations");
//! 8. what this device cannot hold or cannot tell apart (3.8).

use std::collections::{BTreeMap, BTreeSet};

use crate::name::{
    DeviceRefusal, HEARTH_NAME_BYTES_MAX, HearthRefusal, Stamp, Volume, conflict_name,
    device_refuses, hearth_refuses_local, is_engine_name, name_key, nfc,
};
use crate::tree::{IdTree, Kind, Node, NodeId, ROOT};

/// Rights bits of a mount (sync-api 4, `perms`; 5.2 says what each
/// operation needs).
pub mod perms {
    /// Read: the feed, listings, contents.
    pub const READ: u8 = 1;
    /// Replace a file's content.
    pub const UPDATE: u8 = 2;
    /// Create a file or folder, be the target of a move.
    pub const CREATE: u8 = 4;
    /// Delete, be the source of a move.
    pub const DELETE: u8 = 8;
    /// Everything (the home mount).
    pub const ALL: u8 = 31;
}

/// A mount as the merge needs it: the folder at its top and the rights
/// the person has in all of it (rights are per mount in version 1,
/// sync-api 5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRule {
    /// The node at the mount's top ([`ROOT`] for the home mount).
    pub root: NodeId,
    /// The rights bits, see [`perms`].
    pub perms: u8,
}

/// Who the device is and what its disk can do: everything about the
/// device the outcome depends on.
#[derive(Debug, Clone)]
pub struct DeviceContext {
    /// The device's name given at sign-in, for conflict copy names
    /// (3.5.1).
    pub device: String,
    /// The device's offset from UTC in seconds: conflict copies carry
    /// local time (3.5.1).
    pub utc_offset_secs: i64,
    /// The volume the sync root is on (3.8).
    pub volume: Volume,
    /// The mounts below the root, with their rights. A node in no listed
    /// mount has every right (the home mount).
    pub mounts: Vec<MountRule>,
}

/// How one side changed one node against the base (3.5, the list).
/// A type change never appears: a node keeps its kind for life, so a
/// retype is a `Delete` of one id and a `Create` of another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Change {
    /// Not in the base and not on this side.
    Absent,
    /// Same as base.
    None,
    /// Content differs from base (files only).
    Edit,
    /// Parent or name differs from base.
    Move,
    /// Edit and move.
    Both,
    /// In base, absent now.
    Delete,
    /// Absent in base, present now.
    Create,
}

impl Change {
    fn code(self) -> &'static str {
        match self {
            Change::Absent => "absent",
            Change::None => "none",
            Change::Edit => "edit",
            Change::Move => "move",
            Change::Both => "both",
            Change::Delete => "delete",
            Change::Create => "create",
        }
    }

    fn moves(self) -> bool {
        matches!(self, Change::Move | Change::Both)
    }
}

/// Classifies one side's change of one node. `nfc_names` is true for the
/// device side: there "the same name in another form is the same file"
/// (3.8), so a name is compared in NFC; on Hearth names are compared
/// exactly (sync-api 13). A case-only change is a move on both sides
/// (3.8: "A case-only rename ... is a rename").
pub fn classify(base: Option<&Node>, now: Option<&Node>, nfc_names: bool) -> Change {
    match (base, now) {
        (None, None) => Change::Absent,
        (None, Some(_)) => Change::Create,
        (Some(_), None) => Change::Delete,
        (Some(b), Some(n)) => {
            let renamed = if nfc_names {
                nfc(&b.name) != nfc(&n.name)
            } else {
                b.name != n.name
            };
            let moved = b.parent != n.parent || renamed;
            let edited = !b.kind.same_content(&n.kind);
            match (edited, moved) {
                (false, false) => Change::None,
                (true, false) => Change::Edit,
                (false, true) => Change::Move,
                (true, true) => Change::Both,
            }
        }
    }
}

/// Where a decided node takes its place or content from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    Base,
    Local,
    Remote,
}

/// One cell of the table, decided: does the node survive, where does it
/// go, what does it hold, and is it the A case (two edits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    survives: bool,
    place: Pick,
    content: Pick,
    /// A: both sides edited a file. Equal hashes: nothing moves; else the
    /// remote content takes the name and the local one becomes a copy.
    both_edited: bool,
}

const fn cell(survives: bool, place: Pick, content: Pick) -> Cell {
    Cell {
        survives,
        place,
        content,
        both_edited: false,
    }
}

const fn cell_a(place: Pick) -> Cell {
    Cell {
        survives: true,
        place,
        content: Pick::Remote,
        both_edited: true,
    }
}

/// The files table of 3.5, one arm per cell, rows local, columns remote.
/// The cell's text from the plan is beside each arm. "Place" is where the
/// file ends; "content" whose bytes it holds.
fn file_cell(local: Change, remote: Change) -> Option<Cell> {
    use Change::*;
    use Pick::{Base as B, Local as L, Remote as R};
    Some(match (local, remote) {
        // row none
        (None, None) => cell(true, B, B),    // nothing
        (None, Edit) => cell(true, B, R),    // download
        (None, Move) => cell(true, R, B),    // move locally
        (None, Both) => cell(true, R, R),    // move, then download
        (None, Delete) => cell(false, B, B), // remove locally (H)
        // row edit
        (Edit, None) => cell(true, B, L),   // upload
        (Edit, Edit) => cell_a(B),          // A
        (Edit, Move) => cell(true, R, L),   // upload to the id; move locally
        (Edit, Both) => cell_a(R),          // A, at the remote's new place
        (Edit, Delete) => cell(true, L, L), // B: the edit wins, at its local path
        // row move
        (Move, None) => cell(true, L, B),    // move on Hearth
        (Move, Edit) => cell(true, L, R),    // move on Hearth; download
        (Move, Move) => cell(true, R, B),    // C: Hearth's place wins
        (Move, Both) => cell(true, R, R),    // C, then download
        (Move, Delete) => cell(false, L, B), // D: the delete is applied
        // row both
        (Both, None) => cell(true, L, L),   // move and upload
        (Both, Edit) => cell_a(L),          // A, at the local's new place
        (Both, Move) => cell(true, R, L),   // C, then upload
        (Both, Both) => cell_a(R),          // C for the place, A for content
        (Both, Delete) => cell(true, L, L), // B
        // row delete
        (Delete, None) => cell(false, B, B), // delete on Hearth
        (Delete, Edit) => cell(true, R, R),  // E: the edit wins
        (Delete, Move) => cell(false, R, B), // delete on Hearth (D mirrored)
        (Delete, Both) => cell(true, R, R),  // E
        (Delete, Delete) => cell(false, B, B), // nothing (drop node)
        // creations, no base: each side's own new node goes where that
        // side put it. The same id new on both sides happens only when
        // the base lost a node both sides hold (a crash before the base
        // was written, or no base at all, I12): it is the creation row
        // "file and file" for one name: equal hashes adopt, else A with
        // the remote keeping the name.
        (Create, Absent) => cell(true, L, L),
        (Absent, Create) => cell(true, R, R),
        (Create, Create) => cell_a(R),
        _ => return Option::None,
    })
}

/// The folders table of 3.5. Folders have no content; an edit cannot
/// occur. A deleted folder's "survives: false" is provisional: by F it
/// survives after all when anything placed in it survives (stage 6), and
/// by G that happens at the mover's place, which is the `place` here.
fn folder_cell(local: Change, remote: Change) -> Option<Cell> {
    use Change::*;
    use Pick::{Base as B, Local as L, Remote as R};
    Some(match (local, remote) {
        (None, None) => cell(true, B, B),      // nothing
        (None, Move) => cell(true, R, B),      // move locally
        (None, Delete) => cell(false, B, B),   // remove locally (F)
        (Move, None) => cell(true, L, B),      // move on Hearth
        (Move, Move) => cell(true, R, B),      // C
        (Move, Delete) => cell(false, L, B),   // G: F at the folder's new place
        (Delete, None) => cell(false, B, B),   // delete on Hearth (F)
        (Delete, Move) => cell(false, R, B),   // G
        (Delete, Delete) => cell(false, B, B), // nothing
        (Create, Absent) => cell(true, L, B),
        (Absent, Create) => cell(true, R, B),
        // The same folder id on both sides without base: one folder,
        // Hearth's place (the creation row "folder and folder").
        (Create, Create) => cell(true, R, B),
        _ => return Option::None,
    })
}

/// Why an item is listed on the device instead of synced (I10).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Listing {
    /// A local name Hearth refuses: it stays local (3.8, last rule;
    /// sync-api 13: "The client lists the item with the reason and leaves
    /// it alone").
    HearthRefused(HearthRefusal),
    /// A remote name this device cannot hold: it stays on Hearth (3.8).
    DeviceRefused(DeviceRefusal),
    /// A remote name this device would hold as the same name as another
    /// item already here ("name clash on this device", 3.8).
    NameClash,
    /// A remote entry with the engine's own name at the sync root
    /// (sync-api 13, "The engine's own names").
    EngineName,
    /// A local edit of a read-only file, kept as a conflict copy that is
    /// "not uploaded: read-only" (3.5, Permissions).
    ReadOnlyEdit,
    /// A local create in a folder the person may not create in: "stays
    /// local, listed".
    ReadOnlyCreate,
    /// A local move out of or within a read-only folder: "undone locally,
    /// listed".
    ReadOnlyMove,
    /// Inside a folder the device keeps at an old name because it cannot
    /// hold Hearth's new one: left as it is with its folder (3.8; the
    /// folder's own listing covers it).
    InsideListed,
}

impl Listing {
    /// The reason's name in the scenario text and in observations.
    pub fn code(&self) -> String {
        match self {
            Listing::HearthRefused(r) => format!("name_refused:{}", r.code()),
            Listing::DeviceRefused(r) => format!("device_refused:{}", r.code()),
            Listing::NameClash => "name_clash".into(),
            Listing::EngineName => "engine_name".into(),
            Listing::ReadOnlyEdit => "read_only_edit".into(),
            Listing::ReadOnlyCreate => "read_only_create".into(),
            Listing::ReadOnlyMove => "read_only_move".into(),
            Listing::InsideListed => "inside_listed".into(),
        }
    }
}

/// Why a conflict copy was made (3.5.1, "They arise only in the cells
/// that say so").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CopyReason {
    /// A: two different contents for one file.
    BothEdited,
    /// The creation rows: two different files, or a file and a folder,
    /// wanting one name.
    NameTaken,
    /// A refused write to a read-only file.
    ReadOnly,
}

/// A conflict copy the outcome contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyMade {
    /// The copy's node (new for A and read-only copies; the renamed node
    /// itself for the creation rows).
    pub id: NodeId,
    /// Its content.
    pub hash: String,
    /// The name the copy's version wanted, before it stepped aside.
    pub original_name: String,
    /// Why.
    pub reason: CopyReason,
}

/// The settled outcome for one device and Hearth.
#[derive(Debug, Clone, Default)]
pub struct Settled {
    /// Hearth's tree afterwards. Names in NFC.
    pub server: IdTree,
    /// The device's tree afterwards, names in NFC (an observed device
    /// tree is compared in NFC, I8).
    pub device: IdTree,
    /// What is listed on the device and why, by node. A listed node is in
    /// only one of the two trees, or in both at different places (a local
    /// rename Hearth refused).
    pub listed: BTreeMap<NodeId, Listing>,
    /// Every conflict copy, in the trees above.
    pub copies: Vec<CopyMade>,
    /// Nodes whose local move was undone because Hearth's place won (C),
    /// for the activity list.
    pub moved_back: BTreeSet<NodeId>,
    /// Sets of nodes of which the device holds exactly one, the others
    /// listed as a name clash: "the first to arrive is synced" (3.8), and
    /// which arrives first is not fixed. `device` holds the lowest id;
    /// any other member is equally right.
    pub device_alternatives: Vec<BTreeSet<NodeId>>,
    /// The synced tree afterwards: what both sides hold at one place with
    /// one content, and for a listed item that was synced before, its
    /// synced state as it was, so that the next merge decides it the same
    /// way again (a refused item is never retried, sync-api 13).
    pub base: IdTree,
    /// The cells of the decision table this case reached ("file:edit/
    /// delete" and so on), for the coverage gate of 3.5 and 7.2.
    pub cells: BTreeSet<String>,
}

/// The synced tree after a merge (see [`Settled::base`]).
fn settled_base(out: &Settled, old_base: &IdTree) -> IdTree {
    let mut base = IdTree::new();
    for (id, s) in &out.server.nodes {
        if out.listed.contains_key(id) {
            continue;
        }
        if let Some(d) = out.device.get(*id)
            && d.parent == s.parent
            && d.name == s.name
            && d.kind.same_content(&s.kind)
        {
            base.nodes.insert(*id, s.clone());
        }
    }
    for id in out.listed.keys() {
        if let Some(old) = old_base.get(*id) {
            base.nodes.insert(*id, old.clone());
        }
    }
    base
}

/// An input the merge cannot read. These are faults of whoever built the
/// trees (the model's own runner, or an observation), never outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    /// One id is a file in one tree and a folder in another.
    KindChanged(NodeId),
    /// A node's parent is not a folder of the same tree, or the parents
    /// loop.
    BadParent(NodeId),
    /// The name clash stage did not finish within its bound: a fault of
    /// the model itself, reported instead of looping.
    NoProgress,
}

/// Every cell of the two tables, by the names in [`Settled::cells`]. The
/// coverage gate (sync.md 3.5 last paragraph) compares the cells a run
/// reached with this list.
pub fn all_cells() -> Vec<String> {
    use Change::*;
    let mut out = Vec::new();
    let present = [None, Edit, Move, Both, Delete];
    for l in present {
        for r in present {
            out.push(format!("file:{}/{}", l.code(), r.code()));
        }
    }
    for (l, r) in [(Create, Absent), (Absent, Create), (Create, Create)] {
        out.push(format!("file:{}/{}", l.code(), r.code()));
    }
    let folder = [None, Move, Delete];
    for l in folder {
        for r in folder {
            out.push(format!("folder:{}/{}", l.code(), r.code()));
        }
    }
    for (l, r) in [(Create, Absent), (Absent, Create), (Create, Create)] {
        out.push(format!("folder:{}/{}", l.code(), r.code()));
    }
    out
}

/// A decided node while the stages run: where it goes on each side and
/// what it holds.
#[derive(Debug, Clone)]
struct Decided {
    alive: bool,
    is_folder: bool,
    /// Where it sits on Hearth.
    server: Node,
    /// Where it sits on the device (differs only for refused names).
    device: Node,
    /// Which side the place came from, for "Hearth's place wins".
    place_from: Pick,
    /// The local change, for the permission rules.
    local: Change,
    /// On the device only (a refused create or a read-only copy).
    device_only: bool,
    /// On Hearth only (the device cannot hold or tell it apart).
    server_only: bool,
}

/// The settled outcome of `base`, `local` and `remote` for the device in
/// `ctx`. `next_id` is the first id free in all three trees; conflict
/// copies made by A are numbered from it.
pub fn reconcile(
    base: &IdTree,
    local: &IdTree,
    remote: &IdTree,
    ctx: &DeviceContext,
    next_id: NodeId,
) -> Result<Settled, ModelError> {
    check_input(base, local, remote)?;
    let mut out = Settled::default();
    let mut next_id = next_id;

    // Stages 1 and 2: classify and decide every node.
    let ids: BTreeSet<NodeId> = base
        .nodes
        .keys()
        .chain(local.nodes.keys())
        .chain(remote.nodes.keys())
        .copied()
        .collect();
    let mut decided: BTreeMap<NodeId, Decided> = BTreeMap::new();
    let mut a_conflicts: Vec<NodeId> = Vec::new();
    for id in &ids {
        let (b, l, r) = (base.get(*id), local.get(*id), remote.get(*id));
        let lc = classify(b, l, true);
        let rc = classify(b, r, false);
        let any = b
            .or(r)
            .or(l)
            .map(|n| n.kind.clone())
            .unwrap_or(Kind::Folder);
        let is_folder = any.is_folder();
        let table = if is_folder { "folder" } else { "file" };
        let found = if is_folder {
            folder_cell(lc, rc)
        } else {
            file_cell(lc, rc)
        };
        let Some(c) = found else {
            // Unreachable for well-formed input (stage 0 checked it);
            // kept as an error rather than a panic.
            return Err(ModelError::BadParent(*id));
        };
        out.cells
            .insert(format!("{table}:{}/{}", lc.code(), rc.code()));
        let pick = |p: Pick| -> Option<&Node> {
            match p {
                Pick::Base => b.or(r).or(l),
                Pick::Local => l.or(b).or(r),
                Pick::Remote => r.or(b).or(l),
            }
        };
        let place = pick(c.place).cloned().unwrap_or_else(|| Node {
            parent: ROOT,
            name: String::new(),
            kind: Kind::Folder,
        });
        let content = pick(c.content)
            .map(|n| n.kind.clone())
            .unwrap_or(Kind::Folder);
        let mut both_edited = c.both_edited;
        if both_edited
            && let (Some(ln), Some(rn)) = (l, r)
            && ln.kind.same_content(&rn.kind)
        {
            // A with equal hashes: "nothing moves".
            both_edited = false;
        }
        let node = Node {
            parent: place.parent,
            name: nfc(&place.name),
            kind: content,
        };
        decided.insert(
            *id,
            Decided {
                alive: c.survives,
                is_folder,
                server: node.clone(),
                device: node,
                place_from: c.place,
                local: lc,
                device_only: false,
                server_only: false,
            },
        );
        if both_edited {
            a_conflicts.push(*id);
        }
    }

    // Stage 3: the remote side's rights (3.5, "Permissions").
    let mut read_only_copies: Vec<NodeId> = Vec::new();
    for (id, d) in decided.iter_mut() {
        let source_perms = mount_perms(base, remote, *id, ctx);
        match d.local {
            // A local edit of a read-only file: "remote version restored
            // at the name; the edit kept as a conflict copy beside it".
            Change::Edit | Change::Both if source_perms & perms::UPDATE == 0 && d.alive => {
                a_conflicts.retain(|x| x != id);
                match remote.get(*id) {
                    Some(r) => {
                        d.server.kind = r.kind.clone();
                        d.device.kind = r.kind.clone();
                        read_only_copies.push(*id);
                    }
                    // Gone on Hearth (B in a read-only mount): it cannot
                    // be uploaded again, so it stays local, listed.
                    None => {
                        d.device_only = true;
                        out.listed.insert(*id, Listing::ReadOnlyEdit);
                    }
                }
            }
            // "local delete in a read-only folder -> the file is
            // downloaded again".
            Change::Delete if source_perms & perms::DELETE == 0 => {
                if let Some(r) = remote.get(*id) {
                    d.alive = true;
                    d.server = Node {
                        parent: r.parent,
                        name: r.name.clone(),
                        kind: r.kind.clone(),
                    };
                    d.device = d.server.clone();
                    d.place_from = Pick::Remote;
                }
            }
            _ => {}
        }
        // "local move out of or within a read-only folder -> undone
        // locally, listed"; a move into a folder the person may not
        // create in is refused the same way (5.2: the target needs 4).
        if d.local.moves() && d.place_from == Pick::Local {
            let target_perms = folder_perms(local, d.server.parent, ctx);
            if source_perms & perms::DELETE == 0 || target_perms & perms::CREATE == 0 {
                let back = remote.get(*id).or(base.get(*id));
                if let Some(back) = back {
                    d.server.parent = back.parent;
                    d.server.name = back.name.clone();
                    d.device.parent = back.parent;
                    d.device.name = back.name.clone();
                    d.place_from = Pick::Remote;
                    out.listed.insert(*id, Listing::ReadOnlyMove);
                }
            }
        }
        // "local create in a read-only folder -> stays local, listed".
        if d.local == Change::Create && d.place_from == Pick::Local {
            let target_perms = folder_perms(local, d.server.parent, ctx);
            if target_perms & perms::CREATE == 0 {
                d.device_only = true;
                out.listed.insert(*id, Listing::ReadOnlyCreate);
            }
        }
    }

    // Stage 4: a local name Hearth refuses stays local, listed (3.8). A
    // new item stays only on the device; a renamed one keeps Hearth's
    // place and content on Hearth and the local ones on the device.
    for (id, d) in decided.iter_mut() {
        if d.place_from != Pick::Local || !d.alive {
            continue;
        }
        let Some(l) = local.get(*id) else { continue };
        if let Some(why) = hearth_refuses_local(&l.name) {
            out.listed.insert(*id, Listing::HearthRefused(why));
            match remote.get(*id) {
                Some(r) => {
                    d.server = r.clone();
                    d.device = Node {
                        parent: l.parent,
                        name: nfc(&l.name),
                        kind: l.kind.clone(),
                    };
                }
                None => d.device_only = true,
            }
        }
    }
    // Whatever is inside a device-only folder is device-only too: there
    // is nowhere on Hearth to put it.
    propagate_flag(
        &mut decided,
        |d| d.device_only,
        |d| d.device_only = true,
        |d| &d.device,
    );

    // Stages 5 and 6, repeated until nothing changes: undo local moves in
    // cycles (Hearth's wins), then bring back every folder that holds a
    // survivor (F, G, and "a node moved into a folder that the other side
    // deleted ... the folder does" survive).
    loop {
        let mut changed = false;
        while let Some(cycle) = find_cycle(&decided) {
            let mut undone = false;
            for id in &cycle {
                if let Some(d) = decided.get_mut(id)
                    && d.place_from == Pick::Local
                    && let Some(back) = remote.get(*id).or(base.get(*id))
                {
                    d.server.parent = back.parent;
                    d.server.name = back.name.clone();
                    d.device.parent = back.parent;
                    d.device.name = back.name.clone();
                    d.place_from = Pick::Remote;
                    out.moved_back.insert(*id);
                    undone = true;
                }
            }
            if !undone {
                // A cycle with no local move in it would be a cycle in
                // Hearth's own tree, which check_input excludes.
                break;
            }
            changed = true;
        }
        let alive: Vec<NodeId> = decided
            .iter()
            .filter(|(_, d)| d.alive)
            .map(|(id, _)| *id)
            .collect();
        for id in alive {
            for parent in [
                decided.get(&id).map(|d| d.server.parent),
                decided.get(&id).map(|d| d.device.parent),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(p) = decided.get_mut(&parent)
                    && !p.alive
                {
                    p.alive = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    for (id, d) in &decided {
        let l = local.get(*id);
        if d.place_from == Pick::Remote
            && d.alive
            && l.is_some_and(|l| l.parent != d.server.parent || nfc(&l.name) != d.server.name)
            && classify(base.get(*id), l, true).moves()
        {
            out.moved_back.insert(*id);
        }
    }

    // The conflict copies of A and of refused read-only edits: a new
    // node holding the local content, beside the node, on both sides
    // (A: "the conflict copy is uploaded as a new file") or on the
    // device only (read-only: "not uploaded").
    let mut copies: BTreeMap<NodeId, Decided> = BTreeMap::new();
    for (id, reason) in a_conflicts
        .iter()
        .map(|id| (*id, CopyReason::BothEdited))
        .chain(
            read_only_copies
                .iter()
                .map(|id| (*id, CopyReason::ReadOnly)),
        )
    {
        let (Some(d), Some(l)) = (decided.get(&id), local.get(id)) else {
            continue;
        };
        if !d.alive {
            continue;
        }
        let copy_id = next_id;
        next_id += 1;
        let node = Node {
            parent: d.server.parent,
            name: String::new(),
            kind: l.kind.clone(),
        };
        copies.insert(
            copy_id,
            Decided {
                alive: true,
                is_folder: false,
                server: node.clone(),
                device: node,
                place_from: Pick::Local,
                local: Change::Create,
                device_only: reason == CopyReason::ReadOnly,
                server_only: false,
            },
        );
        out.copies.push(CopyMade {
            id: copy_id,
            hash: l.kind.hash().cloned().unwrap_or_default(),
            original_name: d.server.name.clone(),
            reason,
        });
        if reason == CopyReason::ReadOnly {
            out.listed.insert(copy_id, Listing::ReadOnlyEdit);
        }
    }
    // Name them in id order, each against the names already taken.
    for copy in out.copies.iter() {
        let Some(c) = copies.get(&copy.id).cloned() else {
            continue;
        };
        let stamp = Stamp::local(mtime(&c.server.kind), ctx.utc_offset_secs);
        let mut all = decided.clone();
        all.extend(copies.clone());
        let name = free_conflict_name(&all, c.server.parent, &copy.original_name, ctx, stamp);
        if let Some(c) = copies.get_mut(&copy.id) {
            c.server.name = name.clone();
            c.device.name = name;
        }
    }
    decided.extend(copies);

    // Stage 7: two nodes wanting one name on Hearth (3.5, "Creations").
    resolve_server_clashes(&mut decided, remote, ctx, &mut out)?;

    // Stage 8: what this device cannot hold (3.8).
    resolve_device_names(&mut decided, local, ctx, &mut out);

    for (id, d) in &decided {
        if !d.alive {
            continue;
        }
        if !d.device_only {
            out.server.nodes.insert(*id, d.server.clone());
        }
        if !d.server_only {
            out.device.nodes.insert(*id, d.device.clone());
        }
    }
    out.base = settled_base(&out, base);
    Ok(out)
}

/// Stage 0: the trees are well formed: one kind per id, parents are
/// folders of the same tree.
fn check_input(base: &IdTree, local: &IdTree, remote: &IdTree) -> Result<(), ModelError> {
    let trees = [base, local, remote];
    for t in trees {
        for (id, n) in &t.nodes {
            if n.parent != ROOT && !t.get(n.parent).is_some_and(|p| p.kind.is_folder()) {
                return Err(ModelError::BadParent(*id));
            }
            if t.path(*id).is_none() {
                return Err(ModelError::BadParent(*id));
            }
            for other in trees {
                if let Some(o) = other.get(*id)
                    && o.kind.is_folder() != n.kind.is_folder()
                {
                    return Err(ModelError::KindChanged(*id));
                }
            }
        }
    }
    Ok(())
}

/// The rights of the mount a node is in on Hearth's side (its base or
/// remote place): the nearest mount root above it.
fn mount_perms(base: &IdTree, remote: &IdTree, id: NodeId, ctx: &DeviceContext) -> u8 {
    let tree = if remote.has(id) { remote } else { base };
    match tree.get(id) {
        Some(n) => folder_perms(tree, n.parent, ctx),
        None => perms::ALL,
    }
}

/// The rights inside `folder` of `tree`: the nearest mount root at or
/// above it, or every right (the home mount).
fn folder_perms(tree: &IdTree, folder: NodeId, ctx: &DeviceContext) -> u8 {
    let mut at = folder;
    for _ in 0..=tree.nodes.len() {
        if let Some(m) = ctx.mounts.iter().find(|m| m.root == at) {
            return m.perms;
        }
        if at == ROOT {
            break;
        }
        match tree.get(at) {
            Some(n) => at = n.parent,
            None => break,
        }
    }
    perms::ALL
}

/// Sets a flag on everything below a flagged node, following the given
/// side's parents, until nothing changes.
fn propagate_flag(
    decided: &mut BTreeMap<NodeId, Decided>,
    has: impl Fn(&Decided) -> bool,
    set: impl Fn(&mut Decided),
    side: impl Fn(&Decided) -> &Node,
) {
    loop {
        let flagged: BTreeSet<NodeId> = decided
            .iter()
            .filter(|(_, d)| has(d))
            .map(|(id, _)| *id)
            .collect();
        let mut changed = false;
        for d in decided.values_mut() {
            if !has(d) && flagged.contains(&side(d).parent) {
                set(d);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// A set of live nodes whose server parents loop, if there is one.
fn find_cycle(decided: &BTreeMap<NodeId, Decided>) -> Option<Vec<NodeId>> {
    for start in decided.keys() {
        let mut seen: Vec<NodeId> = Vec::new();
        let mut at = *start;
        while at != ROOT {
            if let Some(pos) = seen.iter().position(|x| *x == at) {
                return Some(seen[pos..].to_vec());
            }
            seen.push(at);
            match decided.get(&at) {
                Some(d) => at = d.server.parent,
                None => break,
            }
        }
    }
    None
}

fn mtime(kind: &Kind) -> i64 {
    match kind {
        Kind::File { mtime, .. } => *mtime,
        Kind::Folder => 0,
    }
}

/// The first conflict copy name for `original` in `parent` that no live
/// node holds there, on Hearth exactly or on the device by key ("If the
/// name is taken, ` 2`, ` 3` and so on go before the closing bracket").
fn free_conflict_name(
    decided: &BTreeMap<NodeId, Decided>,
    parent: NodeId,
    original: &str,
    ctx: &DeviceContext,
    stamp: Stamp,
) -> String {
    let taken: BTreeSet<String> = decided
        .values()
        .filter(|d| d.alive)
        .flat_map(|d| [&d.server, &d.device])
        .filter(|n| n.parent == parent && !n.name.is_empty())
        .map(|n| name_key(ctx.volume, &n.name))
        .collect();
    let mut n = 1;
    loop {
        let candidate = conflict_name(original, &ctx.device, stamp, n, HEARTH_NAME_BYTES_MAX);
        if !taken.contains(&name_key(ctx.volume, &candidate)) {
            return candidate;
        }
        n += 1;
    }
}

/// Stage 7: in each folder of Hearth's tree, two live nodes with one name
/// (same bytes, sync-api 13) are resolved by the creation rows of 3.5:
///
/// - folder and folder: one folder, contents merged (and then merged by
///   the same rules, which the loop does);
/// - file and folder: the folder keeps the name, the file becomes a
///   conflict copy;
/// - file and file, equal hash: adopted as one node;
/// - file and file, different: "remote keeps the name", the other
///   becomes a conflict copy.
///
/// "Remote" is the node Hearth already holds at that name; a node placed
/// there by this device is the one that steps aside.
fn resolve_server_clashes(
    decided: &mut BTreeMap<NodeId, Decided>,
    remote: &IdTree,
    ctx: &DeviceContext,
    out: &mut Settled,
) -> Result<(), ModelError> {
    // Each pass removes a node or renames one to a free name, so the
    // loop ends; the bound is a guard against a model bug, not a rule.
    for _ in 0..=(decided.len() * 4 + 8) {
        let mut groups: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
        for (id, d) in decided.iter() {
            if d.alive && !d.device_only {
                groups
                    .entry((d.server.parent, d.server.name.clone()))
                    .or_default()
                    .push(*id);
            }
        }
        let Some(((parent, wanted), group)) = groups.into_iter().find(|(_, g)| g.len() > 1) else {
            return Ok(());
        };
        let on_hearth = |id: &NodeId| {
            remote
                .get(*id)
                .is_some_and(|r| r.parent == parent && r.name == wanted)
        };
        let folders: Vec<NodeId> = group
            .iter()
            .copied()
            .filter(|id| decided[id].is_folder)
            .collect();
        let files: Vec<NodeId> = group
            .iter()
            .copied()
            .filter(|id| !decided[id].is_folder)
            .collect();
        if folders.len() > 1 {
            // folder and folder: one folder. Hearth's keeps its id.
            let keeper = folders
                .iter()
                .copied()
                .find(on_hearth)
                .unwrap_or(folders[0]);
            for gone in folders.iter().copied().filter(|f| *f != keeper) {
                for d in decided.values_mut() {
                    if d.server.parent == gone {
                        d.server.parent = keeper;
                    }
                    if d.device.parent == gone {
                        d.device.parent = keeper;
                    }
                }
                if let Some(d) = decided.get_mut(&gone) {
                    d.alive = false;
                }
            }
            continue;
        }
        if folders.len() == 1 {
            // file and folder: the folder keeps the name.
            for f in files {
                rename_to_copy(decided, f, &wanted, ctx, out);
            }
            continue;
        }
        // file and file.
        let keeper = files.iter().copied().find(on_hearth);
        let Some(keeper) = keeper else {
            // No node of Hearth's: two of the device's own files with
            // one name on Hearth (an NFC and an NFD spelling on a volume
            // that keeps both). Not in the table; the model's reading
            // (docs/contract-questions.md Q4): the first by id keeps the
            // name, the others stay local, listed as a name clash.
            for f in files.iter().skip(1) {
                if let Some(d) = decided.get_mut(f) {
                    d.device_only = true;
                }
                out.listed.insert(*f, Listing::NameClash);
            }
            continue;
        };
        let keeper_kind = decided[&keeper].server.kind.clone();
        for f in files.into_iter().filter(|f| *f != keeper) {
            if decided[&f].server.kind.same_content(&keeper_kind) {
                // equal hash: adopt as one node, no transfer.
                if let Some(d) = decided.get_mut(&f) {
                    d.alive = false;
                }
            } else {
                rename_to_copy(decided, f, &wanted, ctx, out);
            }
        }
    }
    Err(ModelError::NoProgress)
}

/// Renames node `id` (which wanted `wanted`) to a conflict copy name on
/// both sides and records the copy.
fn rename_to_copy(
    decided: &mut BTreeMap<NodeId, Decided>,
    id: NodeId,
    wanted: &str,
    ctx: &DeviceContext,
    out: &mut Settled,
) {
    let Some(d) = decided.get(&id) else { return };
    let stamp = Stamp::local(mtime(&d.server.kind), ctx.utc_offset_secs);
    let parent = d.server.parent;
    let name = free_conflict_name(decided, parent, wanted, ctx, stamp);
    if let Some(d) = decided.get_mut(&id) {
        d.server.name = name.clone();
        d.device.name = name;
        out.copies.push(CopyMade {
            id,
            hash: d.server.kind.hash().cloned().unwrap_or_default(),
            original_name: wanted.to_string(),
            reason: CopyReason::NameTaken,
        });
    }
}

/// Stage 8: the device's side of names (3.8). Hearth's entries that the
/// device cannot hold, or that it would hold under the same name as
/// another item, stay on Hearth and are listed; so does an entry with the
/// engine's own name at the root.
fn resolve_device_names(
    decided: &mut BTreeMap<NodeId, Decided>,
    local: &IdTree,
    ctx: &DeviceContext,
    out: &mut Settled,
) {
    for (id, d) in decided.iter_mut() {
        if !d.alive || d.device_only {
            continue;
        }
        let why = if is_engine_name(&d.device.name, d.device.parent == ROOT) {
            Listing::EngineName
        } else if let Some(why) = device_refuses(ctx.volume, &d.device.name) {
            Listing::DeviceRefused(why)
        } else {
            continue;
        };
        match local.get(*id) {
            // Hearth renamed a file this device holds to a name it cannot
            // hold: the local file is left untouched where it is (I10).
            Some(l) => {
                d.device = Node {
                    parent: l.parent,
                    name: nfc(&l.name),
                    kind: l.kind.clone(),
                };
            }
            None => d.server_only = true,
        }
        out.listed.insert(*id, why);
    }
    // A folder kept at its old local name keeps what is in it as it is
    // locally: the device does not sync into a folder it stopped syncing.
    let kept: Vec<NodeId> = decided
        .iter()
        .filter(|(id, d)| {
            d.is_folder && out.listed.contains_key(*id) && local.has(**id) && !d.server_only
        })
        .map(|(id, _)| *id)
        .collect();
    for folder in kept {
        for id in local.subtree(folder).into_iter().filter(|id| *id != folder) {
            if let (Some(d), Some(l)) = (decided.get_mut(&id), local.get(id)) {
                d.alive = true;
                d.device = Node {
                    parent: l.parent,
                    name: nfc(&l.name),
                    kind: l.kind.clone(),
                };
                out.listed.entry(id).or_insert(Listing::InsideListed);
            }
        }
        for (id, d) in decided.iter_mut() {
            if !local.has(*id)
                && d.server.parent != ROOT
                && local.subtree(folder).contains(&d.server.parent)
            {
                d.server_only = true;
                out.listed.entry(*id).or_insert(Listing::InsideListed);
            }
        }
    }
    propagate_flag(
        decided,
        |d| d.server_only,
        |d| d.server_only = true,
        |d| &d.server,
    );
    // Name clashes by the volume's key. The item already on the device
    // keeps its name ("the first to arrive is synced"); among items none
    // of which is here yet, any one may arrive first.
    loop {
        let mut groups: BTreeMap<(NodeId, String), Vec<NodeId>> = BTreeMap::new();
        for (id, d) in decided.iter() {
            if d.alive && !d.server_only {
                groups
                    .entry((d.device.parent, name_key(ctx.volume, &d.device.name)))
                    .or_default()
                    .push(*id);
            }
        }
        let Some((_, group)) = groups.into_iter().find(|(_, g)| g.len() > 1) else {
            break;
        };
        let here: Vec<NodeId> = group.iter().copied().filter(|id| local.has(*id)).collect();
        let keeper = here.first().copied().unwrap_or(group[0]);
        if here.is_empty() {
            out.device_alternatives
                .push(group.iter().copied().collect());
        }
        for id in group.into_iter().filter(|id| *id != keeper) {
            if let Some(d) = decided.get_mut(&id) {
                d.server_only = true;
            }
            out.listed.insert(id, Listing::NameClash);
        }
        propagate_flag(
            decided,
            |d| d.server_only,
            |d| d.server_only = true,
            |d| &d.server,
        );
    }
}
