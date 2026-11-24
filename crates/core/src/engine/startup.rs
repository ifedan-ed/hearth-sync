//! Start-up: Hearth's hello and the instance check, the root guard, and
//! recovery of operations a crash interrupted (docs/sync.md 3.10, 3.11).
//!
//! Recovery looks at the world, never trusts the journal alone. The rule
//! that keeps it small: an operation is kept across a restart only where
//! starting over would redo a transfer or leave a file in an in-between
//! place (a download's temp file, an upload session, a swap or a two-step
//! rename half done). Every other operation is dropped and its node marked
//! dirty: the three pictures still say what is to be done, and every
//! remote write is safe to plan again because it carries its condition
//! (an earlier attempt that did succeed shows up in the feed, and the
//! planner then finds both sides already agree).

use super::{Engine, EngineResult, PauseReason, Stage};
use crate::env::Env;
use crate::fs::{FsError, layout};
use crate::store::{MetaKey, Store, Write};
use crate::tree::{OpKind, OpState};

impl<S: Store> Engine<S> {
    /// Hearth's hello: the contract version and the instance.
    pub(super) fn start_hello(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        self.load_counters()?;
        let hello = match env.remote.hello() {
            Ok(h) => h,
            Err(e) => {
                // Offline at start: the disk side still works; try again
                // after a backoff (3.10, "retry soon").
                if !self.remote_unreachable(env, &e)? {
                    return Ok(true);
                }
                return Ok(false);
            }
        };
        if hello.api_min > 1 || hello.api_max < 1 {
            self.pause = Some(PauseReason::ApiVersion);
            return Ok(true);
        }
        match self.store.meta(MetaKey::InstanceId)? {
            Some(was) if was != hello.instance_id => {
                self.pause = Some(PauseReason::InstanceChanged {
                    was,
                    now: hello.instance_id.clone(),
                });
                return Ok(true);
            }
            Some(_) => {}
            None => self.commit(vec![Write::PutMeta(
                MetaKey::InstanceId,
                hello.instance_id.clone(),
            )])?,
        }
        self.hello = Some(hello);
        self.remote_retry_at = None;
        self.remote_attempt = 0;
        self.stage = Stage::Root;
        Ok(true)
    }

    /// The root guard (3.11): the marker and the volume must be the ones
    /// set up. A first start writes them; a start with a lost database
    /// adopts the marker it finds (I12).
    pub(super) fn start_root(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        if !self.root_is_ours(env)? {
            return Ok(true);
        }
        for dir in [layout::engine_dir(), layout::tmp_dir(), layout::held_root()] {
            match env.fs.mkdir(&dir) {
                Ok(()) | Err(FsError::Exists) => {}
                Err(_) => {
                    self.pause = Some(PauseReason::RootUnavailable);
                    return Ok(true);
                }
            }
        }
        self.stage = Stage::Recover;
        Ok(true)
    }

    /// Checks (and on a first start, writes) the root marker and volume.
    /// False when the root is not available: the engine is then paused.
    pub(super) fn root_is_ours(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        let found = match env.fs.read_marker() {
            Ok(m) => m,
            Err(_) => {
                self.pause = Some(PauseReason::RootUnavailable);
                return Ok(false);
            }
        };
        let volume = match env.fs.volume() {
            Ok(v) => v.to_string(),
            Err(_) => {
                self.pause = Some(PauseReason::RootUnavailable);
                return Ok(false);
            }
        };
        let stored = self.store.meta(MetaKey::RootMarker)?;
        let stored_volume = self.store.meta(MetaKey::Volume)?;
        match (stored, found) {
            (Some(s), Some(f)) if s == f && stored_volume.as_deref() == Some(volume.as_str()) => {
                Ok(true)
            }
            (Some(_), _) => {
                self.pause = Some(PauseReason::RootUnavailable);
                Ok(false)
            }
            (None, Some(f)) => {
                self.commit(vec![
                    Write::PutMeta(MetaKey::RootMarker, f),
                    Write::PutMeta(MetaKey::Volume, volume),
                ])?;
                Ok(true)
            }
            (None, None) => {
                let marker = format!(
                    "{:016x}{:016x}",
                    env.random.next_u64(),
                    env.random.next_u64()
                );
                if env.fs.write_marker(&marker).is_err() {
                    self.pause = Some(PauseReason::RootUnavailable);
                    return Ok(false);
                }
                self.commit(vec![
                    Write::PutMeta(MetaKey::RootMarker, marker),
                    Write::PutMeta(MetaKey::Volume, volume),
                ])?;
                Ok(true)
            }
        }
    }

