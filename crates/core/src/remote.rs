//! Hearth's sync API as the core sees it: the `Remote` trait, one method
//! per request of the contract (docs/sync-api.md, version 1, at the commit
//! `CONTRACT` pins).
//!
//! The HTTP implementation is `hearth-sync-remote`; the simulator's is a
//! simulated Hearth built from the contract alone. Both give the core the
//! same typed answers, so nothing here knows about headers or JSON.
//!
//! Why the write methods take condition types rather than options: every
//! write of the contract must carry a condition (8.1), and here a write
//! without one cannot be written down. `FileCondition` has no "none"
//! variant; a create always sends `If-None-Match: *`; a folder move always
//! sends its `ifSeq`; a folder delete always sends its cursor. I3 is
//! enforced by the compiler before any test runs.

use std::fmt;

use crate::ids::{Cursor, Etag, Hash, Kind, MountId, RemoteId};

/// The answer to `GET /sync/hello` (sync-api.md 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The instance's id; also every answer's `X-Hearth-Instance`.
    pub instance_id: String,
    /// The lowest contract version spoken.
    pub api_min: u32,
    /// The highest contract version spoken.
    pub api_max: u32,
    /// Named features beyond the bare version.
    pub features: Vec<String>,
    /// Sizes and times the client must keep to.
    pub limits: Limits,
}

impl Hello {
    /// Whether a named feature is offered.
    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

/// The server's limits (sync-api.md 2): the client has none of its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most entries a page may hold.
    pub page_max: u32,
    /// Longest wait in seconds.
    pub wait_max: u32,
    /// Largest body of one request in bytes.
    pub body_max: u64,
    /// Smallest chunk (but the last).
    pub chunk_min: u64,
    /// Largest chunk.
    pub chunk_max: u64,
    /// Most chunks in one session.
    pub chunks_max: u64,
    /// Seconds an idle upload session lives.
    pub upload_ttl: u64,
    /// Longest name in UTF-8 bytes.
    pub name_bytes_max: usize,
    /// Most hashes in one hash check.
    pub hashes_max: usize,
    /// Days removal markers are kept.
    pub gone_days: u32,
}

/// One mount as `GET /sync/mounts` lists it (sync-api.md 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountInfo {
    /// The mount's id.
    pub id: MountId,
    /// `home`, `share`, ...
    pub kind: String,
    /// The storage key.
    pub storage: String,
    /// The storage's restore epoch.
    pub epoch: String,
    /// The root entry's id.
    pub root: RemoteId,
    /// `folder`, or `file` for a share of one file.
    pub root_type: String,
    /// Where it sits in the person's tree, `/`-separated, `""` for home.
    pub path: String,
    /// Rights bits.
    pub perms: u32,
    /// Deletes go to the trash.
    pub trash: bool,
    /// Replaced content becomes a version.
    pub versions: bool,
    /// `always` or `ask`.
    pub prompt: String,
}

/// The whole answer of `GET /sync/mounts`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mounts {
    /// Changes whenever anything but quota changes.
    pub tag: String,
    /// Every mount.
    pub mounts: Vec<MountInfo>,
}

/// An entry (sync-api.md 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its id.
    pub id: RemoteId,
    /// Its folder; none for a mount's root.
    pub parent: Option<RemoteId>,
    /// NFC, exact case.
    pub name: String,
    /// File or folder; never changes for an id.
    pub kind: Kind,
    /// Bytes; none for folders.
    pub size: Option<u64>,
    /// Microseconds.
    pub mtime_us: i64,
    /// None for folders.
    pub etag: Option<Etag>,
    /// None for folders and not-yet-hashed files.
    pub hash: Option<Hash>,
    /// Execute bit.
    pub exec: bool,
    /// Narrower rights on this entry (version 1 always none).
    pub perms: Option<u32>,
    /// `crypt` on the top of an encrypted folder: none, `rest`, `e2ee`.
    pub crypt: Option<String>,
    /// Sequence number of the last change.
    pub seq: u64,
    /// `list: true` on a folder moved or restored into view (6.3).
    pub list: bool,
}

