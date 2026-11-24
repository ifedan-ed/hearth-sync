//! The store contract as tests, run against every implementation: the
//! in-memory store in this crate's tests and the SQLite store in its
//! crate's tests. Two implementations that pass the same suite, and agree
//! with each other on random batches, are what lets the simulator's runs on
//! `MemStore` speak for the real database.
//!
//! Each function takes a constructor of fresh, empty stores and panics with
//! a description (and the seed, for random cases) when the store breaks a
//! promise. Random cases use a small seeded generator so a failure replays
//! from the seed printed.

use crate::ids::{
    ConflictId, Cursor, Etag, FileId, Hash, HeldId, Kind, MountId, NodeId, OpId, Perms, RemoteId,
};
use crate::memstore::MemStore;
use crate::names::{LocalName, LocalPath, NameKey};
use crate::store::{MetaKey, Store, StoreError, Write, check_consistency};
use crate::tree::{
    Condition, ConflictRow, Crypt, HeldReason, HeldRow, ListReason, Listing, LocalRow, MountKind,
    MountRow, OpKind, OpRow, OpState, ProblemKind, ProblemRow, PurgeBasis, RemoteRow, Subject,
    SyncedRow, TransferRow,
};

/// A complete, deterministic text of everything a store holds, for
/// comparing two stores or a store before and after a failed batch.
pub fn dump(store: &dyn Store) -> String {
    let mut out = String::new();
    for k in MetaKey::ALL {
        if let Ok(Some(v)) = store.meta(k) {
            out.push_str(&format!("meta {} = {v}\n", k.as_str()));
        }
    }
    for m in store.mounts().unwrap_or_default() {
        out.push_str(&format!("{m:?}\n"));
    }
    for n in store.all_nodes().unwrap_or_default() {
        out.push_str(&format!("{:?}\n", store.node(n).ok().flatten()));
        out.push_str(&format!(
            "  local kids {:?} synced kids {:?}\n",
            store.local_children(n).unwrap_or_default(),
            store.synced_children(n).unwrap_or_default()
        ));
    }
    for o in store.ops().unwrap_or_default() {
        out.push_str(&format!("{o:?}\n"));
        out.push_str(&format!("  {:?}\n", store.transfer(o.id).ok().flatten()));
    }
    for p in store.problems().unwrap_or_default() {
        out.push_str(&format!("{p:?}\n"));
    }
    for h in store.held().unwrap_or_default() {
        out.push_str(&format!("{h:?}\n"));
    }
    for c in store.conflicts().unwrap_or_default() {
        out.push_str(&format!("{c:?}\n"));
    }
    out.push_str(&format!("dirty {:?}\n", store.dirty(None, usize::MAX)));
    out.push_str(&format!("waiting {:?}\n", store.waiting(usize::MAX)));
    for w in store.waiting(usize::MAX).unwrap_or_default() {
        out.push_str(&format!(
            "  {w} waits; waiters on it {:?}\n",
            store.waiters_on(w)
        ));
    }
    out.push_str(&format!(
        "waiters on any {:?}\n",
        store.waiters_on(crate::store::WAIT_ANY)
    ));
    out.push_str(&format!("unselected {:?}\n", store.unselected()));
    out.push_str(&format!("departed {:?}\n", store.remote_departed()));
    out
}

fn mount(id: &str, root: u64) -> MountRow {
    MountRow {
        id: MountId(id.into()),
        kind: MountKind::Home,
        storage: "home:1".into(),
        epoch: "e1".into(),
        root_remote: RemoteId(format!("r{root}")),
        root_node: NodeId(root),
        local_path: vec![],
        perms: Perms(31),
        trash: true,
        versions: true,
        prompt_ask: false,
        selected: true,
        cursor: Some(Cursor("c1".into())),
        listing: Some(Listing {
            reason: ListReason::Relist,
            page: Some("p".into()),
            cursor: None,
            generation: 3,
        }),
        caught_up: true,
        caught_up_round: 7,
    }
}

fn remote(n: u64, parent: Option<u64>, name: &str, kind: Kind) -> RemoteRow {
    RemoteRow {
        node: NodeId(n),
        id: RemoteId(format!("r{n}")),
        mount: MountId("home".into()),
        parent: parent.map(|p| RemoteId(format!("r{p}"))),
        name: name.into(),
        kind,
        size: n * 10,
        mtime_us: -5,
        etag: (kind == Kind::File).then(|| Etag(format!("e{n}"))),
        hash: (kind == Kind::File).then(|| Hash::of(name.as_bytes())),
        exec: n.is_multiple_of(2),
        seq: n,
        crypt: Crypt::None,
        departed: None,
        listed: 0,
    }
}

