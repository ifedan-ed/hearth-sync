//! The rows of the state database (docs/sync.md 3.2), as plain values.
//!
//! The three pictures of an item are three rows keyed by its node: what
//! Hearth last reported (`RemoteRow`), what the disk last showed
//! (`LocalRow`), and the base both sides last agreed on (`SyncedRow`).
//! Rows hold no path strings: a path is the chain of parents, so a rename
//! of a folder with a hundred thousand files changes one row.
//!
//! These types are shared by both store implementations and by the core;
//! they carry no behaviour beyond small accessors, so the store crate can
//! map them to columns one to one.

use crate::ids::{
    ConflictId, Cursor, Etag, FileId, Hash, HeldId, Kind, MountId, NodeId, OpId, Perms, RemoteId,
};
use crate::names::{LocalName, LocalPath, NameKey};

/// The kind of mount (sync-api.md 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MountKind {
    /// The person's own files.
    Home,
    /// A share they received.
    Share,
    /// A team folder.
    Team,
    /// A site library.
    Library,
    /// External storage (a delete there is final; renames may look like
    /// delete plus create).
    External,
    /// A share from another server.
    Remote,
}

impl MountKind {
    /// The contract's word for it.
    pub fn as_str(self) -> &'static str {
        match self {
            MountKind::Home => "home",
            MountKind::Share => "share",
            MountKind::Team => "team",
            MountKind::Library => "library",
            MountKind::External => "external",
            MountKind::Remote => "remote",
        }
    }

    /// From the contract's word; none for a word this version does not
    /// know (the engine then leaves the mount alone and lists it).
    pub fn parse(s: &str) -> Option<MountKind> {
        Some(match s {
            "home" => MountKind::Home,
            "share" => MountKind::Share,
            "team" => MountKind::Team,
            "library" => MountKind::Library,
            "external" => MountKind::External,
            "remote" => MountKind::Remote,
            _ => return None,
        })
    }
}

/// Why a mount is being listed from the start (sync-api.md 6.4, 6.8). The
/// reason decides what an entry the engine holds but the listing lacks
/// means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ListReason {
    /// The first sync of the mount: nothing is held yet.
    First,
    /// `cursor_expired` with reason `retention`: a missing entry was
    /// removed on Hearth while the device was away.
    Retention,
    /// The mounts list changed so that entries may have passed between
    /// mounts of one storage (sync-api.md 4): an entry the listing lacks
    /// may have moved to another mount, so it is held as departed and
    /// resolved by contract 6.6 rule 7.
    Relist,
    /// `epoch`, `ahead` or `invalid`, a database without state, or a new
    /// instance the person confirmed: Hearth's past cannot be trusted, so
    /// a missing entry means nothing and the base is forgotten; local
    /// files are uploaded again, never deleted (sync-api.md 6.8).
    Untrusted,
}

/// A whole-mount listing in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// Why it runs.
    pub reason: ListReason,
    /// The page to ask for next; none for the first page.
    pub page: Option<String>,
    /// The cursor the first page gave (the same on every page).
    pub cursor: Option<Cursor>,
    /// The mark put on every remote row this listing delivers, so rows it
    /// did not deliver can be found at the end.
    pub generation: u64,
}

/// One mount the person has (sync-api.md 4), as the engine keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRow {
    /// Hearth's id of the mount.
    pub id: MountId,
    /// Home, share, team folder...
    pub kind: MountKind,
    /// The storage key; sequence numbers are per storage.
    pub storage: String,
    /// The storage's restore epoch.
    pub epoch: String,
    /// The id of the entry at the mount's top.
    pub root_remote: RemoteId,
    /// The node of that entry.
    pub root_node: NodeId,
    /// Where the mount sits under the sync root (empty for home).
    pub local_path: Vec<String>,
    /// The person's rights in the mount.
    pub perms: Perms,
    /// Deletes go to Hearth's trash.
    pub trash: bool,
    /// Replaced content is kept as a version.
    pub versions: bool,
    /// Hearth says to ask before syncing (external and remote mounts).
    pub prompt_ask: bool,
    /// The person chose to sync it (true for `always` mounts).
    pub selected: bool,
    /// The cursor durably applied; none before the first full listing.
    pub cursor: Option<Cursor>,
    /// A whole-mount listing in progress, if any.
    pub listing: Option<Listing>,
    /// The last feed answer said `more: false` (contract 6.6 rule 7).
    pub caught_up: bool,
    /// The feed round counter at the last request that answered
    /// `more: false`; departed entries are resolved against it.
    pub caught_up_round: u64,
}

