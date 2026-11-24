//! Ignore rules (docs/sync.md 3.14): patterns in gitignore syntax, one list
//! per sync root, stored in the database.
//!
//! Why written here rather than taken from a crate: the well-known
//! implementation (the `ignore` crate) comes with a directory walker,
//! threads and file reading, and the core must do no I/O and stay small
//! enough for a phone's file provider extension. The part needed is the
//! matcher alone: about two hundred lines, tested against gitignore's
//! documented examples below.
//!
//! What is supported, as gitignore documents it: blank lines and `#`
//! comments; `\#` and `\!` at the start; `!` to re-include; a trailing `/`
//! for folders only; a pattern with a `/` at the start or in the middle is
//! anchored at the sync root, one without matches a name at any depth;
//! `*`, `?`, `[a-z]`, `[!a-z]`; `**/` at the start, `/**` at the end and
//! `/**/` in the middle. As in git, nothing inside an ignored folder can be
//! re-included.
//!
//! Ignored items are never uploaded, downloaded or deleted by the engine.
//! `.hearthignore` files inside the tree are not read (3.14: a synced file
//! must not change what another device ignores).

/// The list that ships, exactly as docs/sync.md 3.14 gives it: lock and
/// temporary files of Office, LibreOffice and editors, operating system
/// litter, and the engine's own names. The engine's names are also kept
/// out by `names::is_engine_name` whatever the list says, so editing the
/// list can never make the engine sync its own state (R14).
pub const DEFAULT_RULES: &[&str] = &[
    "~$*",
    ".~lock.*#",
    "*.tmp",
    ".DS_Store",
    "Thumbs.db",
    "desktop.ini",
    "*.hearthtmp",
    "/.hearthsync/",
];

/// One compiled pattern.
#[derive(Clone, Debug)]
struct Rule {
    tokens: Vec<Token>,
    negate: bool,
    dir_only: bool,
    anchored: bool,
}

/// A piece of a compiled pattern.
#[derive(Clone, Debug, PartialEq)]
enum Token {
    /// One exact character.
    Lit(char),
    /// `?`: one character but `/`.
    One,
    /// `*`: any run of characters without `/`.
    Star,
    /// The `**` of a trailing `/**`: anything, `/` included.
    AnyAll,
    /// A leading `**/` or a middle `/**/`: zero or more whole folders,
    /// that is, empty or anything ending in `/`.
    AnyDirs,
    /// `[...]`: one character in (or, negated, not in) the ranges.
    Class(bool, Vec<(char, char)>),
}

