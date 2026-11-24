//! Names: what Hearth accepts, what each kind of device volume can hold,
//! when two names are "the same" on a device, and how conflict copies are
//! named.
//!
//! Sources: docs/sync-api.md section 13 (Hearth's rules), docs/sync.md
//! 3.8 (devices, case, Unicode, Windows) and 3.5.1 (conflict copy names).
//! Each rule below names its sentence. Nothing here asks the engine.

use unicode_normalization::{UnicodeNormalization, is_nfc};

/// The longest name Hearth stores, in bytes of UTF-8
/// (docs/sync-api.md 13, `limits.nameBytesMax`).
pub const HEARTH_NAME_BYTES_MAX: usize = 250;

/// A name in Unicode Normalization Form C. Hearth stores and compares
/// names in this form (sync-api 13); devices are compared to it by it
/// (sync.md 3.8, I8).
pub fn nfc(name: &str) -> String {
    name.nfc().collect()
}

/// The same name decomposed (NFD): what an HFS+ volume hands back for a
/// composed name (sync.md 3.8, the volume table).
pub fn nfd(name: &str) -> String {
    name.nfd().collect()
}

/// Why Hearth refuses a name on the sync API: the `details.reason` of
/// sync-api 13, one variant per row of its table. Inside end-to-end
/// encrypted folders there is one more rule (`e2ee_name`); the model has
/// no such folders yet (sync.md 5 is out of its scope for now).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HearthRefusal {
    /// Not in NFC. The engine normalises before sending, so for the engine
    /// this never applies to a name it sends; it is here for completeness
    /// of the server's table.
    NotNfc,
    /// The empty name.
    Empty,
    /// `.` or `..`.
    Dot,
    /// More than 250 bytes of UTF-8.
    TooLong,
    /// Contains `/` or `\`.
    Separator,
    /// Contains U+0000 to U+001F or U+007F.
    Control,
    /// Only white space.
    Blank,
    /// `.htaccess` or `.part`, in any case of letters.
    Reserved,
}

impl HearthRefusal {
    /// The reason's wire name (sync-api 13), used in listings and the
    /// scenario text.
    pub fn code(self) -> &'static str {
        match self {
            HearthRefusal::NotNfc => "not_nfc",
            HearthRefusal::Empty => "empty",
            HearthRefusal::Dot => "dot",
            HearthRefusal::TooLong => "too_long",
            HearthRefusal::Separator => "separator",
            HearthRefusal::Control => "control",
            HearthRefusal::Blank => "blank",
            HearthRefusal::Reserved => "reserved",
        }
    }
}

/// Whether Hearth would refuse `name` exactly as given (sync-api 13, the
/// table, top to bottom; the first rule that applies is the reason).
pub fn hearth_refuses(name: &str) -> Option<HearthRefusal> {
    if !is_nfc(name) {
        return Some(HearthRefusal::NotNfc);
    }
    if name.is_empty() {
        return Some(HearthRefusal::Empty);
    }
    if name == "." || name == ".." {
        return Some(HearthRefusal::Dot);
    }
    if name.len() > HEARTH_NAME_BYTES_MAX {
        return Some(HearthRefusal::TooLong);
    }
    if name.contains('/') || name.contains('\\') {
        return Some(HearthRefusal::Separator);
    }
    if name.chars().any(|c| (c as u32) <= 0x1F || c as u32 == 0x7F) {
        return Some(HearthRefusal::Control);
    }
    if name.chars().all(char::is_whitespace) {
        return Some(HearthRefusal::Blank);
    }
    let lower = name.to_lowercase();
    if lower == ".htaccess" || lower == ".part" {
        return Some(HearthRefusal::Reserved);
    }
    None
}

/// Whether Hearth refuses the name a device would send for a local name:
/// the engine sends the NFC form (sync.md 3.8: "A local file created in
/// NFD is uploaded under its NFC name"), so normalisation itself is never
/// the reason.
pub fn hearth_refuses_local(name: &str) -> Option<HearthRefusal> {
    hearth_refuses(&nfc(name))
}

/// The kinds of volume a sync root can be on (sync.md 3.8, the table).
/// The simulator gives each device one; the model needs to know only the
/// three facts below about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Volume {
    /// ext4, btrfs, xfs: case sensitive, bytes as given, 255-byte names.
    Ext4,
    /// APFS by default: case insensitive and preserving, compares
    /// normalised, 255 UTF-16 units a name (sync.md 3.8 "Long paths").
    Apfs,
    /// HFS+: case insensitive, stores a decomposed form.
    HfsPlus,
    /// NTFS: case insensitive and preserving, Windows names, 255 UTF-16.
    Ntfs,
    /// exFAT, FAT32: case insensitive, Windows names ("fewer characters":
    /// the model applies the Windows rules, which is the documented part).
    Fat,
}

