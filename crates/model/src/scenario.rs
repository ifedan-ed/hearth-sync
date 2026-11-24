//! Scenarios: what people, the server and the world do in one run, as
//! data and as plain text.
//!
//! A scenario is abstract: it names paths and content tokens, never
//! bytes, and says nothing about how the engine works. The simulator
//! (crates/sim) runs it against the engine; the model's own tests run it
//! against a naive "perfect sync". The text form is what the seed archive
//! keeps, so a failing run can be read, edited by hand and replayed
//! (sync.md 7.2, "Shrinking"; docs/model.md, "The scenario format").

use std::fmt::Write as _;

use crate::name::Volume;

/// The first line of every scenario text: the format and its version.
pub const HEADER: &str = "hearth-sync-scenario 1";

/// One device taking part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSpec {
    /// The name given at sign-in; it appears in conflict copy names.
    pub name: String,
    /// The sync root's volume.
    pub volume: Volume,
    /// The device's offset from UTC, seconds (conflict copy stamps).
    pub utc_offset_secs: i64,
}

/// A mount other than home (a share, a team folder, external storage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    /// Where it sits in the person's tree (sync-api 4, `path`).
    pub path: String,
    /// A delete there goes to Hearth's trash.
    pub trash: bool,
    /// A replaced content there is kept as a version.
    pub versions: bool,
    /// The person's rights bits (sync-api 4).
    pub perms: u8,
    /// Whether it exists when the run starts (else it arrives later by
    /// [`AdminOp::ShareArrive`]).
    pub at_start: bool,
}

/// Who acts: a person on one device, or a person on Hearth's web app
/// ("a web user acting on the server directly", sync.md 7.3).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Side {
    /// A person working in the device's synced folder.
    Device(String),
    /// A person in the web app (writes straight to Hearth).
    Web,
}

/// What a person does. Paths are `/`-separated names from the sync root
/// (or the person's tree on the web). An act whose target is missing when
/// it runs is skipped and recorded so (see docs/model.md): shrinking and
/// timing can remove what an act needed, and that must not fail a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// A new file with this content.
    Create {
        /// Where.
        path: String,
        /// The content token.
        content: String,
    },
    /// New content for an existing file.
    Edit {
        /// Which file.
        path: String,
        /// The content token.
        content: String,
    },
    /// A new folder.
    Mkdir {
        /// Where.
        path: String,
    },
    /// Rename or move (also case-only renames, and folders with all they
    /// hold).
    Rename {
        /// From.
        from: String,
        /// To.
        to: String,
    },
    /// Delete a file, or a folder with all it holds.
    Delete {
        /// Which.
        path: String,
    },
    /// Copy a file (a second file with the same content).
    Copy {
        /// From.
        from: String,
        /// To.
        to: String,
    },
    /// Replace a file by a folder of the same name, or a folder (and all
    /// it holds) by a file with `content`.
    Retype {
        /// Which.
        path: String,
        /// The new file's content (unused when a file becomes a folder).
        content: String,
    },
}

/// What the server's operator or another person does to Hearth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminOp {
    /// A share (a mount of the scenario) is given to the person.
    ShareArrive {
        /// The mount's path.
        path: String,
    },
    /// The share is withdrawn or left (sync-api 4, last rule).
    ShareLeave {
        /// The mount's path.
        path: String,
    },
    /// Hearth is restored from a backup taken at the previous `settle`;
    /// the epoch changes (sync-api 6.8, sync.md 3.11).
    Restore,
    /// Hearth restarts (in-flight requests fail).
    Restart,
    /// Every device's cursor expires by retention (sync-api 6.8).
    CursorExpire,
    /// The person empties Hearth's trash.
    EmptyTrash,
    /// Hearth refuses every write to `path` with `code` from now on
    /// (`locked` 423, `quota` 507, `virus`, `forbidden` 403; I10).
    Refuse {
        /// The refused path.
        path: String,
        /// The refusal code.
        code: String,
    },
}

/// A fault on one device. "At n" means: at the device's n-th request (or
/// disk call) after this point; the simulator numbers them (sync.md 7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultOp {
    /// The device loses the network until `Online`.
    Offline,
    /// The network is back.
    Online,
    /// The connection is reset at the n-th request.
    Drop(u32),
    /// The n-th request stalls beyond the timeout.
    Stall(u32),
    /// The n-th request is done by Hearth but its answer is lost.
    LoseAnswer(u32),
    /// The process is killed at the n-th disk call and restarted.
    Crash(u32),
    /// The disk is full at the n-th write.
    DiskFull(u32),
    /// Permission denied on this path (locally).
    Deny(String),
    /// The device's clock moves by this many seconds.
    Skew(i64),
    /// The n-th download's bytes are altered in flight.
    Corrupt(u32),
    /// The sync root disappears (unplugged disk, unmounted share).
    RootVanish,
    /// It comes back as it was.
    RootBack,
    /// An empty folder without the root marker takes its place.
    RootReplaced,
    /// The state database is deleted (I12).
    DbDeleted,
}