    /// Recovery of every operation in the journal, then a full scan.
    pub(super) fn start_recover(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        let mut batch = Vec::new();
        for op in self.store.ops()? {
            let keep = match (op.kind, op.state) {
                // A download's bytes so far are kept (I11: no completed
                // transfer redone); a swap half done is finished by its
                // own step, which looks at the file ids first.
                (OpKind::Download, OpState::Transferring)
                | (OpKind::Download, OpState::Verified)
                | (OpKind::Download, OpState::Swapping)
                | (OpKind::Download, OpState::Swapped) => true,
                // An upload session is asked for its status and resumed.
                (OpKind::Upload | OpKind::UploadNew, OpState::Transferring)
                | (OpKind::Upload | OpKind::UploadNew, OpState::Finishing) => true,
                // A rename into the holding area or a two-step rename may
                // be half done: their steps look before acting.
                (OpKind::DeleteLocal | OpKind::MoveLocal, _) => true,
                _ => false,
            };
            if keep {
                // Retry at once after a restart.
                let mut op = op;
                op.next_try_ns = None;
                batch.push(Write::PutOp(op));
            } else {
                if op.kind == OpKind::Download {
                    // A temp file of a download that had not started.
                    let _ = env.fs.remove_temp(&layout::temp_for(op.id));
                }
                batch.push(Write::DelOp(op.id));
                batch.push(Write::MarkDirty(op.node));
            }
        }
        self.commit(batch)?;
        // Files in the temp folder that no operation names are taken in
        // once the mounts are known (`adopt_orphan_temps`).
        self.orphan_temps_pending = true;
        for p in self.store.problems()? {
            if let crate::tree::Subject::Node(n) = p.subject {
                self.with_problem.insert(n);
            }
        }
        self.stage = Stage::Running;
        self.mounts_stale = true;
        self.queue_full_scan();
        let now = env.clock.now_ns();
        self.next_rescan_ns = Some(now + super::secs(self.cfg.rescan_interval_s));
        self.next_problem_retry_ns = Some(now + super::secs(self.cfg.problem_retry_s));
        Ok(true)
    }

    /// A temp file of an operation dropped at recovery held only downloaded
    /// bytes and was removed with it. Any other file in the temp folder is
    /// tied to no operation: it may be a replaced file waiting for its
    /// check, so it is never unlinked (R14, R17). It goes to the holding
    /// area, emptied only when Hearth is shown to have its bytes, and is
    /// listed once. Run once after start, when the home mount is known.
    pub(super) fn adopt_orphan_temps(&mut self, env: &mut Env<'_>) -> EngineResult<()> {
        self.orphan_temps_pending = false;
        let Some(mount) = self
            .store
            .mounts()?
            .into_iter()
            .find(|m| m.local_path.is_empty())
        else {
            return Ok(());
        };
        let tied: std::collections::BTreeSet<_> = self
            .store
            .ops()?
            .iter()
            .map(|o| layout::temp_for(o.id))
            .collect();
        let Ok(entries) = env.fs.list_dir(&layout::tmp_dir()) else {
            return Ok(());
        };
        let now = env.clock.now_ns();
        for e in entries {
            let p = layout::tmp_dir().join(&e.name);
            if tied.contains(&p) || e.meta.kind != crate::fs::EntryKind::File {
                continue;
            }
            let Ok((id, held)) = self.held_place(env, &e.name) else {
                continue;
            };
            if env.fs.rename_noreplace(&p, &held).is_err() {
                continue;
            }
            let hash = super::scan::read_hash(env, &held, e.meta.size, self.cfg.range_size).ok();
            let mut b = vec![Write::PutHeld(crate::tree::HeldRow {
                id,
                original: p.clone(),
                held: held.clone(),
                hash,
                size: e.meta.size,
                reason: crate::tree::HeldReason::Retired,
                basis: crate::tree::PurgeBasis::Unproven,
                mount: mount.id.clone(),
                at_ns: now,
            })];
            b.push(
                self.problem_write(
                    crate::tree::Subject::Path(held),
                    crate::tree::ProblemKind::Recovered,
                    "found in the engine's temp folder after a restart; kept in the holding area"
                        .into(),
                    now,
                )?,
            );
            self.commit(b)?;
        }
        Ok(())
    }
}