impl Volume {
    /// Every volume, for generators and tests.
    pub const ALL: [Volume; 5] = [
        Volume::Ext4,
        Volume::Apfs,
        Volume::HfsPlus,
        Volume::Ntfs,
        Volume::Fat,
    ];

    /// The name used in the scenario text.
    pub fn code(self) -> &'static str {
        match self {
            Volume::Ext4 => "ext4",
            Volume::Apfs => "apfs",
            Volume::HfsPlus => "hfsplus",
            Volume::Ntfs => "ntfs",
            Volume::Fat => "fat",
        }
    }

    /// The volume named `code`, the inverse of [`Volume::code`].
    pub fn from_code(code: &str) -> Option<Volume> {
        Volume::ALL.into_iter().find(|v| v.code() == code)
    }

    /// Whether `a` and `A` are one name here (sync.md 3.8 table).
    pub fn case_insensitive(self) -> bool {
        !matches!(self, Volume::Ext4)
    }

    /// Whether names Windows refuses are refused here (sync.md 3.8,
    /// "Names Windows refuses"; NTFS and FAT are the Windows volumes).
    pub fn windows_names(self) -> bool {
        matches!(self, Volume::Ntfs | Volume::Fat)
    }

    /// Whether a name's length is counted in UTF-16 units (else in UTF-8
    /// bytes); the limit is 255 either way (sync.md 3.8 "Long paths").
    fn counts_utf16(self) -> bool {
        matches!(
            self,
            Volume::Apfs | Volume::HfsPlus | Volume::Ntfs | Volume::Fat
        )
    }
}

/// The longest name a volume holds, in its own unit (sync.md 3.8).
pub const VOLUME_NAME_MAX: usize = 255;

/// Why a device cannot hold a name Hearth has (sync.md 3.8). Such an item
/// is left on Hearth and listed on that device (I10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DeviceRefusal {
    /// A character Windows refuses: `< > : " / \ | ? *` or U+0000-001F.
    WindowsCharacter,
    /// The name ends in a dot or a space.
    WindowsTrailing,
    /// CON, PRN, AUX, NUL, COM1-9, LPT1-9 (and superscript digit forms),
    /// alone or before an extension, in any case.
    WindowsReserved,
    /// Longer than the volume's single-name limit. Listed, never
    /// truncated.
    TooLong,
}

impl DeviceRefusal {
    /// The reason's name in listings and the scenario text.
    pub fn code(self) -> &'static str {
        match self {
            DeviceRefusal::WindowsCharacter => "windows_character",
            DeviceRefusal::WindowsTrailing => "windows_trailing",
            DeviceRefusal::WindowsReserved => "windows_reserved",
            DeviceRefusal::TooLong => "device_too_long",
        }
    }
}

/// Whether `volume` cannot hold `name` (sync.md 3.8 "Names Windows
/// refuses" and "Long paths"). The name is the one written locally, which
/// is the NFC form (3.8: "Locally the engine writes the NFC form").
pub fn device_refuses(volume: Volume, name: &str) -> Option<DeviceRefusal> {
    if volume.windows_names() {
        if name.chars().any(|c| {
            matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) <= 0x1F
        }) {
            return Some(DeviceRefusal::WindowsCharacter);
        }
        if name.ends_with('.') || name.ends_with(' ') {
            return Some(DeviceRefusal::WindowsTrailing);
        }
        if is_windows_device_name(name) {
            return Some(DeviceRefusal::WindowsReserved);
        }
    }
    let length = if volume.counts_utf16() {
        name.encode_utf16().count()
    } else {
        name.len()
    };
    if length > VOLUME_NAME_MAX {
        return Some(DeviceRefusal::TooLong);
    }
    None
}

/// CON, PRN, AUX, NUL, COM1-COM9, LPT1-LPT9 and COM/LPT with superscript
/// 1, 2, 3, "alone or before an extension", in any case (sync.md 3.8).
/// "Before an extension" is read as: the part before the first dot.
fn is_windows_device_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).to_uppercase();
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    for prefix in ["COM", "LPT"] {
        if let Some(rest) = base.strip_prefix(prefix) {
            let mut chars = rest.chars();
            if let (Some(c), None) = (chars.next(), chars.next())
                && matches!(c, '1'..='9' | '\u{B9}' | '\u{B2}' | '\u{B3}')
            {
                return true;
            }
        }
    }
    false
}