/// One step of a scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// A person acts.
    Person {
        /// On which device, or on the web.
        side: Side,
        /// What they do.
        act: Act,
    },
    /// The server or its operator acts.
    Admin(AdminOp),
    /// Something goes wrong on a device.
    Fault {
        /// Which device.
        device: String,
        /// What.
        fault: FaultOp,
    },
    /// The person answers a pause (mass change, root, instance) with
    /// "go on" (sync.md 3.11).
    Confirm {
        /// Which device.
        device: String,
    },
    /// Faults stop, every device comes online, everything is left to
    /// settle; then the convergence checks run (sync.md 7.3).
    Settle,
}

/// A whole scenario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    /// The seed it was generated from (0 for a hand-written one).
    pub seed: u64,
    /// The devices.
    pub devices: Vec<DeviceSpec>,
    /// The mounts besides home.
    pub mounts: Vec<MountSpec>,
    /// What happens, in order.
    pub ops: Vec<Op>,
}

/// Quotes a string for the text form: `"..."` with `\\`, `\"` and
/// `\u{hex}` for anything not printable ASCII-safe, so every name of the
/// corpus (controls, bidi marks, NFD, lone combining marks) survives a
/// round trip byte for byte and is visible to a reader.
pub fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            ' '..='~' => out.push(c),
            _ => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
        }
    }
    out.push('"');
    out
}

impl Scenario {
    /// The scenario as text (see docs/model.md for the grammar).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "{HEADER}");
        let _ = writeln!(out, "seed {}", self.seed);
        for d in &self.devices {
            let _ = writeln!(out, "device {} {} {:+}", quote(&d.name), d.volume.code(), d.utc_offset_secs);
        }
        for m in &self.mounts {
            let _ = writeln!(
                out,
                "mount {} trash={} versions={} perms={} at-start={}",
                quote(&m.path),
                yes(m.trash),
                yes(m.versions),
                m.perms,
                yes(m.at_start)
            );
        }
        let _ = writeln!(out, "---");
        for op in &self.ops {
            let _ = writeln!(out, "{}", op_text(op));
        }
        out
    }

    /// Reads the text form. The error names the line and what was wrong.
    pub fn parse(text: &str) -> Result<Scenario, String> {
        let mut lines = text.lines().enumerate().filter(|(_, l)| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#')
        });
        match lines.next() {
            Some((_, l)) if l.trim() == HEADER => {}
            _ => return Err(format!("line 1: expected {HEADER:?}")),
        }
        let mut sc = Scenario {
            seed: 0,
            devices: Vec::new(),
            mounts: Vec::new(),
            ops: Vec::new(),
        };
        let mut body = false;
        for (n, line) in lines {
            let at = |e: String| format!("line {}: {e}", n + 1);
            let words = split_words(line).map_err(at)?;
            let w: Vec<&str> = words.iter().map(String::as_str).collect();
            if !body {
                match w.as_slice() {
                    ["---"] => body = true,
                    ["seed", s] => sc.seed = s.parse().map_err(|_| at(format!("bad seed {s:?}")))?,
                    ["device", name, vol, off] => sc.devices.push(DeviceSpec {
                        name: name.to_string(),
                        volume: Volume::from_code(vol).ok_or_else(|| at(format!("unknown volume {vol:?}")))?,
                        utc_offset_secs: off.parse().map_err(|_| at(format!("bad offset {off:?}")))?,
                    }),
                    ["mount", path, rest @ ..] => sc.mounts.push(parse_mount(path, rest).map_err(at)?),
                    _ => return Err(at(format!("unexpected {line:?} before ---"))),
                }
            } else {
                sc.ops.push(parse_op(&w, &sc).map_err(at)?);
            }
        }
        if !body {
            return Err("no --- line".into());
        }
        Ok(sc)
    }

    /// The device named `name`.
    pub fn device(&self, name: &str) -> Option<&DeviceSpec> {
        self.devices.iter().find(|d| d.name == name)
    }
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn side_text(side: &Side) -> String {
    match side {
        Side::Device(d) => quote(d),
        Side::Web => "web".into(),
    }
}