fn local(n: u64, parent: Option<u64>, name: &str, kind: Kind) -> LocalRow {
    LocalRow {
        node: NodeId(n),
        parent: parent.map(NodeId),
        name: LocalName(name.as_bytes().to_vec()),
        key: NameKey(name.to_lowercase()),
        kind,
        file_id: FileId {
            volume: 1,
            id: u128::from(n) << 70,
        },
        size: n,
        mtime_ns: i64::MIN + 1,
        ctime_ns: i64::MAX,
        exec: false,
        hash: (kind == Kind::File).then(|| Hash::of(&[n as u8])),
    }
}

fn synced(n: u64, parent: Option<u64>, name: &str, kind: Kind) -> SyncedRow {
    SyncedRow {
        node: NodeId(n),
        parent: parent.map(NodeId),
        name: name.into(),
        kind,
        size: n,
        hash: (kind == Kind::File).then(|| Hash::of(&[n as u8])),
        remote_etag: Some(Etag(format!("e{n}"))),
        local_file_id: FileId { volume: 2, id: 3 },
        local_mtime_ns: 9,
        exec: true,
    }
}

fn op(id: u64, node: u64) -> OpRow {
    OpRow {
        id: OpId(id),
        kind: OpKind::ALL[(id as usize) % OpKind::ALL.len()],
        node: NodeId(node),
        state: OpState::ALL[(id as usize) % OpState::ALL.len()],
        attempt: 2,
        next_try_ns: Some(-1),
        condition: Some(match id % 5 {
            0 => Condition::IfMatch(Etag("x".into())),
            1 => Condition::IfHash(Hash::of(b"c")),
            2 => Condition::IfNoneMatch,
            3 => Condition::IfSeq(u64::MAX >> 12),
            _ => Condition::UnchangedSince(Cursor("c9".into())),
        }),
        target_parent: Some(NodeId(1)),
        target_name: Some("t\u{e9}".into()),
        expect_hash: Some(Hash::of(b"t")),
        created_ns: 5,
    }
}

/// The rows of a small tree: root folder 1, folder 2 inside, files 3 and 4.
fn base_batch() -> Vec<Write> {
    let m = MountId("home".into());
    let mut b = vec![Write::PutMount(mount("home", 1))];
    for n in 1..=4 {
        b.push(Write::PutNode(NodeId(n), m.clone()));
    }
    b.push(Write::PutRemote(remote(1, None, "", Kind::Folder)));
    b.push(Write::PutLocal(local(1, None, "", Kind::Folder)));
    b.push(Write::PutSynced(synced(1, None, "", Kind::Folder)));
    b.push(Write::PutRemote(remote(2, Some(1), "Docs", Kind::Folder)));
    b.push(Write::PutLocal(local(2, Some(1), "Docs", Kind::Folder)));
    b.push(Write::PutSynced(synced(2, Some(1), "Docs", Kind::Folder)));
    b.push(Write::PutRemote(remote(3, Some(2), "a.txt", Kind::File)));
    b.push(Write::PutLocal(local(3, Some(2), "a.txt", Kind::File)));
    b.push(Write::PutSynced(synced(3, Some(2), "a.txt", Kind::File)));
    let mut r4 = remote(4, Some(2), "B.txt", Kind::File);
    r4.departed = Some(9);
    r4.listed = 4;
    b.push(Write::PutRemote(r4));
    b.push(Write::PutLocal(local(4, Some(2), "B.txt", Kind::File)));
    b
}