/// The remote picture of one entry: what Hearth last reported (sync-api.md
/// 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRow {
    /// The node it belongs to.
    pub node: NodeId,
    /// Hearth's id.
    pub id: RemoteId,
    /// The mount it was delivered through.
    pub mount: MountId,
    /// The holding folder's id; none for a mount's root.
    pub parent: Option<RemoteId>,
    /// The name as stored: NFC, exact case.
    pub name: String,
    /// File or folder.
    pub kind: Kind,
    /// Bytes, for files.
    pub size: u64,
    /// Modification time in microseconds (people and other tools only).
    pub mtime_us: i64,
    /// The etag; none for folders.
    pub etag: Option<Etag>,
    /// The content hash; none for folders and for files Hearth has not
    /// hashed yet.
    pub hash: Option<Hash>,
    /// The execute bit.
    pub exec: bool,
    /// The sequence number of the entry's last change.
    pub seq: u64,
    /// The top of an encrypted folder: none, at rest, or end to end.
    pub crypt: Crypt,
    /// Set when a `gone` with reason `moved` arrived: the round of the
    /// feed at that moment. Resolved by contract 6.6 rule 7.
    pub departed: Option<u64>,
    /// The listing generation that last delivered it (0: none).
    pub listed: u64,
}

/// Encryption of a folder's subtree (sync-api.md 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Crypt {
    /// Not the top of an encrypted folder.
    None,
    /// Encrypted at rest: an ordinary folder for sync.
    Rest,
    /// End to end: not synced by this version (listed once).
    E2ee,
}

/// The local picture of one item: what the last scan or the engine's own
/// last write saw on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRow {
    /// The node it belongs to.
    pub node: NodeId,
    /// The holding folder's node; none for the sync root itself.
    pub parent: Option<NodeId>,
    /// The name as the disk holds it.
    pub name: LocalName,
    /// Its comparison form on this volume; unique per folder.
    pub key: NameKey,
    /// File or folder.
    pub kind: Kind,
    /// The file system's identity of it.
    pub file_id: FileId,
    /// Bytes.
    pub size: u64,
    /// Modification time, nanoseconds.
    pub mtime_ns: i64,
    /// Status change time, nanoseconds (0 where the OS has none).
    pub ctime_ns: i64,
    /// The execute bit.
    pub exec: bool,
    /// The content hash, once hashed at rest; none until then.
    pub hash: Option<Hash>,
}

impl LocalRow {
    /// Whether the metadata the scanner compares (docs/sync.md 3.3) equals
    /// another observation's.
    pub fn same_metadata(&self, m: &crate::fs::Meta) -> bool {
        self.file_id == m.file_id
            && self.size == m.size
            && self.mtime_ns == m.mtime_ns
            && self.ctime_ns == m.ctime_ns
            && self.exec == m.exec
    }
}

/// The base: the last state both sides were known to agree on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncedRow {
    /// The node it belongs to.
    pub node: NodeId,
    /// The holding folder's node then.
    pub parent: Option<NodeId>,
    /// The name then, NFC.
    pub name: String,
    /// File or folder.
    pub kind: Kind,
    /// Bytes.
    pub size: u64,
    /// The content then.
    pub hash: Option<Hash>,
    /// Hearth's etag then (used to see a change of a file whose hash
    /// Hearth does not know yet).
    pub remote_etag: Option<Etag>,
    /// The local file's identity then.
    pub local_file_id: FileId,
    /// The local modification time then.
    pub local_mtime_ns: i64,
    /// The execute bit then.
    pub exec: bool,
}

/// What an operation does. Each is one small state machine
/// (docs/sync.md 3.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpKind {
    /// Hearth's content to the disk, replacing or creating a file.
    Download,
    /// Make a folder on disk.
    MkdirLocal,
    /// Local content to an existing Hearth file.
    Upload,
    /// A local file to a new Hearth file (or a server-side copy).
    UploadNew,
    /// Make a folder on Hearth.
    MkdirRemote,
    /// Move or rename on disk, to where Hearth has the item.
    MoveLocal,
    /// Move or rename on Hearth, to where the disk has the item.
    MoveRemote,
    /// Take a file off the disk, into the holding area.
    DeleteLocal,
    /// Take an empty folder off the disk.
    RemoveLocalFolder,
    /// Delete a file on Hearth.
    DeleteRemote,
    /// Delete a folder on Hearth in one request.
    DeleteRemoteFolder,
    /// Rename a local item to a conflict copy's name.
    ConflictAsideLocal,
    /// Rename a Hearth file to a conflict copy's name.
    ConflictAsideRemote,
}

impl OpKind {
    /// Every kind, for the store's mapping and the tests.
    pub const ALL: [OpKind; 13] = [
        OpKind::Download,
        OpKind::MkdirLocal,
        OpKind::Upload,
        OpKind::UploadNew,
        OpKind::MkdirRemote,
        OpKind::MoveLocal,
        OpKind::MoveRemote,
        OpKind::DeleteLocal,
        OpKind::RemoveLocalFolder,
        OpKind::DeleteRemote,
        OpKind::DeleteRemoteFolder,
        OpKind::ConflictAsideLocal,
        OpKind::ConflictAsideRemote,
    ];

