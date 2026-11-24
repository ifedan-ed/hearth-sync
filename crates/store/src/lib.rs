//! The engine's state database: SQLite behind the core's `Store`
//! interface (docs/sync.md 3.2).
//!
//! Owns: the schema and its forward-only migrations, the pragmas (WAL,
//! full synchronous writes, full fsync on Apple systems), every query, and
//! the mapping of the core's rows to columns.
//!
//! Why its own crate: the choice of SQLite is then one crate, not the
//! engine; and a phone's file provider extension, which has a few
//! megabytes and a few milliseconds, can link the store and its read
//! functions without the planner or the scanner.
//!
//! Why SQLite and these settings: crash safety is the property this
//! database must not get wrong, and SQLite's own suite covers power loss at
//! every step. A batch is one transaction; `synchronous=FULL` makes a
//! commit durable before `apply` returns; WAL lets the app and a file
//! provider extension read while the engine writes.
//!
//! The constraints are the in-memory store's, one for one (the core's
//! conformance suite runs against both and compares them on random
//! batches), so a simulated run on `MemStore` speaks for this database.
//!
//! The database is a cache of facts, never the only copy of anything a
//! person saved: deleting it must lose no data (I12).
#![forbid(unsafe_code)]

mod codec;
mod schema;

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use hearth_sync_core::ids::{FileId, MountId, NodeId, OpId, RemoteId};
use hearth_sync_core::names::NameKey;
use hearth_sync_core::store::{Batch, MetaKey, Store, StoreError, StoreResult, Write};
use hearth_sync_core::tree::{
    ConflictRow, HeldRow, MountRow, NodeRecord, OpRow, ProblemRow, TransferRow,
};

use codec::*;

/// The state database in a SQLite file.
pub struct SqliteStore {
    conn: Connection,
    fail_after: Option<usize>,
}

/// A database error in the store's terms: a refused constraint is the
/// engine's bug; anything else is the disk's.
pub(crate) fn sql_err(e: rusqlite::Error) -> StoreError {
    match &e {
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            StoreError::Constraint(e.to_string())
        }
        _ => StoreError::Io(e.to_string()),
    }
}

fn constraint<T>(msg: String) -> StoreResult<T> {
    Err(StoreError::Constraint(msg))
}

impl SqliteStore {
    /// Opens (or creates) the database at a path, applying the settings of
    /// 3.2 and the migrations up to the current schema. The path must be
    /// outside the synced folder and on a local file system (WAL needs
    /// shared memory); the host checks both.
    pub fn open(path: &Path) -> StoreResult<SqliteStore> {
        let conn = Connection::open(path).map_err(sql_err)?;
        Self::init(conn)
    }

    /// A database in memory (tests and benchmarks).
    pub fn open_in_memory() -> StoreResult<SqliteStore> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> StoreResult<SqliteStore> {
        schema::configure(&conn)?;
        schema::migrate(&conn)?;
        Ok(SqliteStore {
            conn,
            fail_after: None,
        })
    }

    /// The schema version of the open database.
    pub fn schema_version(&self) -> StoreResult<i64> {
        self.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(sql_err)
    }

    fn ids(&self, sql: &str, p: impl rusqlite::Params) -> StoreResult<Vec<NodeId>> {
        let mut st = self.conn.prepare_cached(sql).map_err(sql_err)?;
        let rows = st.query_map(p, |r| r.get::<_, i64>(0)).map_err(sql_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(NodeId(r.map_err(sql_err)? as u64));
        }
        Ok(out)
    }

    fn exists(tx: &Transaction<'_>, sql: &str, p: impl rusqlite::Params) -> StoreResult<bool> {
        tx.prepare_cached(sql)
            .map_err(sql_err)?
            .exists(p)
            .map_err(sql_err)
    }

    fn need_node(tx: &Transaction<'_>, n: NodeId) -> StoreResult<()> {
        if Self::exists(tx, "SELECT 1 FROM node WHERE id = ?1", [n.0 as i64])? {
            Ok(())
        } else {
            constraint(format!("{n} does not exist"))
        }
    }

    fn exec(tx: &Transaction<'_>, sql: &str, p: impl rusqlite::Params) -> StoreResult<()> {
        tx.prepare_cached(sql)
            .map_err(sql_err)?
            .execute(p)
            .map_err(sql_err)
            .map(|_| ())
    }