/// Why an entry left a mount's view (sync-api.md 6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoneReason {
    /// In Hearth's trash; may come back with the same id.
    Trashed,
    /// Removed for good.
    Deleted,
    /// Moved out of this mount's view; may be in another mount.
    Moved,
}

/// One change of the feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// An entry in its present state.
    Upsert(Entry),
    /// A removal marker.
    Gone {
        /// The entry's id.
        id: RemoteId,
        /// The marker's number.
        seq: u64,
        /// Why.
        reason: GoneReason,
    },
}

/// The answer of `GET /sync/changes` (sync-api.md 6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesPage {
    /// The storage's epoch now.
    pub epoch: String,
    /// The changes, in ascending `seq`.
    pub changes: Vec<Change>,
    /// Where to continue.
    pub cursor: Cursor,
    /// More is waiting now.
    pub more: bool,
    /// The person's current mounts tag.
    pub mounts_tag: String,
}

/// The answer of `GET /sync/tree` (sync-api.md 6.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreePage {
    /// The storage's epoch now.
    pub epoch: String,
    /// Entries in no promised order.
    pub entries: Vec<Entry>,
    /// The next page; none on the last.
    pub page: Option<String>,
    /// The cursor as it stood at the first page.
    pub cursor: Cursor,
    /// The person's current mounts tag.
    pub mounts_tag: String,
}

/// A lock in the way of a write (sync-api.md 6.5, 14.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lock {
    /// `webdav`, `office` or `checkout`.
    pub kind: String,
    /// Who holds it.
    pub by: String,
    /// Until when (microseconds), if known.
    pub until_us: Option<i64>,
}

/// The answer of `GET /sync/files/{id}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryInfo {
    /// The mount it is in for this person.
    pub mount: MountId,
    /// Its present state.
    pub entry: Entry,
    /// A lock on it.
    pub lock: Option<Lock>,
}

/// The answer of `POST /sync/wait` (sync-api.md 6.7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitAnswer {
    /// Mounts that may have something new.
    pub changed: Vec<MountId>,
    /// The mounts tag differs from the one sent.
    pub mounts_changed: bool,
    /// The current tag.
    pub mounts_tag: String,
}

/// Bytes read from a file (sync-api.md 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadAnswer {
    /// The bytes of the range asked for (or of the whole file).
    pub bytes: Vec<u8>,
    /// The whole file's size.
    pub total: u64,
    /// The whole file's hash, if known.
    pub hash: Option<Hash>,
}

/// The condition of a write to a file (sync-api.md 8.1). There is no way
/// to say "no condition".
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FileCondition {
    /// `If-Match` / `ifMatch`.
    IfMatch(Etag),
    /// `X-Hearth-If-Hash` / `ifHash`.
    IfHash(Hash),
}

/// The condition of a move (sync-api.md 9.5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MoveCondition {
    /// A file moves on its etag or hash.
    File(FileCondition),
    /// A folder moves on its own `seq`.
    Folder(u64),
}

/// The body of a move or rename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRequest {
    /// The folder it should be in.
    pub parent: RemoteId,
    /// The name it should have.
    pub name: String,
    /// Its condition.
    pub condition: MoveCondition,
}

/// What an upload session writes to (sync-api.md 10.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadTarget {
    /// A new file; `ifNoneMatch: "*"` is always sent.
    Create {
        /// Its folder.
        parent: RemoteId,
        /// Its name.
        name: String,
    },
    /// New content for an existing file, on its condition.
    Replace {
        /// The file.
        file: RemoteId,
        /// Its condition.
        condition: FileCondition,
    },
}

/// The body of `POST /sync/uploads`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadStart {
    /// The session id, derived from the operation's id.
    pub id: String,
    /// What it writes.
    pub target: UploadTarget,
    /// The whole size.
    pub size: u64,
    /// The whole file's hash.
    pub hash: Hash,
    /// The chunk size.
    pub chunk_size: u64,
    /// Modification time, microseconds.
    pub mtime_us: i64,
    /// The execute bit.
    pub exec: bool,
}

/// A session's state (sync-api.md 10.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadState {
    /// Accepting chunks.
    Open,
    /// A finish is running.
    Finishing,
    /// Done; the entry it made.
    Finished(Entry),
}