/// One op as one line.
pub fn op_text(op: &Op) -> String {
    match op {
        Op::Person { side, act } => {
            let s = side_text(side);
            match act {
                Act::Create { path, content } => format!("{s} create {} {content}", quote(path)),
                Act::Edit { path, content } => format!("{s} edit {} {content}", quote(path)),
                Act::Mkdir { path } => format!("{s} mkdir {}", quote(path)),
                Act::Rename { from, to } => format!("{s} rename {} {}", quote(from), quote(to)),
                Act::Delete { path } => format!("{s} delete {}", quote(path)),
                Act::Copy { from, to } => format!("{s} copy {} {}", quote(from), quote(to)),
                Act::Retype { path, content } => format!("{s} retype {} {content}", quote(path)),
            }
        }
        Op::Admin(a) => match a {
            AdminOp::ShareArrive { path } => format!("admin share-arrive {}", quote(path)),
            AdminOp::ShareLeave { path } => format!("admin share-leave {}", quote(path)),
            AdminOp::Restore => "admin restore".into(),
            AdminOp::Restart => "admin restart".into(),
            AdminOp::CursorExpire => "admin cursor-expire".into(),
            AdminOp::EmptyTrash => "admin empty-trash".into(),
            AdminOp::Refuse { path, code } => format!("admin refuse {} {code}", quote(path)),
        },
        Op::Fault { device, fault } => {
            let d = quote(device);
            match fault {
                FaultOp::Offline => format!("fault {d} offline"),
                FaultOp::Online => format!("fault {d} online"),
                FaultOp::Drop(n) => format!("fault {d} drop {n}"),
                FaultOp::Stall(n) => format!("fault {d} stall {n}"),
                FaultOp::LoseAnswer(n) => format!("fault {d} lose-answer {n}"),
                FaultOp::Crash(n) => format!("fault {d} crash {n}"),
                FaultOp::DiskFull(n) => format!("fault {d} disk-full {n}"),
                FaultOp::Deny(p) => format!("fault {d} deny {}", quote(p)),
                FaultOp::Skew(s) => format!("fault {d} skew {s:+}"),
                FaultOp::Corrupt(n) => format!("fault {d} corrupt {n}"),
                FaultOp::RootVanish => format!("fault {d} root-vanish"),
                FaultOp::RootBack => format!("fault {d} root-back"),
                FaultOp::RootReplaced => format!("fault {d} root-replaced"),
                FaultOp::DbDeleted => format!("fault {d} db-deleted"),
            }
        }
        Op::Confirm { device } => format!("confirm {}", quote(device)),
        Op::Settle => "settle".into(),
    }
}

fn parse_mount(path: &str, rest: &[&str]) -> Result<MountSpec, String> {
    let mut m = MountSpec {
        path: path.to_string(),
        trash: true,
        versions: true,
        perms: 31,
        at_start: true,
    };
    let mut seen = Vec::new();
    for kv in rest {
        let (k, v) = kv.split_once('=').ok_or(format!("expected key=value, got {kv:?}"))?;
        let flag = || match v {
            "yes" => Ok(true),
            "no" => Ok(false),
            _ => Err(format!("{k} must be yes or no")),
        };
        match k {
            "trash" => m.trash = flag()?,
            "versions" => m.versions = flag()?,
            "perms" => m.perms = v.parse().map_err(|_| format!("bad perms {v:?}"))?,
            "at-start" => m.at_start = flag()?,
            _ => return Err(format!("unknown mount key {k:?}")),
        }
        seen.push(k);
    }
    // One explicit setting per thing: every key must be written.
    for k in ["trash", "versions", "perms", "at-start"] {
        if !seen.contains(&k) {
            return Err(format!("mount needs {k}="));
        }
    }
    Ok(m)
}

