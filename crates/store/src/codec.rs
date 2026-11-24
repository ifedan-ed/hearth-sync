//! The core's rows to columns and back, one function per table.
//!
//! Numbers: SQLite integers are signed 64-bit; node ids, sizes and
//! sequence numbers are below 2^63 (ids are counters, sizes are bytes on a
//! disk, `seq` is below 2^53 by the contract), so `as i64` keeps them
//! exactly. File ids are 128-bit and stored as 16 big-endian bytes. Enums
//! are stored as their stable codes; an unknown code is a corrupt row,
//! reported, never guessed.

use rusqlite::{Row, Transaction, params};

use hearth_sync_core::ids::{
    ConflictId, Cursor, Etag, FileId, Hash, HeldId, Kind, MountId, NodeId, OpId, Perms, RemoteId,
};
use hearth_sync_core::names::{LocalName, LocalPath, NameKey};
use hearth_sync_core::store::{StoreError, StoreResult};
use hearth_sync_core::tree::{
    Condition, ConflictRow, Crypt, HeldReason, HeldRow, ListReason, Listing, LocalRow, MountKind,
    MountRow, OpKind, OpRow, OpState, ProblemKind, ProblemRow, PurgeBasis, RemoteRow, Subject,
    SyncedRow, TransferRow,
};

use crate::sql_err;

fn corrupt(what: &str) -> StoreError {
    StoreError::Corrupt(format!("unreadable {what}"))
}

fn kind_code(k: Kind) -> i64 {
    match k {
        Kind::File => 0,
        Kind::Folder => 1,
    }
}

fn kind_of(c: i64) -> StoreResult<Kind> {
    match c {
        0 => Ok(Kind::File),
        1 => Ok(Kind::Folder),
        _ => Err(corrupt("kind")),
    }
}

fn hash_bytes(h: &Option<Hash>) -> Option<Vec<u8>> {
    h.map(|x| x.0.to_vec())
}

fn hash_of(b: Option<Vec<u8>>) -> StoreResult<Option<Hash>> {
    match b {
        None => Ok(None),
        Some(v) => {
            let a: [u8; 32] = v.try_into().map_err(|_| corrupt("hash"))?;
            Ok(Some(Hash(a)))
        }
    }
}

fn fid_bytes(f: FileId) -> Vec<u8> {
    f.id.to_be_bytes().to_vec()
}

fn fid_of(volume: i64, b: Vec<u8>) -> StoreResult<FileId> {
    let a: [u8; 16] = b.try_into().map_err(|_| corrupt("file id"))?;
    Ok(FileId {
        volume: volume as u64,
        id: u128::from_be_bytes(a),
    })
}

// ----- mount -----

/// The columns read for a mount.
pub const MOUNT_COLS: &str = "id, kind, storage, epoch, root_remote, root_node, local_path, perms, trash, versions, prompt_ask, selected, cursor, listing_reason, listing_page, listing_cursor, listing_generation, caught_up, caught_up_round";

fn mount_kind_code(k: MountKind) -> i64 {
    match k {
        MountKind::Home => 0,
        MountKind::Share => 1,
        MountKind::Team => 2,
        MountKind::Library => 3,
        MountKind::External => 4,
        MountKind::Remote => 5,
    }
}

fn mount_kind_of(c: i64) -> StoreResult<MountKind> {
    Ok(match c {
        0 => MountKind::Home,
        1 => MountKind::Share,
        2 => MountKind::Team,
        3 => MountKind::Library,
        4 => MountKind::External,
        5 => MountKind::Remote,
        _ => return Err(corrupt("mount kind")),
    })
}

fn reason_code(r: ListReason) -> i64 {
    match r {
        ListReason::First => 0,
        ListReason::Retention => 1,
        ListReason::Relist => 2,
        ListReason::Untrusted => 3,
    }
}

fn reason_of(c: i64) -> StoreResult<ListReason> {
    Ok(match c {
        0 => ListReason::First,
        1 => ListReason::Retention,
        2 => ListReason::Relist,
        3 => ListReason::Untrusted,
        _ => return Err(corrupt("listing reason")),
    })
}

/// A mount's path components joined by NUL (no component holds one).
fn path_text(parts: &[String]) -> Vec<u8> {
    parts.join("\0").into_bytes()
}

fn path_parts(b: Vec<u8>) -> StoreResult<Vec<String>> {
    if b.is_empty() {
        return Ok(Vec::new());
    }
    String::from_utf8(b)
        .map_err(|_| corrupt("mount path"))
        .map(|s| s.split('\0').map(str::to_owned).collect())
}

