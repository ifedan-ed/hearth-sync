//! The speed budgets of docs/sync.md 7.9 that steps 1 can measure: the
//! store and the planner on a tree of a million entries. Ignored by
//! default (they take a while and a few hundred megabytes of disk); run
//! with
//!
//!     cargo test --release -p hearth-sync-store --test budgets -- --ignored --nocapture
//!
//! and record the figures in docs/performance.md. `BUDGET_NODES` changes
//! the size (default 1,000,000).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use hearth_sync_core::config::Config;
use hearth_sync_core::ids::{Etag, FileId, Hash, Kind, MountId, NodeId, Perms, RemoteId};
use hearth_sync_core::ignore::IgnoreRules;
use hearth_sync_core::names::{LocalName, NameKey, VolumeRules};
use hearth_sync_core::plan::{Decision, Planner};
use hearth_sync_core::store::{Store, Write};
use hearth_sync_core::tree::{Crypt, LocalRow, MountKind, MountRow, RemoteRow, SyncedRow};
use hearth_sync_store::SqliteStore;

fn nodes_wanted() -> u64 {
    std::env::var("BUDGET_NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000)
}

/// Resident memory now and at its peak, in MB (Linux; zero elsewhere).
fn memory_mb() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        s.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            / 1024
    };
    (field("VmRSS:"), field("VmHWM:"))
}

fn mount() -> MountRow {
    MountRow {
        id: MountId("home".into()),
        kind: MountKind::Home,
        storage: "home:1".into(),
        epoch: "e".into(),
        root_remote: RemoteId("r1".into()),
        root_node: NodeId(1),
        local_path: vec![],
        perms: Perms(31),
        trash: true,
        versions: true,
        prompt_ask: false,
        selected: true,
        cursor: Some(hearth_sync_core::ids::Cursor("c".into())),
        listing: None,
        caught_up: true,
        caught_up_round: 1,
    }
}

/// The three pictures of one node: folder `d` (1000 files each) or file.
fn rows(n: u64, folders: u64, synced: bool) -> Vec<Write> {
    let id = NodeId(n);
    let (parent, kind, name) = if n == 1 {
        (None, Kind::Folder, String::new())
    } else if n <= folders + 1 {
        (Some(1), Kind::Folder, format!("folder {n:06}"))
    } else {
        (
            Some(2 + (n % folders)),
            Kind::File,
            format!("file {n:08}.txt"),
        )
    };
    let hash = (kind == Kind::File).then(|| Hash::of(&n.to_le_bytes()));
    let mut w = vec![
        Write::PutNode(id, MountId("home".into())),
        Write::PutRemote(RemoteRow {
            node: id,
            id: RemoteId(format!("r{n}")),
            mount: MountId("home".into()),
            parent: parent.map(|p| RemoteId(format!("r{p}"))),
            name: name.clone(),
            kind,
            size: 1000,
            mtime_us: 1,
            etag: (kind == Kind::File).then(|| Etag(format!("e{n}"))),
            hash,
            exec: false,
            seq: n,
            crypt: Crypt::None,
            departed: None,
            listed: 0,
        }),
    ];
    if synced {
        let fid = FileId {
            volume: 1,
            id: u128::from(n),
        };
        w.push(Write::PutLocal(LocalRow {
            node: id,
            parent: parent.map(NodeId),
            name: LocalName(name.clone().into_bytes()),
            key: NameKey(name.clone()),
            kind,
            file_id: fid,
            size: if kind == Kind::File { 1000 } else { 0 },
            mtime_ns: 1,
            ctime_ns: 1,
            exec: false,
            hash,
        }));
        w.push(Write::PutSynced(SyncedRow {
            node: id,
            parent: parent.map(NodeId),
            name,
            kind,
            size: if kind == Kind::File { 1000 } else { 0 },
            hash,
            remote_etag: (kind == Kind::File).then(|| Etag(format!("e{n}"))),
            local_file_id: fid,
            local_mtime_ns: 1,
            exec: false,
        }));
    }
    w
}

