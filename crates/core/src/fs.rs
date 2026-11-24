//! The file system as the core sees it: the `Fs` trait.
//!
//! The real implementation is `hearth-sync-fs` (one per operating system);
//! the simulator's is a fake disk with faults. The core does nothing to a
//! disk except through these calls, so a simulated run exercises exactly
//! the code a real one does.
//!
//! Why these calls and no others:
//!
//! - Paths are `LocalPath`s under the sync root, never strings: no code
//!   here joins or splits separators.
//! - There is no "delete a file" call for synced content. A file leaves a
//!   synced place only by being renamed into the holding area (I1);
//!   `remove_temp` exists for the engine's own temp files (bytes that also
//!   exist on Hearth), and `remove_held` for holding-area entries whose
//!   bytes are proven to exist elsewhere, and nothing else.
//! - Replacing a file is `exchange`: the old file ends up under the temp
//!   name, so the engine examines what it replaced after the fact with no
//!   gap (docs/sync.md 3.7, I2). Creating is `rename_noreplace`, which
//!   fails loudly if the name was taken meanwhile.
//! - `list_dir` returns metadata with names in one call, as the scanner's
//!   fast paths do on every OS (3.3).

use std::fmt;

use crate::ids::FileId;
use crate::names::{LocalName, LocalPath};

/// What a directory entry is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A folder.
    Folder,
    /// A symbolic link (not followed, not synced: 3.9), with its target as
    /// text for the problem list.
    Symlink(String),
    /// A socket, FIFO, device, junction or mount point (skipped: 3.9).
    Special,
}

/// Metadata of one item, as one `stat` (or one directory entry) shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// What it is.
    pub kind: EntryKind,
    /// Its identity.
    pub file_id: FileId,
    /// Bytes (0 for folders).
    pub size: u64,
    /// Modification time, nanoseconds since 1970.
    pub mtime_ns: i64,
    /// Status change time, nanoseconds (0 where the OS has none).
    pub ctime_ns: i64,
    /// The execute bit (Unix; false on Windows).
    pub exec: bool,
}

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The name as the disk holds it.
    pub name: LocalName,
    /// Its metadata.
    pub meta: Meta,
}

/// What went wrong on the disk. The engine decides from the kind; the text
/// is for logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    /// Nothing at that path.
    NotFound,
    /// Something is already at the target.
    Exists,
    /// A folder that is not empty.
    NotEmpty,
    /// A path component is not a folder.
    NotAFolder,
    /// Refused by the system's permissions.
    PermissionDenied,
    /// The volume is full.
    NoSpace,
    /// The file is open elsewhere in a way that blocks this (Windows).
    InUse,
    /// The sync root itself is not there or not the same volume (3.11).
    RootUnavailable,
    /// The volume cannot hold this name.
    NameNotHoldable,
    /// Any other failure, with the system's words.
    Io(String),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for FsError {}

/// Shorthand for disk results.
pub type FsResult<T> = Result<T, FsError>;

/// The disk under the sync root.
pub trait Fs {
    /// The id of the volume the sync root is on.
    fn volume(&mut self) -> FsResult<u64>;
    /// The content of `.hearthsync/root`, the root marker (3.11): none if
    /// the root is there but the marker is not; `RootUnavailable` if the
    /// root itself is missing.
    fn read_marker(&mut self) -> FsResult<Option<String>>;
    /// Writes the root marker durably (set-up only).
    fn write_marker(&mut self, marker: &str) -> FsResult<()>;
    /// Every entry of a folder, with metadata, in no promised order.
    fn list_dir(&mut self, dir: &LocalPath) -> FsResult<Vec<DirEntry>>;
    /// The metadata at a path, or none if nothing is there. Symbolic links
    /// are reported as links, never followed.
    fn stat(&mut self, path: &LocalPath) -> FsResult<Option<Meta>>;
    /// Up to `len` bytes from `offset` (fewer only at the end of the file).
    fn read(&mut self, path: &LocalPath, offset: u64, len: u64) -> FsResult<Vec<u8>>;
    /// Creates an empty file; fails with `Exists` if something is there.
    fn create(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Writes bytes at an offset of an existing file.
    fn write_at(&mut self, path: &LocalPath, offset: u64, data: &[u8]) -> FsResult<()>;
    /// Cuts or extends a file to a length.
    fn truncate(&mut self, path: &LocalPath, len: u64) -> FsResult<()>;
    /// Sets the modification time and the execute bit.
    fn set_meta(&mut self, path: &LocalPath, mtime_ns: i64, exec: bool) -> FsResult<()>;
    /// Flushes a file's content and metadata to stable storage (fsync,
    /// F_FULLFSYNC, FlushFileBuffers).
    fn sync_file(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Flushes a folder's entries (fsync of the directory where the OS has
    /// it; nothing to do on Windows).
    fn sync_dir(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Renames, failing with `Exists` if the target is taken (in any case
    /// or normalisation the volume treats as the same name).
    fn rename_noreplace(&mut self, from: &LocalPath, to: &LocalPath) -> FsResult<()>;
    /// Swaps two existing items atomically (RENAME_EXCHANGE, RENAME_SWAP,
    /// ReplaceFileW with a backup name).
    fn exchange(&mut self, a: &LocalPath, b: &LocalPath) -> FsResult<()>;
    /// Makes a folder; fails with `Exists` if something is there.
    fn mkdir(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Removes an empty folder; fails with `NotEmpty` otherwise.
    fn remove_empty_dir(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Removes one of the engine's own temp files. Never called on a path
    /// outside `.hearthsync/tmp` or a `.hearthtmp` name.
    fn remove_temp(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Empties one entry of the holding area after its days, once its
    /// bytes are proven to exist elsewhere (3.7). Never called on a path
    /// outside `.hearthsync/held`.
    fn remove_held(&mut self, path: &LocalPath) -> FsResult<()>;
    /// Free bytes on the sync root's volume.
    fn free_space(&mut self) -> FsResult<u64>;
}

/// The engine's own folders under the sync root.
pub mod layout {
    use crate::ids::OpId;
    use crate::names::{ENGINE_DIR, LocalName, LocalPath};

    /// `.hearthsync`.
    pub fn engine_dir() -> LocalPath {
        LocalPath::from_strs(&[ENGINE_DIR])
    }

    /// `.hearthsync/tmp`.
    pub fn tmp_dir() -> LocalPath {
        LocalPath::from_strs(&[ENGINE_DIR, "tmp"])
    }

    /// `.hearthsync/held`.
    pub fn held_root() -> LocalPath {
        LocalPath::from_strs(&[ENGINE_DIR, "held"])
    }

    /// `.hearthsync/tmp/<op id>`: a download's temp file, named by its
    /// operation so recovery finds it.
    pub fn temp_for(op: OpId) -> LocalPath {
        tmp_dir().join(&LocalName(format!("{op}").into_bytes()))
    }

    /// A temp name beside a target, used for a case-only rename's middle
    /// step: `.<op id>.hearthtmp` in the same folder.
    pub fn beside(folder: &LocalPath, op: OpId) -> LocalPath {
        folder.join(&LocalName(
            format!(".{op}{}", crate::names::TEMP_SUFFIX).into_bytes(),
        ))
    }
}