/// Every kind of row goes in and comes out unchanged.
pub fn round_trips_every_row<S: Store>(make: &dyn Fn() -> S) {
    let mut s = make();
    let mut b = base_batch();
    b.push(Write::PutMeta(MetaKey::InstanceId, "inst-\u{e9}".into()));
    b.push(Write::PutMeta(MetaKey::NextNode, "5".into()));
    b.push(Write::PutOp(op(10, 3)));
    b.push(Write::PutOp(op(11, 4)));
    b.push(Write::PutTransfer(TransferRow {
        op: OpId(10),
        temp: Some(LocalPath::from_strs(&[".hearthsync", "tmp", "op10"])),
        upload_id: Some("u_x".into()),
        total: u64::MAX >> 1,
        chunk_size: 8 << 20,
        done: 3,
        expect_etag: Some(Etag("e".into())),
        new_file_id: Some(FileId {
            volume: u64::MAX,
            id: u128::MAX,
        }),
        aside: Some(LocalPath(vec![LocalName(vec![0xff])])),
    }));
    b.push(Write::PutProblem(ProblemRow {
        subject: Subject::Node(NodeId(3)),
        kind: ProblemKind::Locked,
        reason: "locked by ada".into(),
        first_ns: 1,
        last_ns: 2,
        count: 3,
    }));
    b.push(Write::PutProblem(ProblemRow {
        subject: Subject::Path(LocalPath(vec![LocalName(vec![0xfe, 0x01])])),
        kind: ProblemKind::NameRefused,
        reason: "not utf8".into(),
        first_ns: 1,
        last_ns: 1,
        count: 1,
    }));
    b.push(Write::PutHeld(HeldRow {
        id: HeldId(1),
        original: LocalPath::from_strs(&["Docs", "x"]),
        held: LocalPath::from_strs(&[".hearthsync", "held", "2026-11-24", "1-x"]),
        hash: Some(Hash::of(b"x")),
        size: 1,
        reason: HeldReason::Replaced,
        basis: PurgeBasis::RemoteVersions,
        mount: MountId("home".into()),
        at_ns: 77,
    }));
    b.push(Write::PutConflict(ConflictRow {
        id: ConflictId(1),
        node: NodeId(3),
        copy: NodeId(4),
        hash: None,
        name: "a (conflict d, 2026-11-24 10.00.00).txt".into(),
        at_ns: 8,
    }));
    b.push(Write::MarkDirty(NodeId(4)));
    b.push(Write::MarkDirty(NodeId(2)));
    b.push(Write::MarkDirty(NodeId(3)));
    b.push(Write::Wait(NodeId(3), NodeId(2)));
    b.push(Write::Wait(NodeId(1), crate::store::WAIT_ANY));
    b.push(Write::Unselect(NodeId(2)));
    s.apply(b.clone()).expect("batch applies");

    let rec = s.node(NodeId(3)).unwrap().unwrap();
    assert_eq!(rec.remote, Some(remote(3, Some(2), "a.txt", Kind::File)));
    assert_eq!(rec.local, Some(local(3, Some(2), "a.txt", Kind::File)));
    assert_eq!(rec.synced, Some(synced(3, Some(2), "a.txt", Kind::File)));
    assert_eq!(s.mounts().unwrap(), vec![mount("home", 1)]);
    assert_eq!(
        s.meta(MetaKey::InstanceId).unwrap().as_deref(),
        Some("inst-\u{e9}")
    );
    assert_eq!(s.ops().unwrap(), vec![op(10, 3), op(11, 4)]);
    assert_eq!(s.transfer(OpId(10)).unwrap().unwrap().total, u64::MAX >> 1);
    assert_eq!(s.problems().unwrap().len(), 2);
    assert_eq!(s.held().unwrap().len(), 1);
    assert_eq!(s.conflicts().unwrap().len(), 1);
    assert_eq!(s.dirty(None, 10).unwrap(), vec![NodeId(2), NodeId(4)]);
    assert_eq!(s.dirty(None, 1).unwrap(), vec![NodeId(2)]);
    assert_eq!(s.dirty(Some(NodeId(2)), 5).unwrap(), vec![NodeId(4)]);
    assert_eq!(s.waiting(10).unwrap(), vec![NodeId(1), NodeId(3)]);
    assert_eq!(s.waiters_on(NodeId(2)).unwrap(), vec![NodeId(3)]);
    assert_eq!(
        s.waiters_on(crate::store::WAIT_ANY).unwrap(),
        vec![NodeId(1)]
    );
    assert_eq!(s.unselected().unwrap(), vec![NodeId(2)]);
    assert_eq!(
        s.node_by_remote(&RemoteId("r4".into())).unwrap(),
        Some(NodeId(4))
    );
    assert_eq!(s.remote_departed().unwrap(), vec![NodeId(4)]);
    assert_eq!(
        s.nodes_by_file_id(local(4, None, "", Kind::File).file_id)
            .unwrap(),
        vec![NodeId(4)]
    );
    assert_eq!(
        s.local_children(NodeId(2)).unwrap(),
        vec![NodeId(3), NodeId(4)]
    );
    assert_eq!(
        s.local_child_by_key(NodeId(2), &NameKey("b.txt".into()))
            .unwrap(),
        Some(NodeId(4))
    );
    assert_eq!(
        s.remote_children(&RemoteId("r2".into())).unwrap(),
        vec![NodeId(3), NodeId(4)]
    );
    assert_eq!(s.synced_children(NodeId(2)).unwrap(), vec![NodeId(3)]);
    assert_eq!(s.count_nodes(&MountId("home".into())).unwrap(), 4);
    assert!(check_consistency(&s).unwrap().is_empty());

    // The same batch on the reference store gives the same dump.
    let mut reference = MemStore::new();
    reference.apply(b).unwrap();
    assert_eq!(dump(&s), dump(&reference));
}

