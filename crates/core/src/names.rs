//! Names: how a name on disk relates to a name on Hearth, which names each
//! side refuses, and how conflict copies are named (docs/sync.md 3.8 and
//! 3.5.1, sync-api.md 13).
//!
//! Why this is one module: every decision about names in the engine goes
//! through these few functions, so the corpus of nasty names (7.7) tests
//! all of them in one place, and no other code compares names by hand.
//!
//! The rules in one paragraph. Hearth stores names in NFC and compares them
//! byte for byte. A disk stores whatever bytes it was given (or, on HFS+, a
//! decomposed form) and may compare without regard to case or
//! normalisation. So the local tree keeps the bytes as found on disk and a
//! comparison key (`NameKey`): NFC, plus simple case folding on a
//! case-insensitive volume. Two local names with one key are one name on
//! that volume. A name is written to disk in NFC; a local name found in
//! another normalisation is uploaded under its NFC form and never renamed
//! locally.

use std::fmt;

use unicode_normalization::{UnicodeNormalization, is_nfc};

/// A name as the disk holds it: raw bytes. On Linux any bytes but `/` and
/// NUL; on macOS UTF-8 (perhaps decomposed); on Windows the file system
/// crate encodes the UTF-16 name as WTF-8, so an unpaired surrogate still
/// has a byte form here. Not every local name is valid UTF-8, and such a
/// name cannot be synced (it is listed, never a crash).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalName(pub Vec<u8>);

impl LocalName {
    /// A local name from text (the engine writes names in NFC).
    pub fn from_str_nfc(s: &str) -> LocalName {
        LocalName(to_nfc(s).into_bytes())
    }

    /// The name as text when it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// A readable form for logs and the problem list: invalid bytes shown
    /// as the replacement character.
    pub fn display(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

impl fmt::Debug for LocalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.display())
    }
}

/// A path under the sync root, as name components. Never a string with
/// separators: the engine never builds or parses a path string, so a name
/// holding a separator character cannot be misread as two components.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalPath(pub Vec<LocalName>);

impl LocalPath {
    /// The sync root itself.
    pub fn root() -> LocalPath {
        LocalPath(Vec::new())
    }

    /// This path with one more component.
    pub fn join(&self, name: &LocalName) -> LocalPath {
        let mut v = self.0.clone();
        v.push(name.clone());
        LocalPath(v)
    }

    /// The folder holding this path, or none for the root.
    pub fn parent(&self) -> Option<LocalPath> {
        if self.0.is_empty() {
            None
        } else {
            Some(LocalPath(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    /// The last component, or none for the root.
    pub fn file_name(&self) -> Option<&LocalName> {
        self.0.last()
    }

    /// A path from text components (tests, the engine's own folders).
    pub fn from_strs(parts: &[&str]) -> LocalPath {
        LocalPath(
            parts
                .iter()
                .map(|p| LocalName(p.as_bytes().to_vec()))
                .collect(),
        )
    }

    /// A readable form with `/` between components, for people and logs
    /// only (never parsed back).
    pub fn display(&self) -> String {
        self.0
            .iter()
            .map(LocalName::display)
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The bytes of this path for storage: components joined by NUL, the
    /// one byte no name on any supported system can contain.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, n) in self.0.iter().enumerate() {
            if i > 0 {
                out.push(0);
            }
            out.extend_from_slice(&n.0);
        }
        out
    }

    /// The inverse of `encode`.
    pub fn decode(bytes: &[u8]) -> LocalPath {
        if bytes.is_empty() {
            return LocalPath::root();
        }
        LocalPath(
            bytes
                .split(|b| *b == 0)
                .map(|p| LocalName(p.to_vec()))
                .collect(),
        )
    }
}

impl fmt::Debug for LocalPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.display())
    }
}

/// The comparison form of a name on this volume (docs/sync.md 3.8): two
/// names with one key are one name here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NameKey(pub String);

/// What the sync root's volume does with names, as probed at set-up and
/// re-probed at every start by the file system crate (docs/sync.md 3.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeRules {
    /// `a` finds `A` (APFS default, NTFS, HFS+, FAT).
    pub case_insensitive: bool,
    /// The volume refuses the names Windows refuses (NTFS, FAT, and any
    /// volume a Windows host syncs).
    pub windows_names: bool,
    /// The longest name in UTF-8 bytes, if the volume limits it so (255 on
    /// most Linux file systems).
    pub max_name_bytes: Option<usize>,
    /// The longest name in UTF-16 units, if the volume limits it so (255 on
    /// NTFS and APFS).
    pub max_name_utf16: Option<usize>,
}