    /// Applies one write inside the batch's transaction, with the same
    /// constraints as the in-memory store.
    fn apply_one(tx: &Transaction<'_>, w: &Write) -> StoreResult<()> {
        match w {
            Write::PutMeta(k, v) => Self::exec(
                tx,
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![k.as_str(), v],
            ),
            Write::DelMeta(k) => Self::exec(tx, "DELETE FROM meta WHERE key = ?1", [k.as_str()]),
            Write::PutMount(m) => put_mount(tx, m),
            Write::DelMount(id) => Self::exec(tx, "DELETE FROM mount WHERE id = ?1", [&id.0]),
            Write::PutNode(n, m) => Self::exec(
                tx,
                "INSERT INTO node(id, mount) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET mount = excluded.mount",
                params![n.0 as i64, m.0],
            ),
            Write::DelNode(n) => {
                let id = n.0 as i64;
                if Self::exists(tx, "SELECT 1 FROM op WHERE node = ?1", [id])? {
                    return constraint(format!("{n} removed while an operation acts on it"));
                }
                if Self::exists(tx, "SELECT 1 FROM local WHERE parent = ?1", [id])?
                    || Self::exists(tx, "SELECT 1 FROM synced WHERE parent = ?1", [id])?
                {
                    return constraint(format!("{n} removed while it has children"));
                }
                Self::need_node(tx, *n)?;
                for sql in [
                    "DELETE FROM remote WHERE node = ?1",
                    "DELETE FROM local WHERE node = ?1",
                    "DELETE FROM synced WHERE node = ?1",
                    "DELETE FROM dirty WHERE node = ?1",
                    "DELETE FROM unselected WHERE node = ?1",
                    "DELETE FROM node WHERE id = ?1",
                ] {
                    Self::exec(tx, sql, [id])?;
                }
                Self::exec(
                    tx,
                    "DELETE FROM problem WHERE subject_kind = 0 AND subject = ?1",
                    [node_subject(*n)],
                )
            }
            Write::PutRemote(r) => {
                Self::need_node(tx, r.node)?;
                put_remote(tx, r)
            }
            Write::DelRemote(n) => {
                Self::need_node(tx, *n)?;
                Self::exec(tx, "DELETE FROM remote WHERE node = ?1", [n.0 as i64])
            }
            Write::PutLocal(l) => {
                Self::need_node(tx, l.node)?;
                put_local(tx, l)
            }
            Write::DelLocal(n) => {
                if Self::exists(tx, "SELECT 1 FROM local WHERE parent = ?1", [n.0 as i64])? {
                    return constraint(format!("local row of {n} removed while it has children"));
                }
                Self::need_node(tx, *n)?;
                Self::exec(tx, "DELETE FROM local WHERE node = ?1", [n.0 as i64])
            }
            Write::PutSynced(s) => {
                Self::need_node(tx, s.node)?;
                put_synced(tx, s)
            }
            Write::DelSynced(n) => {
                Self::need_node(tx, *n)?;
                Self::exec(tx, "DELETE FROM synced WHERE node = ?1", [n.0 as i64])
            }
            Write::PutOp(o) => {
                if Self::need_node(tx, o.node).is_err() {
                    return constraint(format!("{} on missing {}", o.id, o.node));
                }
                put_op(tx, o)
            }
            Write::DelOp(id) => {
                Self::exec(tx, "DELETE FROM transfer WHERE op = ?1", [id.0 as i64])?;
                Self::exec(tx, "DELETE FROM op WHERE id = ?1", [id.0 as i64])
            }
            Write::PutTransfer(t) => {
                if !Self::exists(tx, "SELECT 1 FROM op WHERE id = ?1", [t.op.0 as i64])? {
                    return constraint(format!("transfer of missing {}", t.op));
                }
                put_transfer(tx, t)
            }
            Write::PutProblem(p) => put_problem(tx, p),
            Write::DelProblem(s) => {
                let (k, b) = subject_cols(s);
                Self::exec(
                    tx,
                    "DELETE FROM problem WHERE subject_kind = ?1 AND subject = ?2",
                    params![k, b],
                )
            }
            Write::PutHeld(h) => put_held(tx, h),
            Write::DelHeld(id) => Self::exec(tx, "DELETE FROM held WHERE id = ?1", [id.0 as i64]),
            Write::PutConflict(c) => put_conflict(tx, c),
            Write::MarkDirty(n) => {
                if Self::need_node(tx, *n).is_err() {
                    return constraint(format!("dirty mark on missing {n}"));
                }
                Self::exec(
                    tx,
                    "INSERT INTO dirty(node, waits_on) VALUES (?1, NULL)
                     ON CONFLICT(node) DO UPDATE SET waits_on = NULL",
                    [n.0 as i64],
                )
            }
            Write::Wait(n, t) => {
                if Self::need_node(tx, *n).is_err() {
                    return constraint(format!("wait of missing {n}"));
                }
                Self::exec(
                    tx,
                    "INSERT INTO dirty(node, waits_on) VALUES (?1, ?2)
                     ON CONFLICT(node) DO UPDATE SET waits_on = excluded.waits_on",
                    params![n.0 as i64, t.0 as i64],
                )
            }
            Write::ClearDirty(n) => {
                Self::exec(tx, "DELETE FROM dirty WHERE node = ?1", [n.0 as i64])
            }
            Write::Unselect(n) => Self::exec(
                tx,
                "INSERT OR IGNORE INTO unselected(node) VALUES (?1)",
                [n.0 as i64],
            ),
            Write::Reselect(n) => {
                Self::exec(tx, "DELETE FROM unselected WHERE node = ?1", [n.0 as i64])
            }
        }
    }
}