/// A session as `start` and `status` describe it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadSession {
    /// The session id.
    pub id: String,
    /// Its state.
    pub state: UploadState,
    /// The numbers of the chunks stored, ascending.
    pub chunks: Vec<u64>,
}

/// The answer of a finish (sync-api.md 10.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishAnswer {
    /// 200 or 201 with the entry.
    Done(Entry),
    /// 202: still joining; ask again after this many seconds.
    Finishing(u64),
}

/// One answer line of the hash check (sync-api.md 12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HaveHash {
    /// The hash.
    pub hash: Hash,
    /// A file in the mount that has it now.
    pub id: RemoteId,
    /// Its size.
    pub size: u64,
}

/// The stable error codes of sync-api.md 14, as the client acts on them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// 400 `bad_request`.
    BadRequest,
    /// 400 `sync_api_required` or `sync_api_unsupported`.
    SyncApi,
    /// 400 `device_required`.
    DeviceRequired,
    /// 401 `unauthorized`.
    Unauthorized,
    /// 401 `device_revoked`.
    DeviceRevoked,
    /// 403 `forbidden`.
    Forbidden,
    /// 403 `client_refused`.
    ClientRefused,
    /// 403 `account_disabled`.
    AccountDisabled,
    /// 403 `no_permission`.
    NoPermission,
    /// 403 `mount_root`.
    MountRoot,
    /// 404 `not_found`.
    NotFound,
    /// 404 `upload_gone`.
    UploadGone,
    /// 409 `exists`.
    Exists,
    /// 409 `is_folder`.
    IsFolder,
    /// 409 `not_folder`.
    NotFolder,
    /// 409 `into_itself`.
    IntoItself,
    /// 409 `cross_storage`.
    CrossStorage,
    /// 409 `hash_unknown`.
    HashUnknown,
    /// 409 `crypt_boundary` or `crypt_counter`.
    Crypt,
    /// 409 `upload_mismatch`.
    UploadMismatch,
    /// 409 `upload_incomplete`.
    UploadIncomplete,
    /// 409 `upload_finished`.
    UploadFinished,
    /// 410 `cursor_expired` (also 400 `cursor_invalid`, which the client
    /// treats as expiry with reason `invalid`).
    CursorExpired,
    /// 400 `page_invalid`.
    PageInvalid,
    /// 411 `length_required`.
    LengthRequired,
    /// 412 `precondition`.
    Precondition,
    /// 413 `too_large`.
    TooLarge,
    /// 416 `range`.
    Range,
    /// 422 `invalid`.
    Invalid,
    /// 422 `name_refused`.
    NameRefused,
    /// 422 `hash_mismatch`.
    HashMismatch,
    /// 422 `chunk_range` or `chunk_size`.
    Chunk,
    /// 422 `infected`.
    Infected,
    /// 423 `locked`.
    Locked,
    /// 428 `precondition_required`.
    PreconditionRequired,
    /// 429 `throttled`.
    Throttled,
    /// 500 `error` (and 502, 504 from a proxy).
    ServerError,
    /// 503 `maintenance`, `draining`, `unavailable`, `scan_unavailable`.
    Unavailable,
    /// 507 `quota`.
    Quota,
    /// A code this version does not know, with its status (1.8: acted on
    /// by status class, never as success).
    Unknown(String),
}

