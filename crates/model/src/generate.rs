//! The scenario generator: from a seed, a random scenario weighted
//! toward the cases that break sync engines (sync.md 7.2 "Generation",
//! 7.3 "Faults injected", 7.7 the names).
//!
//! It keeps a rough picture of the person's tree (Hearth's names, as if
//! every change synced at once) only to choose targets that probably
//! exist; an act whose target is gone when it runs is skipped, so the
//! picture does not need to be exact. The weights are in [`WEIGHTS`],
//! one table, and docs/model.md shows them.

use crate::apply::{Namespace, apply_act};
use crate::corpus::{CLASH_PAIRS, NAMES, long_names};
use crate::name::Volume;
use crate::rng::Rng;
use crate::scenario::{Act, AdminOp, DeviceSpec, FaultOp, MountSpec, Op, Scenario, Side};
use crate::tree::{IdTree, NodeId, ROOT};

/// Settings of a generated scenario.
#[derive(Debug, Clone)]
pub struct GenConfig {
    /// At most this many devices (at least one).
    pub max_devices: usize,
    /// At most this many rounds, each ending in `settle`.
    pub max_rounds: usize,
    /// At most this many patterns in a round.
    pub max_patterns: usize,
    /// Percent of new names taken from the nasty-names corpus.
    pub corpus_percent: u32,
}

impl GenConfig {
    /// Small scenarios: fast to run, easy to read when they fail.
    pub fn small() -> Self {
        GenConfig {
            max_devices: 3,
            max_rounds: 4,
            max_patterns: 10,
            corpus_percent: 35,
        }
    }
}

/// The patterns the generator chooses from, one per kind of trouble.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pattern {
    /// A new file.
    Create,
    /// New content for a file.
    Edit,
    /// A new folder.
    Mkdir,
    /// A rename in place.
    Rename,
    /// A move to another folder.
    Move,
    /// A rename that changes only the case of letters.
    CaseRename,
    /// A delete.
    Delete,
    /// A copy.
    Copy,
    /// A file becomes a folder or the reverse.
    Retype,
    /// A delete, then a new item at the same name.
    DeleteRecreate,
    /// An edit, a fault on the upload, another edit.
    EditDuringUpload,
    /// Two sides edit one file.
    ConcurrentEdit,
    /// A rename of the top of a deep folder chain.
    DeepRename,
    /// Two names one device would hold as one.
    ClashPair,
    /// A network or process fault.
    Fault,
    /// The root vanishing or replaced, the database deleted, a denied path.
    HeavyFault,
    /// Something the server or its operator does.
    Admin,
    /// A delete of many files at once.
    MassDelete,
}

/// The weights of the patterns. Weighted toward edits and conflicts, and
/// toward the nasty cases (concurrent edits, delete-then-recreate, names).
pub const WEIGHTS: &[(Pattern, u32)] = &[
    (Pattern::Create, 12),
    (Pattern::Edit, 12),
    (Pattern::Mkdir, 5),
    (Pattern::Rename, 6),
    (Pattern::Move, 5),
    (Pattern::CaseRename, 3),
    (Pattern::Delete, 7),
    (Pattern::Copy, 2),
    (Pattern::Retype, 3),
    (Pattern::DeleteRecreate, 4),
    (Pattern::EditDuringUpload, 3),
    (Pattern::ConcurrentEdit, 8),
    (Pattern::DeepRename, 2),
    (Pattern::ClashPair, 3),
    (Pattern::Fault, 12),
    (Pattern::HeavyFault, 3),
    (Pattern::Admin, 4),
    (Pattern::MassDelete, 1),
];

const DEVICE_NAMES: &[&str] = &[
    "Lap",
    "Desk",
    "Phone-1",
    "My:PC|with a name over thirty characters",
];
// (At most three devices are generated, so the fourth name is only ever
// the first device's.)
const OFFSETS: &[i64] = &[0, 3600, -18_000, 19_800, 46_800];
const MOUNT: &str = "Team";

/// The generator's working state.
struct Gen {
    rng: Rng,
    cfg: GenConfig,
    world: IdTree,
    next_id: NodeId,
    content: u64,
    names: u64,
    ops: Vec<Op>,
    devices: Vec<String>,
    mount_present: bool,
    has_mount: bool,
    /// A pause is waiting for the person (a mass change, a replaced
    /// root): every `settle` is preceded by the confirmations.
    pending_confirm: bool,
}

