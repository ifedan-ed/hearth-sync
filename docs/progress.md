# Progress

Newest first. What was done, what is half done, what is next.

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