impl ErrorCode {
    /// From the contract's word.
    pub fn parse(code: &str, status: u16) -> ErrorCode {
        match code {
            "bad_request" => ErrorCode::BadRequest,
            "sync_api_required" | "sync_api_unsupported" => ErrorCode::SyncApi,
            "device_required" => ErrorCode::DeviceRequired,
            "unauthorized" => ErrorCode::Unauthorized,
            "device_revoked" => ErrorCode::DeviceRevoked,
            "forbidden" => ErrorCode::Forbidden,
            "client_refused" => ErrorCode::ClientRefused,
            "account_disabled" => ErrorCode::AccountDisabled,
            "no_permission" => ErrorCode::NoPermission,
            "mount_root" => ErrorCode::MountRoot,
            "not_found" => ErrorCode::NotFound,
            "upload_gone" => ErrorCode::UploadGone,
            "exists" => ErrorCode::Exists,
            "is_folder" => ErrorCode::IsFolder,
            "not_folder" => ErrorCode::NotFolder,
            "into_itself" => ErrorCode::IntoItself,
            "cross_storage" => ErrorCode::CrossStorage,
            "hash_unknown" => ErrorCode::HashUnknown,
            "crypt_boundary" | "crypt_counter" => ErrorCode::Crypt,
            "upload_mismatch" => ErrorCode::UploadMismatch,
            "upload_incomplete" => ErrorCode::UploadIncomplete,
            "upload_finished" => ErrorCode::UploadFinished,
            "cursor_expired" | "cursor_invalid" => ErrorCode::CursorExpired,
            "page_invalid" => ErrorCode::PageInvalid,
            "length_required" => ErrorCode::LengthRequired,
            "precondition" => ErrorCode::Precondition,
            "too_large" => ErrorCode::TooLarge,
            "range" => ErrorCode::Range,
            "invalid" => ErrorCode::Invalid,
            "name_refused" => ErrorCode::NameRefused,
            "hash_mismatch" => ErrorCode::HashMismatch,
            "chunk_range" | "chunk_size" => ErrorCode::Chunk,
            "infected" => ErrorCode::Infected,
            "locked" => ErrorCode::Locked,
            "precondition_required" => ErrorCode::PreconditionRequired,
            "throttled" => ErrorCode::Throttled,
            "error" => ErrorCode::ServerError,
            "maintenance" | "draining" | "unavailable" | "scan_unavailable" => {
                ErrorCode::Unavailable
            }
            "quota" => ErrorCode::Quota,
            _ if status == 502 || status == 504 => ErrorCode::ServerError,
            other => ErrorCode::Unknown(other.to_owned()),
        }
    }
}

/// The facts a refusal carries (sync-api.md 14, `details`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// The entry as it is now (412, 409 `exists`, `hash_unknown`...).
    pub entry: Option<Entry>,
    /// Which condition failed (412).
    pub failed: Option<String>,
    /// `cursor_expired`'s reason, or `name_refused`'s.
    pub reason: Option<String>,
    /// The lock in the way (423).
    pub lock: Option<Lock>,
    /// Missing chunk numbers (409 `upload_incomplete`).
    pub missing: Vec<u64>,
}

/// A refusal from Hearth (sync-api.md 1.8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    /// The HTTP status.
    pub status: u16,
    /// The code the client acts on.
    pub code: ErrorCode,
    /// One sentence for a person.
    pub message: String,
    /// Machine-readable facts.
    pub details: Details,
    /// `Retry-After` in seconds, where given.
    pub retry_after_s: Option<u64>,
}

/// What can go wrong with a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteError {
    /// No answer, or a cut one: the request may or may not have happened
    /// (sync-api.md 14.5). Treated as a lost answer: repeat or ask.
    Lost(String),
    /// Hearth answered with a refusal (boxed: it carries an entry, and
    /// errors travel through every request's result).
    Api(Box<ApiError>),
    /// An answer carried another `X-Hearth-Instance` than the one the
    /// engine connected to (1.9): stop and ask, before trusting anything.
    InstanceChanged(String),
}

impl RemoteError {
    /// The refusal's code, when Hearth answered.
    pub fn code(&self) -> Option<&ErrorCode> {
        match self {
            RemoteError::Api(e) => Some(&e.code),
            _ => None,
        }
    }

    /// The present entry a refusal carried.
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            RemoteError::Api(e) => e.details.entry.as_ref(),
            _ => None,
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Lost(s) => write!(f, "no answer: {s}"),
            RemoteError::Api(e) => write!(f, "{} {:?}: {}", e.status, e.code, e.message),
            RemoteError::InstanceChanged(id) => write!(f, "another Hearth instance: {id}"),
        }
    }
}

impl std::error::Error for RemoteError {}

/// Shorthand for request results.
pub type RemoteResult<T> = Result<T, RemoteError>;