/// The key two local names are compared by on a volume: NFC, and on case
/// insensitive volumes also case folded (sync.md 3.8, `name_key`). Two
/// names with one key are one name on that device.
pub fn name_key(volume: Volume, name: &str) -> String {
    let n = nfc(name);
    if volume.case_insensitive() {
        n.chars().map(simple_fold).collect()
    } else {
        n
    }
}

/// Unicode simple case folding of one character (sync.md 3.8 names
/// "Unicode simple folding"). For almost every character this is its
/// lower case when that is one character; the exceptions below are the
/// characters whose simple fold differs from their lower case (from
/// CaseFolding.txt, status C and S). A character whose lower case is
/// more than one character (U+0130, capital I with dot) has no simple
/// fold and stays as it is.
fn simple_fold(c: char) -> char {
    match c {
        '\u{3C2}' => '\u{3C3}',              // final sigma -> sigma
        '\u{17F}' => 's',                    // long s
        '\u{345}' | '\u{1FBE}' => '\u{3B9}', // ypogegrammeni, prosgegrammeni -> iota
        '\u{3D0}' => '\u{3B2}',              // beta symbol
        '\u{3D1}' => '\u{3B8}',              // theta symbol
        '\u{3D5}' => '\u{3C6}',              // phi symbol
        '\u{3D6}' => '\u{3C0}',              // pi symbol
        '\u{3F0}' => '\u{3BA}',              // kappa symbol
        '\u{3F1}' => '\u{3C1}',              // rho symbol
        '\u{3F5}' => '\u{3B5}',              // lunate epsilon
        '\u{1E9B}' => '\u{1E61}',            // long s with dot above
        // Cherokee folds to upper case (the one script that does).
        '\u{13F8}'..='\u{13FD}' => char::from_u32(c as u32 - 8).unwrap_or(c),
        '\u{AB70}'..='\u{ABBF}' => char::from_u32(c as u32 - 0xAB70 + 0x13A0).unwrap_or(c),
        '\u{13A0}'..='\u{13F5}' => c,
        _ => {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(l), None) => l,
                _ => c,
            }
        }
    }
}

/// Whether the engine keeps `name` for itself in a folder (`at_root`:
/// the sync root itself) and never syncs an entry so named there
/// (sync-api 13, "The engine's own names"; sync.md 3.7): `.hearthsync` at
/// the root, and anything ending `.hearthtmp` in any folder, since the
/// engine's temporary files sit beside their targets wherever those are
/// (docs/contract-questions.md Q14).
pub fn is_engine_name(name: &str, at_root: bool) -> bool {
    (at_root && name == ".hearthsync") || name.ends_with(".hearthtmp")
}

/// A name split into stem and extension for a conflict copy. The
/// extension is from the last dot when that dot is neither the first nor
/// the last character (".bashrc" and "notes." have none). The documents do
/// not define the split; docs/contract-questions.md Q2 records this
/// reading.
pub fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The device name as it appears in a conflict copy: "cut to 30
/// characters and cleaned of characters any OS refuses" (sync.md 3.5.1).
/// Cleaning removes the Windows characters and controls (the union of
/// what the volumes of 3.8 refuse) and then trailing dots and spaces;
/// cutting is by characters, after cleaning.
pub fn conflict_device_label(device: &str) -> String {
    let cleaned: String = device
        .chars()
        .filter(|c| {
            !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
                && (*c as u32) > 0x1F
                && *c as u32 != 0x7F
        })
        .collect();
    let cut: String = cleaned.chars().take(30).collect();
    cut.trim_end_matches(['.', ' ']).to_string()
}

/// A civil date and time, for the stamp of a conflict copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// Year, for example 2026.
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    /// 0 to 23.
    pub hour: u32,
    /// 0 to 59.
    pub minute: u32,
    /// 0 to 59.
    pub second: u32,
}

impl Stamp {
    /// The local civil time of a moment: `unix_micros` is the file's last
    /// change (microseconds since 1970 UTC, sync-api 1.6) and
    /// `utc_offset_secs` the device's offset from UTC. "The time is the
    /// local file's last change, in local time" (sync.md 3.5.1).
    pub fn local(unix_micros: i64, utc_offset_secs: i64) -> Stamp {
        let secs = unix_micros.div_euclid(1_000_000) + utc_offset_secs;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Stamp {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: (rem % 3600 / 60) as u32,
            second: (rem % 60) as u32,
        }
    }

