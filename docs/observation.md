# What the engine lets you observe

This is the agreement between the core (`crates/core`) and the crates that
check it: the reference model and the invariant checkers (`crates/model`)
and the simulator (`crates/sim`). It says what can be read from a running
engine and what each field means. It does not say how the planner decides
anything: the model is written without that knowledge, on purpose.

Everything named here is public in `hearth_sync_core` and keeps its meaning
for as long as this document is not changed. A change to it is announced to
the owners of `crates/model` and `crates/sim` in the same commit.

## 1. How a run is driven

The engine is a deterministic state machine. It owns no thread, timer or
socket. A host (the CLI, the desktop service, the simulator) owns it and:

1. builds it with `Engine::new(store, config)`, where `store` is any
   `Store` (the SQLite one in `hearth-sync-store`, or `MemStore` from this
   crate for simulations);
2. gives it the outside world on every call as an `Env`: the file system
   (`Fs`), Hearth (`Remote`), the clock (`Clock`), randomness (`Random`)
   and the scheduler (`Scheduler`), each a trait the simulator implements;
3. tells it what happened outside with `Engine::notify(Event)`: a watcher
   hint for a folder, a watcher overflow, a wake-up from the event stream,
   a network change, a request to scan everything;
4. calls `Engine::step(&mut env)` repeatedly. One step does at most one
   unit of work: one request to Hearth, or one file system call, or one
   planning pass, and commits what it learned to the store before it
   returns. It returns a `StepOutcome`:
   - `Worked`: call again;
   - `Idle { wake_at }`: nothing to do until an event, the time `wake_at`
     (nanoseconds on the `Clock`), or a change on Hearth. The host then
     calls `Engine::wait_remote(&mut env, seconds)`, which is the one
     request an idle engine makes (`POST /sync/wait`);
   - `Paused(PauseReason)`: a guard stopped the root or a mount; nothing
     is changed anywhere until the host calls the matching `Engine::resolve_*`
     method (for example `approve_mass_change`).

Because every call to the outside goes through `Env`, and every choice
between two runnable pieces of work goes through `Scheduler::pick`, the
same seed in the simulator gives the same sequence of calls (I13). The
core never iterates a randomly seeded map where the order could reach a
decision.

A crash is simulated by dropping the `Engine` at any point (between any
two calls it makes to `Env`, or with a call that failed) and building a
new one on the same store. A power cut is simulated by the store and the
fake file system losing what was not yet made durable (the store's
`MemStore::crash` keeps exactly the committed batches). Deleting the
database is building a new engine on an empty store.

## 2. The observation

`Engine::observe(&self) -> Observation` returns a snapshot of the engine's
own state, read from the store in one consistent view. It is meant for
tests and for the client's status screens; it costs a pass over the store
and is not called on any hot path.

    Observation {
        phase: Phase,                 // Starting, Syncing, Idle, Paused(PauseReason)
        mounts: Vec<MountView>,
        nodes: Vec<NodeView>,         // sorted by node id
        ops: Vec<OpView>,             // operations in flight, by op id
        problems: Vec<ProblemView>,   // what is not synced and why (I10)
        held: Vec<HeldView>,          // the holding area, "Removed by sync"
        mass_change: Option<MassChangeView>,
    }

### MountView

| Field | Meaning |
| --- | --- |
| `id` | The mount's id from Hearth (`MountId`). |
| `storage`, `epoch` | As Hearth last reported them. |
| `root_node` | The node of the mount's root entry. |
| `local_path` | Where the mount sits under the sync root (components). |
| `cursor` | The feed cursor the engine has durably applied, or none before the first complete listing. |
| `settled` | True when the last feed answer said `more: false`, no listing is in progress and no departed entry waits (contract 6.6 rule 7). The engine plans only from settled mounts. |
| `selected` | Whether the person chose to sync it (mounts with `prompt: ask` start unselected). |
| `perms` | The mount's rights bits (contract 5.2). |
| `trash`, `versions` | As Hearth reported them. |

### NodeView

A node is one file or folder the engine has linked. It carries up to three
pictures of that item. Absent means "this side does not have it".

| Field | Meaning |
| --- | --- |
| `node` | The engine's own id (`NodeId`); stable for the node's life, never reused within one database. |
| `mount` | The mount the node belongs to. |
| `remote` | `Option<RemoteView>`: what Hearth last reported (id, parent id, name, kind, size, etag, hash, exec, seq, mtime). |
| `local` | `Option<LocalView>`: what the engine last saw on disk (parent node, name bytes, kind, file id, size, mtime and ctime in ns, exec, hash if known). `hash` is `None` while the file is not yet hashed at rest. |
| `synced` | `Option<SyncedView>`: the base, the last state both sides agreed on (parent node, NFC name, kind, size, hash, the remote etag and local file id at that moment). |
| `local_path` | The node's path under the sync root, by the local tree's parents, as name components (bytes), when the local picture exists. |
| `remote_path` | The node's path under the sync root by the remote tree (mount path plus names), when the remote picture exists and every parent is known. |
| `problem` | The problem kind if the node is listed as not syncable, else none. |