/// Builds the store; returns the time it took.
fn build(store: &mut SqliteStore, total: u64, synced: bool) -> f64 {
    let folders = (total / 1000).max(1);
    let t = Instant::now();
    store.apply(vec![Write::PutMount(mount())]).expect("mount");
    let mut batch = Vec::new();
    for n in 1..=total {
        batch.extend(rows(n, folders, synced));
        if n % 5000 == 0 {
            store.apply(std::mem::take(&mut batch)).expect("batch");
        }
    }
    store.apply(batch).expect("batch");
    t.elapsed().as_secs_f64()
}

fn planner_pass(store: &SqliteStore, nodes: &[NodeId]) -> (f64, BTreeMap<String, u64>) {
    let cfg = Config::recommended("Bench", VolumeRules::LINUX);
    let mounts: BTreeMap<MountId, MountRow> = [(MountId("home".into()), mount())].into();
    let none = BTreeSet::new();
    let (ignore, _) = IgnoreRules::new(&cfg.ignore);
    let next = Cell::new(10_000_000_000);
    let planner = Planner::new(
        store,
        &cfg,
        &mounts,
        &none,
        &none,
        &ignore,
        BTreeSet::new(),
        &none,
        250,
        &none,
        NodeId(1),
        &next,
    );
    let t = Instant::now();
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    for n in nodes {
        let d = planner.plan(*n).expect("plans");
        let k = match d {
            Decision::Op(op) => format!("{:?}", op.kind),
            other => format!("{:?}", std::mem::discriminant(&other)),
        };
        *kinds.entry(k).or_default() += 1;
    }
    (t.elapsed().as_secs_f64(), kinds)
}

#[test]
#[ignore = "a benchmark: run explicitly in release mode"]
fn store_and_planner_at_a_million_entries() {
    let total = nodes_wanted();
    let dir = std::env::temp_dir().join(format!("hearth-sync-budget-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let path = dir.join("state.db");

    // A synced tree: the steady state of a big root.
    let mut store = SqliteStore::open(&path).expect("opens");
    let build_s = build(&mut store, total, true);
    let size_mb = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) as f64 / 1e6
        + std::fs::metadata(dir.join("state.db-wal"))
            .map(|m| m.len())
            .unwrap_or(0) as f64
            / 1e6;
    println!("nodes: {total}");
    println!("build of the synced tree: {build_s:.1} s");
    println!("database size (file and WAL): {size_mb:.0} MB (budget 300 MB)");

    // 100 changed: their local content differs from the base.
    let folders = (total / 1000).max(1);
    let changed: Vec<NodeId> = (0..100)
        .map(|i| NodeId(folders + 2 + i * (total / 100).max(1) % (total - folders - 1)))
        .collect();
    let mut b = Vec::new();
    for n in &changed {
        let mut l = store
            .node(*n)
            .expect("ok")
            .expect("exists")
            .local
            .expect("local");
        l.hash = Some(Hash::of(b"changed"));
        b.push(Write::PutLocal(l));
        b.push(Write::MarkDirty(*n));
    }
    store.apply(b).expect("changes");
    let (rss, _) = memory_mb();
    let (t100, kinds) = planner_pass(&store, &changed);
    println!(
        "planner, {total} nodes, 100 changed: {:.1} ms (budget 50 ms) {kinds:?}",
        t100 * 1000.0
    );
    println!("memory resident at idle after build: {rss} MB (budget 150 MB)");
    drop(store);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(dir.join("state.db-wal"));
    let _ = std::fs::remove_file(dir.join("state.db-shm"));

    // A first sync: Hearth's listing is in, nothing is on disk yet.
    let mut store = SqliteStore::open(&path).expect("opens");
    build(&mut store, total, false);
    // The root itself is on disk (the scan made it).
    let root = store.node(NodeId(1)).expect("ok").expect("root");
    let mut rl = rows(1, folders, true);
    rl.retain(|w| matches!(w, Write::PutLocal(_) | Write::PutSynced(_)));
    store.apply(rl).expect("root");
    drop(root);
    let all: Vec<NodeId> = (1..=total).map(NodeId).collect();
    let (tfirst, kinds) = planner_pass(&store, &all);
    let (_, peak) = memory_mb();
    println!("planner, first sync of {total} nodes: {tfirst:.1} s (budget 60 s) {kinds:?}");
    println!("memory peak over the whole run: {peak} MB (budget 400 MB during a full scan)");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