    /// `yyyy-MM-dd HH.mm.ss`: "with dots, never colons" (3.5.1).
    pub fn text(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}.{:02}.{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Days since 1970-01-01 to (year, month, day) in the proleptic Gregorian
/// calendar (H. Hinnant's public-domain algorithm "civil_from_days").
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
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

/// The name of a conflict copy (sync.md 3.5.1):
/// `<stem> (conflict <device>, <yyyy-MM-dd HH.mm.ss>)<.ext>`, with ` n`
/// before the closing bracket for the n-th taken name (n >= 2), and the
/// stem shortened (never the extension or the marker) until the whole
/// name fits `max_bytes`.
///
/// `max_bytes` is the tighter of Hearth's 250 bytes and the device's own
/// limit; the model passes Hearth's, because a copy is uploaded and must
/// be a name Hearth keeps (I9: copies sync as ordinary files).
pub fn conflict_name(
    original: &str,
    device: &str,
    stamp: Stamp,
    n: u32,
    max_bytes: usize,
) -> String {
    let (stem, ext) = split_extension(original);
    let number = if n >= 2 {
        format!(" {n}")
    } else {
        String::new()
    };
    let marker = format!(
        " (conflict {}, {}{})",
        conflict_device_label(device),
        stamp.text(),
        number
    );
    let mut stem: String = stem.to_string();
    while stem.len() + marker.len() + ext.len() > max_bytes && !stem.is_empty() {
        stem.pop();
    }
    format!("{stem}{marker}{ext}")
}

/// A conflict copy's name taken apart; the inverse of [`conflict_name`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictName {
    /// The (possibly shortened) stem of the original name.
    pub stem: String,
    /// The extension with its dot, or empty.
    pub ext: String,
    /// The device label inside the marker.
    pub device: String,
    /// The stamp text, `yyyy-MM-dd HH.mm.ss`.
    pub stamp: String,
    /// The number before the bracket, 1 when there is none.
    pub n: u32,
}

/// Reads `name` as a conflict copy name, or `None` when it is not one.
/// Used by the checkers to find copies in an observed tree whatever the
/// engine's clock said.
pub fn parse_conflict_name(name: &str) -> Option<ConflictName> {
    let open = name.rfind(" (conflict ")?;
    let close = open + name[open..].find(')')?;
    let inner = &name[open + " (conflict ".len()..close];
    let rest = &name[close + 1..];
    let (device, when) = inner.rsplit_once(", ")?;
    // The stamp is 19 characters; an optional " n" follows.
    if when.len() < 19 || !when.is_char_boundary(19) {
        return None;
    }
    let (stamp, number) = when.split_at(19);
    if !is_stamp(stamp) {
        return None;
    }
    let n = if number.is_empty() {
        1
    } else {
        let digits = number.strip_prefix(' ')?;
        let n: u32 = digits.parse().ok()?;
        if n < 2 || digits.starts_with('0') {
            return None;
        }
        n
    };
    if !rest.is_empty() && !rest.starts_with('.') {
        return None;
    }
    Some(ConflictName {
        stem: name[..open].to_string(),
        ext: rest.to_string(),
        device: device.to_string(),
        stamp: stamp.to_string(),
        n,
    })
}

/// Whether `s` has the shape `yyyy-MM-dd HH.mm.ss`.
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b' ',
            13 | 16 => *c == b'.',
            _ => c.is_ascii_digit(),
        })
}

