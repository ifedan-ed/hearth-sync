//! Identifiers and small value types shared by every part of the core.
//!
//! Each kind of id is its own type, so a remote entry id can never be
//! passed where a node id is meant (the compiler refuses). Ids that come
//! from Hearth are opaque strings (sync-api.md 1.4): compared for equality,
//! stored and sent back, never parsed or ordered for meaning. Ids the
//! engine makes itself are integers from counters kept in the store, never
//! reused within one database.

use std::fmt;

use sha2::{Digest, Sha256};

/// The engine's own id for one file or folder it has linked: the key that
/// ties the remote, local and synced pictures of an item together.
///
/// Why a separate id instead of Hearth's or the disk's: an item may exist
/// on one side only (a new local file has no Hearth id yet; a new remote
/// file has no local file id), and both outside ids can change (a file
/// crossing to external storage gets a new Hearth id; an editor's save
/// gives a new inode). The node is what stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "n{}", self.0)
    }
}

/// The id of one operation in the journal (the `op` table). The temp file
/// name of a download and the upload session id are derived from it, so a
/// retry after a crash finds its own leftovers (docs/sync.md 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op{}", self.0)
    }
}

/// The id of one entry of the holding area ("Removed by sync").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HeldId(pub u64);

/// The id of one recorded conflict copy (for I9's count and the client's
/// list of open conflicts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConflictId(pub u64);

macro_rules! opaque_string_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub String);

        impl $name {
            /// The id as Hearth sent it.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

opaque_string_id!(
    /// Hearth's id of an entry (sync-api.md 1.4): stays with the entry
    /// across renames and moves inside the primary store.
    RemoteId
);
opaque_string_id!(
    /// Hearth's id of a mount (sync-api.md 4), for example `home` or
    /// `share:17`.
    MountId
);
opaque_string_id!(
    /// A file's etag (sync-api.md 1.7): one committed state of its content
    /// and place. Folders have none.
    Etag
);
opaque_string_id!(
    /// A feed cursor (sync-api.md 6.3): opaque, at most 512 characters.
    Cursor
);

/// A local file's identity on its volume (docs/sync.md 3.4): device and
/// inode on Linux and macOS, the volume serial and the 128-bit file id on
/// Windows. Two different files never share one while both exist; a
/// number may be reused after a delete (ext4), which is why a match by
/// file id alone never decides a move (3.4 rule 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId {
    /// The volume (device) the file is on.
    pub volume: u64,
    /// The file's number on that volume.
    pub id: u128,
}

/// File or folder. Hearth never changes the kind of an id (sync-api.md 5);
/// on disk a name can change kind, which the scanner turns into a new
/// node (docs/sync.md 3.5, "retype").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A regular file with content.
    File,
    /// A folder.
    Folder,
}

/// A SHA-256 content hash: the one hash of the contract (sync-api.md 1.5).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash(pub [u8; 32]);

/// Why a hash string from Hearth was not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashParseError(pub String);

impl Hash {
    /// The hash of a byte string in memory.
    pub fn of(bytes: &[u8]) -> Hash {
        let mut h = ContentHasher::new();
        h.update(bytes);
        h.finish()
    }

    /// Reads the contract's written form, `sha256:` and 64 lower-case hex
    /// digits. Anything else is refused: a hash that is not exactly in
    /// this form must never be trusted as one (the server answers 422 to
    /// the same thing, sync-api.md 1.5).
    pub fn parse(s: &str) -> Result<Hash, HashParseError> {
        let hex = s
            .strip_prefix("sha256:")
            .ok_or_else(|| HashParseError(s.to_owned()))?;
        if hex.len() != 64 {
            return Err(HashParseError(s.to_owned()));
        }
        let mut out = [0u8; 32];
        for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
            let hi = hex_value(pair[0]).ok_or_else(|| HashParseError(s.to_owned()))?;
            let lo = hex_value(pair[1]).ok_or_else(|| HashParseError(s.to_owned()))?;
            out[i] = (hi << 4) | lo;
        }
        Ok(Hash(out))
    }

    /// The 64 lower-case hex digits without the prefix.
    pub fn hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in self.0 {
            s.push(DIGITS[(b >> 4) as usize] as char);
            s.push(DIGITS[(b & 15) as usize] as char);
        }
        s
    }
}

/// Only lower-case digits are valid in the contract's form.
fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Short form keeps test failures readable; the full form is Display.
        write!(f, "sha256:{}..", &self.hex()[..12])
    }
}

/// A streaming SHA-256, so a file of any size is hashed in pieces as it is
/// read (scan, download verification, upload's second observation).
#[derive(Clone, Default)]
pub struct ContentHasher(Sha256);

impl ContentHasher {
    /// A hasher with nothing fed yet.
    pub fn new() -> Self {
        ContentHasher(Sha256::new())
    }

    /// Feeds the next bytes of the content.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The hash of everything fed.
    pub fn finish(self) -> Hash {
        Hash(self.0.finalize().into())
    }
}

/// Rights bits of a mount (sync-api.md 4 and 5.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Perms(pub u32);

impl Perms {
    /// May read.
    pub const READ: u32 = 1;
    /// May replace a file's content, mtime or exec bit.
    pub const UPDATE: u32 = 2;
    /// May create, and be the target of a move or copy.
    pub const CREATE: u32 = 4;
    /// May delete, be the source of a move, be replaced by a move.
    pub const DELETE: u32 = 8;
    /// May share (not used by sync).
    pub const SHARE: u32 = 16;

    /// Whether every bit of `bits` is granted.
    pub fn allows(self, bits: u32) -> bool {
        self.0 & bits == bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_round_trips_through_the_contract_form() {
        let h = Hash::of(b"test");
        assert_eq!(
            h.to_string(),
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
        assert_eq!(Hash::parse(&h.to_string()), Ok(h));
    }

    #[test]
    fn hash_parse_refuses_every_other_form() {
        let good = Hash::of(b"x").to_string();
        assert!(Hash::parse(&good.to_uppercase()).is_err());
        assert!(Hash::parse(&good.replace("sha256:", "sha1:")).is_err());
        assert!(Hash::parse(&good[..good.len() - 1]).is_err());
        assert!(Hash::parse(&format!("{good}0")).is_err());
        assert!(Hash::parse("").is_err());
        assert!(Hash::parse("sha256:").is_err());
    }

    #[test]
    fn streaming_hash_equals_one_shot() {
        let mut h = ContentHasher::new();
        h.update(b"te");
        h.update(b"st");
        assert_eq!(h.finish(), Hash::of(b"test"));
    }

    #[test]
    fn perms_bits() {
        let p = Perms(Perms::READ | Perms::DELETE);
        assert!(p.allows(Perms::READ));
        assert!(p.allows(Perms::READ | Perms::DELETE));
        assert!(!p.allows(Perms::UPDATE));
    }
}
