//! The executor: one step of one operation at a time, each step ending at
//! a durable point (docs/sync.md 3.6, 3.7, 3.10).
//!
//! The order of effects is the plan's: intent first (the operation's row
//! exists before its first effect), effects idempotent or detectable (temp
//! names and session ids derived from the operation's id; the place an old
//! file is put aside written down before the rename), completion last (the
//! pictures change in the transaction that removes the operation).
//!
//! What every step checks before it touches anything: the item is still
//! what the planner saw. A local file about to be replaced or moved must
//! have the file id and metadata of its local picture (else a local edit
//! arrived: back to the planner); a remote write carries the condition the
//! planner gave it (I3). A step never guesses: when the world is not what
//! it expects, it hands the node back to the planner.

use super::overlay::Overlay;
use super::scan::read_hash;
use super::{Engine, EngineResult, OpRuntime};
use crate::env::Env;
use crate::failure::{Failure, backoff_ms, classify_fs, classify_remote};
use crate::fs::{FsError, layout};
use crate::ids::{Hash, Kind, NodeId, RemoteId};
use crate::names::{LocalName, LocalPath, conflict_name, format_date, key_of_str, to_nfc};
use crate::remote::{
    Entry, ErrorCode, FileCondition, FinishAnswer, MoveCondition, MoveRequest, RemoteError,
    UploadStart, UploadState, UploadTarget,
};
use crate::store::{Batch, Store, Write};
use crate::tree::{
    Condition, ConflictRow, HeldReason, HeldRow, LocalRow, NodeRecord, OpKind, OpRow, OpState,
    PurgeBasis, RemoteRow, Subject, SyncedRow, TransferRow,
};
use crate::view;

/// What one step of an operation came to.
enum Outcome {
    /// A durable point was reached; these writes record it.
    Continue(Batch),
    /// The operation is complete; these writes change the pictures.
    Done(Batch),
    /// It failed.
    Fail(Fail),
}

/// A failure with what the engine should take in from it.
struct Fail {
    failure: Failure,
    /// The present entry Hearth sent with a refusal (8.4).
    entry: Option<Entry>,
    /// Folders to read again.
    rescan: Vec<NodeId>,
    /// The local file's hash is to be computed again.
    rehash: bool,
}

impl Fail {
    fn new(failure: Failure) -> Fail {
        Fail {
            failure,
            entry: None,
            rescan: Vec::new(),
            rehash: false,
        }
    }

    fn replan() -> Fail {
        Fail::new(Failure::Replan)
    }

    fn remote(e: &RemoteError) -> Fail {
        Fail {
            failure: classify_remote(e),
            entry: e.entry().cloned(),
            rescan: Vec::new(),
            rehash: false,
        }
    }

    fn fs(e: &FsError) -> Fail {
        Fail::new(classify_fs(e))
    }

    fn rescan(mut self, n: Option<NodeId>) -> Fail {
        self.rescan.extend(n);
        self
    }
}

fn fail(f: Fail) -> EngineResult<Outcome> {
    Ok(Outcome::Fail(f))
}

/// The contract's file condition from the planner's condition.
fn file_condition(c: &Option<Condition>) -> Option<FileCondition> {
    match c {
        Some(Condition::IfMatch(e)) => Some(FileCondition::IfMatch(e.clone())),
        Some(Condition::IfHash(h)) => Some(FileCondition::IfHash(*h)),
        _ => None,
    }
}

impl<S: Store> Engine<S> {
    /// One step of one runnable operation, chosen by the scheduler. False
    /// when none is runnable now.
    pub(super) fn op_work(&mut self, env: &mut Env<'_>, now: i64) -> EngineResult<bool> {
        let ready: Vec<OpRow> = self
            .store
            .ops()?
            .into_iter()
            .filter(|o| o.next_try_ns.is_none_or(|t| t <= now))
            .collect();
        if ready.is_empty() {
            return Ok(false);
        }
        let i = if ready.len() > 1 {
            env.scheduler.pick(ready.len()) % ready.len()
        } else {
            0
        };
        let op = ready[i].clone();
        let outcome = match op.kind {
            OpKind::Download => self.op_download(env, op.clone())?,
            OpKind::MkdirLocal => self.op_mkdir_local(env, &op)?,
            OpKind::Upload | OpKind::UploadNew => self.op_upload(env, op.clone())?,
            OpKind::MkdirRemote => self.op_mkdir_remote(env, &op)?,
            OpKind::MoveLocal => self.op_move_local(env, op.clone())?,
            OpKind::MoveRemote => self.op_move_remote(env, &op)?,
            OpKind::DeleteLocal => self.op_delete_local(env, op.clone())?,
            OpKind::RemoveLocalFolder => self.op_remove_local_folder(env, &op)?,
            OpKind::DeleteRemote => self.op_delete_remote(env, &op)?,
            OpKind::DeleteRemoteFolder => self.op_delete_remote_folder(env, &op)?,
            OpKind::ConflictAsideLocal => self.op_aside_local(env, &op)?,
            OpKind::ConflictAsideRemote => self.op_aside_remote(env, &op)?,
        };
        match outcome {
            Outcome::Continue(w) => self.commit(w)?,
            Outcome::Done(w) => {
                let mut b = vec![Write::DelOp(op.id)];
                let gone = w
                    .iter()
                    .any(|x| matches!(x, Write::DelNode(n) if *n == op.node));
                b.extend(w);
                if !gone {
                    b.push(Write::MarkDirty(op.node));
                    self.clear_problem(op.node, &mut b);
                } else {
                    self.with_problem.remove(&op.node);
                }
                self.runtime.remove(&op.id);
                self.commit(b)?;
            }
            Outcome::Fail(f) => self.fail_op(env, op, f)?,
        }
        Ok(true)
    }

    /// Acts on a failed step (3.10's four classes).
    fn fail_op(&mut self, env: &mut Env<'_>, mut op: OpRow, f: Fail) -> EngineResult<()> {
        let now = env.clock.now_ns();
        for n in &f.rescan {
            self.scan_queue.entry(*n).or_insert(false);
        }
        match f.failure {
            Failure::Retry { after_s } => {
                op.attempt = op.attempt.saturating_add(1);
                let ms = backoff_ms(
                    op.attempt,
                    self.cfg.backoff_base_ms,
                    self.cfg.backoff_cap_ms,
                    env.random.next_u64(),
                    after_s,
                );
                let ms = i64::try_from(ms).unwrap_or(i64::MAX / 2_000_000);
                op.next_try_ns = Some(now.saturating_add(ms * 1_000_000));
                self.commit(vec![Write::PutOp(op)])?;
            }
            Failure::WaitForChange(kind, why) => {
                self.runtime.remove(&op.id);
                if op.kind == OpKind::Download {
                    let _ = env.fs.remove_temp(&layout::temp_for(op.id));
                }
                let w = self.problem_write(Subject::Node(op.node), kind, why, now)?;
                self.commit(vec![Write::DelOp(op.id), w, Write::ClearDirty(op.node)])?;
            }
            Failure::Replan => {
                self.runtime.remove(&op.id);
                if op.kind == OpKind::Download && op.state < OpState::Swapping {
                    let _ = env.fs.remove_temp(&layout::temp_for(op.id));
                }
                let mut b = vec![Write::DelOp(op.id), Write::MarkDirty(op.node)];
                if f.rehash {
                    if let Some(mut l) = self.store.node(op.node)?.and_then(|r| r.local) {
                        l.hash = None;
                        b.push(Write::PutLocal(l));
                        self.hash_queue.insert(op.node);
                    }
                }
                self.commit(b)?;
                if let Some(e) = f.entry {
                    self.take_in_entry(op.node, &e)?;
                }
            }
            Failure::Stop(r) => self.stop(r),
        }
        Ok(())
    }

