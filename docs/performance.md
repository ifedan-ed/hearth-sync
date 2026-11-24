# Performance

Figures against the budgets of docs/sync.md 7.9. The budgets were set
before anything was measured; where a figure misses one, it says by how
much and why, and what would bring it in. Nothing here is a release-build
figure on a reference machine yet (that is step 3, on GitHub's runners and
a laptop per OS).

## 2026-11-24: step 1, the store and the planner at a million entries

How: `cargo test --release -p hearth-sync-store --test budgets -- --ignored
--nocapture` (crates/store/tests/budgets.rs). A tree of 1,000 folders of
1,000 files each in a SQLite file, built through the store's own batches
of 5,000 nodes. Measured on the shared build server inside a container
capped at 4 CPUs, while the server's load average was between 60 and 95
(it runs many other jobs), so the times are pessimistic and noisy; memory
and size are not affected by the load.

| Budget (7.9) | Target | Measured | |
| --- | --- | --- | --- |
| planner, 1,000,000 nodes, 100 changed | 50 ms | 7 ms | met |
| planner, first sync of 1,000,000 nodes | 60 s | 40 s | met (136 s before the per-pass folder caches; see below) |
| database size at 1,000,000 files | 300 MB | 473 MB | missed by 58 % |
| memory idle at 1,000,000 files | 150 MB | 15 MB (the store and planner alone) | met, but see below |
| memory, peak | 400 MB | 17 MB (the store and planner alone) | met, but see below |
| writing the synced tree (not a budget) | | 84 to 90 s | |

What these figures do and do not cover:

- The planner figure for 100 changed nodes is the planner alone: 100
  decisions read through the SQLite store's indexes, about 70 µs each.
- The first-sync figure is one planning pass over a million nodes Hearth
  listed and the disk does not have: 1,000 folders to make and 998,999
  files that wait for their folder. With persisted, targeted waits the
  files are not planned again until their folder exists, so the whole
  first sync plans each node about twice. The first measurement, 136 s,
  read every folder's ancestors again for every file below it (selective
  sync, encryption and ignore checks); caching each folder's answer for
  the pass brought it to 40 s.
- Memory is the test process's resident size: the store (SQLite's page
  cache at its default 2 MB) and the planner. It does not include a host's
  scanner, transfers or HTTP, which come in step 3, so the budget is not
  yet proven for a whole client.
- The database is over its budget. Per file it holds three pictures with
  three hashes (32 bytes each), the name three times plus its comparison
  key, two 16-byte file ids and six indexes. Ways to bring it in, to be
  measured before choosing: keep the comparison key only where it differs
  from the name (most names are their own key on a case-sensitive
  volume); keep the base's hash only where it differs from the local
  one; a numeric remote id where Hearth's ids are numbers; drop the
  node-by-mount index in favour of the remote table's mount column. The
  plan's budget stands until then.

Tests' speed (debug build, same server): the core's 102 tests take about
45 s with 1,024 property cases each; the store's contract suite 6 s; the
crash test (8 killed writers) under 1 s.