impl VolumeRules {
    /// ext4, btrfs, xfs: case-sensitive, 255 bytes.
    pub const LINUX: VolumeRules = VolumeRules {
        case_insensitive: false,
        windows_names: false,
        max_name_bytes: Some(255),
        max_name_utf16: None,
    };
    /// APFS by default: case-insensitive, 255 UTF-16 units.
    pub const MACOS: VolumeRules = VolumeRules {
        case_insensitive: true,
        windows_names: false,
        max_name_bytes: None,
        max_name_utf16: Some(255),
    };
    /// NTFS: case-insensitive, Windows' name rules, 255 UTF-16 units.
    pub const WINDOWS: VolumeRules = VolumeRules {
        case_insensitive: true,
        windows_names: true,
        max_name_bytes: None,
        max_name_utf16: Some(255),
    };
}

/// The NFC form of a name.
pub fn to_nfc(s: &str) -> String {
    if is_nfc(s) {
        s.to_owned()
    } else {
        s.nfc().collect()
    }
}

/// The comparison key of a local name on a volume, or none when the name is
/// not valid UTF-8 (such a name cannot be synced; the scanner lists it).
///
/// Normalisation is always folded: even on ext4, where two spellings of
/// `é` can be two files, they are one name on Hearth, so they are one name
/// to the engine; the second is listed as a clash instead of being synced
/// over the first.
pub fn name_key(name: &LocalName, rules: &VolumeRules) -> Option<NameKey> {
    let s = name.as_str()?;
    Some(key_of_str(s, rules))
}

/// The comparison key of a name given as text (a remote name).
pub fn key_of_str(s: &str, rules: &VolumeRules) -> NameKey {
    let nfc = to_nfc(s);
    if rules.case_insensitive {
        NameKey(simple_fold(&nfc))
    } else {
        NameKey(nfc)
    }
}

/// Unicode simple case folding, one character to one character, as the
/// insensitive volumes compare (docs/sync.md 3.8). Built on the standard
/// library's lowercase mapping where that maps one character to one, plus
/// the characters whose folding differs from their lowercase. Characters
/// whose lowercase is several characters keep their own form, as simple
/// folding does (the German sharp s stays itself; full folding would make
/// it `ss`, which no volume does).
///
/// Applied to the NFC name and not normalised again (docs/sync.md 3.17,
/// R7: statuses C and S of CaseFolding.txt, never T and never F). The
/// Unicode version is the standard library's (`char::UNICODE_VERSION` of
/// the pinned compiler, named in docs/decisions.md); changing it is a
/// migration of the database's keys.
fn simple_fold(s: &str) -> String {
    s.chars().map(fold_char).collect()
}

/// One character's simple case fold.
fn fold_char(c: char) -> char {
    match c {
        // Folding targets that differ from lowercase (CaseFolding.txt,
        // status C and S, where the lowercase mapping is the identity or
        // something else).
        '\u{00B5}' => '\u{03BC}', // micro sign -> mu
        '\u{017F}' => 's',        // long s
        '\u{0345}' => '\u{03B9}', // combining ypogegrammeni -> iota
        '\u{03C2}' => '\u{03C3}', // final sigma -> sigma
        '\u{03D0}' => '\u{03B2}', // beta symbol
        '\u{03D1}' => '\u{03B8}', // theta symbol
        '\u{03D5}' => '\u{03C6}', // phi symbol
        '\u{03D6}' => '\u{03C0}', // pi symbol
        '\u{03F0}' => '\u{03BA}', // kappa symbol
        '\u{03F1}' => '\u{03C1}', // rho symbol
        '\u{03F5}' => '\u{03B5}', // lunate epsilon
        '\u{1E9B}' => '\u{1E61}', // long s with dot above
        '\u{1FBE}' => '\u{03B9}', // prosgegrammeni
        '\u{1E9E}' => '\u{00DF}', // capital sharp s -> sharp s
        // Cherokee: folding maps the small letters to the capitals.
        '\u{AB70}'..='\u{ABBF}' => char::from_u32(c as u32 - 0xAB70 + 0x13A0).unwrap_or(c),
        '\u{13F8}'..='\u{13FD}' => char::from_u32(c as u32 - 8).unwrap_or(c),
        '\u{13A0}'..='\u{13F5}' => c,
        _ => {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(l), None) => l,
                // Several characters: simple folding keeps the character.
                _ => c,
            }
        }
    }
}