/// Whether `copy` is a well-formed conflict copy name of `original` made
/// by `device` (any stamp, any number). The stem may be shortened.
pub fn is_conflict_copy_of(copy: &str, original: &str, device: &str) -> bool {
    let Some(parsed) = parse_conflict_name(copy) else {
        return false;
    };
    let (stem, ext) = split_extension(original);
    parsed.ext == ext
        && stem.starts_with(&parsed.stem)
        && parsed.device == conflict_device_label(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hearth_rules_follow_the_table() {
        assert_eq!(hearth_refuses("a\u{301}"), Some(HearthRefusal::NotNfc));
        assert_eq!(hearth_refuses(""), Some(HearthRefusal::Empty));
        assert_eq!(hearth_refuses(".."), Some(HearthRefusal::Dot));
        assert_eq!(
            hearth_refuses(&"x".repeat(251)),
            Some(HearthRefusal::TooLong)
        );
        assert_eq!(hearth_refuses(&"x".repeat(250)), None);
        assert_eq!(hearth_refuses("a\\b"), Some(HearthRefusal::Separator));
        assert_eq!(hearth_refuses("a\u{7F}"), Some(HearthRefusal::Control));
        assert_eq!(hearth_refuses("   "), Some(HearthRefusal::Blank));
        assert_eq!(hearth_refuses(".HTaccess"), Some(HearthRefusal::Reserved));
        // Accepted as sent (sync-api 13, "Everything else").
        for ok in [
            "CON", "a:b", "trail. ", " lead", "-x", ".hidden", "\u{200B}",
        ] {
            assert_eq!(hearth_refuses(ok), None, "{ok:?}");
        }
    }

    #[test]
    fn windows_rules() {
        let v = Volume::Ntfs;
        assert_eq!(
            device_refuses(v, "a:b"),
            Some(DeviceRefusal::WindowsCharacter)
        );
        assert_eq!(
            device_refuses(v, "a."),
            Some(DeviceRefusal::WindowsTrailing)
        );
        assert_eq!(
            device_refuses(v, "a "),
            Some(DeviceRefusal::WindowsTrailing)
        );
        assert_eq!(
            device_refuses(v, "con.txt"),
            Some(DeviceRefusal::WindowsReserved)
        );
        assert_eq!(
            device_refuses(v, "COM\u{B9}"),
            Some(DeviceRefusal::WindowsReserved)
        );
        assert_eq!(device_refuses(v, "COM10"), None);
        assert_eq!(device_refuses(v, "CONSOLE"), None);
        assert_eq!(device_refuses(Volume::Ext4, "a:b"), None);
        assert_eq!(
            device_refuses(Volume::Ext4, &"é".repeat(128)),
            Some(DeviceRefusal::TooLong)
        );
        assert_eq!(device_refuses(Volume::Apfs, &"é".repeat(128)), None);
    }

    #[test]
    fn keys() {
        assert_eq!(
            name_key(Volume::Apfs, "Report.PDF"),
            name_key(Volume::Apfs, "report.pdf")
        );
        assert_ne!(
            name_key(Volume::Ext4, "Report.pdf"),
            name_key(Volume::Ext4, "report.pdf")
        );
        assert_eq!(
            name_key(Volume::Ext4, "e\u{301}"),
            name_key(Volume::Ext4, "\u{E9}")
        );
        assert_eq!(
            name_key(Volume::Ntfs, "\u{3A3}"),
            name_key(Volume::Ntfs, "\u{3C2}")
        );
        // Turkish dotted capital I has no simple fold.
        assert_ne!(
            name_key(Volume::Ntfs, "\u{130}"),
            name_key(Volume::Ntfs, "i")
        );
    }

    #[test]
    fn conflict_names_round_trip() {
        let stamp = Stamp::local(1_795_615_402_000_000, 3600);
        assert_eq!(stamp.text(), "2026-11-25 15.03.22");
        let name = conflict_name("Budget.xlsx", "Daniels-Laptop", stamp, 1, 250);
        assert_eq!(
            name,
            "Budget (conflict Daniels-Laptop, 2026-11-25 15.03.22).xlsx"
        );
        let parsed = parse_conflict_name(&name).expect("parses");
        assert_eq!(
            (parsed.stem.as_str(), parsed.ext.as_str(), parsed.n),
            ("Budget", ".xlsx", 1)
        );
        let second = conflict_name("Budget.xlsx", "Daniels-Laptop", stamp, 2, 250);
        assert!(second.ends_with(" 2).xlsx"));
        assert_eq!(parse_conflict_name(&second).map(|p| p.n), Some(2));
        let long = conflict_name(&format!("{}.txt", "a".repeat(240)), "D", stamp, 1, 250);
        assert_eq!(long.len(), 250);
        assert!(long.ends_with(").txt"));
        assert!(is_conflict_copy_of(
            &long,
            &format!("{}.txt", "a".repeat(240)),
            "D"
        ));
        assert_eq!(parse_conflict_name("plain.txt"), None);
    }

    #[test]
    fn device_label_is_cleaned_and_cut() {
        assert_eq!(conflict_device_label("My:PC|"), "MyPC");
        assert_eq!(conflict_device_label(&"x".repeat(40)).len(), 30);
        assert_eq!(conflict_device_label("Laptop. "), "Laptop");
    }

    #[test]
    fn dates() {
        assert_eq!(Stamp::local(0, 0).text(), "1970-01-01 00.00.00");
        assert_eq!(Stamp::local(-1_000_000, 0).text(), "1969-12-31 23.59.59");
        assert_eq!(
            Stamp::local(951_782_400_000_000, 0).text(),
            "2000-02-29 00.00.00"
        );
    }
}