/// A random scenario for `seed`.
pub fn generate(seed: u64, cfg: &GenConfig) -> Scenario {
    let mut g = Gen {
        rng: Rng::new(seed),
        cfg: cfg.clone(),
        world: IdTree::new(),
        next_id: 1,
        content: 0,
        names: 0,
        ops: Vec::new(),
        devices: Vec::new(),
        mount_present: false,
        has_mount: false,
        pending_confirm: false,
    };
    let n_devices = 1 + g.rng.below(cfg.max_devices.max(1));
    let mut devices = Vec::new();
    for (i, name) in DEVICE_NAMES.iter().take(n_devices).enumerate() {
        let volume = match g.rng.weighted(&[3, 2, 1, 3, 1]) {
            Some(v) => Volume::ALL[v],
            None => Volume::Ext4,
        };
        let offset = *g.rng.pick(OFFSETS).unwrap_or(&0);
        // The first device sometimes has the long, odd name that conflict
        // copy names must clean and cut (3.5.1).
        let name = if i == 0 && g.rng.chance(10) {
            DEVICE_NAMES[3]
        } else {
            name
        };
        let name = name.to_string();
        devices.push(DeviceSpec {
            name,
            volume,
            utc_offset_secs: offset,
        });
    }
    g.devices = devices.iter().map(|d| d.name.clone()).collect();
    let mut mounts = Vec::new();
    if g.rng.chance(30) {
        let at_start = g.rng.chance(70);
        mounts.push(MountSpec {
            path: MOUNT.into(),
            trash: g.rng.chance(70),
            versions: g.rng.chance(70),
            perms: *g.rng.pick(&[31u8, 31, 1, 5]).unwrap_or(&31),
            at_start,
        });
        g.has_mount = true;
        g.mount_present = at_start;
        if at_start {
            g.track_mount();
        }
    }
    g.setup();
    let rounds = 1 + g.rng.below(cfg.max_rounds.max(1));
    for _ in 0..rounds {
        g.round();
    }
    Scenario {
        seed,
        devices,
        mounts,
        ops: g.ops,
    }
}

impl Gen {
    fn track_mount(&mut self) {
        let mut next = self.next_id;
        apply_act(
            &mut self.world,
            Namespace::Hearth,
            &Act::Mkdir { path: MOUNT.into() },
            0,
            &mut next,
        );
        self.next_id = next;
    }

    fn content(&mut self) -> String {
        self.content += 1;
        format!("c{}", self.content)
    }

    fn plain_name(&mut self, folder: bool) -> String {
        self.names += 1;
        if folder {
            format!("d{}", self.names)
        } else {
            format!("f{}.txt", self.names)
        }
    }

    /// A new name: from the corpus now and then, else plain.
    fn name(&mut self, folder: bool) -> String {
        if self.rng.chance(self.cfg.corpus_percent) {
            if self.rng.chance(8) {
                let longs = long_names();
                if let Some((n, _)) = self.rng.pick(&longs) {
                    return n.clone();
                }
            }
            if let Some(n) = self.rng.pick(NAMES) {
                return n.name.to_string();
            }
        }
        self.plain_name(folder)
    }

    /// A name for an act on `side`: names like the engine's own are
    /// made only on the web (a person's own `x.hearthtmp` on a disk is
    /// left alone by the engine, docs/contract-questions.md M14).
    fn name_for(&mut self, side: &Side, folder: bool) -> String {
        let n = self.name(folder);
        if matches!(side, Side::Device(_)) && crate::name::is_engine_name(&n, true) {
            self.plain_name(folder)
        } else {
            n
        }
    }

    fn side(&mut self) -> Side {
        if self.rng.chance(30) || self.devices.is_empty() {
            Side::Web
        } else {
            let d = self.rng.pick(&self.devices).cloned().unwrap_or_default();
            Side::Device(d)
        }
    }

    fn device(&mut self) -> String {
        self.rng.pick(&self.devices).cloned().unwrap_or_default()
    }

    fn paths(&self, folders: bool) -> Vec<String> {
        self.world
            .nodes
            .iter()
            .filter(|(_, n)| n.kind.is_folder() == folders)
            .filter_map(|(id, _)| self.world.path(*id).map(|p| p.join("/")))
            .filter(|p| p != MOUNT)
            .collect()
    }

    fn folder(&mut self) -> String {
        let mut folders = self.paths(true);
        if self.mount_present {
            folders.push(MOUNT.into());
        }
        folders.push(String::new());
        self.rng.pick(&folders).cloned().unwrap_or_default()
    }

