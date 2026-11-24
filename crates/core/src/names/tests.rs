//! Tests of the name rules: the corpus of nasty names (docs/sync.md 7.7)
//! line by line, and property tests over arbitrary names.

use super::*;
use proptest::prelude::*;

/// One line of the corpus.
struct Case {
    name: Vec<u8>,
    hearth: String,
    windows: String,
    linux: String,
    note: String,
}

/// Reads the corpus file, decoding its escapes.
fn corpus() -> Vec<Case> {
    let text = include_str!("../../corpus/nasty-names.txt");
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 5, "corpus line has five columns: {line:?}");
        out.push(Case {
            name: unescape(cols[0]),
            hearth: cols[1].to_owned(),
            windows: cols[2].to_owned(),
            linux: cols[3].to_owned(),
            note: cols[4].to_owned(),
        });
    }
    out
}

/// Decodes `\u{XXXX}`, `\xHH`, `\t` and `\\`.
fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'u' => {
                    let end = s[i..].find('}').expect("closing brace") + i;
                    let cp = u32::from_str_radix(&s[i + 3..end], 16).expect("hex");
                    let c = char::from_u32(cp).expect("a character");
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    i = end + 1;
                }
                b'x' => {
                    out.push(u8::from_str_radix(&s[i + 2..i + 4], 16).expect("hex byte"));
                    i += 4;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                _ => panic!("unknown escape in {s:?}"),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn remote_word(r: Option<RemoteRefusal>, raw: &LocalName) -> String {
    // Hearth refuses a non-NFC name; the engine never sends one (it sends
    // the NFC form), so the corpus's `not_nfc` means "the engine must
    // normalise", which `remote_refusal` does before checking.
    if let Some(s) = raw.as_str()
        && !unicode_normalization::is_nfc(s)
        && r.is_none()
    {
        return "not_nfc".into();
    }
    match r {
        None => "ok",
        Some(RemoteRefusal::NotUtf8) => "not_utf8",
        Some(RemoteRefusal::Empty) => "empty",
        Some(RemoteRefusal::Dot) => "dot",
        Some(RemoteRefusal::TooLong) => "too_long",
        Some(RemoteRefusal::Separator) => "separator",
        Some(RemoteRefusal::Control) => "control",
        Some(RemoteRefusal::Blank) => "blank",
        Some(RemoteRefusal::Reserved) => "reserved",
    }
    .into()
}

fn hold_word(r: Option<HoldRefusal>) -> String {
    match r {
        None => "ok",
        Some(HoldRefusal::Character) => "character",
        Some(HoldRefusal::TrailingDotOrSpace) => "trailing_dot_or_space",
        Some(HoldRefusal::ReservedDevice) => "reserved_device",
        Some(HoldRefusal::TooLong) => "too_long",
        Some(HoldRefusal::EngineName) => "engine_name",
    }
    .into()
}

#[test]
fn every_corpus_name_gets_the_answer_it_lists() {
    let cases = corpus();
    assert!(cases.len() >= 50, "the corpus is complete");
    for c in &cases {
        let name = LocalName(c.name.clone());
        assert_eq!(
            remote_word(remote_refusal(&name, 250), &name),
            c.hearth,
            "Hearth's answer for {:?} ({})",
            name,
            c.note
        );
        if let Some(s) = name.as_str() {
            // The corpus lists names as Hearth would hold them: NFC.
            let nfc = to_nfc(s);
            assert_eq!(
                hold_word(hold_refusal(&nfc, &VolumeRules::WINDOWS, true)),
                c.windows,
                "Windows for {:?} ({})",
                name,
                c.note
            );
            assert_eq!(
                hold_word(hold_refusal(&nfc, &VolumeRules::LINUX, true)),
                c.linux,
                "Linux for {:?} ({})",
                name,
                c.note
            );
        } else {
            assert_eq!(name_key(&name, &VolumeRules::LINUX), None);
        }
    }
}

#[test]
fn normalisation_forms_share_one_key_everywhere() {
    let composed = LocalName("caf\u{e9}".as_bytes().to_vec());
    let decomposed = LocalName("cafe\u{301}".as_bytes().to_vec());
    for rules in [VolumeRules::LINUX, VolumeRules::MACOS, VolumeRules::WINDOWS] {
        assert_eq!(name_key(&composed, &rules), name_key(&decomposed, &rules));
    }
}

#[test]
fn case_shares_a_key_only_on_insensitive_volumes() {
    let a = LocalName(b"Report.pdf".to_vec());
    let b = LocalName(b"report.pdf".to_vec());
    assert_ne!(
        name_key(&a, &VolumeRules::LINUX),
        name_key(&b, &VolumeRules::LINUX)
    );
    assert_eq!(
        name_key(&a, &VolumeRules::MACOS),
        name_key(&b, &VolumeRules::MACOS)
    );
}

#[test]
fn simple_folding_cases() {
    let mac = VolumeRules::MACOS;
    // Greek sigmas fold together.
    assert_eq!(
        key_of_str("\u{3a3}", &mac),
        key_of_str("\u{3c2}", &mac),
        "capital and final sigma"
    );
    // Sharp s stays itself under simple folding: not equal to "ss".
    assert_ne!(key_of_str("stra\u{df}e", &mac), key_of_str("strasse", &mac));
    assert_eq!(key_of_str("\u{1e9e}", &mac), key_of_str("\u{df}", &mac));
    // Kelvin sign folds to k.
    assert_eq!(key_of_str("\u{212a}", &mac), key_of_str("k", &mac));
}

#[test]
fn conflict_names_follow_the_plan() {
    let t = 1_795_615_402; // 2026-11-25 14:03:22 UTC
    let name = conflict_name("Budget.xlsx", "Daniels-Laptop", t, 1, 250).unwrap();
    assert_eq!(
        name,
        "Budget (conflict Daniels-Laptop, 2026-11-25 14.03.22).xlsx"
    );
    let second = conflict_name("Budget.xlsx", "Daniels-Laptop", t, 2, 250).unwrap();
    assert_eq!(
        second,
        "Budget (conflict Daniels-Laptop, 2026-11-25 14.03.22 2).xlsx"
    );
    assert_eq!(
        conflict_name(".bashrc", "d", 0, 1, 250).unwrap(),
        ".bashrc (conflict d, 1970-01-01 00.00.00)"
    );
    // A long stem is shortened; the marker and extension stay.
    let long = format!("{}.pdf", "x".repeat(300));
    let c = conflict_name(&long, "dev", t, 1, 250).unwrap();
    assert!(c.len() <= 250);
    assert!(c.ends_with(").pdf"));
    assert!(c.contains("(conflict dev, "));
}

#[test]
fn device_names_are_cleaned() {
    assert_eq!(
        clean_device_name("Dan's:Laptop?").as_deref(),
        Some("Dan'sLaptop")
    );
    assert_eq!(clean_device_name("  ...").as_deref(), None);
    assert_eq!(
        clean_device_name(&"x".repeat(50)).map(|s| s.len()),
        Some(30)
    );
}

#[test]
fn dates_before_and_after_1970() {
    assert_eq!(format_local_time(0), "1970-01-01 00.00.00");
    assert_eq!(format_local_time(-1), "1969-12-31 23.59.59");
    assert_eq!(format_date(951_782_400), "2000-02-29");
}

#[test]
fn local_paths_encode_and_decode() {
    let p = LocalPath(vec![
        LocalName(b"a".to_vec()),
        LocalName(vec![0xff, 0xfe]),
        LocalName("\u{e9}".as_bytes().to_vec()),
    ]);
    assert_eq!(LocalPath::decode(&p.encode()), p);
    assert_eq!(
        LocalPath::decode(&LocalPath::root().encode()),
        LocalPath::root()
    );
}

/// Names from the corpus's alphabet: the characters that make trouble.
fn nasty_char() -> impl Strategy<Value = char> {
    prop_oneof![
        Just('a'),
        Just('A'),
        Just('.'),
        Just(' '),
        Just('\u{e9}'),
        Just('\u{301}'),
        Just('\u{df}'),
        Just('\u{1e9e}'),
        Just('\u{3a3}'),
        Just('\u{3c2}'),
        Just('\u{130}'),
        Just('\u{131}'),
        Just('\u{1f600}'),
        Just('\u{200b}'),
        Just(':'),
        Just('*'),
        Just('\\'),
        Just('\u{1}'),
        any::<char>(),
    ]
}

proptest! {
    /// The key of a key is itself: comparing by key is an equivalence.
    #[test]
    fn name_key_is_idempotent(chars in proptest::collection::vec(nasty_char(), 0..40)) {
        let s: String = chars.into_iter().collect();
        for rules in [VolumeRules::LINUX, VolumeRules::MACOS, VolumeRules::WINDOWS] {
            let k = key_of_str(&s, &rules);
            prop_assert_eq!(key_of_str(&k.0, &rules), k.clone());
        }
    }

    /// Any bytes at all give an answer, never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
        let n = LocalName(bytes);
        let _ = name_key(&n, &VolumeRules::WINDOWS);
        let _ = remote_refusal(&n, 250);
        if let Some(s) = n.as_str() {
            let _ = hold_refusal(s, &VolumeRules::WINDOWS, true);
        }
    }

    /// A conflict copy of a name Hearth accepts is a name Hearth accepts,
    /// within the limit, and holdable wherever the original was.
    #[test]
    fn conflict_names_are_always_valid(
        chars in proptest::collection::vec(nasty_char(), 1..300),
        number in 1u32..20,
        t in -10_000_000_000i64..10_000_000_000,
    ) {
        // Make the generated name one Hearth accepts: drop separators and
        // control characters, avoid blank and reserved names, keep it
        // within 250 bytes.
        let mut s: String = chars
            .into_iter()
            .filter(|c| !matches!(c, '/' | '\\') && (*c as u32) >= 0x20 && *c as u32 != 0x7f)
            .collect();
        s.push('x');
        let mut s = to_nfc(&s);
        while s.len() > 250 {
            s.pop();
        }
        let name = LocalName(s.clone().into_bytes());
        prop_assume!(remote_refusal(&name, 250).is_none());
        let c = conflict_name(&s, "Laptop", t, number, 250).unwrap();
        prop_assert!(c.len() <= 250);
        prop_assert_eq!(remote_refusal(&LocalName(c.clone().into_bytes()), 250), None);
        for rules in [VolumeRules::LINUX, VolumeRules::MACOS, VolumeRules::WINDOWS] {
            if hold_refusal(&s, &rules, false).is_none() {
                prop_assert_eq!(hold_refusal(&c, &rules, false), None, "{}", c);
            }
        }
        prop_assert_ne!(key_of_str(&c, &VolumeRules::MACOS), key_of_str(&s, &VolumeRules::MACOS));
    }
}