fn parse_op(w: &[&str], sc: &Scenario) -> Result<Op, String> {
    let device = |d: &str| -> Result<String, String> {
        sc.device(d)
            .map(|x| x.name.clone())
            .ok_or_else(|| format!("unknown device {d:?}"))
    };
    let num = |s: &str| s.parse::<u32>().map_err(|_| format!("bad number {s:?}"));
    Ok(match w {
        ["settle"] => Op::Settle,
        ["confirm", d] => Op::Confirm { device: device(d)? },
        ["admin", rest @ ..] => Op::Admin(match rest {
            ["share-arrive", p] => AdminOp::ShareArrive { path: p.to_string() },
            ["share-leave", p] => AdminOp::ShareLeave { path: p.to_string() },
            ["restore"] => AdminOp::Restore,
            ["restart"] => AdminOp::Restart,
            ["cursor-expire"] => AdminOp::CursorExpire,
            ["empty-trash"] => AdminOp::EmptyTrash,
            ["refuse", p, code] => AdminOp::Refuse {
                path: p.to_string(),
                code: code.to_string(),
            },
            _ => return Err(format!("unknown admin op {rest:?}")),
        }),
        ["fault", d, rest @ ..] => Op::Fault {
            device: device(d)?,
            fault: match rest {
                ["offline"] => FaultOp::Offline,
                ["online"] => FaultOp::Online,
                ["drop", n] => FaultOp::Drop(num(n)?),
                ["stall", n] => FaultOp::Stall(num(n)?),
                ["lose-answer", n] => FaultOp::LoseAnswer(num(n)?),
                ["crash", n] => FaultOp::Crash(num(n)?),
                ["disk-full", n] => FaultOp::DiskFull(num(n)?),
                ["deny", p] => FaultOp::Deny(p.to_string()),
                ["skew", s] => FaultOp::Skew(s.parse().map_err(|_| format!("bad skew {s:?}"))?),
                ["corrupt", n] => FaultOp::Corrupt(num(n)?),
                ["root-vanish"] => FaultOp::RootVanish,
                ["root-back"] => FaultOp::RootBack,
                ["root-replaced"] => FaultOp::RootReplaced,
                ["db-deleted"] => FaultOp::DbDeleted,
                _ => return Err(format!("unknown fault {rest:?}")),
            },
        },
        [who, verb, args @ ..] => {
            let side = if *who == "web" {
                Side::Web
            } else {
                Side::Device(device(who)?)
            };
            let act = match (*verb, args) {
                ("create", [p, c]) => Act::Create {
                    path: p.to_string(),
                    content: c.to_string(),
                },
                ("edit", [p, c]) => Act::Edit {
                    path: p.to_string(),
                    content: c.to_string(),
                },
                ("mkdir", [p]) => Act::Mkdir { path: p.to_string() },
                ("rename", [a, b]) => Act::Rename {
                    from: a.to_string(),
                    to: b.to_string(),
                },
                ("delete", [p]) => Act::Delete { path: p.to_string() },
                ("copy", [a, b]) => Act::Copy {
                    from: a.to_string(),
                    to: b.to_string(),
                },
                ("retype", [p, c]) => Act::Retype {
                    path: p.to_string(),
                    content: c.to_string(),
                },
                _ => return Err(format!("unknown act {verb:?} with {} arguments", args.len())),
            };
            Op::Person { side, act }
        }
        _ => return Err(format!("unknown line {w:?}")),
    })
}

/// Splits a line into words; a quoted word may hold spaces and escapes.
fn split_words(line: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = line.trim().chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == ' ' {
            chars.next();
            continue;
        }
        if c == '"' {
            chars.next();
            let mut word = String::new();
            loop {
                match chars.next() {
                    None => return Err("unclosed quote".into()),
                    Some('"') => break,
                    Some('\\') => match chars.next() {
                        Some('\\') => word.push('\\'),
                        Some('"') => word.push('"'),
                        Some('u') => {
                            if chars.next() != Some('{') {
                                return Err("expected { after \\u".into());
                            }
                            let hex: String = chars.by_ref().take_while(|c| *c != '}').collect();
                            let code = u32::from_str_radix(&hex, 16).map_err(|_| format!("bad \\u{{{hex}}}"))?;
                            word.push(char::from_u32(code).ok_or(format!("not a character: {hex}"))?);
                        }
                        other => return Err(format!("unknown escape {other:?}")),
                    },
                    Some(c) => word.push(c),
                }
            }
            out.push(word);
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c == ' ' {
                    break;
                }
                word.push(c);
                chars.next();
            }
            out.push(word);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips() {
        let sc = Scenario {
            seed: 9,
            devices: vec![DeviceSpec {
                name: "Lap top".into(),
                volume: Volume::Ntfs,
                utc_offset_secs: -18000,
            }],
            mounts: vec![MountSpec {
                path: "Shared/T".into(),
                trash: false,
                versions: true,
                perms: 1,
                at_start: false,
            }],
            ops: vec![
                Op::Person {
                    side: Side::Device("Lap top".into()),
                    act: Act::Create {
                        path: "e\u{301}\u{200F}\"\\.txt".into(),
                        content: "c1".into(),
                    },
                },
                Op::Person {
                    side: Side::Web,
                    act: Act::Rename {
                        from: "a".into(),
                        to: "A".into(),
                    },
                },
                Op::Fault {
                    device: "Lap top".into(),
                    fault: FaultOp::Skew(-3600),
                },
                Op::Admin(AdminOp::Refuse {
                    path: "x".into(),
                    code: "locked".into(),
                }),
                Op::Settle,
            ],
        };
        let text = sc.to_text();
        assert_eq!(Scenario::parse(&text), Ok(sc));
    }

    #[test]
    fn errors_name_the_line() {
        let bad = format!("{HEADER}\nseed 1\n---\n\"X\" create \"a\" c1\n");
        assert!(Scenario::parse(&bad).unwrap_err().starts_with("line 4"));
    }
}
