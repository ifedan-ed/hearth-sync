# Progress

Newest first. What was done, what is half done, what is next.

## 2026-11-24, night: rulings on M1 to M16 and the judges' disagreements

Docs only. Decided in Hearth's docs/sync.md 3.17 (R1 to R19); the table
of who changes is in docs/contract-questions.md, "Rulings". Next, in
code: the engine (R8, R10, R16, R18, R19), the model (R1, R4, R8, R10,
R16), the simulator (R10, R16, R17); then `CONTRACT` moves to the
contract's entry (c).

## 2026-11-24, later: CI findings fixed, contract (b) followed (sync-core)

- The build copy's CI (2,048 cases) found two faults in the
  case-insensitive property test, and local runs of 20,000 cases found
  four more. Fixed at the root, each with a named regression test in
  crates/core/src/tests/props.rs: a folder read where names pass between
  nodes (a store constraint error: moving nodes now let go of their names
  first); a folder Hearth deleted around a listed item (it now stays with
  the item); a circle of waits through folders not yet on disk (broken by
  a parking name); a folder deleted on disk that Hearth moved onto a name
  the disk gave another folder (Hearth's keeps the name, the other steps
  aside).
- The engine follows contract 2026-11-24 (b), commit a0e967e: move
  markers for entries gone with a folder, the amended rule 6, `rootSize`.
- Property cases: the nightly workflow runs 200,000 per property; CI on
  every push 2,048; here the case-insensitive and the case-sensitive
  random-run properties passed 20,000 cases each, clean, after the last
  fix (the runs before it found the faults above at 627, 2,990, 13,105
  and 17,088 cases).
## 2026-11-24: documentation catch-up (docs only)

- README: the status and "What exists" now name the reference model and
  say which crates are still placeholders; `ci/cargo.sh`'s settings and
  the random tests' settings listed.
- AGENTS.md: the standing rule (README, AGENTS.md and the roadmap in
  every merge; the roadmap is the Hearth repository's), helpers'
  worktrees and small rebased merges, the mirror run by the coordinator
  only, scratch on disk.
- Seen, not changed: `CONTRACT` pins the contract before its changelog
  entry "2026-11-24 (b)" (the answers to Q1 to Q6); `ci/cargo.sh` does
  not pass `MODEL_SEEDS` and `MODEL_FIRST_SEED` into its container.

## 2026-11-24: the reference model and the checkers (sync-model)

Done:

- `crates/model`, written from docs/sync.md and docs/sync-api.md only
  (and docs/observation.md for what can be seen); it depends on no crate
  of the workspace. Name rules of Hearth and of each volume, conflict
  copy names; the decision table of sync.md 3.5 as a three-way merge
  (`reconcile`); one checker per guarantee I1 to I13 and the mass-change
  guard, over a trace of observations; the settled outcome, exact where
  the plan fixes it and as a set of allowed outcomes where the order of
  devices is free; a seeded scenario generator weighted toward the nasty
  cases, the names corpus of sync.md 7.7, a plain-text scenario format
  and a shrinker.
- docs/model.md (the rules in prose, the allowed-outcome sets, the
  scenario format, how to add a guarantee), docs/model-needs.md (what a
  runner must observe, and how to fill it from docs/observation.md),
  questions M1 to M16 in docs/contract-questions.md, README section.
- Tests: 19 unit tests; 59 decision-table tests (one per cell, all 46
  cells reached); 18 mutant tests: a naive test sync broken on purpose
  in 17 ways (at least one per guarantee), each caught by the checker
  meant for it while the unbroken sync passes the same scenario; the
  property run of the naive sync, clean for seeds 0 to 999 (CI runs
  100). `cargo fmt`, `cargo clippy -- -D warnings` clean; no unsafe.

Half done:

- Seeds 1000 to 3999 still show six rare cases, listed with their
  scripts in docs/model.md, "Open findings". Not yet settled whether
  the test stand-in, the merge or a checker is at fault in each.

Next:

- The simulator (`sync-sim`) maps the engine's observation to
  `hearth_sync_model::trace::Trace` (docs/model-needs.md) and runs the
  generated scenarios against the engine, calling `check::check_trace`.
- The open findings; answers to M1 to M16.

## 2026-11-24: step 1, the core and the store (sync-core)

Done:

- `crates/store`: SQLite state database (schema 1, forward-only
  migrations, WAL, `synchronous=FULL`, full fsync on Apple systems),
  behind the core's `Store` interface; same contract as the in-memory
  `MemStore` (conformance suite, random batches compared with it, failures
  injected at every write point, a killed writer process leaving only
  whole batches).
- `crates/core`: the engine as a deterministic state machine stepped by
  its host; every outside thing behind a trait (`Fs`, `Remote` with the
  contract's requests and conditions as types, `Clock`, `Random`,
  `Scheduler`); the feed client of contract 6.6 (rules 1 to 7, listings,
  cursor expiry by reason, epoch and instance changes); scanning with
  rename and move detection by file id; hashing at rest; the decision
  table as data and the planner; the executor with resumable downloads
  (ranges, temp files, hash check, swap that keeps the replaced file) and
  upload sessions (chunk hashes, second hash while reading, repeatable
  finish); conflict copies; the holding area with its 7 days; selective
  sync and ignore rules; the root marker, mass-change and server identity
  guards; error classes and backoff; recovery after a crash; the
  observation interface and the guarantees as functions; the photo
  uploader's shapes.
- Tests (all green, `cargo clippy -- -D warnings` clean): 102 in the core
  (every cell of the files and folders tables end to end by name, the
  creations, rights, names from the corpus, guards, the holding area,
  cursor expiry and restored servers, lost answers, corrupted downloads,
  a crash between every two steps of a scenario, a store failure at each
  of 60 write points, a deleted database, the same seed giving the same
  run, the store contract, and property tests: random changes on both
  sides on a case-sensitive and a case-insensitive volume with random
  crashes, a replayed feed, names over the corpus' alphabet); 8 for the
  SQLite store against the same contract and random batches compared
  with the in-memory store; a killed-writer crash test; the budgets
  benchmark (docs/performance.md). Property cases run locally: 1,024 per
  property; the nightly workflow runs 200,000.
- The random tests found, and the code now handles: a resync listing
  that removed what its last page delivered; swapped names waiting on
  each other (now untangled through a parking name); a move into a name
  whose holder Hearth itself moved there; folder moves whose children
  were renamed at the same time; and a gap in the contract's feed rules
  (docs/contract-questions.md Q6).

Not done in step 1 (by the plan's split or the brief):

- The real file system (`crates/fs`), HTTP (`crates/remote`), the CLI, the
  simulator (`crates/sim`) and the reference model (`crates/model`) are
  other helpers' work.
- Exec bit and mtime-only changes are not synced; folder size prompts are
  not asked (docs/contract-questions.md Q4, Q5).
- Moves between mounts are done as delete and create, not as the
  server's id-keeping move.
- Parallel transfers: one operation step at a time; the host may later run
  steps of different operations side by side.
- The holding area is not trimmed early when the disk runs short.

Next: sync-sim drives the core through `Env` and `Engine::step`
(docs/observation.md); sync-model checks the guarantees over traces.