/// Hearth's sync API. Implementations must compare `X-Hearth-Instance` on
/// every answer with the id of the latest `hello` and answer
/// `RemoteError::InstanceChanged` before giving any body (1.9); must send
/// `X-Hearth-Sync-Api: 1` on every request but `hello`; and must never
/// retry a write on their own (the engine decides, from 8.5).
pub trait Remote {
    /// `GET /sync/hello`.
    fn hello(&mut self) -> RemoteResult<Hello>;
    /// `GET /sync/mounts`.
    fn mounts(&mut self) -> RemoteResult<Mounts>;
    /// `GET /sync/changes`.
    fn changes(
        &mut self,
        mount: &MountId,
        cursor: &Cursor,
        limit: u32,
    ) -> RemoteResult<ChangesPage>;
    /// `GET /sync/tree`.
    fn tree(
        &mut self,
        mount: &MountId,
        root: Option<&RemoteId>,
        page: Option<&str>,
        limit: u32,
    ) -> RemoteResult<TreePage>;
    /// `GET /sync/files/{id}`.
    fn entry(&mut self, id: &RemoteId) -> RemoteResult<EntryInfo>;
    /// `POST /sync/wait`.
    fn wait(
        &mut self,
        cursors: &[(MountId, Cursor)],
        mounts_tag: &str,
        wait_s: u32,
    ) -> RemoteResult<WaitAnswer>;
    /// `GET /sync/files/{id}/content` with `If-Match` and one range
    /// (`offset`, `len`; the whole file when `len` covers it).
    fn read(
        &mut self,
        id: &RemoteId,
        etag: &Etag,
        offset: u64,
        len: u64,
    ) -> RemoteResult<ReadAnswer>;
    /// `PUT /sync/files?parent=&name=` with `If-None-Match: *`.
    fn create_file(
        &mut self,
        parent: &RemoteId,
        name: &str,
        body: &[u8],
        hash: Hash,
        mtime_us: i64,
        exec: bool,
    ) -> RemoteResult<Entry>;
    /// `PUT /sync/files/{id}/content` on a condition.
    fn replace_content(
        &mut self,
        id: &RemoteId,
        condition: &FileCondition,
        body: &[u8],
        hash: Hash,
        mtime_us: i64,
    ) -> RemoteResult<Entry>;
    /// `POST /sync/folders` (create only: its own condition).
    fn make_folder(&mut self, parent: &RemoteId, name: &str) -> RemoteResult<Entry>;
    /// `POST /sync/files/{id}/move`.
    fn move_entry(&mut self, id: &RemoteId, req: &MoveRequest) -> RemoteResult<Entry>;
    /// `POST /sync/files/{id}/copy` on the source's hash.
    fn copy(
        &mut self,
        id: &RemoteId,
        parent: &RemoteId,
        name: &str,
        if_hash: Hash,
    ) -> RemoteResult<Entry>;
    /// `DELETE /sync/files/{id}` of a file, on its condition; answers the
    /// marker's reason.
    fn delete_file(&mut self, id: &RemoteId, condition: &FileCondition)
    -> RemoteResult<GoneReason>;
    /// `DELETE /sync/files/{id}` of a folder, on "unchanged since".
    fn delete_folder(
        &mut self,
        id: &RemoteId,
        unchanged_since: &Cursor,
    ) -> RemoteResult<GoneReason>;
    /// `POST /sync/uploads`.
    fn upload_start(&mut self, start: &UploadStart) -> RemoteResult<UploadSession>;
    /// `PUT /sync/uploads/{id}/{n}`.
    fn upload_chunk(&mut self, id: &str, n: u64, body: &[u8], hash: Hash) -> RemoteResult<()>;
    /// `GET /sync/uploads/{id}`.
    fn upload_status(&mut self, id: &str) -> RemoteResult<UploadSession>;
    /// `POST /sync/uploads/{id}/finish`.
    fn upload_finish(&mut self, id: &str) -> RemoteResult<FinishAnswer>;
    /// `DELETE /sync/uploads/{id}`.
    fn upload_abort(&mut self, id: &str) -> RemoteResult<()>;
    /// `POST /sync/hashes/have`.
    fn hashes_have(&mut self, mount: &MountId, hashes: &[Hash]) -> RemoteResult<Vec<HaveHash>>;
}
