//! The corpus of nasty names (docs/sync.md 7.7), as the generator uses
//! it: one name per line of the plan's list, with what it is for.
//!
//! Names Rust strings cannot hold (bytes that are not UTF-8, unpaired
//! surrogates) are the file-system layer's business (crates/fs); the
//! model's names are always valid Unicode, as Hearth's are (sync-api 13).

/// A name and the case it stands for.
#[derive(Debug, Clone, Copy)]
pub struct NastyName {
    /// The name.
    pub name: &'static str,
    /// What it tests.
    pub purpose: &'static str,
}

const fn n(name: &'static str, purpose: &'static str) -> NastyName {
    NastyName { name, purpose }
}

/// Pairs of names one device holds as one: case and normalisation
/// (3.8). The generator puts both of a pair in one folder on purpose.
pub const CLASH_PAIRS: &[(&str, &str)] = &[
    ("Caf\u{E9}.txt", "Cafe\u{301}.txt"),
    ("Report.pdf", "report.pdf"),
    ("README", "readme"),
    ("Stra\u{DF}e", "STRA\u{1E9E}E"),
    (
        "\u{3A3}\u{3BF}\u{3C6}\u{3AF}\u{3B1}\u{3C2}",
        "\u{3C3}\u{3BF}\u{3C6}\u{3AF}\u{3B1}\u{3C3}",
    ),
    ("\u{212B}ngstr\u{F6}m", "\u{C5}ngstr\u{F6}m"),
    ("K\u{212A}", "kk"),
];

/// The corpus, one entry per item of 7.7.
pub const NAMES: &[NastyName] = &[
    n("Caf\u{E9}.txt", "composed form"),
    n("Cafe\u{301}.txt", "decomposed form of the same name"),
    n("Report.pdf", "case pair, upper"),
    n("report.pdf", "case pair, lower"),
    n("Stra\u{DF}e", "German sharp s: no simple fold to ss"),
    n("STRA\u{1E9E}E", "capital sharp s folds to sharp s"),
    n("\u{130}stanbul", "Turkish dotted capital I: no simple fold"),
    n("\u{131}zmir", "Turkish dotless i"),
    n(
        "\u{3C3}\u{3BF}\u{3C6}\u{3AF}\u{3B1}\u{3C2}",
        "Greek final sigma",
    ),
    n("a<b", "Windows refuses <"),
    n("a>b", "Windows refuses >"),
    n("time 10:30", "Windows refuses :, macOS shows it as /"),
    n("say \"hi\"", "Windows refuses \""),
    n("a|b", "Windows refuses |"),
    n("what?", "Windows refuses ?"),
    n("star*", "Windows refuses *"),
    n("tab\u{9}name", "control character: Hearth refuses"),
    n("trailing.", "trailing dot: Windows refuses"),
    n("trailing ", "trailing space: Windows refuses"),
    n("CON", "reserved device name"),
    n(
        "con.txt",
        "reserved device name before an extension, lower case",
    ),
    n("Nul.tar.gz", "reserved name, mixed case, two extensions"),
    n("COM1", "reserved COM"),
    n("LPT9.log", "reserved LPT with extension"),
    n("COM\u{B9}", "reserved COM with superscript one"),
    n("COM10", "not reserved: two digits"),
    n("\u{1F600}.txt", "outside the basic plane"),
    n("\u{200F}rtl", "right-to-left mark"),
    n("zero\u{200B}width", "zero-width space"),
    n("-rf", "leading dash"),
    n(".hidden", "leading dot"),
    n(" ", "a single space: Hearth refuses (blank)"),
    n("  lead", "leading spaces"),
    n(".htaccess", "Hearth refuses"),
    n(".PART", "Hearth refuses, any case"),
    n("x.part", "allowed: only the bare name is reserved"),
    n("back\\slash", "a backslash in a Linux name: Hearth refuses"),
    n(".hearthsync", "the engine's own folder name"),
    n("x.hearthtmp", "looks like the engine's temp file"),
    n(
        "Budget (conflict Lap, 2026-11-24 14.03.22).xlsx",
        "looks like a conflict copy",
    ),
    n(
        "Budget (conflict Lap, 2026-11-24 14.03.22 2).xlsx",
        "looks like a numbered conflict copy",
    ),
    n(
        "\u{E9}\u{301}",
        "a character Hearth's NFC and Apple's variant treat differently",
    ),
    n("\u{FB01}le", "ligature fi: no simple fold"),
    n("~$lock.docx", "an office lock file name"),
];

/// A name of exactly `bytes` bytes of UTF-8, of the given repeated
/// character (for the 250, 251, 255 and 256-byte cases of 7.7).
pub fn long_name(c: char, bytes: usize) -> String {
    let mut s = String::new();
    while s.len() + c.len_utf8() <= bytes {
        s.push(c);
    }
    while s.len() < bytes {
        s.push('x');
    }
    s
}

/// The long names of 7.7: Hearth's limit, one over, the volumes' limits
/// in bytes and in UTF-16 units, one over each.
pub fn long_names() -> Vec<(String, &'static str)> {
    vec![
        (long_name('a', 250), "250 bytes: Hearth's limit"),
        (long_name('a', 251), "251 bytes: Hearth refuses"),
        (
            long_name('\u{E9}', 254),
            "127 two-byte characters: 254 bytes, 127 UTF-16 units",
        ),
        (
            long_name('\u{1F600}', 248),
            "62 four-byte characters: 124 UTF-16 units",
        ),
    ]
}