    /// A stable small number for storage.
    pub fn code(self) -> u8 {
        OpKind::ALL.iter().position(|k| *k == self).unwrap_or(0) as u8
    }

    /// The inverse of `code`.
    pub fn from_code(c: u8) -> Option<OpKind> {
        OpKind::ALL.get(c as usize).copied()
    }
}

/// The durable point an operation has reached. Only the points recovery
/// needs to tell apart are kept (docs/sync.md 3.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OpState {
    /// Written, nothing done yet.
    Planned,
    /// Bytes are moving (download ranges or upload chunks).
    Transferring,
    /// A download's temp file is whole and verified and flushed.
    Verified,
    /// A download is about to swap or rename into place.
    Swapping,
    /// The swap happened; the old file is under the temp name.
    Swapped,
    /// An upload's finish was sent and answered 202 (Hearth is joining).
    Finishing,
}

impl OpState {
    /// Every state, for storage.
    pub const ALL: [OpState; 6] = [
        OpState::Planned,
        OpState::Transferring,
        OpState::Verified,
        OpState::Swapping,
        OpState::Swapped,
        OpState::Finishing,
    ];

    /// A stable small number for storage.
    pub fn code(self) -> u8 {
        OpState::ALL.iter().position(|k| *k == self).unwrap_or(0) as u8
    }

    /// The inverse of `code`.
    pub fn from_code(c: u8) -> Option<OpState> {
        OpState::ALL.get(c as usize).copied()
    }
}

/// The condition a remote write carries (sync-api.md 8.1). There is no
/// variant for "none": a write without a condition cannot be expressed.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Condition {
    /// The file's etag is exactly this.
    IfMatch(Etag),
    /// The file's stored hash is exactly this.
    IfHash(Hash),
    /// Nothing has this name in the parent (creates).
    IfNoneMatch,
    /// The folder's own sequence number is exactly this (folder moves).
    IfSeq(u64),
    /// Nothing at or below the folder changed after this cursor (folder
    /// deletes).
    UnchangedSince(Cursor),
}

/// One operation of the journal (`op` table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRow {
    /// Its id.
    pub id: OpId,
    /// What it does.
    pub kind: OpKind,
    /// The node it acts on.
    pub node: NodeId,
    /// The durable point reached.
    pub state: OpState,
    /// How many times it was tried.
    pub attempt: u32,
    /// Not before this time (ns), after a failure.
    pub next_try_ns: Option<i64>,
    /// The condition its remote write carries, when it writes to Hearth.
    pub condition: Option<Condition>,
    /// The folder it moves or creates into (a node), when it does.
    pub target_parent: Option<NodeId>,
    /// The name it gives, when it names something.
    pub target_name: Option<String>,
    /// The content it expects (the hash it downloads or uploads).
    pub expect_hash: Option<Hash>,
    /// When it was written (ns).
    pub created_ns: i64,
}

/// Resumable state of a transfer (`transfer` table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransferRow {
    /// The operation it belongs to.
    pub op: OpId,
    /// The temp file of a download, under the sync root.
    pub temp: Option<LocalPath>,
    /// The upload session id.
    pub upload_id: Option<String>,
    /// The whole size.
    pub total: u64,
    /// Chunk or range size.
    pub chunk_size: u64,
    /// Bytes known to be written in order from the start (downloads), or
    /// the number of chunks sent (uploads; Hearth's status is trusted over
    /// it after a restart).
    pub done: u64,
    /// The etag being read or replaced.
    pub expect_etag: Option<Etag>,
    /// The temp file's file id once written, to see whether a swap
    /// happened before a crash.
    pub new_file_id: Option<FileId>,
    /// Where the old file is being put (the holding area or a conflict
    /// name), written before the rename so recovery can find it.
    pub aside: Option<LocalPath>,
}

/// What kind of problem keeps an item from syncing (I10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProblemKind {
    /// The device cannot hold Hearth's name.
    NameNotHoldable,
    /// Hearth refuses the local name.
    NameRefused,
    /// Two names are one name on this device.
    NameClash,
    /// The mount does not allow the change.
    ReadOnly,
    /// A lock stands in the way.
    Locked,
    /// Over quota.
    Quota,
    /// The virus scanner refused it.
    Infected,
    /// The bytes did not match their hash twice.
    HashMismatch,
    /// It keeps changing while being read.
    StillChanging,
    /// A symbolic link: not synced in this version.
    Symlink,
    /// A socket, device or other special file.
    Special,
    /// Inside an end-to-end encrypted folder, not supported yet.
    E2eeUnsupported,
    /// A mount's root folder is missing on disk.
    MountRootMissing,
    /// A mount disappeared; its local folder is left alone.
    MountGone,
    /// A folder kept because ignored files remain in it.
    KeptForIgnored,
    /// A local item where the person chose not to sync.
    NotSelected,
    /// The engine found no way forward (a bug; listed so it is seen).
    Stuck,
    /// Hearth refused for another reason (its code is in the text).
    Refused,
}

