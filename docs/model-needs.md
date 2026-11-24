# What the model needs to observe

The reference model (crates/model) checks a run by reading a `Trace`
(crates/model/src/trace.rs): the scenario, the state before the first
step, and, after every step, a snapshot of everything observable. This
page says what a runner (the simulator in crates/sim, or anything else)
must put in it. It names only what can be seen from outside the engine's
decisions: trees, holding areas, listings, requests and their outcomes,
and two facts of the engine's state that the plan makes part of the
state database (its synced tree and its view of Hearth). It never asks
for a plan, a queue or a reason for a decision.

If the engine's own observation document (docs/observation.md, written
by the core) offers these under other names, the simulator maps them;
the model does not read the engine.

## One step

A step is one thing that happened, and the state right after it. The
runner records a step for:

- every act of a person (`Event::Person`), with `applied: false` when
  the act could not be done (its target was missing or its name taken
  at that moment, as `apply::apply_act` defines);
- every operator act (`Event::Admin`) and every fault set or happening
  (`Event::Fault`), and every confirmation of a pause (`Event::Confirm`);
- every request an engine makes, with its outcome
  (`Event::Engine` / `EngineAction::Request`);
- every change an engine makes to its disk (`EngineAction::Disk`), at
  least one step per change that a reader of the disk could see: putting
  a download in place, holding a file, a rename, a folder made or
  removed. Several invisible system calls may share one step; two
  visible changes must not (I4 is checked between steps);
- a purge of the holding area (`EngineAction::Purge`);
- `Event::Settled` once the scenario's `settle` has been carried out:
  faults stopped, every device online, its root back, nothing left to
  do. A paused device must have been confirmed first (the scenario says
  so); a device paused at `Settled` is reported as a runner fault.

After `Settled`, the runner lets every engine run until it waits, and
records those steps too (I7 checks they are only `/sync/wait`).

## The snapshot