Names: `RemoteView.name` and `SyncedView.name` are NFC strings exactly as
Hearth holds them. `LocalView.name` is the bytes on disk (which may be NFD,
or not UTF-8 at all on Linux); `LocalView.name_key` is the comparison form
the engine uses on this volume (NFC, plus simple case folding on a
case-insensitive volume).

### OpView

`id` (`OpId`), `kind` (`OpKind`: Download, Upload, UploadNew, MoveLocal,
MoveRemote, DeleteLocal, DeleteRemote, DeleteRemoteFolder, MkdirLocal,
MkdirRemote, ConflictAside, CopyRemote), `node`, `state` (the durable
point reached, as a string for logs), `attempt`, `next_try` (ns or none).

Every op that writes to Hearth exposes its condition in
`OpView.condition: Option<ConditionView>` (`IfMatch(etag)`,
`IfHash(hash)`, `IfNoneMatch`, `IfSeq(n)`, `UnchangedSince(cursor)`).
Writes to Hearth without a condition cannot be expressed in the core's
types; the field is there so a checker can compare the condition with
what the engine had seen (I3).

### ProblemView

`node` (if any), `path` (display form), `kind` (`ProblemKind`: for example
`NameNotHoldable`, `NameRefused`, `NameClash`, `ReadOnly`, `Locked`,
`Quota`, `Infected`, `HashMismatch`, `StillChanging`, `Ignored`,
`NotSelected`, `Symlink`, `Special`, `E2eeUnsupported`, `MountRootMissing`,
`KeptForIgnored`, `HeldPurgeUnproven`), `reason` (free text for people),
`count`.

### HeldView

`id`, `original_path`, `held_path` (under `.hearthsync/held/<date>/`),
`hash`, `size`, `reason` (`HeldReason`: RemoteDelete, Replaced, Unselected,
Retired), `at` (ns), `basis` (`PurgeBasis`: why it may be emptied after
its 7 days: `RemoteTrash`, `RemoteVersions`, `IdenticalReplacement`, or
`Unproven`, which is never emptied without a hash check against Hearth).

### MassChangeView

When a plan would delete or overwrite more than the thresholds allow, the
engine pauses with this: per mount and side, the count and the first
paths. `Engine::approve_mass_change(token)` lets exactly that plan go on.

## 3. Facts the engine records for checkers

- `Engine::conflict_copies()`: every conflict copy the engine made, with
  the node it came from, the losing version's hash, and the name given.
  I9 counts these.
- The holding area (`held`) is the device's own place where bytes the
  engine removed still exist (I1).
- `Observation::ops` shows intent written before effect (I11): an op
  appears before its first side effect and disappears in the transaction
  that changes the trees.

## 4. The guarantees as functions

`hearth_sync_core::guarantees` has one function per guarantee that can be
judged from a single observation plus the simulated world's own state. The
model's checkers call them after every step and add what needs a whole
trace (I1's history of saved hashes, I7's bound and quiescence, I13's
byte-identical logs). Each returns `Vec<Violation>`; empty means it holds.

| Guarantee | Function | Needs |
| --- | --- | --- |
| I1 | `i1_saved_bytes_present(saved, obs, world)` | the set of saved hashes the model keeps; `world` lists hashes present on disk, on Hearth (files, versions, trash) |
| I2 | `i2_no_unsynced_overwrite(before, after)` | two disk snapshots around one step and the observation before it |
| I3 | `i3_conditions_seen(obs, request)` | each write request the fake Hearth received, with the state the engine had seen |
| I4 | `i4_nothing_half_written(disk)` | a disk snapshot: no engine temp name outside `.hearthsync/tmp` except the documented beside-the-target form while its op exists |
| I5 | `i5_hashes_verified(disk, obs)` | a disk snapshot: every file the local tree records as synced has the hash recorded |
| I6 | `i6_no_deletion_from_missing_root(obs, root_present)` | whether the simulated root is present |
| I7 | (trace) the model counts steps and requests after faults stop |
| I8 | `i8_converged(obs)` | the observation alone, when the engine is idle |
| I9 | `i9_conflict_copies(obs, copies)` | the conflict copy log |
| I10 | `i10_problems_listed(obs)` | the observation: every node not converged is listed with a reason |
| I11 | `i11_store_consistent(store)` | the store after a crash: parents exist and are folders, no cycles, unique name keys per folder, no orphan ops or transfers |
| I12 | (trace) the model deletes the store and checks I1 to I5 and I8 |
| I13 | (trace) the simulator compares logs of two runs |

## 5. What is deliberately not observable

How the planner chose an operation, the dirty set, and the order of the
scheduler's queue are internal and may change without notice. The model
must judge outcomes, never mirror the engine's reasoning.