impl Store for SqliteStore {
    fn meta(&self, key: MetaKey) -> StoreResult<Option<String>> {
        self.conn
            .prepare_cached("SELECT value FROM meta WHERE key = ?1")
            .map_err(sql_err)?
            .query_row([key.as_str()], |r| r.get(0))
            .optional()
            .map_err(sql_err)
    }

    fn mounts(&self) -> StoreResult<Vec<MountRow>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT {MOUNT_COLS} FROM mount ORDER BY id"))
            .map_err(sql_err)?;
        let rows = st.query_map([], read_mount).map_err(sql_err)?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r.map_err(sql_err)??);
        }
        Ok(v)
    }

    fn node(&self, node: NodeId) -> StoreResult<Option<NodeRecord>> {
        let id = node.0 as i64;
        let mount: Option<String> = self
            .conn
            .prepare_cached("SELECT mount FROM node WHERE id = ?1")
            .map_err(sql_err)?
            .query_row([id], |r| r.get(0))
            .optional()
            .map_err(sql_err)?;
        let Some(mount) = mount else {
            return Ok(None);
        };
        let remote = self
            .conn
            .prepare_cached(&format!("SELECT {REMOTE_COLS} FROM remote WHERE node = ?1"))
            .map_err(sql_err)?
            .query_row([id], read_remote)
            .optional()
            .map_err(sql_err)?
            .transpose()?;
        let local = self
            .conn
            .prepare_cached(&format!("SELECT {LOCAL_COLS} FROM local WHERE node = ?1"))
            .map_err(sql_err)?
            .query_row([id], read_local)
            .optional()
            .map_err(sql_err)?
            .transpose()?;
        let synced = self
            .conn
            .prepare_cached(&format!("SELECT {SYNCED_COLS} FROM synced WHERE node = ?1"))
            .map_err(sql_err)?
            .query_row([id], read_synced)
            .optional()
            .map_err(sql_err)?
            .transpose()?;
        Ok(Some(NodeRecord {
            node,
            mount: MountId(mount),
            remote,
            local,
            synced,
        }))
    }

    fn node_by_remote(&self, id: &RemoteId) -> StoreResult<Option<NodeId>> {
        Ok(self
            .ids("SELECT node FROM remote WHERE remote_id = ?1", [&id.0])?
            .into_iter()
            .next())
    }

    fn nodes_by_file_id(&self, fid: FileId) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT node FROM local WHERE volume = ?1 AND file_id = ?2 ORDER BY node",
            params![fid.volume as i64, fid.id.to_be_bytes().to_vec()],
        )
    }

    fn local_children(&self, parent: NodeId) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT node FROM local WHERE parent = ?1 ORDER BY node",
            [parent.0 as i64],
        )
    }

    fn local_child_by_key(&self, parent: NodeId, key: &NameKey) -> StoreResult<Option<NodeId>> {
        Ok(self
            .ids(
                "SELECT node FROM local WHERE parent = ?1 AND name_key = ?2",
                params![parent.0 as i64, key.0],
            )?
            .into_iter()
            .next())
    }

    fn remote_children(&self, parent: &RemoteId) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT node FROM remote WHERE parent = ?1 ORDER BY node",
            [&parent.0],
        )
    }

    fn synced_children(&self, parent: NodeId) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT node FROM synced WHERE parent = ?1 ORDER BY node",
            [parent.0 as i64],
        )
    }

    fn mount_nodes(&self, mount: &MountId) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT id FROM node WHERE mount = ?1 ORDER BY id",
            [&mount.0],
        )
    }

    fn count_nodes(&self, mount: &MountId) -> StoreResult<u64> {
        self.conn
            .prepare_cached("SELECT count(*) FROM node WHERE mount = ?1")
            .map_err(sql_err)?
            .query_row([&mount.0], |r| r.get::<_, i64>(0))
            .map(|c| c as u64)
            .map_err(sql_err)
    }

    fn all_nodes(&self) -> StoreResult<Vec<NodeId>> {
        self.ids("SELECT id FROM node ORDER BY id", [])
    }

    fn ops(&self) -> StoreResult<Vec<OpRow>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT {OP_COLS} FROM op ORDER BY id"))
            .map_err(sql_err)?;
        let rows = st.query_map([], read_op).map_err(sql_err)?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r.map_err(sql_err)??);
        }
        Ok(v)
    }

    fn transfer(&self, op: OpId) -> StoreResult<Option<TransferRow>> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {TRANSFER_COLS} FROM transfer WHERE op = ?1"
            ))
            .map_err(sql_err)?
            .query_row([op.0 as i64], read_transfer)
            .optional()
            .map_err(sql_err)?
            .transpose()
    }

    fn problems(&self) -> StoreResult<Vec<ProblemRow>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT {PROBLEM_COLS} FROM problem"))
            .map_err(sql_err)?;
        let rows = st.query_map([], read_problem).map_err(sql_err)?;
        let mut v: Vec<ProblemRow> = Vec::new();
        for r in rows {
            v.push(r.map_err(sql_err)??);
        }
        // The in-memory store's order: by subject.
        v.sort_by(|a, b| a.subject.cmp(&b.subject));
        Ok(v)
    }

    fn held(&self) -> StoreResult<Vec<HeldRow>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT {HELD_COLS} FROM held ORDER BY id"))
            .map_err(sql_err)?;
        let rows = st.query_map([], read_held).map_err(sql_err)?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r.map_err(sql_err)??);
        }
        Ok(v)
    }

    fn conflicts(&self) -> StoreResult<Vec<ConflictRow>> {
        let mut st = self
            .conn
            .prepare_cached(&format!("SELECT {CONFLICT_COLS} FROM conflict ORDER BY id"))
            .map_err(sql_err)?;
        let rows = st.query_map([], read_conflict).map_err(sql_err)?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r.map_err(sql_err)??);
        }
        Ok(v)
    }

    fn dirty(&self, after: Option<NodeId>, limit: usize) -> StoreResult<Vec<NodeId>> {
        let after = after.map_or(-1, |a| a.0 as i64);
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.ids(
            "SELECT node FROM dirty WHERE node > ?1 AND waits_on IS NULL ORDER BY node LIMIT ?2",
            params![after, limit],
        )
    }

    fn waiters_on(&self, target: NodeId) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT node FROM dirty WHERE waits_on = ?1 ORDER BY node",
            [target.0 as i64],
        )
    }

    fn waiting(&self, limit: usize) -> StoreResult<Vec<NodeId>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.ids(
            "SELECT node FROM dirty WHERE waits_on IS NOT NULL ORDER BY node LIMIT ?1",
            [limit],
        )
    }

    fn remote_departed(&self) -> StoreResult<Vec<NodeId>> {
        self.ids(
            "SELECT node FROM remote WHERE departed IS NOT NULL ORDER BY node",
            [],
        )
    }

    fn unselected(&self) -> StoreResult<Vec<NodeId>> {
        self.ids("SELECT node FROM unselected ORDER BY node", [])
    }

    fn apply(&mut self, batch: Batch) -> StoreResult<()> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql_err)?;
        for w in &batch {
            if let Some(n) = self.fail_after.as_mut() {
                if *n == 0 {
                    // Dropping the transaction rolls it back: nothing of
                    // the batch remains, as after a crash before commit.
                    return Err(StoreError::Injected);
                }
                *n -= 1;
            }
            Self::apply_one(&tx, w)?;
        }
        tx.commit().map_err(sql_err)
    }

    fn fail_writes_after(&mut self, n: Option<usize>) {
        self.fail_after = n;
    }
}