    /// A present entry from a refusal goes into the remote picture by the
    /// feed's own rules (8.4: "apply entry, plan again").
    fn take_in_entry(&mut self, n: NodeId, e: &Entry) -> EngineResult<()> {
        let mount_id = match self.store.node(n)? {
            Some(rec) => rec.remote.map(|r| r.mount).unwrap_or(rec.mount),
            None => return Ok(()),
        };
        let Some(m) = self.store.mounts()?.into_iter().find(|m| m.id == mount_id) else {
            return Ok(());
        };
        let mut lists = Vec::new();
        let mut ov = Overlay::new(&self.store);
        Self::upsert(&mut self.ids, &mut ov, &m, e, None, &mut lists)?;
        let w = std::mem::take(&mut ov.writes);
        drop(ov);
        self.commit(w)
    }

    // ----- small helpers -----

    fn rec(&self, n: NodeId) -> EngineResult<Option<NodeRecord>> {
        Ok(self.store.node(n)?)
    }

    fn path(&self, n: NodeId) -> EngineResult<Option<LocalPath>> {
        Ok(view::local_path(&self.store, n)?)
    }

    fn remote_id(&self, n: Option<NodeId>) -> EngineResult<Option<RemoteId>> {
        match n {
            Some(n) => Ok(self.store.node(n)?.and_then(|r| r.remote).map(|r| r.id)),
            None => Ok(None),
        }
    }

    /// The path an operation puts something at: its target folder's path
    /// and its name, written in NFC (3.8).
    fn target_path(&self, op: &OpRow) -> EngineResult<Option<LocalPath>> {
        let (Some(p), Some(name)) = (op.target_parent, &op.target_name) else {
            return Ok(None);
        };
        Ok(self
            .path(p)?
            .map(|pp| pp.join(&LocalName::from_str_nfc(name))))
    }

    fn mount_of(&self, rec: &NodeRecord) -> EngineResult<Option<crate::tree::MountRow>> {
        Ok(self.store.mounts()?.into_iter().find(|m| m.id == rec.mount))
    }

    /// The local picture of an item at a path, from a fresh `stat`.
    fn fresh_local(
        &self,
        env: &mut Env<'_>,
        n: NodeId,
        parent: Option<NodeId>,
        name: &LocalName,
        path: &LocalPath,
        kind: Kind,
        hash: Option<Hash>,
    ) -> Result<LocalRow, FsError> {
        let m = env.fs.stat(path)?.ok_or(FsError::NotFound)?;
        let key = crate::names::name_key(name, &self.cfg.volume).ok_or(FsError::NameNotHoldable)?;
        Ok(LocalRow {
            node: n,
            parent,
            name: name.clone(),
            key,
            kind,
            file_id: m.file_id,
            size: if kind == Kind::File { m.size } else { 0 },
            mtime_ns: m.mtime_ns,
            ctime_ns: m.ctime_ns,
            exec: m.exec,
            hash: if kind == Kind::File { hash } else { None },
        })
    }

    /// The base for an item both sides now hold the same way.
    fn base_from(l: &LocalRow, remote_etag: Option<crate::ids::Etag>) -> SyncedRow {
        SyncedRow {
            node: l.node,
            parent: l.parent,
            name: to_nfc(l.name.as_str().unwrap_or_default()),
            kind: l.kind,
            size: l.size,
            hash: l.hash,
            remote_etag,
            local_file_id: l.file_id,
            local_mtime_ns: l.mtime_ns,
            exec: l.exec,
        }
    }

    /// The remote picture from a write's answer (F8: the answer's entry is
    /// what the feed will carry).
    fn remote_from(n: NodeId, mount: &crate::ids::MountId, e: &Entry) -> RemoteRow {
        RemoteRow {
            node: n,
            id: e.id.clone(),
            mount: mount.clone(),
            parent: e.parent.clone(),
            name: e.name.clone(),
            kind: e.kind,
            size: e.size.unwrap_or(0),
            mtime_us: e.mtime_us,
            etag: e.etag.clone(),
            hash: e.hash,
            exec: e.exec,
            seq: e.seq,
            crypt: crate::tree::Crypt::None,
            departed: None,
            listed: 0,
        }
    }

    /// Writes for a remote picture from an answer, unless the feed already
    /// brought a newer one, or the id belongs to another node (then the
    /// planner pairs the two).
    fn remote_answer_writes(
        &self,
        rec: &NodeRecord,
        e: &Entry,
        b: &mut Batch,
    ) -> EngineResult<bool> {
        if let Some(o) = self.store.node_by_remote(&e.id)? {
            if o != rec.node {
                return Ok(false);
            }
        }
        let mount = rec
            .remote
            .as_ref()
            .map(|r| r.mount.clone())
            .unwrap_or(rec.mount.clone());
        let newer = rec
            .remote
            .as_ref()
            .is_some_and(|r| r.id == e.id && r.seq > e.seq);
        if !newer {
            b.push(Write::PutRemote(Self::remote_from(rec.node, &mount, e)));
        }
        Ok(true)
    }

    /// The holding area's place for an item: `.hearthsync/held/<date>/<id>-<name>`.
    fn held_place(
        &mut self,
        env: &mut Env<'_>,
        name: &LocalName,
    ) -> Result<(crate::ids::HeldId, LocalPath), FsError> {
        let date = format_date(env.clock.now_ns().div_euclid(1_000_000_000));
        let dir = layout::held_root().join(&LocalName(date.into_bytes()));
        match env.fs.mkdir(&layout::held_root()) {
            Ok(()) | Err(FsError::Exists) => {}
            Err(e) => return Err(e),
        }
        match env.fs.mkdir(&dir) {
            Ok(()) | Err(FsError::Exists) => {}
            Err(e) => return Err(e),
        }
        let id = self.alloc_held();
        let mut n = format!("{}-", id.0).into_bytes();
        n.extend_from_slice(&name.0);
        Ok((id, dir.join(&LocalName(n))))
    }

    /// A free conflict-copy name in a local folder (3.5.1).
    fn local_conflict_name(
        &self,
        env: &mut Env<'_>,
        parent: NodeId,
        parent_path: &LocalPath,
        original: &str,
        mtime_ns: i64,
    ) -> EngineResult<Option<String>> {
        let secs = mtime_ns.div_euclid(1_000_000_000);
        let local = secs + i64::from(env.clock.utc_offset_s(mtime_ns));
        let max = self.name_limit();
        for k in 1..1000 {
            let Some(name) = conflict_name(original, &self.device, local, k, max) else {
                return Ok(None);
            };
            let key = key_of_str(&name, &self.cfg.volume);
            if self.store.local_child_by_key(parent, &key)?.is_some() {
                continue;
            }
            match env
                .fs
                .stat(&parent_path.join(&LocalName::from_str_nfc(&name)))
            {
                Ok(None) => return Ok(Some(name)),
                Ok(Some(_)) => continue,
                Err(_) => return Ok(None),
            }
        }
        Ok(None)
    }

    /// The longest name both Hearth and the volume take.
    fn name_limit(&self) -> usize {
        let hearth = self.hello.as_ref().map_or(250, |h| h.limits.name_bytes_max);
        match self.cfg.volume.max_name_bytes {
            Some(v) => hearth.min(v),
            None => hearth,
        }
    }

    // ----- downloads (3.6, 3.7) -----