/// A batch that breaks a constraint changes nothing.
pub fn constraints_refuse_whole_batches<S: Store>(make: &dyn Fn() -> S) {
    let mut s = make();
    s.apply(base_batch()).unwrap();
    let before = dump(&s);
    // Two names with one key in one folder.
    let mut dup = local(4, Some(2), "A.TXT", Kind::File);
    dup.key = NameKey("a.txt".into());
    let r = s.apply(vec![Write::MarkDirty(NodeId(1)), Write::PutLocal(dup)]);
    assert!(matches!(r, Err(StoreError::Constraint(_))), "{r:?}");
    assert_eq!(dump(&s), before);
    // One remote id on two nodes.
    let mut r3 = remote(4, Some(2), "B.txt", Kind::File);
    r3.id = RemoteId("r3".into());
    assert!(s.apply(vec![Write::PutRemote(r3)]).is_err());
    assert_eq!(dump(&s), before);
    // A transfer without its operation.
    let t = TransferRow {
        op: OpId(99),
        temp: None,
        upload_id: None,
        total: 0,
        chunk_size: 0,
        done: 0,
        expect_etag: None,
        new_file_id: None,
        aside: None,
    };
    assert!(s.apply(vec![Write::PutTransfer(t)]).is_err());
    // A node removed while an operation acts on it, or while it has
    // children.
    s.apply(vec![Write::PutOp(op(1, 3))]).unwrap();
    let before = dump(&s);
    assert!(s.apply(vec![Write::DelNode(NodeId(3))]).is_err());
    assert!(s.apply(vec![Write::DelNode(NodeId(2))]).is_err());
    assert!(s.apply(vec![Write::DelLocal(NodeId(2))]).is_err());
    // Rows on a node that does not exist.
    assert!(
        s.apply(vec![Write::PutLocal(local(50, Some(2), "z", Kind::File))])
            .is_err()
    );
    assert_eq!(dump(&s), before);
    // The same key moved between nodes in one batch is fine when the old
    // holder lets go first.
    let mut moved = local(4, Some(2), "a.txt", Kind::File);
    moved.key = NameKey("a.txt".into());
    s.apply(vec![
        Write::DelOp(OpId(1)),
        Write::DelLocal(NodeId(3)),
        Write::PutLocal(moved),
    ])
    .unwrap();
    assert_eq!(
        s.local_child_by_key(NodeId(2), &NameKey("a.txt".into()))
            .unwrap(),
        Some(NodeId(4))
    );
}

/// Removing a node removes everything about it.
pub fn removing_a_node_cascades<S: Store>(make: &dyn Fn() -> S) {
    let mut s = make();
    s.apply(base_batch()).unwrap();
    s.apply(vec![
        Write::MarkDirty(NodeId(4)),
        Write::PutProblem(ProblemRow {
            subject: Subject::Node(NodeId(4)),
            kind: ProblemKind::Quota,
            reason: String::new(),
            first_ns: 0,
            last_ns: 0,
            count: 1,
        }),
        Write::DelNode(NodeId(4)),
    ])
    .unwrap();
    assert!(s.node(NodeId(4)).unwrap().is_none());
    assert!(s.node_by_remote(&RemoteId("r4".into())).unwrap().is_none());
    assert!(s.dirty(None, 10).unwrap().is_empty());
    assert!(s.problems().unwrap().is_empty());
    assert_eq!(s.local_children(NodeId(2)).unwrap(), vec![NodeId(3)]);
    assert!(check_consistency(&s).unwrap().is_empty());
}