/// Why Hearth would refuse a name (sync-api.md 13), checked before the
/// engine sends one so the item is listed instead of failing a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RemoteRefusal {
    /// Not valid UTF-8 (a Linux name with other bytes).
    NotUtf8,
    /// Empty.
    Empty,
    /// `.` or `..`.
    Dot,
    /// Longer than the server's `nameBytesMax`.
    TooLong,
    /// Holds `/` or `\`.
    Separator,
    /// Holds a control character.
    Control,
    /// Only white space.
    Blank,
    /// `.htaccess` or `.part` in any case.
    Reserved,
}

/// Whether Hearth would refuse this local name, after the engine has put it
/// in NFC (it sends NFC; sync-api.md 13).
pub fn remote_refusal(name: &LocalName, name_bytes_max: usize) -> Option<RemoteRefusal> {
    let Some(s) = name.as_str() else {
        return Some(RemoteRefusal::NotUtf8);
    };
    let nfc = to_nfc(s);
    if nfc.is_empty() {
        return Some(RemoteRefusal::Empty);
    }
    if nfc == "." || nfc == ".." {
        return Some(RemoteRefusal::Dot);
    }
    if nfc.len() > name_bytes_max {
        return Some(RemoteRefusal::TooLong);
    }
    if nfc.contains('/') || nfc.contains('\\') {
        return Some(RemoteRefusal::Separator);
    }
    if nfc.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7F) {
        return Some(RemoteRefusal::Control);
    }
    if nfc.chars().all(char::is_whitespace) {
        return Some(RemoteRefusal::Blank);
    }
    let lower = nfc.to_lowercase();
    if lower == ".htaccess" || lower == ".part" {
        return Some(RemoteRefusal::Reserved);
    }
    None
}

/// Why this device cannot hold a name Hearth has (docs/sync.md 3.8). Such
/// an entry stays on Hearth, untouched, and is listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HoldRefusal {
    /// A character the volume refuses (Windows' `< > : " / \ | ? *`,
    /// control characters; `/` and NUL everywhere).
    Character,
    /// Ends in a dot or a space (Windows).
    TrailingDotOrSpace,
    /// A device name Windows keeps (`CON`, `COM1`, `LPT¹`...), alone or
    /// before an extension.
    ReservedDevice,
    /// Longer than the volume allows. Listed, never truncated.
    TooLong,
    /// A name the engine uses for itself (`.hearthsync` at a sync root,
    /// anything ending `.hearthtmp`).
    EngineName,
}