    fn op_download(&mut self, env: &mut Env<'_>, mut op: OpRow) -> EngineResult<Outcome> {
        let n = op.node;
        let Some(rec) = self.rec(n)? else {
            return fail(Fail::replan());
        };
        let Some(r) = rec.remote.clone() else {
            return fail(Fail::replan());
        };
        let Some(Condition::IfMatch(etag)) = op.condition.clone() else {
            return fail(Fail::new(Failure::WaitForChange(
                crate::tree::ProblemKind::Refused,
                "a download without the etag it reads".into(),
            )));
        };
        if r.etag.as_ref() != Some(&etag) && op.state < OpState::Swapping {
            // The feed moved on: the plan is stale.
            return fail(Fail::replan());
        }
        let temp = layout::temp_for(op.id);
        match op.state {
            OpState::Planned => {
                let free = match env.fs.free_space() {
                    Ok(f) => f,
                    Err(e) => return fail(Fail::fs(&e)),
                };
                if free < r.size.saturating_add(self.cfg.disk_reserve_bytes) {
                    return fail(Fail::new(Failure::Stop(
                        crate::failure::StopReason::DiskFull,
                    )));
                }
                match env.fs.create(&temp) {
                    Ok(()) => {}
                    Err(FsError::Exists) => {
                        if let Err(e) = env.fs.truncate(&temp, 0) {
                            return fail(Fail::fs(&e));
                        }
                    }
                    Err(e) => return fail(Fail::fs(&e)),
                }
                op.state = OpState::Transferring;
                let t = TransferRow {
                    op: op.id,
                    temp: Some(temp),
                    upload_id: None,
                    total: r.size,
                    chunk_size: self.cfg.range_size,
                    done: 0,
                    expect_etag: Some(etag),
                    new_file_id: None,
                    aside: None,
                };
                Ok(Outcome::Continue(vec![
                    Write::PutOp(op),
                    Write::PutTransfer(t),
                ]))
            }
            OpState::Transferring => {
                let Some(mut t) = self.store.transfer(op.id)? else {
                    op.state = OpState::Planned;
                    return Ok(Outcome::Continue(vec![Write::PutOp(op)]));
                };
                if t.done < t.total {
                    let len = t.chunk_size.min(t.total - t.done);
                    let ans = match env.remote.read(&r.id, &etag, t.done, len) {
                        Ok(a) => a,
                        Err(e) => return fail(Fail::remote(&e)),
                    };
                    if ans.bytes.len() as u64 != len || ans.total != t.total {
                        // A short body: the connection was cut (7).
                        return fail(Fail::new(Failure::Retry { after_s: None }));
                    }
                    if let Err(e) = env.fs.write_at(&temp, t.done, &ans.bytes) {
                        return fail(Fail::fs(&e));
                    }
                    t.done += len;
                    return Ok(Outcome::Continue(vec![Write::PutTransfer(t)]));
                }
                // Whole: verify before anything is visible (I5).
                let got = match read_hash(env, &temp, t.total, self.cfg.range_size) {
                    Ok(h) => h,
                    Err(e) => return fail(Fail::fs(&e)),
                };
                if let Some(want) = r.hash {
                    if want != got {
                        if op.attempt == 0 {
                            // Once more from zero, then it is Hearth's
                            // problem to report (3.6).
                            let _ = env.fs.truncate(&temp, 0);
                            t.done = 0;
                            op.attempt = 1;
                            return Ok(Outcome::Continue(vec![
                                Write::PutOp(op),
                                Write::PutTransfer(t),
                            ]));
                        }
                        return fail(Fail::new(Failure::WaitForChange(
                            crate::tree::ProblemKind::HashMismatch,
                            format!(
                                "Hearth's copy does not match its hash: expected {want}, read {got}"
                            ),
                        )));
                    }
                }
                let now = env.clock.now_ns();
                let mtime_ns = if r.mtime_us > 0 {
                    r.mtime_us.saturating_mul(1000)
                } else {
                    // A zero or 1970 time from Hearth is replaced by now
                    // (3.15).
                    now
                };
                if let Err(e) = env.fs.set_meta(&temp, mtime_ns, r.exec) {
                    return fail(Fail::fs(&e));
                }
                if let Err(e) = env.fs.sync_file(&temp) {
                    return fail(Fail::fs(&e));
                }
                let m = match env.fs.stat(&temp) {
                    Ok(Some(m)) if m.size == t.total => m,
                    Ok(_) => return fail(Fail::new(Failure::Retry { after_s: None })),
                    Err(e) => return fail(Fail::fs(&e)),
                };
                t.new_file_id = Some(m.file_id);
                op.expect_hash = Some(got);
                op.state = OpState::Verified;
                Ok(Outcome::Continue(vec![
                    Write::PutOp(op),
                    Write::PutTransfer(t),
                ]))
            }
            OpState::Verified => self.download_swap(env, op, &rec),
            OpState::Swapping => {
                // After a crash: did the swap or rename happen? The file ids
                // say, without trusting the row.
                let Some(t) = self.store.transfer(op.id)? else {
                    return fail(Fail::replan());
                };
                let target = if op.target_name.is_some() {
                    self.target_path(&op)?
                } else {
                    self.path(n)?
                };
                let Some(target) = target else {
                    return fail(Fail::replan());
                };
                let at_target = env.fs.stat(&target).ok().flatten().map(|m| m.file_id);
                let at_temp = env.fs.stat(&temp).ok().flatten().map(|m| m.file_id);
                if at_target.is_some() && at_target == t.new_file_id {
                    if op.target_name.is_some() {
                        return self.download_new_done(env, &op, &rec, &target);
                    }
                    op.state = OpState::Swapped;
                    Ok(Outcome::Continue(vec![Write::PutOp(op)]))
                } else if at_temp.is_some() && at_temp == t.new_file_id {
                    op.state = OpState::Verified;
                    Ok(Outcome::Continue(vec![Write::PutOp(op)]))
                } else {
                    fail(Fail::replan().rescan(rec.local.and_then(|l| l.parent)))
                }
            }
            OpState::Swapped => self.download_examine(env, op, &rec),
            OpState::Finishing => fail(Fail::replan()),
        }
    }

    /// Steps 3 and 4 of 3.7: look at the target once more, then swap (or
    /// rename without replacing, for a new file).
    fn download_swap(
        &mut self,
        env: &mut Env<'_>,
        mut op: OpRow,
        rec: &NodeRecord,
    ) -> EngineResult<Outcome> {
        let temp = layout::temp_for(op.id);
        let creating = op.target_name.is_some();
        let target = if creating {
            self.target_path(&op)?
        } else {
            self.path(op.node)?
        };
        let Some(target) = target else {
            return fail(Fail::replan());
        };
        let now = match env.fs.stat(&target) {
            Ok(m) => m,
            Err(e) => return fail(Fail::fs(&e)),
        };
        if creating {
            if now.is_some() {
                return fail(Fail::replan().rescan(op.target_parent));
            }
        } else {
            let Some(l) = &rec.local else {
                return fail(Fail::replan());
            };
            match &now {
                Some(m) if l.same_metadata(m) => {}
                // A local edit arrived: stop, back to the planner (3.7, 3).
                _ => return fail(Fail::replan().rescan(l.parent)),
            }
        }
        op.state = OpState::Swapping;
        self.commit(vec![Write::PutOp(op.clone())])?;
        if creating {
            match env.fs.rename_noreplace(&temp, &target) {
                Ok(()) => self.download_new_done(env, &op, rec, &target),
                Err(FsError::Exists) => fail(Fail::replan().rescan(op.target_parent)),
                Err(e) => fail(Fail::fs(&e)),
            }
        } else {
            match env.fs.exchange(&temp, &target) {
                Ok(()) => {
                    op.state = OpState::Swapped;
                    Ok(Outcome::Continue(vec![Write::PutOp(op)]))
                }
                Err(e) => fail(Fail::fs(&e).rescan(rec.local.as_ref().and_then(|l| l.parent))),
            }
        }
    }