/// Writes a mount.
pub fn put_mount(tx: &Transaction<'_>, m: &MountRow) -> StoreResult<()> {
    let l = m.listing.as_ref();
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO mount({MOUNT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)"
    ))
    .map_err(sql_err)?
    .execute(params![
        m.id.0,
        mount_kind_code(m.kind),
        m.storage,
        m.epoch,
        m.root_remote.0,
        m.root_node.0 as i64,
        path_text(&m.local_path),
        i64::from(m.perms.0),
        m.trash,
        m.versions,
        m.prompt_ask,
        m.selected,
        m.cursor.as_ref().map(|c| c.0.clone()),
        l.map(|l| reason_code(l.reason)),
        l.and_then(|l| l.page.clone()),
        l.and_then(|l| l.cursor.as_ref().map(|c| c.0.clone())),
        l.map(|l| l.generation as i64),
        m.caught_up,
        m.caught_up_round as i64,
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a mount.
pub fn read_mount(r: &Row<'_>) -> rusqlite::Result<StoreResult<MountRow>> {
    let id: String = r.get(0)?;
    let kind: i64 = r.get(1)?;
    let storage: String = r.get(2)?;
    let epoch: String = r.get(3)?;
    let root_remote: String = r.get(4)?;
    let root_node: i64 = r.get(5)?;
    let path: Vec<u8> = r.get(6)?;
    let perms: i64 = r.get(7)?;
    let trash: bool = r.get(8)?;
    let versions: bool = r.get(9)?;
    let prompt_ask: bool = r.get(10)?;
    let selected: bool = r.get(11)?;
    let cursor: Option<String> = r.get(12)?;
    let reason: Option<i64> = r.get(13)?;
    let page: Option<String> = r.get(14)?;
    let lcursor: Option<String> = r.get(15)?;
    let generation: Option<i64> = r.get(16)?;
    let caught_up: bool = r.get(17)?;
    let round: i64 = r.get(18)?;
    Ok((|| {
        let listing = match reason {
            None => None,
            Some(c) => Some(Listing {
                reason: reason_of(c)?,
                page,
                cursor: lcursor.map(Cursor),
                generation: generation.unwrap_or(0) as u64,
            }),
        };
        Ok(MountRow {
            id: MountId(id),
            kind: mount_kind_of(kind)?,
            storage,
            epoch,
            root_remote: RemoteId(root_remote),
            root_node: NodeId(root_node as u64),
            local_path: path_parts(path)?,
            perms: Perms(perms as u32),
            trash,
            versions,
            prompt_ask,
            selected,
            cursor: cursor.map(Cursor),
            listing,
            caught_up,
            caught_up_round: round as u64,
        })
    })())
}

// ----- remote -----

/// The columns read for a remote picture.
pub const REMOTE_COLS: &str = "node, remote_id, mount, parent, name, is_dir, size, mtime_us, etag, hash, exec, seq, crypt, departed, listed";

fn crypt_code(c: Crypt) -> i64 {
    match c {
        Crypt::None => 0,
        Crypt::Rest => 1,
        Crypt::E2ee => 2,
    }
}

fn crypt_of(c: i64) -> StoreResult<Crypt> {
    Ok(match c {
        0 => Crypt::None,
        1 => Crypt::Rest,
        2 => Crypt::E2ee,
        _ => return Err(corrupt("crypt")),
    })
}

/// Writes a remote picture.
pub fn put_remote(tx: &Transaction<'_>, x: &RemoteRow) -> StoreResult<()> {
    tx.prepare_cached(&format!(
        "INSERT INTO remote({REMOTE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
         ON CONFLICT(node) DO UPDATE SET remote_id = excluded.remote_id, mount = excluded.mount,
           parent = excluded.parent, name = excluded.name, is_dir = excluded.is_dir,
           size = excluded.size, mtime_us = excluded.mtime_us, etag = excluded.etag,
           hash = excluded.hash, exec = excluded.exec, seq = excluded.seq,
           crypt = excluded.crypt, departed = excluded.departed, listed = excluded.listed"
    ))
    .map_err(sql_err)?
    .execute(params![
        x.node.0 as i64,
        x.id.0,
        x.mount.0,
        x.parent.as_ref().map(|p| p.0.clone()),
        x.name,
        kind_code(x.kind),
        x.size as i64,
        x.mtime_us,
        x.etag.as_ref().map(|e| e.0.clone()),
        hash_bytes(&x.hash),
        x.exec,
        x.seq as i64,
        crypt_code(x.crypt),
        x.departed.map(|d| d as i64),
        x.listed as i64,
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a remote picture.
pub fn read_remote(r: &Row<'_>) -> rusqlite::Result<StoreResult<RemoteRow>> {
    let kind = kind_of(r.get(5)?);
    let hash = hash_of(r.get(9)?);
    let crypt = crypt_of(r.get(12)?);
    let departed: Option<i64> = r.get(13)?;
    let node: i64 = r.get(0)?;
    let id: String = r.get(1)?;
    let mount: String = r.get(2)?;
    let parent: Option<String> = r.get(3)?;
    let name: String = r.get(4)?;
    let size: i64 = r.get(6)?;
    let mtime_us: i64 = r.get(7)?;
    let etag: Option<String> = r.get(8)?;
    let exec: bool = r.get(10)?;
    let seq: i64 = r.get(11)?;
    let listed: i64 = r.get(14)?;
    Ok((|| {
        Ok(RemoteRow {
            node: NodeId(node as u64),
            id: RemoteId(id),
            mount: MountId(mount),
            parent: parent.map(RemoteId),
            name,
            kind: kind?,
            size: size as u64,
            mtime_us,
            etag: etag.map(Etag),
            hash: hash?,
            exec,
            seq: seq as u64,
            crypt: crypt?,
            departed: departed.map(|d| d as u64),
            listed: listed as u64,
        })
    })())
}

// ----- local -----

/// The columns read for a local picture.
pub const LOCAL_COLS: &str =
    "node, parent, name, name_key, is_dir, volume, file_id, size, mtime_ns, ctime_ns, exec, hash";

/// Writes a local picture.
pub fn put_local(tx: &Transaction<'_>, x: &LocalRow) -> StoreResult<()> {
    tx.prepare_cached(&format!(
        "INSERT INTO local({LOCAL_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(node) DO UPDATE SET parent = excluded.parent, name = excluded.name,
           name_key = excluded.name_key, is_dir = excluded.is_dir, volume = excluded.volume,
           file_id = excluded.file_id, size = excluded.size, mtime_ns = excluded.mtime_ns,
           ctime_ns = excluded.ctime_ns, exec = excluded.exec, hash = excluded.hash"
    ))
    .map_err(sql_err)?
    .execute(params![
        x.node.0 as i64,
        x.parent.map(|p| p.0 as i64),
        x.name.0,
        x.key.0,
        kind_code(x.kind),
        x.file_id.volume as i64,
        fid_bytes(x.file_id),
        x.size as i64,
        x.mtime_ns,
        x.ctime_ns,
        x.exec,
        hash_bytes(&x.hash),
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a local picture.
pub fn read_local(r: &Row<'_>) -> rusqlite::Result<StoreResult<LocalRow>> {
    let node: i64 = r.get(0)?;
    let parent: Option<i64> = r.get(1)?;
    let name: Vec<u8> = r.get(2)?;
    let key: String = r.get(3)?;
    let kind = kind_of(r.get(4)?);
    let fid = fid_of(r.get(5)?, r.get(6)?);
    let size: i64 = r.get(7)?;
    let mtime_ns: i64 = r.get(8)?;
    let ctime_ns: i64 = r.get(9)?;
    let exec: bool = r.get(10)?;
    let hash = hash_of(r.get(11)?);
    Ok((|| {
        Ok(LocalRow {
            node: NodeId(node as u64),
            parent: parent.map(|p| NodeId(p as u64)),
            name: LocalName(name),
            key: NameKey(key),
            kind: kind?,
            file_id: fid?,
            size: size as u64,
            mtime_ns,
            ctime_ns,
            exec,
            hash: hash?,
        })
    })())
}

// ----- synced -----

/// The columns read for a base.
pub const SYNCED_COLS: &str = "node, parent, name, is_dir, size, hash, remote_etag, local_volume, local_file_id, local_mtime_ns, exec";

/// Writes a base.
pub fn put_synced(tx: &Transaction<'_>, x: &SyncedRow) -> StoreResult<()> {
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO synced({SYNCED_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"
    ))
    .map_err(sql_err)?
    .execute(params![
        x.node.0 as i64,
        x.parent.map(|p| p.0 as i64),
        x.name,
        kind_code(x.kind),
        x.size as i64,
        hash_bytes(&x.hash),
        x.remote_etag.as_ref().map(|e| e.0.clone()),
        x.local_file_id.volume as i64,
        fid_bytes(x.local_file_id),
        x.local_mtime_ns,
        x.exec,
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a base.
pub fn read_synced(r: &Row<'_>) -> rusqlite::Result<StoreResult<SyncedRow>> {
    let node: i64 = r.get(0)?;
    let parent: Option<i64> = r.get(1)?;
    let name: String = r.get(2)?;
    let kind = kind_of(r.get(3)?);
    let size: i64 = r.get(4)?;
    let hash = hash_of(r.get(5)?);
    let etag: Option<String> = r.get(6)?;
    let fid = fid_of(r.get(7)?, r.get(8)?);
    let mtime: i64 = r.get(9)?;
    let exec: bool = r.get(10)?;
    Ok((|| {
        Ok(SyncedRow {
            node: NodeId(node as u64),
            parent: parent.map(|p| NodeId(p as u64)),
            name,
            kind: kind?,
            size: size as u64,
            hash: hash?,
            remote_etag: etag.map(Etag),
            local_file_id: fid?,
            local_mtime_ns: mtime,
            exec,
        })
    })())
}

// ----- op and transfer -----

/// The columns read for an operation.
pub const OP_COLS: &str = "id, kind, node, state, attempt, next_try_ns, cond_kind, cond_text, cond_hash, cond_seq, target_parent, target_name, expect_hash, created_ns";

/// A condition as (kind, text, hash, seq) columns.
fn cond_cols(c: &Option<Condition>) -> (Option<i64>, Option<String>, Option<Vec<u8>>, Option<i64>) {
    match c {
        None => (None, None, None, None),
        Some(Condition::IfMatch(e)) => (Some(0), Some(e.0.clone()), None, None),
        Some(Condition::IfHash(h)) => (Some(1), None, Some(h.0.to_vec()), None),
        Some(Condition::IfNoneMatch) => (Some(2), None, None, None),
        Some(Condition::IfSeq(s)) => (Some(3), None, None, Some(*s as i64)),
        Some(Condition::UnchangedSince(c)) => (Some(4), Some(c.0.clone()), None, None),
    }
}

fn cond_of(
    k: Option<i64>,
    t: Option<String>,
    h: Option<Vec<u8>>,
    s: Option<i64>,
) -> StoreResult<Option<Condition>> {
    Ok(match k {
        None => None,
        Some(0) => Some(Condition::IfMatch(Etag(
            t.ok_or_else(|| corrupt("condition"))?,
        ))),
        Some(1) => Some(Condition::IfHash(
            hash_of(h)?.ok_or_else(|| corrupt("condition"))?,
        )),
        Some(2) => Some(Condition::IfNoneMatch),
        Some(3) => Some(Condition::IfSeq(
            s.ok_or_else(|| corrupt("condition"))? as u64
        )),
        Some(4) => Some(Condition::UnchangedSince(Cursor(
            t.ok_or_else(|| corrupt("condition"))?,
        ))),
        Some(_) => return Err(corrupt("condition")),
    })
}

/// Writes an operation.
pub fn put_op(tx: &Transaction<'_>, o: &OpRow) -> StoreResult<()> {
    let (ck, ct, ch, cs) = cond_cols(&o.condition);
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO op({OP_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
    ))
    .map_err(sql_err)?
    .execute(params![
        o.id.0 as i64,
        i64::from(o.kind.code()),
        o.node.0 as i64,
        i64::from(o.state.code()),
        i64::from(o.attempt),
        o.next_try_ns,
        ck,
        ct,
        ch,
        cs,
        o.target_parent.map(|p| p.0 as i64),
        o.target_name,
        hash_bytes(&o.expect_hash),
        o.created_ns,
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads an operation.
pub fn read_op(r: &Row<'_>) -> rusqlite::Result<StoreResult<OpRow>> {
    let id: i64 = r.get(0)?;
    let kind: i64 = r.get(1)?;
    let node: i64 = r.get(2)?;
    let state: i64 = r.get(3)?;
    let attempt: i64 = r.get(4)?;
    let next: Option<i64> = r.get(5)?;
    let cond = cond_of(r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?);
    let tp: Option<i64> = r.get(10)?;
    let tn: Option<String> = r.get(11)?;
    let eh = hash_of(r.get(12)?);
    let created: i64 = r.get(13)?;
    Ok((|| {
        Ok(OpRow {
            id: OpId(id as u64),
            kind: OpKind::from_code(kind as u8).ok_or_else(|| corrupt("op kind"))?,
            node: NodeId(node as u64),
            state: OpState::from_code(state as u8).ok_or_else(|| corrupt("op state"))?,
            attempt: attempt as u32,
            next_try_ns: next,
            condition: cond?,
            target_parent: tp.map(|p| NodeId(p as u64)),
            target_name: tn,
            expect_hash: eh?,
            created_ns: created,
        })
    })())
}

/// The columns read for a transfer.
pub const TRANSFER_COLS: &str =
    "op, temp, upload_id, total, chunk_size, done, expect_etag, new_volume, new_file_id, aside";

/// Writes a transfer.
pub fn put_transfer(tx: &Transaction<'_>, t: &TransferRow) -> StoreResult<()> {
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO transfer({TRANSFER_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
    ))
    .map_err(sql_err)?
    .execute(params![
        t.op.0 as i64,
        t.temp.as_ref().map(LocalPath::encode),
        t.upload_id,
        t.total as i64,
        t.chunk_size as i64,
        t.done as i64,
        t.expect_etag.as_ref().map(|e| e.0.clone()),
        t.new_file_id.map(|f| f.volume as i64),
        t.new_file_id.map(fid_bytes),
        t.aside.as_ref().map(LocalPath::encode),
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a transfer.
pub fn read_transfer(r: &Row<'_>) -> rusqlite::Result<StoreResult<TransferRow>> {
    let op: i64 = r.get(0)?;
    let temp: Option<Vec<u8>> = r.get(1)?;
    let upload: Option<String> = r.get(2)?;
    let total: i64 = r.get(3)?;
    let cs: i64 = r.get(4)?;
    let done: i64 = r.get(5)?;
    let etag: Option<String> = r.get(6)?;
    let nv: Option<i64> = r.get(7)?;
    let nf: Option<Vec<u8>> = r.get(8)?;
    let aside: Option<Vec<u8>> = r.get(9)?;
    Ok((|| {
        let new_file_id = match (nv, nf) {
            (Some(v), Some(f)) => Some(fid_of(v, f)?),
            _ => None,
        };
        Ok(TransferRow {
            op: OpId(op as u64),
            temp: temp.map(|t| LocalPath::decode(&t)),
            upload_id: upload,
            total: total as u64,
            chunk_size: cs as u64,
            done: done as u64,
            expect_etag: etag.map(Etag),
            new_file_id,
            aside: aside.map(|a| LocalPath::decode(&a)),
        })
    })())
}

// ----- problem, held, conflict -----

/// The columns read for a problem.
pub const PROBLEM_COLS: &str = "subject_kind, subject, kind, reason, first_ns, last_ns, count";

/// A node subject's key bytes.
pub fn node_subject(n: NodeId) -> Vec<u8> {
    n.0.to_be_bytes().to_vec()
}

/// A subject as (kind, key) columns.
pub fn subject_cols(s: &Subject) -> (i64, Vec<u8>) {
    match s {
        Subject::Node(n) => (0, node_subject(*n)),
        Subject::Path(p) => (1, p.encode()),
    }
}

/// Writes a problem.
pub fn put_problem(tx: &Transaction<'_>, p: &ProblemRow) -> StoreResult<()> {
    let (k, b) = subject_cols(&p.subject);
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO problem({PROBLEM_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
    ))
    .map_err(sql_err)?
    .execute(params![
        k,
        b,
        i64::from(p.kind.code()),
        p.reason,
        p.first_ns,
        p.last_ns,
        i64::from(p.count),
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a problem.
pub fn read_problem(r: &Row<'_>) -> rusqlite::Result<StoreResult<ProblemRow>> {
    let sk: i64 = r.get(0)?;
    let sb: Vec<u8> = r.get(1)?;
    let kind: i64 = r.get(2)?;
    let reason: String = r.get(3)?;
    let first: i64 = r.get(4)?;
    let last: i64 = r.get(5)?;
    let count: i64 = r.get(6)?;
    Ok((|| {
        let subject = match sk {
            0 => {
                let a: [u8; 8] = sb.try_into().map_err(|_| corrupt("problem subject"))?;
                Subject::Node(NodeId(u64::from_be_bytes(a)))
            }
            1 => Subject::Path(LocalPath::decode(&sb)),
            _ => return Err(corrupt("problem subject")),
        };
        Ok(ProblemRow {
            subject,
            kind: ProblemKind::from_code(kind as u8).ok_or_else(|| corrupt("problem kind"))?,
            reason,
            first_ns: first,
            last_ns: last,
            count: count as u32,
        })
    })())
}

/// The columns read for a holding-area entry.
pub const HELD_COLS: &str = "id, original, held, hash, size, reason, basis, mount, at_ns";

fn held_reason_code(r: HeldReason) -> i64 {
    match r {
        HeldReason::RemoteDelete => 0,
        HeldReason::Replaced => 1,
        HeldReason::Unselected => 2,
        HeldReason::Retired => 3,
    }
}

fn held_reason_of(c: i64) -> StoreResult<HeldReason> {
    Ok(match c {
        0 => HeldReason::RemoteDelete,
        1 => HeldReason::Replaced,
        2 => HeldReason::Unselected,
        3 => HeldReason::Retired,
        _ => return Err(corrupt("held reason")),
    })
}

fn basis_code(b: PurgeBasis) -> i64 {
    match b {
        PurgeBasis::RemoteTrash => 0,
        PurgeBasis::RemoteVersions => 1,
        PurgeBasis::RemoteFile => 2,
        PurgeBasis::IdenticalReplacement => 3,
        PurgeBasis::Unproven => 4,
    }
}

fn basis_of(c: i64) -> StoreResult<PurgeBasis> {
    Ok(match c {
        0 => PurgeBasis::RemoteTrash,
        1 => PurgeBasis::RemoteVersions,
        2 => PurgeBasis::RemoteFile,
        3 => PurgeBasis::IdenticalReplacement,
        4 => PurgeBasis::Unproven,
        _ => return Err(corrupt("purge basis")),
    })
}

/// Writes a holding-area entry.
pub fn put_held(tx: &Transaction<'_>, h: &HeldRow) -> StoreResult<()> {
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO held({HELD_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"
    ))
    .map_err(sql_err)?
    .execute(params![
        h.id.0 as i64,
        h.original.encode(),
        h.held.encode(),
        hash_bytes(&h.hash),
        h.size as i64,
        held_reason_code(h.reason),
        basis_code(h.basis),
        h.mount.0,
        h.at_ns,
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a holding-area entry.
pub fn read_held(r: &Row<'_>) -> rusqlite::Result<StoreResult<HeldRow>> {
    let id: i64 = r.get(0)?;
    let original: Vec<u8> = r.get(1)?;
    let held: Vec<u8> = r.get(2)?;
    let hash = hash_of(r.get(3)?);
    let size: i64 = r.get(4)?;
    let reason = held_reason_of(r.get(5)?);
    let basis = basis_of(r.get(6)?);
    let mount: String = r.get(7)?;
    let at: i64 = r.get(8)?;
    Ok((|| {
        Ok(HeldRow {
            id: HeldId(id as u64),
            original: LocalPath::decode(&original),
            held: LocalPath::decode(&held),
            hash: hash?,
            size: size as u64,
            reason: reason?,
            basis: basis?,
            mount: MountId(mount),
            at_ns: at,
        })
    })())
}

/// The columns read for a conflict record.
pub const CONFLICT_COLS: &str = "id, node, copy, hash, name, at_ns";

/// Writes a conflict record.
pub fn put_conflict(tx: &Transaction<'_>, c: &ConflictRow) -> StoreResult<()> {
    tx.prepare_cached(&format!(
        "INSERT OR REPLACE INTO conflict({CONFLICT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
    ))
    .map_err(sql_err)?
    .execute(params![
        c.id.0 as i64,
        c.node.0 as i64,
        c.copy.0 as i64,
        hash_bytes(&c.hash),
        c.name,
        c.at_ns,
    ])
    .map_err(sql_err)
    .map(|_| ())
}

/// Reads a conflict record.
pub fn read_conflict(r: &Row<'_>) -> rusqlite::Result<StoreResult<ConflictRow>> {
    let id: i64 = r.get(0)?;
    let node: i64 = r.get(1)?;
    let copy: i64 = r.get(2)?;
    let hash = hash_of(r.get(3)?);
    let name: String = r.get(4)?;
    let at: i64 = r.get(5)?;
    Ok((|| {
        Ok(ConflictRow {
            id: ConflictId(id as u64),
            node: NodeId(node as u64),
            copy: NodeId(copy as u64),
            hash: hash?,
            name,
            at_ns: at,
        })
    })())
}
