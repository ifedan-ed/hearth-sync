//! A planning pass over the dirty nodes, the guards around it (the root
//! marker before deletions, the mass-change pause), and the holding area's
//! trimming (docs/sync.md 3.5, 3.7, 3.11).

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

use super::{Engine, EngineResult, PauseReason};
use crate::env::Env;
use crate::fs::layout;
use crate::ids::{MountId, NodeId};
use crate::observe::{MassChangeItem, MassChangeView, Side};
use crate::plan::{Decision, PlannedOp, Planner};
use crate::store::{MetaKey, Store, Write};
use crate::tree::{OpKind, OpRow, OpState, ProblemKind, PurgeBasis, Subject};
use crate::view;

/// How many dirty nodes are read from the store at a time.
const DIRTY_PAGE: usize = 1024;

impl<S: Store> Engine<S> {
    /// One planning pass. True when it changed anything (or paused).
    pub(super) fn plan_pass(&mut self, env: &mut Env<'_>) -> EngineResult<bool> {
        let ops = self.store.ops()?;
        let budget = self.cfg.max_ops_in_flight.saturating_sub(ops.len());
        if budget == 0 {
            return Ok(false);
        }
        let Some(root) = self.root else {
            return Ok(false);
        };
        let in_flight: BTreeSet<NodeId> = ops.iter().map(|o| o.node).collect();
        let mounts: BTreeMap<MountId, _> = self
            .store
            .mounts()?
            .into_iter()
            .map(|m| (m.id.clone(), m))
            .collect();
        let unselected: BTreeSet<NodeId> = self.store.unselected()?.into_iter().collect();
        let name_max = self.hello.as_ref().map_or(250, |h| h.limits.name_bytes_max);
        let next_node = Cell::new(self.ids.node);
        let mut planned: Vec<(NodeId, PlannedOp)> = Vec::new();
        let mut writes = Vec::new();
        let mut record: Option<Vec<Write>> = None;
        let mut problems: Vec<(NodeId, ProblemKind, String)> = Vec::new();
        let mut cleared: Vec<NodeId> = Vec::new();
        {
            let planner = Planner::new(
                &self.store,
                &self.cfg,
                &mounts,
                &unselected,
                &self.absent_roots,
                &self.ignore,
                in_flight,
                &self.split_deletes,
                name_max,
                &self.with_problem,
                &self.refused_moves,
                root,
                &next_node,
            );
            let mut after = None;
            'pages: loop {
                let page = self.store.dirty(after, DIRTY_PAGE)?;
                let Some(last) = page.last().copied() else {
                    break;
                };
                after = Some(last);
                for n in page {
                    let decision = planner.plan(n)?;
                    // R10: a name this device cannot hold is listed while
                    // everything else about the item syncs.
                    let note = planner.take_note(n);
                    if let Some(why) = &note
                        && !matches!(decision, Decision::Problem(..))
                    {
                        problems.push((n, ProblemKind::NameNotHoldable, why.clone()));
                    }
                    match decision {
                        Decision::Clean | Decision::NotReady => {
                            writes.push(Write::ClearDirty(n));
                            if note.is_none() {
                                cleared.push(n);
                            }
                        }
                        Decision::Wait(on) => {
                            writes.push(Write::Wait(n, on.unwrap_or(crate::store::WAIT_ANY)));
                        }
                        Decision::Record(w) => {
                            // A change of the store alone: applied before
                            // anything else is decided, so no later
                            // decision of this pass rests on the old rows.
                            record = Some(w);
                            break 'pages;
                        }
                        Decision::Problem(k, why) => {
                            writes.push(Write::ClearDirty(n));
                            problems.push((n, k, why));
                        }
                        Decision::Op(op) => {
                            planner.mark_busy(n);
                            planned.push((n, op));
                            if planned.len() >= budget {
                                break 'pages;
                            }
                        }
                    }
                }
            }
        }
        if next_node.get() != self.ids.node {
            self.ids.node = next_node.get();
            self.ids.changed = true;
        }
        let now = env.clock.now_ns();
        for n in cleared {
            self.clear_problem(n, &mut writes);
        }
        for (n, k, why) in problems {
            writes.push(self.problem_write(Subject::Node(n), k, why, now)?);
        }
        if let Some(r) = record {
            // The record first; the decisions made before it stand too.
            let mut b = r;
            b.extend(writes);
            self.commit(b)?;
            return Ok(true);
        }
        // Guards before anything leaves the disk (3.11).
        let local_destructive = planned.iter().any(|(_, op)| {
            matches!(
                op.kind,
                OpKind::DeleteLocal | OpKind::RemoveLocalFolder | OpKind::Download
            )
        });
        if local_destructive && !self.root_is_ours(env)? {
            return Ok(true);
        }
        if let Some(view) = self.mass_change(&planned, &mounts)? {
            if self.mass_approved == Some(view.token) {
                self.mass_approved = None;
            } else {
                self.pause = Some(PauseReason::MassChange(view));
                return Ok(true);
            }
        }
        let progressed = !writes.is_empty() || !planned.is_empty();
        for (n, op) in planned {
            let id = self.alloc_op();
            if let Some((k, why)) = &op.problem {
                writes.push(self.problem_write(Subject::Node(n), *k, why.clone(), now)?);
            }
            writes.push(Write::PutOp(OpRow {
                id,
                kind: op.kind,
                node: n,
                state: OpState::Planned,
                attempt: 0,
                next_try_ns: None,
                condition: op.condition,
                target_parent: op.target_parent,
                target_name: op.target_name,
                expect_hash: op.expect_hash,
                created_ns: now,
            }));
            writes.push(Write::ClearDirty(n));
        }
        let changed = !writes.is_empty();
        self.commit(writes)?;
        if !progressed && self.store.ops()?.is_empty() && !self.store.waiting(1)?.is_empty() {
            self.list_stuck(now)?;
            return Ok(true);
        }
        Ok(progressed || changed)
    }

    /// Nodes that wait for something no operation in flight will bring: a
    /// fault in the engine's reasoning. They are listed (I10) instead of
    /// spinning, and planned again when anything about them changes.
    fn list_stuck(&mut self, now: i64) -> EngineResult<()> {
        let stuck = self.store.waiting(usize::MAX)?;
        let mut b = Vec::new();
        for n in stuck {
            if self.store.node(n)?.is_none() {
                continue;
            }
            b.push(self.problem_write(
                Subject::Node(n),
                ProblemKind::Stuck,
                "waiting for another item that cannot change; reported as a fault".into(),
                now,
            )?);
            b.push(Write::ClearDirty(n));
        }
        self.commit(b)
    }

    /// The mass-change guard (3.11): files a plan would delete or
    /// overwrite, per mount and side, against the thresholds.
    fn mass_change(
        &self,
        planned: &[(NodeId, PlannedOp)],
        mounts: &BTreeMap<MountId, crate::tree::MountRow>,
    ) -> EngineResult<Option<MassChangeView>> {
        let mut counts: BTreeMap<(MountId, Side), (u64, Vec<String>)> = BTreeMap::new();
        for (n, op) in planned {
            let Some(rec) = self.store.node(*n)? else {
                continue;
            };
            let (side, weight) = match op.kind {
                OpKind::DeleteLocal => (Side::Local, 1),
                // A plain overwrite of a synced file counts; a replace that
                // keeps the local version as a conflict copy does not (R1).
                OpKind::Download
                    if rec.local.as_ref().is_some_and(|l| {
                        l.hash.is_some() && rec.synced.as_ref().is_some_and(|s| s.hash == l.hash)
                    }) =>
                {
                    (Side::Local, 1)
                }
                OpKind::DeleteRemote | OpKind::Upload => (Side::Remote, 1),
                OpKind::DeleteRemoteFolder => (Side::Remote, self.remote_files_below(*n)?),
                _ => continue,
            };
            let e = counts.entry((rec.mount.clone(), side)).or_default();
            e.0 += weight;
            if e.1.len() < 10 {
                let p = view::local_path(&self.store, *n)?
                    .map(|p| p.display())
                    .or(view::remote_path(&self.store, mounts, *n)?.map(|v| v.join("/")))
                    .unwrap_or_default();
                e.1.push(p);
            }
        }
        let mut items = Vec::new();
        for ((mount, side), (count, examples)) in counts {
            if count <= self.cfg.mass_change_count {
                continue;
            }
            let total = self.synced_files(&mount)?;
            if count > self.cfg.mass_change_count
                && count.saturating_mul(1000) > self.cfg.mass_change_per_mille.saturating_mul(total)
            {
                items.push(MassChangeItem {
                    mount,
                    side,
                    count,
                    total,
                    examples,
                });
            }
        }
        if items.is_empty() {
            return Ok(None);
        }
        // The token names exactly this plan's sizes, so an approval does not
        // carry over to a different plan.
        let mut token: u64 = 0xcbf2_9ce4_8422_2325;
        for i in &items {
            for b in i
                .mount
                .as_str()
                .bytes()
                .chain(i.count.to_le_bytes())
                .chain([i.side as u8])
            {
                token ^= u64::from(b);
                token = token.wrapping_mul(0x0100_0000_01b3);
            }
        }
        Ok(Some(MassChangeView { token, items }))
    }

    /// The files of a mount in the synced tree: the guard's denominator
    /// (R1). Read only when a plan's count is over the absolute threshold.
    fn synced_files(&self, mount: &MountId) -> EngineResult<u64> {
        let mut n = 0;
        for x in self.store.mount_nodes(mount)? {
            if self
                .store
                .node(x)?
                .and_then(|r| r.synced)
                .is_some_and(|s| s.kind == crate::ids::Kind::File)
            {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Files on Hearth below a folder (for the mass-change count).
    fn remote_files_below(&self, n: NodeId) -> EngineResult<u64> {
        let mut count = 0;
        let mut stack = vec![n];
        while let Some(x) = stack.pop() {
            let Some(r) = self.store.node(x)?.and_then(|r| r.remote) else {
                continue;
            };
            if r.kind == crate::ids::Kind::File {
                count += 1;
            }
            stack.extend(self.store.remote_children(&r.id)?);
        }
        Ok(count)
    }

    /// Empties one holding-area entry whose days are over, when its bytes
    /// are proven to exist elsewhere (3.7). At most one per step; checked
    /// at most once an hour.
    pub(super) fn purge_held(&mut self, env: &mut Env<'_>, now: i64) -> EngineResult<bool> {
        let last: i64 = self
            .store
            .meta(MetaKey::LastPurge)?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        // Short of space, the age does not wait (R12); the proof always
        // does: what is not known to exist elsewhere is never emptied.
        let low = env
            .fs
            .free_space()
            .is_ok_and(|f| f < self.cfg.disk_reserve_bytes);
        if now - last < super::secs(if low { 60 } else { 3600 }) {
            return Ok(false);
        }
        let keep_ns = super::secs(u64::from(self.cfg.held_days) * 86_400);
        for h in self.store.held()? {
            if !low && h.at_ns.saturating_add(keep_ns) > now {
                continue;
            }
            let proven = match h.basis {
                PurgeBasis::RemoteTrash
                | PurgeBasis::RemoteVersions
                | PurgeBasis::RemoteFile
                | PurgeBasis::IdenticalReplacement => true,
                PurgeBasis::Unproven => match h.hash {
                    Some(hash) if self.hello.as_ref().is_some_and(|x| x.has("hashes")) => env
                        .remote
                        .hashes_have(&h.mount, &[hash])
                        .map(|v| v.iter().any(|x| x.hash == hash))
                        .unwrap_or(false),
                    _ => false,
                },
            };
            if !proven {
                continue;
            }
            match env.fs.remove_held(&h.held) {
                Ok(()) | Err(crate::fs::FsError::NotFound) => {}
                Err(_) => continue,
            }
            if let Some(dir) = h.held.parent()
                && dir != layout::held_root()
            {
                let _ = env.fs.remove_empty_dir(&dir);
            }
            self.commit(vec![
                Write::DelHeld(h.id),
                Write::DelProblem(crate::tree::Subject::Path(h.held.clone())),
            ])?;
            return Ok(true);
        }
        self.commit(vec![Write::PutMeta(MetaKey::LastPurge, now.to_string())])?;
        Ok(false)
    }
}