    /// A new file is in place: record it on both pictures and the base.
    fn download_new_done(
        &mut self,
        env: &mut Env<'_>,
        op: &OpRow,
        rec: &NodeRecord,
        target: &LocalPath,
    ) -> EngineResult<Outcome> {
        let Some(name) = target.file_name().cloned() else {
            return fail(Fail::replan());
        };
        if let Some(dir) = target.parent() {
            let _ = env.fs.sync_dir(&dir);
        }
        let l = match self.fresh_local(
            env,
            op.node,
            op.target_parent,
            &name,
            target,
            Kind::File,
            op.expect_hash,
        ) {
            Ok(l) => l,
            Err(e) => return fail(Fail::fs(&e)),
        };
        let etag = rec.remote.as_ref().and_then(|r| r.etag.clone());
        let s = Self::base_from(&l, etag);
        Ok(Outcome::Done(vec![Write::PutLocal(l), Write::PutSynced(s)]))
    }

    /// Steps 5 and 6 of 3.7: what was replaced is examined. The synced
    /// content goes to the holding area; anything else is a version nobody
    /// has, kept beside the target as a conflict copy (I2).
    fn download_examine(
        &mut self,
        env: &mut Env<'_>,
        op: OpRow,
        rec: &NodeRecord,
    ) -> EngineResult<Outcome> {
        let temp = layout::temp_for(op.id);
        let Some(mut t) = self.store.transfer(op.id)? else {
            return fail(Fail::replan());
        };
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        let Some(target) = self.path(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(mount) = self.mount_of(rec)? else {
            return fail(Fail::replan());
        };
        let new_hash = op.expect_hash;
        let old_meta = match env.fs.stat(&temp) {
            Ok(m) => m,
            Err(e) => return fail(Fail::fs(&e)),
        };
        let mut b: Batch = Vec::new();
        let aside = match (old_meta, t.aside.clone()) {
            (None, Some(a)) => a,
            (None, None) => return fail(Fail::replan().rescan(l.parent)),
            (Some(om), _) => {
                // The same file as checked before the swap: identity, size
                // and modification time (the swap itself moves the change
                // time, so it is not compared here).
                let synced_old = l.file_id == om.file_id
                    && l.size == om.size
                    && l.mtime_ns == om.mtime_ns
                    && l.hash.is_some()
                    && rec.synced.as_ref().is_some_and(|s| s.hash == l.hash);
                let old_hash = if synced_old {
                    l.hash
                } else {
                    match read_hash(env, &temp, om.size, self.cfg.range_size) {
                        Ok(h) => Some(h),
                        Err(e) => return fail(Fail::fs(&e)),
                    }
                };
                let aside = if synced_old || old_hash == new_hash {
                    let (id, place) = match self.held_place(env, &l.name) {
                        Ok(x) => x,
                        Err(e) => return fail(Fail::fs(&e)),
                    };
                    let basis = if old_hash == new_hash {
                        PurgeBasis::IdenticalReplacement
                    } else if mount.versions {
                        PurgeBasis::RemoteVersions
                    } else {
                        PurgeBasis::Unproven
                    };
                    b.push(Write::PutHeld(HeldRow {
                        id,
                        original: target.clone(),
                        held: place.clone(),
                        hash: old_hash,
                        size: om.size,
                        reason: HeldReason::Replaced,
                        basis,
                        mount: mount.id.clone(),
                        at_ns: env.clock.now_ns(),
                    }));
                    place
                } else {
                    let Some(parent) = l.parent else {
                        return fail(Fail::replan());
                    };
                    let Some(pp) = target.parent() else {
                        return fail(Fail::replan());
                    };
                    let original = to_nfc(l.name.as_str().unwrap_or_default());
                    let Some(name) =
                        self.local_conflict_name(env, parent, &pp, &original, om.mtime_ns)?
                    else {
                        return fail(Fail::new(Failure::Retry { after_s: None }));
                    };
                    let place = pp.join(&LocalName::from_str_nfc(&name));
                    let copy = self.alloc_node();
                    b.push(Write::PutNode(copy, rec.mount.clone()));
                    b.push(Write::PutLocal(LocalRow {
                        node: copy,
                        parent: Some(parent),
                        name: LocalName::from_str_nfc(&name),
                        key: key_of_str(&name, &self.cfg.volume),
                        kind: Kind::File,
                        file_id: om.file_id,
                        size: om.size,
                        mtime_ns: om.mtime_ns,
                        ctime_ns: om.ctime_ns,
                        exec: om.exec,
                        hash: old_hash,
                    }));
                    b.push(Write::MarkDirty(copy));
                    let cid = self.alloc_conflict();
                    b.push(Write::PutConflict(ConflictRow {
                        id: cid,
                        node: op.node,
                        copy,
                        hash: old_hash,
                        name,
                        at_ns: env.clock.now_ns(),
                    }));
                    place
                };
                // Written down before the rename, so a crash between the
                // two finds it (3.10).
                t.aside = Some(aside.clone());
                self.commit(vec![Write::PutTransfer(t.clone())])?;
                match env.fs.rename_noreplace(&temp, &aside) {
                    Ok(()) => {}
                    Err(FsError::Exists) => {
                        t.aside = None;
                        return Ok(Outcome::Continue(vec![Write::PutTransfer(t)]));
                    }
                    Err(e) => return fail(Fail::fs(&e)),
                }
                aside
            }
        };
        if b.is_empty() {
            // Resumed after a crash between the rename and the commit: the
            // rows that go with the place are made again from the file.
            let Some(m) = env.fs.stat(&aside).ok().flatten() else {
                return fail(Fail::replan().rescan(l.parent));
            };
            if aside.0.first() == layout::held_root().0.first() {
                let id = self.alloc_held();
                b.push(Write::PutHeld(HeldRow {
                    id,
                    original: target.clone(),
                    held: aside.clone(),
                    hash: None,
                    size: m.size,
                    reason: HeldReason::Replaced,
                    basis: PurgeBasis::Unproven,
                    mount: mount.id.clone(),
                    at_ns: env.clock.now_ns(),
                }));
            } else {
                // A conflict copy: the scanner finds it as a new file.
                if let Some(p) = l.parent {
                    self.scan_queue.entry(p).or_insert(false);
                }
            }
        }
        if let Some(dir) = target.parent() {
            let _ = env.fs.sync_dir(&dir);
        }
        let new_l = match self.fresh_local(
            env,
            op.node,
            l.parent,
            &l.name,
            &target,
            Kind::File,
            new_hash,
        ) {
            Ok(x) => x,
            Err(e) => return fail(Fail::fs(&e)),
        };
        let s = Self::base_from(&new_l, rec.remote.as_ref().and_then(|r| r.etag.clone()));
        b.push(Write::PutLocal(new_l));
        b.push(Write::PutSynced(s));
        Ok(Outcome::Done(b))
    }

    fn op_mkdir_local(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(path) = self.target_path(op)? else {
            return fail(Fail::replan());
        };
        match env.fs.mkdir(&path) {
            Ok(()) => {}
            Err(FsError::Exists) => return fail(Fail::replan().rescan(op.target_parent)),
            Err(e) => return fail(Fail::fs(&e)),
        }
        let Some(name) = path.file_name().cloned() else {
            return fail(Fail::replan());
        };
        let l = match self.fresh_local(
            env,
            op.node,
            op.target_parent,
            &name,
            &path,
            Kind::Folder,
            None,
        ) {
            Ok(l) => l,
            Err(e) => return fail(Fail::fs(&e)),
        };
        let s = Self::base_from(&l, None);
        let mut b = vec![Write::PutLocal(l)];
        if rec.remote.is_some() {
            b.push(Write::PutSynced(s));
        }
        Ok(Outcome::Done(b))
    }

    // ----- uploads (3.6, contract 9.1, 9.2, 10) -----

    fn op_upload(&mut self, env: &mut Env<'_>, mut op: OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        if l.hash.is_none() || l.hash != op.expect_hash {
            return fail(Fail::replan());
        }
        let Some(expect) = op.expect_hash else {
            return fail(Fail::replan());
        };
        let Some(path) = self.path(op.node)? else {
            return fail(Fail::replan());
        };
        let body_max = self
            .hello
            .as_ref()
            .map_or(self.cfg.small_file_max, |h| h.limits.body_max);
        let small = l.size <= self.cfg.small_file_max.min(body_max);
        if small && op.state == OpState::Planned {
            return self.upload_small(env, &op, &rec, &l, &path, expect);
        }
        let session = self.session_id(&op)?;
        match op.state {
            OpState::Planned => {
                let chunk = self.chunk_size(l.size);
                let target = match op.kind {
                    OpKind::UploadNew => {
                        let (Some(parent), Some(name)) =
                            (self.remote_id(op.target_parent)?, op.target_name.clone())
                        else {
                            return fail(Fail::replan());
                        };
                        UploadTarget::Create { parent, name }
                    }
                    _ => {
                        let (Some(r), Some(cond)) = (&rec.remote, file_condition(&op.condition))
                        else {
                            return fail(Fail::replan());
                        };
                        UploadTarget::Replace {
                            file: r.id.clone(),
                            condition: cond,
                        }
                    }
                };
                let start = UploadStart {
                    id: session.clone(),
                    target,
                    size: l.size,
                    hash: expect,
                    chunk_size: chunk,
                    mtime_us: l.mtime_ns.div_euclid(1000),
                    exec: l.exec,
                };
                match env.remote.upload_start(&start) {
                    Ok(s) => {
                        self.runtime.insert(
                            op.id,
                            OpRuntime {
                                hasher: Some(crate::ids::ContentHasher::new()),
                                next_chunk: 1,
                                stored: s.chunks.into_iter().collect(),
                            },
                        );
                        op.state = OpState::Transferring;
                        let t = TransferRow {
                            op: op.id,
                            temp: None,
                            upload_id: Some(session),
                            total: l.size,
                            chunk_size: chunk,
                            done: 0,
                            expect_etag: rec.remote.as_ref().and_then(|r| r.etag.clone()),
                            new_file_id: None,
                            aside: None,
                        };
                        Ok(Outcome::Continue(vec![
                            Write::PutOp(op),
                            Write::PutTransfer(t),
                        ]))
                    }
                    Err(e) => self.upload_refused(env, &op, &rec, &e, expect, Some(&session)),
                }
            }
            OpState::Transferring => {
                let Some(mut t) = self.store.transfer(op.id)? else {
                    op.state = OpState::Planned;
                    return Ok(Outcome::Continue(vec![Write::PutOp(op)]));
                };
                if !self.runtime.contains_key(&op.id) {
                    // After a restart Hearth's status is trusted over the
                    // row (3.6).
                    match env.remote.upload_status(&session) {
                        Ok(s) => match s.state {
                            UploadState::Finished(entry) => {
                                return self.upload_done(&rec, &l, &entry, expect);
                            }
                            UploadState::Finishing => {
                                op.state = OpState::Finishing;
                                return Ok(Outcome::Continue(vec![Write::PutOp(op)]));
                            }
                            UploadState::Open => {
                                self.runtime.insert(
                                    op.id,
                                    OpRuntime {
                                        hasher: Some(crate::ids::ContentHasher::new()),
                                        next_chunk: 1,
                                        stored: s.chunks.into_iter().collect(),
                                    },
                                );
                            }
                        },
                        Err(e) if e.code() == Some(&ErrorCode::UploadGone) => {
                            op.state = OpState::Planned;
                            return Ok(Outcome::Continue(vec![Write::PutOp(op)]));
                        }
                        Err(e) => return fail(Fail::remote(&e)),
                    }
                }
                let chunks = t.total.div_ceil(t.chunk_size.max(1));
                let Some(rt) = self.runtime.get(&op.id).cloned() else {
                    return fail(Fail::replan());
                };
                if rt.next_chunk <= chunks {
                    let k = rt.next_chunk;
                    let off = (k - 1) * t.chunk_size;
                    let len = t.chunk_size.min(t.total - off);
                    let bytes = match env.fs.read(&path, off, len) {
                        Ok(b) => b,
                        Err(e) => return fail(Fail::fs(&e)),
                    };
                    if bytes.len() as u64 != len {
                        // The file shrank under the upload (10.4).
                        let _ = env.remote.upload_abort(&session);
                        let mut f = Fail::replan();
                        f.rehash = true;
                        return fail(f);
                    }
                    if !rt.stored.contains(&k) {
                        if let Err(e) =
                            env.remote
                                .upload_chunk(&session, k, &bytes, Hash::of(&bytes))
                        {
                            if e.code() == Some(&ErrorCode::UploadGone) {
                                self.runtime.remove(&op.id);
                                op.state = OpState::Planned;
                                return Ok(Outcome::Continue(vec![Write::PutOp(op)]));
                            }
                            return fail(Fail::remote(&e));
                        }
                    }
                    if let Some(rt) = self.runtime.get_mut(&op.id) {
                        if let Some(h) = rt.hasher.as_mut() {
                            h.update(&bytes);
                        }
                        rt.next_chunk = k + 1;
                    }
                    t.done = k;
                    return Ok(Outcome::Continue(vec![Write::PutTransfer(t)]));
                }
                // Every chunk read: the second observation of the content
                // (10.4). A file that changed under the upload is not
                // finished.
                let whole = rt.hasher.map(|h| h.finish());
                if whole != Some(expect) {
                    let _ = env.remote.upload_abort(&session);
                    self.runtime.remove(&op.id);
                    let mut f = Fail::replan();
                    f.rehash = true;
                    return fail(f);
                }
                op.state = OpState::Finishing;
                self.commit(vec![Write::PutOp(op.clone())])?;
                self.upload_finish(env, op, &rec, &l, &session, expect)
            }
            OpState::Finishing => self.upload_finish(env, op, &rec, &l, &session, expect),
            _ => fail(Fail::replan()),
        }
    }

