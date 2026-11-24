# Decisions

Choices with lasting effect, and why. Newest first.

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
