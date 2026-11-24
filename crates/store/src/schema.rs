//! The schema, its settings and its migrations (docs/sync.md 3.2).
//!
//! Rows hold no path strings: a path is the chain of parents, so a rename
//! of a folder with a hundred thousand files changes one row, and a
//! million files fit the database budget of 7.9. Indexes are exactly the
//! lookups the core's `Store` interface makes.
//!
//! Migrations are forward-only: `PRAGMA user_version` names the schema;
//! each step from version n to n+1 is one transaction, tested on a copy of
//! every earlier version. A database from a newer engine is refused, never
//! guessed at.

use rusqlite::Connection;

use hearth_sync_core::store::{StoreError, StoreResult};

use crate::sql_err;

/// The schema this build writes.
pub(crate) const CURRENT: i64 = 1;

/// The settings of 3.2, each explicit.
pub(crate) fn configure(conn: &Connection) -> StoreResult<()> {
    // WAL: readers (the app, a file provider extension) beside the writer.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .map_err(sql_err)?;
    if mode != "wal" && mode != "memory" {
        // A file system without shared memory (a network mount): refused
        // as a place for the database (3.2).
        return Err(StoreError::Io(format!(
            "the database's folder does not support WAL (journal mode {mode})"
        )));
    }
    conn.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA locking_mode = NORMAL;",
    )
    .map_err(sql_err)?;
    // On Apple systems a plain fsync does not reach the platter.
    #[cfg(target_vendor = "apple")]
    conn.execute_batch("PRAGMA fullfsync = ON; PRAGMA checkpoint_fullfsync = ON;")
        .map_err(sql_err)?;
    Ok(())
}

/// Version 1: the tables of 3.2.
const V1: &str = "
CREATE TABLE meta(
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE mount(
    id TEXT PRIMARY KEY,
    kind INTEGER NOT NULL,
    storage TEXT NOT NULL,
    epoch TEXT NOT NULL,
    root_remote TEXT NOT NULL,
    root_node INTEGER NOT NULL,
    local_path BLOB NOT NULL,
    perms INTEGER NOT NULL,
    trash INTEGER NOT NULL,
    versions INTEGER NOT NULL,
    prompt_ask INTEGER NOT NULL,
    selected INTEGER NOT NULL,
    cursor TEXT,
    listing_reason INTEGER,
    listing_page TEXT,
    listing_cursor TEXT,
    listing_generation INTEGER,
    caught_up INTEGER NOT NULL,
    caught_up_round INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE node(
    id INTEGER PRIMARY KEY,
    mount TEXT NOT NULL
);
CREATE INDEX node_mount ON node(mount, id);

CREATE TABLE remote(
    node INTEGER PRIMARY KEY REFERENCES node(id),
    remote_id TEXT NOT NULL UNIQUE,
    mount TEXT NOT NULL,
    parent TEXT,
    name TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_us INTEGER NOT NULL,
    etag TEXT,
    hash BLOB,
    exec INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    crypt INTEGER NOT NULL,
    departed INTEGER,
    listed INTEGER NOT NULL
);
CREATE INDEX remote_parent ON remote(parent, node);
CREATE INDEX remote_departed ON remote(departed) WHERE departed IS NOT NULL;

CREATE TABLE local(
    node INTEGER PRIMARY KEY REFERENCES node(id),
    parent INTEGER,
    name BLOB NOT NULL,
    name_key TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    volume INTEGER NOT NULL,
    file_id BLOB NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    ctime_ns INTEGER NOT NULL,
    exec INTEGER NOT NULL,
    hash BLOB
);
CREATE UNIQUE INDEX local_key ON local(parent, name_key);
CREATE INDEX local_file ON local(volume, file_id);

CREATE TABLE synced(
    node INTEGER PRIMARY KEY REFERENCES node(id),
    parent INTEGER,
    name TEXT NOT NULL,
    is_dir INTEGER NOT NULL,
    size INTEGER NOT NULL,
    hash BLOB,
    remote_etag TEXT,
    local_volume INTEGER NOT NULL,
    local_file_id BLOB NOT NULL,
    local_mtime_ns INTEGER NOT NULL,
    exec INTEGER NOT NULL
);
CREATE INDEX synced_parent ON synced(parent);

CREATE TABLE op(
    id INTEGER PRIMARY KEY,
    kind INTEGER NOT NULL,
    node INTEGER NOT NULL REFERENCES node(id),
    state INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    next_try_ns INTEGER,
    cond_kind INTEGER,
    cond_text TEXT,
    cond_hash BLOB,
    cond_seq INTEGER,
    target_parent INTEGER,
    target_name TEXT,
    expect_hash BLOB,
    created_ns INTEGER NOT NULL
);
CREATE INDEX op_node ON op(node);

CREATE TABLE transfer(
    op INTEGER PRIMARY KEY REFERENCES op(id),
    temp BLOB,
    upload_id TEXT,
    total INTEGER NOT NULL,
    chunk_size INTEGER NOT NULL,
    done INTEGER NOT NULL,
    expect_etag TEXT,
    new_volume INTEGER,
    new_file_id BLOB,
    aside BLOB
);

CREATE TABLE problem(
    subject_kind INTEGER NOT NULL,
    subject BLOB NOT NULL,
    kind INTEGER NOT NULL,
    reason TEXT NOT NULL,
    first_ns INTEGER NOT NULL,
    last_ns INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY(subject_kind, subject)
) WITHOUT ROWID;

CREATE TABLE held(
    id INTEGER PRIMARY KEY,
    original BLOB NOT NULL,
    held BLOB NOT NULL,
    hash BLOB,
    size INTEGER NOT NULL,
    reason INTEGER NOT NULL,
    basis INTEGER NOT NULL,
    mount TEXT NOT NULL,
    at_ns INTEGER NOT NULL
);

CREATE TABLE conflict(
    id INTEGER PRIMARY KEY,
    node INTEGER NOT NULL,
    copy INTEGER NOT NULL,
    hash BLOB,
    name TEXT NOT NULL,
    at_ns INTEGER NOT NULL
);

CREATE TABLE dirty(
    node INTEGER PRIMARY KEY REFERENCES node(id),
    waits_on INTEGER
);
CREATE INDEX dirty_waits ON dirty(waits_on) WHERE waits_on IS NOT NULL;
CREATE TABLE unselected(node INTEGER PRIMARY KEY);
";

/// Brings the database to the current schema, one version at a time.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql_err)?;
    if version > CURRENT {
        return Err(StoreError::Corrupt(format!(
            "the database is schema {version}, from a newer engine than this one ({CURRENT})"
        )));
    }
    if version < 1 {
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE; {V1} PRAGMA user_version = 1; COMMIT;"
        ))
        .map_err(sql_err)?;
    }
    Ok(())
}