    fn existing(&mut self, folders: bool) -> Option<String> {
        let all = self.paths(folders);
        self.rng.pick(&all).cloned()
    }

    fn push(&mut self, side: Side, act: Act) {
        let mut next = self.next_id;
        apply_act(&mut self.world, Namespace::Hearth, &act, 0, &mut next);
        self.next_id = next;
        self.ops.push(Op::Person { side, act });
    }

    fn fault(&mut self, device: &str, fault: FaultOp) {
        self.ops.push(Op::Fault {
            device: device.to_string(),
            fault,
        });
    }

    fn setup(&mut self) {
        for _ in 0..1 + self.rng.below(3) {
            let path = self.plain_name(true);
            self.push(Side::Web, Act::Mkdir { path });
        }
        for _ in 0..2 + self.rng.below(5) {
            let parent = self.folder();
            let name = self.plain_name(false);
            let content = self.content();
            self.push(
                Side::Web,
                Act::Create {
                    path: join(&parent, &name),
                    content,
                },
            );
        }
        self.ops.push(Op::Settle);
    }

    fn round(&mut self) {
        let mut offline = Vec::new();
        if self.rng.chance(45) {
            for d in self.devices.clone() {
                if self.rng.chance(60) {
                    self.fault(&d, FaultOp::Offline);
                    offline.push(d);
                }
            }
        }
        let mut vanished: Vec<String> = Vec::new();
        let weights: Vec<u32> = WEIGHTS.iter().map(|(_, w)| *w).collect();
        for _ in 0..1 + self.rng.below(self.cfg.max_patterns.max(1)) {
            let Some(i) = self.rng.weighted(&weights) else {
                break;
            };
            let pattern = WEIGHTS[i].0;
            match self.pattern(pattern) {
                After::Nothing => {}
                After::Confirm => self.pending_confirm = true,
                After::RootBack(d) => vanished.push(d),
            }
        }
        for d in offline {
            self.fault(&d, FaultOp::Online);
        }
        for d in vanished {
            self.fault(&d, FaultOp::RootBack);
        }
        self.settle();
    }

    /// `settle`, after the confirmations a pause waits for.
    fn settle(&mut self) {
        if self.pending_confirm {
            for d in self.devices.clone() {
                self.ops.push(Op::Confirm { device: d });
            }
            self.pending_confirm = false;
        }
        self.ops.push(Op::Settle);
    }

