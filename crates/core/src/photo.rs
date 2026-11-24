//! The phone photo uploader's needs from the core, as types only
//! (docs/sync.md 6.5, read with sync-api.md 11). The uploader itself is
//! built with the phone bindings (step 6); these shapes are fixed now so
//! the store, the bindings and Photos' server work can be planned against
//! them.
//!
//! Why apart from folder sync: photo backup is one way and append-only.
//! Nothing on Hearth ever changes or deletes a photo on the phone, so there
//! is no merge and no conflict; the hard part is completeness (never miss
//! an asset, never upload one twice), which comes from reconciling the
//! library's full list against this table, not from notifications.

use crate::ids::{Hash, RemoteId};

/// What kind of library item an asset is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AssetKind {
    /// A still image.
    Photo,
    /// A video.
    Video,
    /// A still with its short video.
    LivePhoto,
}

/// The role of one file of an asset; an asset is done only when every part
/// is on Hearth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PartRole {
    /// The original still.
    Still,
    /// The original video.
    Video,
    /// The video half of a Live Photo.
    LiveVideo,
    /// An edited rendition beside the original.
    Edited,
    /// A camera's sidecar file.
    Sidecar,
}

/// Why an asset waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// The host's constraint: Wi-Fi only.
    NoWifi,
    /// The host's constraint: charging only.
    NotCharging,
    /// Hearth said the quota is full.
    Quota,
    /// Hearth refused it, with its code.
    Refused(String),
}

/// Where an asset is (6.5's states).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetState {
    /// In the library, not yet looked at.
    Found,
    /// Its original bytes are being hashed.
    Hashing,
    /// Hearth is being asked whether it holds the hashes (Photos' check).
    Checking,
    /// Parts are being sent.
    Uploading,
    /// Every part is durably stored on Hearth (8.3's "stored").
    Done,
    /// Waiting, with the reason.
    Waiting(WaitReason),
    /// Refused repeatedly: shown, never blocking the others.
    Failed(String),
}

/// One library item (the `asset` table of 6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    /// The library's identifier (PhotoKit's local id, MediaStore's id).
    pub local_id: String,
    /// What it is.
    pub kind: AssetKind,
    /// Capture time, microseconds.
    pub created_us: i64,
    /// The library's modification time, microseconds.
    pub modified_us: i64,
    /// Where it is.
    pub state: AssetState,
    /// Tries so far.
    pub attempts: u32,
    /// Not before (ns).
    pub next_try_ns: Option<i64>,
}

/// One file of an asset (the `asset_part` table of 6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetPart {
    /// The asset.
    pub local_id: String,
    /// Its role.
    pub role: PartRole,
    /// The original bytes' hash, once known (may stay empty for an asset
    /// Photos' check already knows: sync-api.md 11.3 point 2).
    pub hash: Option<Hash>,
    /// Bytes.
    pub size: u64,
    /// The upload session or resumable upload, when one is open.
    pub upload_id: Option<String>,
    /// Durably stored on Hearth.
    pub done: bool,
    /// The entry Hearth made for it.
    pub remote_id: Option<RemoteId>,
}

/// What the host allows now; passed with every call (6.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraints {
    /// Only on Wi-Fi.
    pub wifi_only: bool,
    /// Only while charging.
    pub charging_only: bool,
    /// Videos at all.
    pub videos: bool,
    /// Parts above this size wait for Wi-Fi.
    pub large_on_wifi_only: Option<u64>,
    /// The time this call may take, in milliseconds (a background task of
    /// thirty seconds hashes and queues what it can and stops cleanly).
    pub budget_ms: u64,
}

/// A transfer the engine describes and the host performs (the OS's
/// background uploader): sync-api.md 11.3 point 3.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTransfer {
    /// The HTTP method.
    pub method: String,
    /// The address under Hearth's origin.
    pub address: String,
    /// The request headers.
    pub headers: Vec<(String, String)>,
    /// The host's handle on the local file.
    pub file: String,
    /// The expected hash, when known.
    pub expect_hash: Option<Hash>,
}

/// What the host reports back about a transfer it performed: the status
/// and the four headers the engine verifies and stores (`ETag`,
/// `X-Hearth-File-Id`, `X-Hearth-Hash`, `X-Hearth-Seq`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostTransferResult {
    /// The HTTP status.
    pub status: u16,
    /// The response headers.
    pub headers: Vec<(String, String)>,
}