/// A compiled list of rules. The last rule that matches decides, as in git.
#[derive(Clone, Debug, Default)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    /// Compiles a list of lines. Lines that cannot be read as patterns (an
    /// unclosed `[`) are returned as errors with their text, so the client
    /// can show which line is wrong; the others still apply.
    pub fn new<S: AsRef<str>>(lines: &[S]) -> (IgnoreRules, Vec<String>) {
        let mut rules = Vec::new();
        let mut bad = Vec::new();
        for line in lines {
            let line = line.as_ref();
            match compile(line) {
                Ok(Some(rule)) => rules.push(rule),
                Ok(None) => {}
                Err(()) => bad.push(line.to_owned()),
            }
        }
        (IgnoreRules { rules }, bad)
    }

    /// Whether the item at `path` (components under the sync root, the
    /// last being the item itself) is ignored. A folder above it that is
    /// ignored ignores it too.
    pub fn is_ignored(&self, path: &[&str], is_dir: bool) -> bool {
        if self.rules.is_empty() || path.is_empty() {
            return false;
        }
        for end in 1..path.len() {
            if self.decide(&path[..end], true) {
                return true;
            }
        }
        self.decide(path, is_dir)
    }

    /// The decision for one path, not looking at its parents.
    fn decide(&self, path: &[&str], is_dir: bool) -> bool {
        let full: Vec<char> = path.join("/").chars().collect();
        let base: Vec<char> = path[path.len() - 1].chars().collect();
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            let text = if rule.anchored { &full } else { &base };
            if matches(&rule.tokens, text) {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

/// Compiles one line: `Ok(None)` for blank lines and comments.
fn compile(line: &str) -> Result<Option<Rule>, ()> {
    let mut s = line.trim_end_matches(' ');
    if s.is_empty() || s.starts_with('#') {
        return Ok(None);
    }
    let mut negate = false;
    if let Some(rest) = s.strip_prefix('!') {
        negate = true;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('\\') {
        s = rest;
    }
    let mut dir_only = false;
    if let Some(rest) = s.strip_suffix('/') {
        dir_only = true;
        s = rest;
    }
    if s.is_empty() {
        return Ok(None);
    }
    let anchored = s.contains('/');
    let s = s.strip_prefix('/').unwrap_or(s);
    Ok(Some(Rule {
        tokens: tokenize(s)?,
        negate,
        dir_only,
        anchored,
    }))
}

/// Turns a pattern into tokens, recognising the three `**` forms.
fn tokenize(p: &str) -> Result<Vec<Token>, ()> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at_seg_start = i == 0 || chars[i - 1] == '/';
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' && at_seg_start {
            let after = i + 2;
            if after == chars.len() {
                // `/**` at the end (or a pattern that is just `**`).
                out.push(Token::AnyAll);
                i = after;
                continue;
            }
            if chars[after] == '/' {
                // `**/` at the start or `/**/` in the middle.
                out.push(Token::AnyDirs);
                i = after + 1;
                continue;
            }
        }
        match c {
            '*' => {
                // Several stars elsewhere are one star, as in git.
                if out.last() != Some(&Token::Star) {
                    out.push(Token::Star);
                }
                i += 1;
            }
            '?' => {
                out.push(Token::One);
                i += 1;
            }
            '[' => {
                let (tok, next) = class(&chars, i)?;
                out.push(tok);
                i = next;
            }
            '\\' if i + 1 < chars.len() => {
                out.push(Token::Lit(chars[i + 1]));
                i += 2;
            }
            _ => {
                out.push(Token::Lit(c));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Reads a `[...]` class starting at `start`; returns it and the index
/// after the closing bracket.
fn class(chars: &[char], start: usize) -> Result<(Token, usize), ()> {
    let mut i = start + 1;
    let mut neg = false;
    if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
        neg = true;
        i += 1;
    }
    let mut ranges = Vec::new();
    let mut first = true;
    while i < chars.len() {
        let c = chars[i];
        if c == ']' && !first {
            return Ok((Token::Class(neg, ranges), i + 1));
        }
        first = false;
        if i + 2 < chars.len() && chars[i + 1] == '-' && chars[i + 2] != ']' {
            ranges.push((c, chars[i + 2]));
            i += 3;
        } else {
            ranges.push((c, c));
            i += 1;
        }
    }
    Err(())
}

/// Whether `tokens` match all of `text`. Dynamic programming over
/// (token, character) positions: linear in their product, never
/// exponential whatever the pattern.
fn matches(tokens: &[Token], text: &[char]) -> bool {
    let n = text.len();
    // can[j]: tokens[..i] can match text[..j], for the current i.
    let mut can = vec![false; n + 1];
    can[0] = true;
    for tok in tokens {
        let mut next = vec![false; n + 1];
        match tok {
            Token::Lit(c) => {
                for j in 0..n {
                    if can[j] && text[j] == *c {
                        next[j + 1] = true;
                    }
                }
            }
            Token::One => {
                for j in 0..n {
                    if can[j] && text[j] != '/' {
                        next[j + 1] = true;
                    }
                }
            }
            Token::Class(neg, ranges) => {
                for j in 0..n {
                    if can[j] && text[j] != '/' {
                        let inside = ranges.iter().any(|(a, b)| *a <= text[j] && text[j] <= *b);
                        if inside != *neg {
                            next[j + 1] = true;
                        }
                    }
                }
            }
            Token::Star => {
                // Extend any reachable position across characters but `/`.
                for j in 0..=n {
                    if can[j] || (j > 0 && next[j - 1] && text[j - 1] != '/') {
                        next[j] = true;
                    }
                }
            }
            Token::AnyAll => {
                let mut on = false;
                for j in 0..=n {
                    on |= can[j];
                    next[j] = on;
                }
            }
            Token::AnyDirs => {
                // Empty, or any text ending in `/` after a reachable point.
                let mut on = false;
                for j in 0..=n {
                    if can[j] {
                        next[j] = true;
                        on = true;
                    }
                    if on && j > 0 && text[j - 1] == '/' {
                        next[j] = true;
                    }
                }
            }
        }
        can = next;
    }
    can[n]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(lines: &[&str]) -> IgnoreRules {
        let (r, bad) = IgnoreRules::new(lines);
        assert!(bad.is_empty(), "{bad:?}");
        r
    }

    fn ig(r: &IgnoreRules, path: &str, dir: bool) -> bool {
        let parts: Vec<&str> = path.split('/').collect();
        r.is_ignored(&parts, dir)
    }

    #[test]
    fn the_default_list() {
        let r = rules(DEFAULT_RULES);
        assert!(ig(&r, "~$Budget.xlsx", false));
        assert!(ig(&r, "a/b/.~lock.Budget.ods#", false));
        assert!(ig(&r, "x/y.tmp", false));
        assert!(ig(&r, ".DS_Store", false));
        assert!(ig(&r, "deep/Thumbs.db", false));
        assert!(!ig(&r, "Budget.xlsx", false));
        assert!(!ig(&r, "tmp", false));
        assert!(!ig(&r, "x.tmpl", false));
    }

    #[test]
    fn anchoring_and_folders() {
        let r = rules(&["/build", "logs/", "doc/frotz"]);
        assert!(ig(&r, "build", true));
        assert!(!ig(&r, "src/build", true));
        assert!(ig(&r, "logs", true));
        assert!(!ig(&r, "logs", false), "a file named logs is not a folder");
        assert!(ig(&r, "a/logs/x.txt", false), "inside an ignored folder");
        assert!(ig(&r, "doc/frotz", false));
        assert!(!ig(&r, "a/doc/frotz", false));
    }

    #[test]
    fn double_stars() {
        let r = rules(&["**/foo", "abc/**", "a/**/b"]);
        assert!(ig(&r, "foo", false));
        assert!(ig(&r, "x/y/foo", false));
        assert!(ig(&r, "abc/x", false));
        assert!(ig(&r, "abc/x/y", false));
        assert!(!ig(&r, "abc", true), "abc/** is what is inside abc");
        assert!(ig(&r, "a/b", false));
        assert!(ig(&r, "a/x/b", false));
        assert!(ig(&r, "a/x/y/b", false));
        assert!(!ig(&r, "a/xb", false));
    }

    #[test]
    fn negation_and_order() {
        let r = rules(&["*.log", "!keep.log"]);
        assert!(ig(&r, "a.log", false));
        assert!(!ig(&r, "keep.log", false));
        // Nothing inside an ignored folder comes back.
        let r = rules(&["out/", "!out/keep"]);
        assert!(ig(&r, "out/keep", false));
    }

    #[test]
    fn classes_and_escapes() {
        let r = rules(&["file[0-9].txt", "[!a]x", "\\#hash", "\\!bang"]);
        assert!(ig(&r, "file7.txt", false));
        assert!(!ig(&r, "fileA.txt", false));
        assert!(ig(&r, "bx", false));
        assert!(!ig(&r, "ax", false));
        assert!(ig(&r, "#hash", false));
        assert!(ig(&r, "!bang", false));
        let (_, bad) = IgnoreRules::new(&["[unclosed"]);
        assert_eq!(bad, vec!["[unclosed".to_owned()]);
    }

    #[test]
    fn star_does_not_cross_folders() {
        let r = rules(&["a/*.txt"]);
        assert!(ig(&r, "a/x.txt", false));
        assert!(!ig(&r, "a/b/x.txt", false));
    }

    #[test]
    fn pathological_patterns_are_fast() {
        let pat = "*a*a*a*a*a*a*a*a*a*a*a*a*b";
        let r = rules(&[pat]);
        let name = "a".repeat(5000);
        assert!(!ig(&r, &name, false));
    }
}