    fn pattern(&mut self, p: Pattern) -> After {
        match p {
            Pattern::Create => {
                let (side, parent, content) = (self.side(), self.folder(), self.content());
                let name = self.name_for(&side, false);
                self.push(
                    side,
                    Act::Create {
                        path: join(&parent, &name),
                        content,
                    },
                );
            }
            Pattern::Edit => {
                if let Some(path) = self.existing(false) {
                    let (side, content) = (self.side(), self.content());
                    self.push(side, Act::Edit { path, content });
                }
            }
            Pattern::Mkdir => {
                let (side, parent) = (self.side(), self.folder());
                let name = self.name_for(&side, true);
                self.push(
                    side,
                    Act::Mkdir {
                        path: join(&parent, &name),
                    },
                );
            }
            Pattern::Rename => {
                let folder = self.rng.chance(30);
                if let Some(from) = self.existing(folder) {
                    let side = self.side();
                    let name = self.name_for(&side, false);
                    let to = join(parent(&from), &name);
                    self.push(side, Act::Rename { from, to });
                }
            }
            Pattern::Move => {
                let folder = self.rng.chance(40);
                if let Some(from) = self.existing(folder) {
                    let (side, folder) = (self.side(), self.folder());
                    let to = join(&folder, last(&from));
                    self.push(side, Act::Rename { from, to });
                }
            }
            Pattern::CaseRename => {
                let folder = self.rng.chance(30);
                if let Some(from) = self.existing(folder) {
                    let side = self.side();
                    let name = last(&from);
                    let flipped: String = name
                        .chars()
                        .map(|c| {
                            if c.is_lowercase() {
                                c.to_uppercase().next().unwrap_or(c)
                            } else {
                                c.to_lowercase().next().unwrap_or(c)
                            }
                        })
                        .collect();
                    let to = join(parent(&from), &flipped);
                    self.push(side, Act::Rename { from, to });
                }
            }
            Pattern::Delete => {
                let folder = self.rng.chance(25);
                if let Some(path) = self.existing(folder) {
                    let side = self.side();
                    self.push(side, Act::Delete { path });
                }
            }
            Pattern::Copy => {
                if let Some(from) = self.existing(false) {
                    let (side, folder) = (self.side(), self.folder());
                    let name = self.name_for(&side, false);
                    self.push(
                        side,
                        Act::Copy {
                            from,
                            to: join(&folder, &name),
                        },
                    );
                }
            }
            Pattern::Retype => {
                let folder = self.rng.chance(30);
                if let Some(path) = self.existing(folder) {
                    let (side, content) = (self.side(), self.content());
                    self.push(side, Act::Retype { path, content });
                }
            }
            Pattern::DeleteRecreate => {
                if let Some(path) = self.existing(false) {
                    let side = self.side();
                    self.push(side.clone(), Act::Delete { path: path.clone() });
                    let content = self.content();
                    if self.rng.chance(30) {
                        self.push(side, Act::Mkdir { path });
                    } else {
                        self.push(side, Act::Create { path, content });
                    }
                }
            }
            Pattern::EditDuringUpload => {
                if let Some(path) = self.existing(false) {
                    let d = self.device();
                    let c1 = self.content();
                    self.push(
                        Side::Device(d.clone()),
                        Act::Edit {
                            path: path.clone(),
                            content: c1,
                        },
                    );
                    let fault = match self.rng.below(3) {
                        0 => FaultOp::LoseAnswer(1),
                        1 => FaultOp::Stall(1),
                        _ => FaultOp::Crash(1 + self.rng.below(4) as u32),
                    };
                    self.fault(&d, fault);
                    let c2 = self.content();
                    self.push(Side::Device(d), Act::Edit { path, content: c2 });
                }
            }
            Pattern::ConcurrentEdit => {
                if let Some(path) = self.existing(false) {
                    let a = self.side();
                    let mut b = self.side();
                    if b == a {
                        b = Side::Web;
                    }
                    for side in [a, b] {
                        let content = self.content();
                        if self.rng.chance(15) {
                            self.push(side, Act::Delete { path: path.clone() });
                        } else {
                            self.push(
                                side,
                                Act::Edit {
                                    path: path.clone(),
                                    content,
                                },
                            );
                        }
                    }
                }
            }
            Pattern::DeepRename => {
                let side = self.side();
                let depth = 4 + self.rng.below(24);
                let mut path = self.plain_name(true);
                let top = path.clone();
                self.push(side.clone(), Act::Mkdir { path: path.clone() });
                for _ in 0..depth {
                    let n = format!("level-{}", self.rng.below(1000));
                    path = join(&path, &n);
                    self.push(side.clone(), Act::Mkdir { path: path.clone() });
                }
                let content = self.content();
                self.push(
                    side.clone(),
                    Act::Create {
                        path: join(&path, "deep.txt"),
                        content,
                    },
                );
                self.settle();
                let renamer = self.side();
                let to = self.plain_name(true);
                self.push(renamer, Act::Rename { from: top, to });
            }
            Pattern::ClashPair => {
                if let Some(&(a, b)) = self.rng.pick(CLASH_PAIRS) {
                    let folder = self.folder();
                    let (s1, s2) = (self.side(), Side::Web);
                    let (c1, c2) = (self.content(), self.content());
                    self.push(
                        s1,
                        Act::Create {
                            path: join(&folder, a),
                            content: c1,
                        },
                    );
                    self.push(
                        s2,
                        Act::Create {
                            path: join(&folder, b),
                            content: c2,
                        },
                    );
                }
            }
            Pattern::Fault => {
                let d = self.device();
                let n = 1 + self.rng.below(8) as u32;
                let fault = match self.rng.below(7) {
                    0 => FaultOp::Drop(n),
                    1 => FaultOp::Stall(n),
                    2 => FaultOp::LoseAnswer(n),
                    3 => FaultOp::Crash(n),
                    4 => FaultOp::DiskFull(n),
                    5 => FaultOp::Corrupt(n),
                    _ => FaultOp::Skew(
                        *self
                            .rng
                            .pick(&[-86_400 * 365 * 3, -3600, 3600, 86_400 * 400])
                            .unwrap_or(&0),
                    ),
                };
                self.fault(&d, fault);
            }
            Pattern::HeavyFault => {
                let d = self.device();
                match self.rng.below(4) {
                    0 => {
                        self.fault(&d, FaultOp::RootVanish);
                        return After::RootBack(d);
                    }
                    1 => {
                        self.fault(&d, FaultOp::RootReplaced);
                        return After::Confirm;
                    }
                    2 => self.fault(&d, FaultOp::DbDeleted),
                    _ => {
                        if let Some(p) = self.existing(false) {
                            self.fault(&d, FaultOp::Deny(p));
                        }
                    }
                }
            }
            Pattern::Admin => {
                let op = match self.rng.below(7) {
                    0 => AdminOp::Restart,
                    1 => AdminOp::CursorExpire,
                    2 => AdminOp::EmptyTrash,
                    3 => AdminOp::Restore,
                    4 => match self.existing(false) {
                        Some(path) => AdminOp::Refuse {
                            path,
                            code: self
                                .rng
                                .pick(&["locked", "quota", "virus", "forbidden"])
                                .copied()
                                .unwrap_or("locked")
                                .to_string(),
                        },
                        None => AdminOp::Restart,
                    },
                    _ if self.has_mount && self.mount_present => {
                        self.mount_present = false;
                        if let Some(id) = self.world.child_named(ROOT, MOUNT) {
                            for gone in self.world.subtree(id) {
                                self.world.nodes.remove(&gone);
                            }
                        }
                        AdminOp::ShareLeave { path: MOUNT.into() }
                    }
                    _ if self.has_mount => {
                        self.mount_present = true;
                        self.track_mount();
                        AdminOp::ShareArrive { path: MOUNT.into() }
                    }
                    _ => AdminOp::Restart,
                };
                self.ops.push(Op::Admin(op));
            }
            Pattern::MassDelete => {
                let side = Side::Web;
                let folder = self.plain_name(true);
                self.push(
                    side.clone(),
                    Act::Mkdir {
                        path: folder.clone(),
                    },
                );
                for i in 0..60 {
                    let content = self.content();
                    self.push(
                        side.clone(),
                        Act::Create {
                            path: format!("{folder}/m{i}.txt"),
                            content,
                        },
                    );
                }
                self.settle();
                let deleter = self.side();
                self.push(deleter, Act::Delete { path: folder });
                return After::Confirm;
            }
        }
        After::Nothing
    }
}

