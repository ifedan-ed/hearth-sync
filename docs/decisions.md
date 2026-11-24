# Decisions

Choices with lasting effect, and why. Newest first.

## 2026-11-24: the core follows the rulings R1 to R19 (sync.md 3.17)

- Case folding (R7): Unicode simple case folding, statuses C and S of
  CaseFolding.txt, applied to the NFC name and not normalised again. The
  Unicode version is the pinned compiler's standard library tables:
  Unicode 18.0.0 (`char::UNICODE_VERSION` of Rust 1.100.0). The table in
  `names::fold_char` covers where folding differs from the library's
  lowercase mapping. Changing the compiler's Unicode version is a
  migration of the database's name keys: a commit of its own.
- The device label (R3), which of two NFC-equal local names syncs (R4),
  the engine's own names compared as the volume compares them (R14), the
  exact default ignore list and ignored items as a count, not problems
  (R16), as the rulings say.
- A circle of waits is broken by parking a node under one of the
  engine's own temp names, tied to that node (R15: no invented visible
  names); the scanner follows a node parked so.
- Temp files the journal does not name go to the holding area, never
  unlinked (R14, R17).
- Read-only moves are undone on the device, a late 403 included (R18);
  an unknown hash on Hearth is not a difference (R19); an item moved out
  of a folder the other side deleted survives (R8); a name this device
  cannot hold leaves only the name behind (R10).

## 2026-11-24: the reference model stands alone (sync-model)

crates/model depends on no crate of the workspace and on one outside
crate, unicode-normalization (NFC is part of what a name is, sync-api
13). It has its own seeded random generator (SplitMix64) and its own
shrinker over the scenario text instead of proptest's: a seed must give
the same scenario on every machine and compiler for ever (I13 and the
seed archive), and the archive keeps scenario text that anyone can read,
edit and replay with this crate alone. The model does not call the
core's `guarantees` functions (docs/observation.md 4): two independent
judgements are the check.

## 2026-11-24: crash dumps, large files, the mirror's one route

- Two crash dumps (13 MB each) were committed in crates/core. Removed in
  an ordinary commit; history is not rewritten here (the public mirror's
  filter drops them from every commit). Core dumps are off in every test
  run (`ulimit -c 0`, `--ulimit core=0` in `ci/cargo.sh`), ignored by
  patterns that match only dump files (a bare `core` rule would also hide
  the crate folder crates/core), and any file over 5 MB is refused by CI
  and by the pre-commit hook.
- The public build copy is made only by `tools/export/public-mirror.sh`,
  deterministic and fast-forward only, so it can run after every merge
  without anyone choosing what to rewrite by hand.

## 2026-11-24: platform order and device sign-in (Maintainer's answers)

- Linux first (his Dell), with good Windows support as well; macOS after.
  The Linux file system implementation and its real-file-system tests come
  first when step 3 arrives. Windows name and path rules (reserved device
  names, trailing dots and spaces, case-insensitive volumes, long paths)
  are first-class in the core's name handling and decision table from the
  start (`VolumeRules::WINDOWS`, the corpus); macOS's decomposed names are
  handled in the core's logic now, its platform work comes last.
- A device's sync sign-in renews every 30 days and ends after 180 days
  idle, as the contract has it (sync-api.md 3.4).

## 2026-11-24: the core's step-1 choices (sync-core)

- One step per node and planning pass. A composite cell of the decision
  table is done as its first step; the base then records what both sides
  agree on and the node is planned again. Every step's precondition is
  checked against the world after the previous one, never against a plan
  made earlier. Slower by a planning pass per composite cell, which is
  cheap; simpler to prove.
- Waits are persisted and targeted: a node that cannot go yet records the
  node it waits for, and the transaction that changes that node wakes it.
  Without this, a first sync of a million files would plan the waiting
  ones again after every completed download (quadratic).
- The planner runs only when every mount is caught up and no scan is half
  done (contract 6.6 rule 7, and so an item moved between two folders is
  never seen as deleted from one before it is found in the other).
- Recovery keeps only operations whose restart would redo a transfer or
  leave a file between two places; all others are dropped and planned
  again from the pictures. Safe because every remote write carries its
  condition: a write that had succeeded before the crash shows in the feed
  and the planner finds both sides agree.
- Hearth's picture keeps the parent's Hearth id, not a node: the feed and
  listings may deliver a child before its parent (F3), and this needs no
  table for orphans.
- Ignore rules: a matcher of our own (about 200 lines, tested against
  gitignore's documented cases) instead of the `ignore` crate, which
  brings a directory walker, threads and file reading the core must not
  have.
- Name keys: NFC always (one name on Hearth is one name here, even on
  ext4), plus Unicode simple case folding on case-insensitive volumes,
  from the standard library's lowercase mapping and a table of the
  characters whose folding differs. The fs crate's probe of the real
  volume decides which rules apply.
- Conflict copies' local time comes from the `Clock` interface's offset,
  so the simulator controls it.
- A missing mount root (a share's folder gone from disk) leaves that
  mount alone, as a missing sync root pauses everything (I6 for mounts).
- Mount gone from Hearth's list: its local folder is kept and listed,
  nothing deleted (docs/contract-questions.md Q2).
- Dependencies: `sha2` (SHA-256, RustCrypto), `unicode-normalization`
  (NFC), `rusqlite` with bundled SQLite (the store), `proptest` (tests
  only). Nothing else.