/// Whether this device can hold `name` (a remote name, NFC) on its volume.
/// `at_root` is true for names directly in the sync root, where
/// `.hearthsync` is the engine's own folder.
pub fn hold_refusal(name: &str, rules: &VolumeRules, at_root: bool) -> Option<HoldRefusal> {
    if is_engine_name(name, at_root, rules) {
        return Some(HoldRefusal::EngineName);
    }
    if name.contains('/') || name.contains('\0') {
        return Some(HoldRefusal::Character);
    }
    if let Some(max) = rules.max_name_bytes
        && name.len() > max
    {
        return Some(HoldRefusal::TooLong);
    }
    if let Some(max) = rules.max_name_utf16
        && name.encode_utf16().count() > max
    {
        return Some(HoldRefusal::TooLong);
    }
    if rules.windows_names {
        if name
            .chars()
            .any(|c| (c as u32) < 32 || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
        {
            return Some(HoldRefusal::Character);
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Some(HoldRefusal::TrailingDotOrSpace);
        }
        if is_windows_device_name(name) {
            return Some(HoldRefusal::ReservedDevice);
        }
    }
    None
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9` and the forms
/// with superscript digits, in any case, alone or before an extension.
fn is_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    // Windows also ignores trailing spaces of the stem ("CON .txt").
    let stem = stem.trim_end_matches(' ').to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let mut chars = stem.chars();
            let prefix: String = chars.by_ref().take(3).collect();
            let rest: Vec<char> = chars.collect();
            (prefix == "COM" || prefix == "LPT")
                && rest.len() == 1
                && (rest[0].is_ascii_digit() || matches!(rest[0], '¹' | '²' | '³'))
        }
    }
}

/// The engine's own names (docs/sync.md 3.17, R14): `.hearthsync` at a
/// sync root (its state and holding folder) and any name ending
/// `.hearthtmp` in any folder, compared as the volume compares names.
/// Never synced in either direction (sync-api.md 13).
pub fn is_engine_name(name: &str, at_root: bool, rules: &VolumeRules) -> bool {
    let key = key_of_str(name, rules).0;
    let dir = key_of_str(ENGINE_DIR, rules).0;
    let suffix = key_of_str(TEMP_SUFFIX, rules).0;
    (at_root && key == dir) || key.ends_with(&suffix)
}

/// The engine's folder at the sync root (docs/sync.md 3.7).
pub const ENGINE_DIR: &str = ".hearthsync";
/// The ending of a temp file made beside its target (docs/sync.md 3.7).
pub const TEMP_SUFFIX: &str = ".hearthtmp";

/// The device label inside a conflict copy's name (docs/sync.md 3.17, R3):
/// the device's name in NFC; `< > : " / \ | ? *` and control characters
/// removed; cut to 30 characters (scalar values); leading spaces and
/// trailing dots and spaces removed; `device` if nothing is left.
pub fn device_label(device: &str) -> String {
    let cleaned: String = to_nfc(device)
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
        .take(30)
        .collect();
    let trimmed = cleaned.trim_start_matches(' ').trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        "device".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The name of a conflict copy (docs/sync.md 3.5.1):
/// `<stem> (conflict <device>, <yyyy-MM-dd HH.mm.ss>[ n])<.ext>`.
///
/// - `original`: the name the two versions both wanted (NFC text).
/// - `device`: the label from `device_label`.
/// - `local_time_s`: the local file's last change, in seconds of local
///   time (UTC plus the offset the clock reports for that moment).
/// - `number`: 1 for the first try; 2, 3... when the name is taken.
/// - `max_bytes`: the smallest limit among Hearth's `nameBytesMax` and the
///   volume's; the stem is shortened, never the marker or the extension.
///
/// Returns none only if even an empty stem would not fit, which cannot
/// happen with the contract's limit of 250 bytes and a 30-character device
/// name, but is answered as a value rather than a panic.
pub fn conflict_name(
    original: &str,
    device: &str,
    local_time_s: i64,
    number: u32,
    max_bytes: usize,
) -> Option<String> {
    let (mut stem, mut ext) = split_extension(original);
    let n = if number > 1 {
        format!(" {number}")
    } else {
        String::new()
    };
    let marker = format!(
        " (conflict {device}, {}{n})",
        format_local_time(local_time_s)
    );
    // An "extension" so long that the marker cannot fit beside it is not
    // an extension anyone relies on: the whole name is then the stem.
    if marker.len() + ext.len() > max_bytes {
        stem = original;
        ext = "";
    }
    let fixed = marker.len() + ext.len();
    if fixed > max_bytes {
        return None;
    }
    let mut stem_out = String::new();
    for c in stem.chars() {
        if stem_out.len() + c.len_utf8() + fixed > max_bytes {
            break;
        }
        stem_out.push(c);
    }
    // A cut stem may end in a space or a dot; the marker follows it, so the
    // whole name never does, and Windows' trailing rule is kept.
    Some(to_nfc(&format!("{stem_out}{marker}{ext}")))
}

/// Whether a name looks like one `conflict_name` makes. Used to tell the
/// client's conflict list which files are copies; never to decide sync.
pub fn looks_like_conflict_copy(name: &str) -> bool {
    name.contains(" (conflict ")
}

/// Splits `Budget.xlsx` into `Budget` and `.xlsx`. A leading dot is part of
/// the stem (`.bashrc` has no extension), and so is a trailing one.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// `yyyy-MM-dd HH.mm.ss` for a count of seconds since 1970 (already in
/// local time). Dots, never colons: a colon is refused by Windows.
pub fn format_local_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}.{:02}.{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// `yyyy-mm-dd` for a count of seconds since 1970, used for the holding
/// area's folders.
pub fn format_date(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (the well-known
/// algorithm of Howard Hinnant's "chrono-compatible low-level date
/// algorithms", public domain).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests;