/// A failure injected at any write of a batch leaves the store exactly as
/// before the batch, and the same batch then applies in full (the crash
/// safety the engine relies on: docs/sync.md 3.2, I11).
pub fn injected_failures_at_every_write_point<S: Store>(make: &dyn Fn() -> S) {
    let mut full = base_batch();
    full.push(Write::PutOp(op(1, 3)));
    full.push(Write::MarkDirty(NodeId(3)));
    let second = vec![
        Write::DelOp(OpId(1)),
        Write::PutLocal(local(3, Some(2), "renamed", Kind::File)),
        Write::PutSynced(synced(3, Some(1), "renamed", Kind::File)),
        Write::DelRemote(NodeId(4)),
        Write::DelLocal(NodeId(4)),
        Write::DelNode(NodeId(4)),
        Write::PutMeta(MetaKey::NextOp, "2".into()),
        Write::ClearDirty(NodeId(3)),
    ];
    let mut expected = MemStore::new();
    expected.apply(full.clone()).unwrap();
    expected.apply(second.clone()).unwrap();
    for k in 0..second.len() {
        let mut s = make();
        s.apply(full.clone()).unwrap();
        let before = dump(&s);
        s.fail_writes_after(Some(k));
        assert_eq!(
            s.apply(second.clone()),
            Err(StoreError::Injected),
            "failure at write {k}"
        );
        assert_eq!(dump(&s), before, "state after a failure at write {k}");
        assert!(check_consistency(&s).unwrap().is_empty());
        s.fail_writes_after(None);
        s.apply(second.clone()).unwrap();
        assert_eq!(dump(&s), dump(&expected), "after the retry of failure {k}");
    }
}

/// splitmix64: a tiny seeded generator, so a failing random case replays
/// from its printed seed without any outside crate.
pub struct Rng(pub u64);

impl Rng {
    /// The next number.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number below `n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }
}

/// A random write over a small universe of nodes, so constraints are hit
/// often.
fn random_write(rng: &mut Rng) -> Write {
    let n = 1 + rng.below(8);
    let p = 1 + rng.below(8);
    let names = ["a", "b", "A", "c\u{e9}"];
    let name = names[rng.below(4) as usize];
    let kind = if rng.below(3) == 0 {
        Kind::Folder
    } else {
        Kind::File
    };
    match rng.below(16) {
        0 | 1 => Write::PutNode(
            NodeId(n),
            MountId(["home", "share"][rng.below(2) as usize].into()),
        ),
        2 => Write::DelNode(NodeId(n)),
        3 => Write::PutRemote(remote(n, Some(p), name, kind)),
        4 => Write::DelRemote(NodeId(n)),
        5 | 6 => {
            let mut l = local(n, Some(p), name, kind);
            l.key = NameKey(name.to_lowercase());
            Write::PutLocal(l)
        }
        7 => Write::DelLocal(NodeId(n)),
        8 => Write::PutSynced(synced(n, Some(p), name, kind)),
        9 => Write::DelSynced(NodeId(n)),
        10 => Write::PutOp(op(rng.below(4), n)),
        11 => Write::DelOp(OpId(rng.below(4))),
        12 => Write::MarkDirty(NodeId(n)),
        13 => Write::Wait(NodeId(n), NodeId(rng.below(9))),
        14 => Write::ClearDirty(NodeId(n)),
        _ => Write::PutMeta(MetaKey::NextNode, rng.below(100).to_string()),
    }
}

/// Random batches, some with injected failures, applied to the store under
/// test and to the reference: the two must agree after every batch, on
/// success, on refusal and on failure alike.
pub fn agrees_with_the_reference_on_random_batches<S: Store>(
    make: &dyn Fn() -> S,
    seeds: std::ops::Range<u64>,
) {
    for seed in seeds {
        let mut rng = Rng(seed);
        let mut s = make();
        let mut reference = MemStore::new();
        for step in 0..40 {
            let len = 1 + rng.below(6) as usize;
            let batch: Vec<Write> = (0..len).map(|_| random_write(&mut rng)).collect();
            let fail = (rng.below(4) == 0).then(|| rng.below(len as u64) as usize);
            s.fail_writes_after(fail);
            reference.fail_writes_after(fail);
            let a = s.apply(batch.clone());
            let b = reference.apply(batch.clone());
            s.fail_writes_after(None);
            reference.fail_writes_after(None);
            assert_eq!(
                a.is_ok(),
                b.is_ok(),
                "seed {seed} step {step}: {a:?} against the reference's {b:?} for {batch:?}"
            );
            assert_eq!(
                dump(&s),
                dump(&reference),
                "seed {seed} step {step}: {batch:?}"
            );
        }
    }
}

/// Runs the whole suite.
pub fn run_all<S: Store>(make: &dyn Fn() -> S, random_seeds: std::ops::Range<u64>) {
    round_trips_every_row(make);
    constraints_refuse_whole_batches(make);
    removing_a_node_cascades(make);
    injected_failures_at_every_write_point(make);
    agrees_with_the_reference_on_random_batches(make, random_seeds);
}

#[cfg(test)]
mod tests {
    #[test]
    fn mem_store_meets_the_contract() {
        super::run_all(&crate::memstore::MemStore::new, 0..300);
    }
}