    /// A file up to the one-request size: read, hashed again, sent with its
    /// hash and condition (9.1, 9.2).
    fn upload_small(
        &mut self,
        env: &mut Env<'_>,
        op: &OpRow,
        rec: &NodeRecord,
        l: &LocalRow,
        path: &LocalPath,
        expect: Hash,
    ) -> EngineResult<Outcome> {
        let mut body = Vec::with_capacity(usize::try_from(l.size).unwrap_or(0));
        let mut off = 0;
        while off < l.size {
            let piece = match env
                .fs
                .read(path, off, self.cfg.range_size.min(l.size - off))
            {
                Ok(p) => p,
                Err(e) => return fail(Fail::fs(&e)),
            };
            if piece.is_empty() {
                break;
            }
            off += piece.len() as u64;
            body.extend_from_slice(&piece);
        }
        // The second observation (3.6): the bytes sent are the bytes
        // hashed at rest, or nothing is sent.
        if Hash::of(&body) != expect {
            let mut f = Fail::replan();
            f.rehash = true;
            return fail(f);
        }
        let mtime_us = l.mtime_ns.div_euclid(1000);
        let res = match op.kind {
            OpKind::UploadNew => {
                let (Some(parent), Some(name)) =
                    (self.remote_id(op.target_parent)?, op.target_name.clone())
                else {
                    return fail(Fail::replan());
                };
                let copied = self.try_copy(env, rec, &parent, &name, l.size, expect)?;
                match copied {
                    Some(r) => r,
                    None => env
                        .remote
                        .create_file(&parent, &name, &body, expect, mtime_us, l.exec),
                }
            }
            _ => {
                let (Some(r), Some(cond)) = (&rec.remote, file_condition(&op.condition)) else {
                    return fail(Fail::replan());
                };
                env.remote
                    .replace_content(&r.id, &cond, &body, expect, mtime_us)
            }
        };
        match res {
            Ok(entry) => self.upload_done(rec, l, &entry, expect),
            Err(e) => self.upload_refused(env, op, rec, &e, expect, None),
        }
    }

