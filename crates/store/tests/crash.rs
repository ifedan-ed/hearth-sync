//! A process killed at a random moment while committing batches leaves a
//! database that opens, is consistent, and holds exactly a prefix of the
//! batches: every batch whole or absent (docs/sync.md 3.2, I11).
//!
//! This kills the writing process (SIGKILL), which tests the store's own
//! transactions. Power loss, where the operating system's buffers are lost
//! too, is SQLite's own tested property under `synchronous=FULL` and is
//! replayed by the crash suite of layer 6 (docs/sync.md 7.6), not here.

use std::process::Command;
use std::time::Duration;

use hearth_sync_core::ids::{FileId, Kind, MountId, NodeId};
use hearth_sync_core::names::{LocalName, NameKey};
use hearth_sync_core::store::{MetaKey, Store, Write, check_consistency};
use hearth_sync_core::tree::LocalRow;
use hearth_sync_store::SqliteStore;

const ENV: &str = "HEARTH_SYNC_CRASH_DB";

/// One batch: a node with its local row, and the progress counter, which
/// names the last whole batch.
fn batch(i: u64) -> Vec<Write> {
    let n = NodeId(i);
    vec![
        Write::PutNode(n, MountId("home".into())),
        Write::PutLocal(LocalRow {
            node: n,
            parent: None,
            name: LocalName(format!("n{i}").into_bytes()),
            key: NameKey(format!("n{i}")),
            kind: Kind::File,
            file_id: FileId {
                volume: 1,
                id: u128::from(i),
            },
            size: i,
            mtime_ns: 0,
            ctime_ns: 0,
            exec: false,
            hash: None,
        }),
        Write::MarkDirty(n),
        Write::PutMeta(MetaKey::NextNode, i.to_string()),
    ]
}

/// The child: writes batches until it is killed. Run only by the parent.
#[test]
#[ignore = "run as a child process by the crash test"]
fn child_writer() {
    let Ok(path) = std::env::var(ENV) else {
        return;
    };
    let mut s = SqliteStore::open(std::path::Path::new(&path)).expect("opens");
    let start: u64 = s
        .meta(MetaKey::NextNode)
        .expect("ok")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut i = start + 1;
    loop {
        s.apply(batch(i)).expect("applies");
        i += 1;
    }
}

#[test]
fn a_killed_writer_leaves_whole_batches_only() {
    let dir = std::env::temp_dir().join(format!("hearth-sync-crash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let db = dir.join("state.db");
    let exe = std::env::current_exe().expect("the test binary");
    // Delays chosen to land at different points of a commit; the same
    // list every run, so a failure replays.
    for (round, ms) in [3u64, 17, 41, 9, 73, 26, 55, 12].iter().enumerate() {
        let mut child = Command::new(&exe)
            .args(["--exact", "child_writer", "--ignored", "--nocapture"])
            .env(ENV, &db)
            .spawn()
            .expect("spawns");
        std::thread::sleep(Duration::from_millis(*ms + 20));
        child.kill().expect("kills");
        let _ = child.wait();
        let s = SqliteStore::open(&db).expect("opens after a kill");
        let last: u64 = s
            .meta(MetaKey::NextNode)
            .expect("ok")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let nodes = s.all_nodes().expect("ok");
        assert_eq!(
            nodes.len() as u64,
            last,
            "round {round}: every batch up to {last} whole, none beyond"
        );
        for n in &nodes {
            let r = s.node(*n).expect("ok").expect("exists");
            assert!(r.local.is_some(), "round {round}: half a batch for {n}");
        }
        assert!(check_consistency(&s).expect("ok").is_empty());
        assert_eq!(s.dirty(None, usize::MAX).expect("ok").len() as u64, last);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