impl ProblemKind {
    /// Every kind, for storage.
    pub const ALL: [ProblemKind; 18] = [
        ProblemKind::NameNotHoldable,
        ProblemKind::NameRefused,
        ProblemKind::NameClash,
        ProblemKind::ReadOnly,
        ProblemKind::Locked,
        ProblemKind::Quota,
        ProblemKind::Infected,
        ProblemKind::HashMismatch,
        ProblemKind::StillChanging,
        ProblemKind::Symlink,
        ProblemKind::Special,
        ProblemKind::E2eeUnsupported,
        ProblemKind::MountRootMissing,
        ProblemKind::MountGone,
        ProblemKind::KeptForIgnored,
        ProblemKind::NotSelected,
        ProblemKind::Stuck,
        ProblemKind::Refused,
    ];

    /// A stable small number for storage.
    pub fn code(self) -> u8 {
        ProblemKind::ALL
            .iter()
            .position(|k| *k == self)
            .unwrap_or(0) as u8
    }

    /// The inverse of `code`.
    pub fn from_code(c: u8) -> Option<ProblemKind> {
        ProblemKind::ALL.get(c as usize).copied()
    }
}

/// What a problem is about: a node, or (for items the engine keeps no node
/// for, such as a name that is not UTF-8) a path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Subject {
    /// A linked item.
    Node(NodeId),
    /// An item with no node, by its path.
    Path(LocalPath),
}

/// One row of "needs attention" (`problem` table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemRow {
    /// What it is about.
    pub subject: Subject,
    /// The kind.
    pub kind: ProblemKind,
    /// Words for people (and Hearth's code where it refused).
    pub reason: String,
    /// When first seen (ns).
    pub first_ns: i64,
    /// When last seen (ns).
    pub last_ns: i64,
    /// How often seen.
    pub count: u32,
}

/// Why something is in the holding area.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HeldReason {
    /// Deleted on Hearth, so taken off the disk.
    RemoteDelete,
    /// Replaced by a newer version from Hearth.
    Replaced,
    /// The person chose not to sync its folder any more.
    Unselected,
    /// Taken off the disk for another reason the engine records.
    Retired,
}

/// Why a held entry may be emptied after its days (docs/sync.md 3.7: only
/// entries whose bytes Hearth still has, or that were replaced by equal
/// content).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PurgeBasis {
    /// Deleted on a mount with a trash: the bytes went to Hearth's trash.
    RemoteTrash,
    /// Replaced on a mount with versions: the bytes are a version.
    RemoteVersions,
    /// The same bytes are a current file on Hearth (unselected folders).
    RemoteFile,
    /// Replaced by identical content.
    IdenticalReplacement,
    /// No proof: kept until a hash check against Hearth finds the bytes.
    Unproven,
}

/// One entry of the holding area (`held` table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldRow {
    /// Its id.
    pub id: HeldId,
    /// Where it was (display form for the client's restore list).
    pub original: LocalPath,
    /// Where it is now, under `.hearthsync/held/<date>/`.
    pub held: LocalPath,
    /// Its content.
    pub hash: Option<Hash>,
    /// Bytes.
    pub size: u64,
    /// Why it was taken.
    pub reason: HeldReason,
    /// Why it may be emptied.
    pub basis: PurgeBasis,
    /// The mount it came from (for the hash check before emptying).
    pub mount: MountId,
    /// When (ns).
    pub at_ns: i64,
}

/// One conflict copy the engine made (I9 counts these).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRow {
    /// Its id.
    pub id: ConflictId,
    /// The node whose name both versions wanted.
    pub node: NodeId,
    /// The node the losing version became.
    pub copy: NodeId,
    /// The losing version's content.
    pub hash: Option<Hash>,
    /// The copy's name.
    pub name: String,
    /// When (ns).
    pub at_ns: i64,
}

/// Everything the store holds about one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRecord {
    /// The node.
    pub node: NodeId,
    /// The mount it belongs to.
    pub mount: MountId,
    /// Hearth's picture.
    pub remote: Option<RemoteRow>,
    /// The disk's picture.
    pub local: Option<LocalRow>,
    /// The base.
    pub synced: Option<SyncedRow>,
}
