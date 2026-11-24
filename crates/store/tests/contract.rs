//! The SQLite store against the core's store contract: the same suite the
//! in-memory store passes, random batches compared with it, injected
//! failures at every write point, a file that survives reopening, the
//! migrations, and a whole engine run on SQLite.

use hearth_sync_core::engine::Engine;
use hearth_sync_core::names::VolumeRules;
use hearth_sync_core::store::conformance;
use hearth_sync_core::store::{MetaKey, Store, StoreError, Write, check_consistency};
use hearth_sync_core::testkit::{Settled, World, run_to_idle};
use hearth_sync_store::SqliteStore;

fn mem() -> SqliteStore {
    SqliteStore::open_in_memory().expect("opens")
}

#[test]
fn round_trips_every_row() {
    conformance::round_trips_every_row(&mem);
}

#[test]
fn constraints_refuse_whole_batches() {
    conformance::constraints_refuse_whole_batches(&mem);
}

#[test]
fn removing_a_node_cascades() {
    conformance::removing_a_node_cascades(&mem);
}

#[test]
fn injected_failures_at_every_write_point_change_nothing() {
    conformance::injected_failures_at_every_write_point(&mem);
}

#[test]
fn agrees_with_the_in_memory_store_on_random_batches() {
    // 200 seeds of 40 batches each locally; more in CI.
    let seeds: u64 = std::env::var("STORE_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    conformance::agrees_with_the_reference_on_random_batches(&mem, 0..seeds);
}

/// A scratch folder for database files, removed afterwards.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let p =
            std::env::temp_dir().join(format!("hearth-sync-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("scratch folder");
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_file_database_survives_reopening() {
    let dir = Scratch::new("reopen");
    let path = dir.0.join("state.db");
    {
        let mut s = SqliteStore::open(&path).expect("opens");
        assert_eq!(s.schema_version().expect("ok"), 1);
        s.apply(vec![Write::PutMeta(MetaKey::InstanceId, "x".into())])
            .expect("applies");
    }
    let s = SqliteStore::open(&path).expect("reopens");
    assert_eq!(
        s.meta(MetaKey::InstanceId).expect("ok").as_deref(),
        Some("x")
    );
}

#[test]
fn a_database_from_a_newer_engine_is_refused() {
    let dir = Scratch::new("newer");
    let path = dir.0.join("state.db");
    drop(SqliteStore::open(&path).expect("opens"));
    {
        let c = rusqlite_set_version(&path, 99);
        assert!(c);
    }
    match SqliteStore::open(&path) {
        Err(StoreError::Corrupt(m)) => assert!(m.contains("newer")),
        Err(e) => panic!("{e:?}"),
        Ok(_) => panic!("a newer schema must be refused"),
    }
}

/// Sets `user_version` through a second connection (as a newer engine
/// would have left it).
fn rusqlite_set_version(path: &std::path::Path, v: i64) -> bool {
    let c = rusqlite::Connection::open(path).expect("opens");
    c.execute_batch(&format!("PRAGMA user_version = {v}"))
        .is_ok()
}

#[test]
fn a_whole_engine_run_on_sqlite_converges() {
    let mut w = World::new(1, VolumeRules::LINUX);
    w.remote.client_write(&["a", "b.txt"], b"from hearth");
    w.fs.write(&["c", "d.txt"], b"from the disk");
    let mut e = Engine::new(mem(), World::config(VolumeRules::LINUX)).expect("ok");
    let r = run_to_idle(&mut e, &mut w, 10_000).expect("ok");
    assert!(matches!(r, Settled::Idle { .. }), "{r:?}");
    assert_eq!(w.fs.user_files(), w.remote.files());
    assert!(check_consistency(e.store()).expect("ok").is_empty());
}