/// What a round must do after a pattern, before it settles.
enum After {
    Nothing,
    /// Every device confirms (a mass change or a replaced root pauses).
    Confirm,
    /// The device's root comes back.
    RootBack(String),
}

fn join(parent: &str, name: &str) -> String {
    crate::tree::join(parent, name)
}

fn parent(path: &str) -> &str {
    crate::tree::parent_path(path)
}

fn last(path: &str) -> &str {
    crate::tree::base_name(path)
}

/// The kind of an op, for the coverage report: the act, admin op or
/// fault without its arguments.
pub fn op_kind(op: &Op) -> String {
    let text = crate::scenario::op_text(op);
    let words = scenario_words(&text);
    match op {
        Op::Person { side, .. } => {
            let who = if *side == Side::Web { "web" } else { "device" };
            format!("{who} {}", words.get(1).map_or("", String::as_str))
        }
        Op::Admin(_) => format!("admin {}", words.get(1).map_or("", String::as_str)),
        Op::Fault { .. } => format!("fault {}", words.get(2).map_or("", String::as_str)),
        Op::Confirm { .. } => "confirm".into(),
        Op::Settle => "settle".into(),
    }
}

/// The words of one scenario line, with quoted words as one word each.
fn scenario_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => out.push(std::mem::take(&mut word)),
            _ => word.push(c),
        }
    }
    out.push(word);
    out
}

/// How often each kind of op appears in `scenarios`.
pub fn op_histogram(scenarios: &[Scenario]) -> std::collections::BTreeMap<String, usize> {
    let mut out = std::collections::BTreeMap::new();
    for s in scenarios {
        for op in &s.ops {
            *out.entry(op_kind(op)).or_insert(0) += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_scenario() {
        let a = generate(42, &GenConfig::small());
        let b = generate(42, &GenConfig::small());
        assert_eq!(a, b);
        assert_ne!(a, generate(43, &GenConfig::small()));
    }

    #[test]
    fn scenarios_round_trip_as_text() {
        for seed in 0..200 {
            let s = generate(seed, &GenConfig::small());
            assert_eq!(
                Scenario::parse(&s.to_text()).as_ref(),
                Ok(&s),
                "seed {seed}"
            );
            assert_eq!(s.ops.last(), Some(&Op::Settle));
        }
    }
}