| Field | What |
| --- | --- |
| `time` | Hearth's simulated clock, microseconds since 1970 UTC. |
| `server.tree` | The person's tree on Hearth: home with every present mount at its path, by path, names as stored (NFC). Files with content hash and mtime, folders. |
| `server.versions` | Every content hash Hearth keeps as an older version. |
| `server.trash` | Every content hash in Hearth's trash. |
| `server.mounts` | Present mounts by path (`""` is home): `trash`, `versions`, `perms`. |
| `devices[name].tree` | The synced folder as a person sees it, names as on disk, without `.hearthsync/` and `*.hearthtmp`. Empty while the root is missing or replaced. |
| `devices[name].holding` | Every content hash in the holding area ("Removed by sync"). |
| `devices[name].listed` | What the device lists as not syncable: path (as on disk, or Hearth's path for an item only on Hearth) to reason code (below). |
| `devices[name].synced` | The engine's synced tree (its base), by Hearth path. |
| `devices[name].remote_view` | The engine's remote tree: what Hearth last told it, by Hearth path. |
| `devices[name].root` | `Present`, `Missing` (gone), `Replaced` (a folder without the marker). |
| `devices[name].online` | Whether the device has network. |
| `devices[name].paused` | `None`, or why the engine waits for the person (`mass_change`, `root`, `instance`). |

## Requests

`RequestKind` names what the checkers need, not the wire format:

- `Wait`, `Changes`, `Tree`, `Mounts`, `Other(text)`;
- `Download { path, hash }`: Hearth path and the content asked for;
- `Upload { path, hash }`: the content announced (a one-request create
  or replace, or a session's finish);
- `Write { op, path, condition, before }`: every write of sync-api 9.
  `path` is the entry's Hearth path when sent; `before` is what Hearth
  held there when the write arrived (a file's hash, `None` for a folder
  or nothing); `condition` is what the request carried, with a file
  condition given as the content hash of the version it names (etags
  map one to one to versions; the runner translates).

`Outcome`: `Done`, `Refused(status, code)`, `AnswerLost` (Hearth acted,
the answer never arrived), `Failed` (nothing happened on Hearth).

## Listing reason codes

The codes a device may list, and when each is true (I10 checks it):

| Code | True when |
| --- | --- |
| `name_refused:<reason>` | Hearth refuses the name, `<reason>` as sync-api 13. |
| `device_refused:windows_character`, `windows_trailing`, `windows_reserved`, `device_too_long` | The device's volume cannot hold the name (sync.md 3.8). |
| `name_clash` | Another item in the folder has the same name on this device (case or normalisation). |
| `engine_name` | A remote `.hearthsync` or `*.hearthtmp` at the sync root. |
| `read_only_edit`, `read_only_create`, `read_only_move` | The mount lacks the right (sync.md 3.5, Permissions). |
| `refused:<code>` | Hearth refused writes there (`locked`, `quota`, `virus`, `forbidden`). |
| `denied` | The device's file system denied access. |
| `no_longer_shared` | Kept local after a share left (sync.md 4.2). |
| `other_name` | One item held under two names because one side cannot take the other's name (Hearth renamed it to a name this device cannot hold, or the person renamed it to a name Hearth refuses): the refused name is listed with its reason, the other with this code. True while exactly one side holds the path. |
| `inside_listed` | Inside a listed folder (the folder's listing covers it; a runner need not list it separately). |
| `transient:<kind>` | A problem that passes on its own (a hash mismatch being retried); accepted at any step, excuses nothing at `Settled`. |

## Scenario ops the runner must honour

The runner plays `Scenario::ops` in order; their meaning is in
crates/model/src/scenario.rs. Fault positions (`drop 3`, `crash 5`)
count the device's requests or disk calls from the moment the fault op
is played. Content tokens (`c17`) become whatever bytes the runner
likes, the same bytes for the same token; their hash is the token's
hash everywhere in the trace (the model compares hashes for equality
only). `admin restore` restores Hearth to its state at the previous
`settle` and changes the epoch.

## Filling it from the engine (docs/observation.md)

The core's docs/observation.md (2026-11-24) offers what is needed. How
each field of the trace maps to it, for the simulator:

| Trace field | From |
| --- | --- |
| `devices[d].tree` | The simulated disk itself (the fake file system), not the engine's picture of it: what a person would see, without `.hearthsync/` and `*.hearthtmp`. |
| `devices[d].holding` | The hashes of the files under `.hearthsync/held/` on the simulated disk (or `Observation::held[].hash`; the disk is the stronger source). |
| `devices[d].listed` | `Observation::problems`: path to code, by `ProblemKind`: `NameNotHoldable` to `device_refused:<reason>`, `NameRefused` to `name_refused:<reason>`, `NameClash` to `name_clash`, `ReadOnly` to `read_only_edit`, `read_only_create` or `read_only_move`, `Locked` to `refused:locked`, `Quota` to `refused:quota`, `Infected` to `refused:virus`. Problems that pass on their own (`HashMismatch`, `StillChanging`) as `transient:<kind>`: accepted at any step, but a device still listing one at `Settled` has not converged (I8). Kinds no scenario provokes yet (`Ignored`, `NotSelected`, `Symlink`, `Special`, `E2eeUnsupported`) are reported as unknown codes until the model learns them. |
| `devices[d].synced` | `NodeView.synced`, by path (the synced parents and NFC names). |
| `devices[d].remote_view` | `NodeView.remote` with `NodeView.remote_path`. |
| `devices[d].paused` | `Phase::Paused(reason)`. |
| `devices[d].root` | The simulator's own root state (the fault it injected). |
| Requests | The simulated Hearth's own log of what it received and answered, translating etags to the content hash of the version they name. |

The model does not call `hearth_sync_core::guarantees` (docs/observation.md
section 4 suggests it): the model depends on no crate of the workspace,
so that its judgement stays independent of the engine's. The simulator
may run both and report both; a guarantee one side finds broken and the
other does not is a bug in one of the two checkers.