    /// A new file whose bytes the mount already holds becomes a server-side
    /// copy (3.4, contract 12 and 9.6). None when no copy was tried.
    fn try_copy(
        &mut self,
        env: &mut Env<'_>,
        rec: &NodeRecord,
        parent: &RemoteId,
        name: &str,
        size: u64,
        hash: Hash,
    ) -> EngineResult<Option<Result<Entry, RemoteError>>> {
        let can = self
            .hello
            .as_ref()
            .is_some_and(|h| h.has("hashes") && h.has("copy"));
        if !can || size < self.cfg.copy_check_min {
            return Ok(None);
        }
        let have = match env.remote.hashes_have(&rec.mount, &[hash]) {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        let Some(src) = have.into_iter().find(|h| h.hash == hash) else {
            return Ok(None);
        };
        Ok(Some(env.remote.copy(&src.id, parent, name, hash)))
    }

    /// The answer to a finish, or to a repeated one (10.5).
    fn upload_finish(
        &mut self,
        env: &mut Env<'_>,
        mut op: OpRow,
        rec: &NodeRecord,
        l: &LocalRow,
        session: &str,
        expect: Hash,
    ) -> EngineResult<Outcome> {
        match env.remote.upload_finish(session) {
            Ok(FinishAnswer::Done(entry)) => self.upload_done(rec, l, &entry, expect),
            Ok(FinishAnswer::Finishing(after)) => {
                op.state = OpState::Finishing;
                let wait = i64::try_from(after.max(1)).unwrap_or(60);
                op.next_try_ns = Some(env.clock.now_ns().saturating_add(wait * 1_000_000_000));
                Ok(Outcome::Continue(vec![Write::PutOp(op)]))
            }
            Err(e) if e.code() == Some(&ErrorCode::UploadIncomplete) => {
                self.runtime.remove(&op.id);
                op.state = OpState::Transferring;
                Ok(Outcome::Continue(vec![Write::PutOp(op)]))
            }
            Err(e) if e.code() == Some(&ErrorCode::UploadGone) => {
                // Finished more than a session's life ago, or never: for a
                // replace, the file says (3.10); otherwise start again and
                // let 8.5 tell.
                if let Some(r) = &rec.remote {
                    if let Ok(info) = env.remote.entry(&r.id) {
                        if info.entry.hash == Some(expect) {
                            return self.upload_done(rec, l, &info.entry, expect);
                        }
                    }
                }
                self.runtime.remove(&op.id);
                op.state = OpState::Planned;
                Ok(Outcome::Continue(vec![Write::PutOp(op)]))
            }
            Err(e) => {
                let r = self.upload_refused(env, &op, rec, &e, expect, Some(session));
                if matches!(r, Ok(Outcome::Fail(_))) {
                    let _ = env.remote.upload_abort(session);
                    self.runtime.remove(&op.id);
                }
                r
            }
        }
    }

    /// A refusal of an upload: 8.5 first (a repeat of a write that had
    /// succeeded), then the ordinary classes.
    fn upload_refused(
        &mut self,
        _env: &mut Env<'_>,
        op: &OpRow,
        rec: &NodeRecord,
        e: &RemoteError,
        expect: Hash,
        _session: Option<&str>,
    ) -> EngineResult<Outcome> {
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        match e.code() {
            Some(ErrorCode::Exists | ErrorCode::Precondition) => {
                if let Some(entry) = e.entry() {
                    if entry.hash == Some(expect) && entry.kind == Kind::File {
                        return self.upload_done(rec, &l, entry, expect);
                    }
                }
                fail(Fail::remote(e))
            }
            Some(ErrorCode::HashMismatch) => {
                if op.attempt == 0 {
                    fail(Fail::new(Failure::Retry { after_s: None }))
                } else {
                    fail(Fail::remote(e))
                }
            }
            _ => fail(Fail::remote(e)),
        }
    }

    /// An upload is committed on Hearth: the remote picture and the base
    /// take its answer (8.3: authoritative).
    fn upload_done(
        &mut self,
        rec: &NodeRecord,
        l: &LocalRow,
        entry: &Entry,
        expect: Hash,
    ) -> EngineResult<Outcome> {
        if entry.hash != Some(expect) {
            return fail(Fail::replan());
        }
        let mut b = Vec::new();
        if !self.remote_answer_writes(rec, entry, &mut b)? {
            return fail(Fail::replan());
        }
        b.push(Write::PutSynced(Self::base_from(l, entry.etag.clone())));
        Ok(Outcome::Done(b))
    }

    /// The session id: derived from the operation and the device, so a
    /// retry after a crash finds its own session (10.1).
    fn session_id(&self, op: &OpRow) -> EngineResult<String> {
        let marker = self
            .store
            .meta(crate::store::MetaKey::RootMarker)?
            .unwrap_or_default();
        let tag: String = marker
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        Ok(format!("u{tag}-{}", op.id.0))
    }

    /// The chunk size: the setting, within Hearth's limits, grown to keep
    /// within its chunk count (3.6).
    fn chunk_size(&self, size: u64) -> u64 {
        let (min, max, count) = self.hello.as_ref().map_or((1, u64::MAX, u64::MAX), |h| {
            (h.limits.chunk_min, h.limits.chunk_max, h.limits.chunks_max)
        });
        let mut c = self.cfg.chunk_size.clamp(min.max(1), max.max(1));
        while size.div_ceil(c) > count && c < max {
            c = (c * 2).min(max);
        }
        c
    }

    fn op_mkdir_remote(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        let (Some(parent), Some(name)) =
            (self.remote_id(op.target_parent)?, op.target_name.clone())
        else {
            return fail(Fail::replan());
        };
        match env.remote.make_folder(&parent, &name) {
            Ok(entry) => {
                let mut b = Vec::new();
                if !self.remote_answer_writes(&rec, &entry, &mut b)? {
                    return fail(Fail::replan());
                }
                b.push(Write::PutSynced(Self::base_from(&l, None)));
                Ok(Outcome::Done(b))
            }
            Err(e) => fail(Fail::remote(&e)),
        }
    }

    // ----- moves (3.4, contract 9.5) -----

    fn op_move_local(&mut self, env: &mut Env<'_>, mut op: OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        let (Some(from), Some(to)) = (self.path(op.node)?, self.target_path(&op)?) else {
            return fail(Fail::replan());
        };
        let Some(name) = to.file_name().cloned() else {
            return fail(Fail::replan());
        };
        let finish = |me: &mut Self, env: &mut Env<'_>| -> EngineResult<Outcome> {
            let new_l =
                match me.fresh_local(env, op.node, op.target_parent, &name, &to, l.kind, l.hash) {
                    Ok(x) => x,
                    Err(e) => return fail(Fail::fs(&e)),
                };
            let mut b = vec![Write::PutLocal(new_l.clone())];
            if let Some(mut s) = rec.synced.clone() {
                s.parent = new_l.parent;
                s.name = to_nfc(name.as_str().unwrap_or_default());
                s.local_file_id = new_l.file_id;
                b.push(Write::PutSynced(s));
            }
            if let Some(d) = to.parent() {
                let _ = env.fs.sync_dir(&d);
            }
            Ok(Outcome::Done(b))
        };
        if op.state == OpState::Transferring {
            // A two-step rename after a crash: where is the item now?
            let beside = self.store.transfer(op.id)?.and_then(|t| t.aside);
            if let Some(b) = beside {
                if env.fs.stat(&b).ok().flatten().is_some() {
                    return match env.fs.rename_noreplace(&b, &to) {
                        Ok(()) => finish(self, env),
                        Err(e) => fail(Fail::fs(&e).rescan(op.target_parent)),
                    };
                }
            }
            if env
                .fs
                .stat(&to)
                .ok()
                .flatten()
                .is_some_and(|m| m.file_id == l.file_id)
            {
                return finish(self, env);
            }
        }
        match env.fs.stat(&from) {
            Ok(Some(m)) if m.file_id == l.file_id => {}
            Ok(_) => return fail(Fail::replan().rescan(l.parent)),
            Err(e) => return fail(Fail::fs(&e)),
        }
        let same_key = l.parent == op.target_parent
            && op
                .target_name
                .as_ref()
                .is_some_and(|n| key_of_str(n, &self.cfg.volume) == l.key);
        if same_key {
            // A case-only or normalisation-only rename: through a temp name
            // where the volume treats both as one name (3.8).
            let Some(dir) = from.parent() else {
                return fail(Fail::replan());
            };
            let beside = layout::beside(&dir, op.id);
            op.state = OpState::Transferring;
            let t = TransferRow {
                op: op.id,
                temp: None,
                upload_id: None,
                total: 0,
                chunk_size: 0,
                done: 0,
                expect_etag: None,
                new_file_id: None,
                aside: Some(beside.clone()),
            };
            self.commit(vec![Write::PutOp(op.clone()), Write::PutTransfer(t)])?;
            if let Err(e) = env.fs.rename_noreplace(&from, &beside) {
                return fail(Fail::fs(&e).rescan(l.parent));
            }
            return match env.fs.rename_noreplace(&beside, &to) {
                Ok(()) => finish(self, env),
                // Leave the item where it is; the kept operation finishes
                // the second rename on the next try.
                Err(e) => fail(Fail::fs(&e)),
            };
        }
        match env.fs.rename_noreplace(&from, &to) {
            Ok(()) => finish(self, env),
            Err(FsError::Exists) => fail(Fail::replan().rescan(op.target_parent)),
            Err(e) => fail(Fail::fs(&e).rescan(l.parent)),
        }
    }

    fn op_move_remote(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(r) = rec.remote.clone() else {
            return fail(Fail::replan());
        };
        let (Some(parent), Some(name)) =
            (self.remote_id(op.target_parent)?, op.target_name.clone())
        else {
            return fail(Fail::replan());
        };
        let condition = match &op.condition {
            Some(Condition::IfSeq(s)) => MoveCondition::Folder(*s),
            c => match file_condition(c) {
                Some(f) => MoveCondition::File(f),
                None => return fail(Fail::replan()),
            },
        };
        let req = MoveRequest {
            parent,
            name,
            condition,
        };
        match env.remote.move_entry(&r.id, &req) {
            Ok(entry) => {
                let mut b = Vec::new();
                if !self.remote_answer_writes(&rec, &entry, &mut b)? {
                    return fail(Fail::replan());
                }
                if let (Some(mut s), Some(l)) = (rec.synced.clone(), rec.local.clone()) {
                    s.parent = l.parent;
                    s.name = to_nfc(l.name.as_str().unwrap_or_default());
                    // The etag changed with the move, not the content.
                    if r.etag == s.remote_etag {
                        s.remote_etag = entry.etag.clone();
                    }
                    b.push(Write::PutSynced(s));
                }
                Ok(Outcome::Done(b))
            }
            Err(e) => fail(Fail::remote(&e)),
        }
    }

    // ----- removals (3.7, contract 9.7) -----

    fn op_delete_local(&mut self, env: &mut Env<'_>, mut op: OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        let Some(path) = self.path(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(mount) = self.mount_of(&rec)? else {
            return fail(Fail::replan());
        };
        let done = |me: &mut Self, env: &mut Env<'_>, held: LocalPath| -> EngineResult<Outcome> {
            let (reason, basis) = if rec.remote.is_some() {
                (HeldReason::Unselected, PurgeBasis::RemoteFile)
            } else if mount.trash {
                (HeldReason::RemoteDelete, PurgeBasis::RemoteTrash)
            } else {
                (HeldReason::RemoteDelete, PurgeBasis::Unproven)
            };
            let id = me.alloc_held();
            let mut b = vec![
                Write::PutHeld(HeldRow {
                    id,
                    original: path.clone(),
                    held,
                    hash: l.hash,
                    size: l.size,
                    reason,
                    basis,
                    mount: mount.id.clone(),
                    at_ns: env.clock.now_ns(),
                }),
                Write::DelLocal(op.node),
                Write::DelSynced(op.node),
            ];
            if rec.remote.is_none() {
                b.push(Write::DelNode(op.node));
            }
            if let Some(p) = l.parent {
                b.push(Write::MarkDirty(p));
            }
            Ok(Outcome::Done(b))
        };
        if op.state == OpState::Transferring {
            if let Some(a) = self.store.transfer(op.id)?.and_then(|t| t.aside) {
                let at_held = env.fs.stat(&a).ok().flatten();
                let at_path = env.fs.stat(&path).ok().flatten();
                if at_held.is_some_and(|m| m.file_id == l.file_id)
                    && at_path.is_none_or(|m| m.file_id != l.file_id)
                {
                    return done(self, env, a);
                }
            }
        }
        // It must be exactly what the base and the local picture say: the
        // holding area takes only synced content (I1).
        match env.fs.stat(&path) {
            Ok(Some(m)) if l.same_metadata(&m) => {}
            Ok(_) => return fail(Fail::replan().rescan(l.parent)),
            Err(e) => return fail(Fail::fs(&e)),
        }
        if l.hash.is_none() || l.hash != op.expect_hash {
            return fail(Fail::replan());
        }
        let (_, held) = match self.held_place(env, &l.name) {
            Ok(x) => x,
            Err(e) => return fail(Fail::fs(&e)),
        };
        op.state = OpState::Transferring;
        let t = TransferRow {
            op: op.id,
            temp: None,
            upload_id: None,
            total: l.size,
            chunk_size: 0,
            done: 0,
            expect_etag: None,
            new_file_id: None,
            aside: Some(held.clone()),
        };
        self.commit(vec![Write::PutOp(op.clone()), Write::PutTransfer(t)])?;
        match env.fs.rename_noreplace(&path, &held) {
            Ok(()) => {
                if let Some(d) = path.parent() {
                    let _ = env.fs.sync_dir(&d);
                }
                done(self, env, held)
            }
            Err(e) => fail(Fail::fs(&e).rescan(l.parent)),
        }
    }

    fn op_remove_local_folder(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(path) = self.path(op.node)? else {
            return fail(Fail::replan());
        };
        if !self.store.local_children(op.node)?.is_empty() {
            return fail(Fail::replan());
        }
        match env.fs.remove_empty_dir(&path) {
            Ok(()) | Err(FsError::NotFound) => {}
            Err(FsError::NotEmpty) => {
                // Something the engine does not sync is inside (ignored
                // files): the folder stays, holding just them (3.14).
                let entries = env.fs.list_dir(&path).unwrap_or_default();
                let syncable = entries.iter().any(|e| {
                    let child = path.join(&e.name);
                    let parts: Option<Vec<&str>> = view::path_strs(&child);
                    match parts {
                        Some(p) => !self
                            .ignore
                            .is_ignored(&p, e.meta.kind == crate::fs::EntryKind::Folder),
                        None => true,
                    }
                });
                if syncable {
                    return fail(Fail::replan().rescan(Some(op.node)));
                }
                return fail(Fail::new(Failure::WaitForChange(
                    crate::tree::ProblemKind::KeptForIgnored,
                    "kept: ignored files remain in it".into(),
                )));
            }
            Err(e) => return fail(Fail::fs(&e)),
        }
        let mut b = vec![Write::DelLocal(op.node), Write::DelSynced(op.node)];
        if rec.remote.is_none() && self.store.synced_children(op.node)?.is_empty() {
            b.push(Write::DelNode(op.node));
        }
        if let Some(p) = rec.local.and_then(|l| l.parent) {
            b.push(Write::MarkDirty(p));
        }
        Ok(Outcome::Done(b))
    }

    fn op_delete_remote(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(r) = rec.remote.clone() else {
            return fail(Fail::replan());
        };
        let Some(cond) = file_condition(&op.condition) else {
            return fail(Fail::replan());
        };
        match env.remote.delete_file(&r.id, &cond) {
            Ok(_) => {}
            // Already gone: a repeat after a lost answer (8.5).
            Err(e) if e.code() == Some(&ErrorCode::NotFound) => {}
            Err(e) => return fail(Fail::remote(&e)),
        }
        let mut b = vec![Write::DelRemote(op.node), Write::DelSynced(op.node)];
        if rec.local.is_none()
            && self.store.local_children(op.node)?.is_empty()
            && self.store.synced_children(op.node)?.is_empty()
        {
            b.push(Write::DelNode(op.node));
        }
        if let Some(p) = &r.parent {
            if let Some(pn) = self.store.node_by_remote(p)? {
                b.push(Write::MarkDirty(pn));
            }
        }
        Ok(Outcome::Done(b))
    }

    fn op_delete_remote_folder(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(r) = rec.remote.clone() else {
            return fail(Fail::replan());
        };
        let Some(Condition::UnchangedSince(cursor)) = op.condition.clone() else {
            return fail(Fail::replan());
        };
        match env.remote.delete_folder(&r.id, &cursor) {
            Ok(_) => {}
            Err(e) if e.code() == Some(&ErrorCode::NotFound) => {}
            Err(e) if e.code() == Some(&ErrorCode::Precondition) => {
                // Something below changed since the cursor: child by child.
                self.split_deletes.insert(op.node);
                return fail(Fail::remote(&e));
            }
            Err(e) => return fail(Fail::remote(&e)),
        }
        // The folder and everything below it left Hearth (its trash): their
        // pictures go, deepest first.
        let mut order = Vec::new();
        let mut stack = vec![(op.node, r.id.clone())];
        while let Some((n, id)) = stack.pop() {
            order.push(n);
            for c in self.store.remote_children(&id)? {
                if let Some(cr) = self.store.node(c)?.and_then(|x| x.remote) {
                    stack.push((c, cr.id));
                }
            }
        }
        let removed: std::collections::BTreeSet<NodeId> = order.iter().copied().collect();
        let busy: std::collections::BTreeSet<NodeId> = self
            .store
            .ops()?
            .iter()
            .filter(|o| o.id != op.id)
            .map(|o| o.node)
            .collect();
        let mut b = Vec::new();
        for n in order.iter().rev() {
            b.push(Write::DelRemote(*n));
            b.push(Write::DelSynced(*n));
        }
        for n in order.iter().rev() {
            let Some(x) = self.store.node(*n)? else {
                continue;
            };
            let kids_ok = self
                .store
                .synced_children(*n)?
                .iter()
                .all(|k| removed.contains(k))
                && self.store.local_children(*n)?.is_empty();
            if x.local.is_none() && kids_ok && !busy.contains(n) {
                b.push(Write::DelNode(*n));
            } else {
                b.push(Write::MarkDirty(*n));
            }
        }
        Ok(Outcome::Done(b))
    }

    // ----- conflict copies (3.5.1) -----

    fn op_aside_local(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(l) = rec.local.clone() else {
            return fail(Fail::replan());
        };
        let (Some(parent), Some(path)) = (l.parent, self.path(op.node)?) else {
            return fail(Fail::replan());
        };
        let Some(dir) = path.parent() else {
            return fail(Fail::replan());
        };
        match env.fs.stat(&path) {
            Ok(Some(m)) if m.file_id == l.file_id => {}
            Ok(_) => return fail(Fail::replan().rescan(Some(parent))),
            Err(e) => return fail(Fail::fs(&e)),
        }
        let original = to_nfc(l.name.as_str().unwrap_or_default());
        let Some(name) = self.local_conflict_name(env, parent, &dir, &original, l.mtime_ns)? else {
            return fail(Fail::new(Failure::Retry { after_s: None }));
        };
        let to = dir.join(&LocalName::from_str_nfc(&name));
        match env.fs.rename_noreplace(&path, &to) {
            Ok(()) => {}
            Err(FsError::Exists) => return fail(Fail::new(Failure::Retry { after_s: None })),
            Err(e) => return fail(Fail::fs(&e).rescan(Some(parent))),
        }
        let _ = env.fs.sync_dir(&dir);
        let mut nl = l.clone();
        nl.name = LocalName::from_str_nfc(&name);
        nl.key = key_of_str(&name, &self.cfg.volume);
        if let Ok(Some(m)) = env.fs.stat(&to) {
            nl.ctime_ns = m.ctime_ns;
            nl.mtime_ns = m.mtime_ns;
            nl.file_id = m.file_id;
        }
        let cid = self.alloc_conflict();
        Ok(Outcome::Done(vec![
            Write::PutLocal(nl),
            Write::PutConflict(ConflictRow {
                id: cid,
                node: op.node,
                copy: op.node,
                hash: l.hash,
                name,
                at_ns: env.clock.now_ns(),
            }),
        ]))
    }

    fn op_aside_remote(&mut self, env: &mut Env<'_>, op: &OpRow) -> EngineResult<Outcome> {
        let Some(rec) = self.rec(op.node)? else {
            return fail(Fail::replan());
        };
        let Some(r) = rec.remote.clone() else {
            return fail(Fail::replan());
        };
        let (Some(parent), Some(cond)) = (r.parent.clone(), file_condition(&op.condition)) else {
            return fail(Fail::replan());
        };
        let taken: std::collections::BTreeSet<String> = self
            .store
            .remote_children(&parent)?
            .into_iter()
            .filter_map(|c| {
                self.store
                    .node(c)
                    .ok()
                    .flatten()
                    .and_then(|x| x.remote)
                    .map(|x| x.name)
            })
            .collect();
        let at = r.mtime_us.saturating_mul(1000);
        let local = at.div_euclid(1_000_000_000) + i64::from(env.clock.utc_offset_s(at));
        let max = self.name_limit();
        let mut name = None;
        for k in 1..1000 {
            match conflict_name(&r.name, &self.device, local, k, max) {
                Some(n) if !taken.contains(&n) => {
                    name = Some(n);
                    break;
                }
                Some(_) => continue,
                None => break,
            }
        }
        let Some(name) = name else {
            return fail(Fail::new(Failure::Retry { after_s: None }));
        };
        let req = MoveRequest {
            parent,
            name: name.clone(),
            condition: MoveCondition::File(cond),
        };
        match env.remote.move_entry(&r.id, &req) {
            Ok(entry) => {
                let mut b = Vec::new();
                if !self.remote_answer_writes(&rec, &entry, &mut b)? {
                    return fail(Fail::replan());
                }
                let cid = self.alloc_conflict();
                b.push(Write::PutConflict(ConflictRow {
                    id: cid,
                    node: op.node,
                    copy: op.node,
                    hash: r.hash,
                    name,
                    at_ns: env.clock.now_ns(),
                }));
                Ok(Outcome::Done(b))
            }
            Err(e) => fail(Fail::remote(&e)),
        }
    }
}
