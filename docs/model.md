# The reference model

crates/model is a second, much simpler statement of what correct sync
means. The engine is run against it over many random scenarios; where
they disagree, one of them is wrong, and the plan decides which.

It is written from two documents in the Hearth repository only: the
plan (docs/sync.md: section 2, the guarantees; 3.5 and 3.5.1, the
decision table and conflict copies; 3.7, the holding area; 3.8, names;
3.11, the guards; 7.2 and 7.3, the tests) and the contract
(docs/sync-api.md, version 1: sections 4, 5, 6.6, 6.8, 8, 9, 13, 17).
Its author has not read the engine's core or store. The one document it
shares with the core is docs/observation.md, which says what can be
observed, never how the engine decides. The crate depends on no other
crate of the workspace.

## What is in it

| Part | What it states |
| --- | --- |
| `name` | Hearth's name rules (sync-api 13), what each volume holds and compares as one (sync.md 3.8), conflict copy names (3.5.1). |
| `tree` | Trees by identity (`IdTree`) and by path (`PathTree`). |
| `reconcile` | The three-way merge: from base, local and remote, the trees after settling. |
| `apply` | What a person's act does to one side. |
| `scenario` | Scenarios as data and as text. |
| `trace` | The observations a run produces (docs/model-needs.md says how to fill them). |
| `check` | One checker per guarantee of sync.md 2, and the mass-change guard. |
| `outcome` | The settled outcome: exact where the plan fixes it, a set where order is free. |
| `generate`, `corpus`, `rng` | Random scenarios from a seed, weighted toward the nasty cases. |
| `shrink` | A failing scenario reduced to a short one. |

## The merge, in prose

`reconcile(base, local, remote, device)` follows sync.md 3.5 in eight
stages. Each is a paragraph of the plan.

1. Classify each node on each side against the base: none, edit, move,
   both, delete, create (absent when neither). A local name is compared
   in NFC, a remote one exactly; a case-only rename is a move. A type
   change is a delete of one node and a create of another (a node never
   changes kind, sync-api 5).
2. Decide each node by its cell. Files: 25 cells plus three creation
   cells; folders: 9 plus three. Each cell says whether the node
   survives, whose place it takes (base, local, remote) and whose
   content. In short: an edit beats a delete on the other side (B, E);
   Hearth's place wins when both moved (C); two different edits keep
   Hearth's at the name and the local one as a conflict copy (A); a
   move against a delete applies the delete (D).
3. Rights (3.5, Permissions): a local edit in a mount without the update
   right is undone and kept as a conflict copy on the device only; a
   local delete without the delete right is undone; a local create
   without the create right stays local; a local move out of or into a
   folder without the rights is undone. Each is listed.
4. Names Hearth refuses (3.8 last rule, sync-api 13): a new local item
   with such a name stays local; a local rename to one leaves Hearth's
   name. Listed.
5. Moves that would make a cycle: Hearth's wins, the local move is
   undone (C).
6. A folder survives when anything placed in it survives (F, G, and
   "a node moved into a folder that the other side deleted").
7. Two nodes wanting one name on Hearth (the creation rows): two
   folders become one and their contents are merged by the same rules;
   a file and a folder: the folder keeps the name; two files with equal
   hashes are one; two different files: the one Hearth already holds
   keeps the name, the other becomes a conflict copy.
8. The device's side of names (3.8): a remote name the volume cannot
   hold, or the engine's own names (`.hearthsync` at the root,
   `*.hearthtmp` anywhere), stay on Hearth, listed; an item the device
   already holds that Hearth renamed to such a name stays at its old
   local name, and a folder kept so keeps everything in it as it is;
   two remote names one volume holds as one: the one already on the
   device stays, the other is listed; if neither is there yet, either
   may arrive first.

The result carries both trees, the listings, the conflict copies, the
moves undone, the cells reached, and the synced tree to continue from
(listed items keep their old synced state, so they are decided the same
way next time and never retried).

Conflict copies are named `<stem> (conflict <device>, <yyyy-MM-dd
HH.mm.ss>)<.ext>` with the device's cleaned label and the losing file's
mtime in the device's local time, numbered ` 2`, ` 3` before the bracket
when taken, the stem shortened to fit Hearth's 250 bytes. Readings of
the open details are in docs/contract-questions.md (Q2, Q3, Q11).

## The settled outcome: exact, or a set

Where the documents fix the outcome, the model computes it and compares
exactly. That is the case for a window between two settled points where
at most one device changed anything and that device was offline from
the start of the window until every web change was made (and no restore,
share change, refusal, denied path, root fault or lost database
happened). The outcome is then `reconcile` of the synced tree, that
device's tree and Hearth's tree; every other device holds Hearth's
result as its own volume allows. Only the choice of which of two
clashing names arrives first on a device is left open, and every choice
is accepted.

Everywhere else the order in which devices reach Hearth is free, so the
outcome is a set. The model describes it by versions:

- Every file a person made is live until a person who saw it replaced
  or deleted it. Liveness is counted per content: a copy is a second
  live file with the same content, and editing one of the two retires
  only that one. At each settled point the counts are brought down to
  what is visible, since two equal files made apart may have become one
  ("equal hash: adopt as one node"). A restore makes live exactly what
  is on any disk or in the restored Hearth; a lost database makes
  everything on that disk live (I12: differing files become conflict
  copies).
- A content a person replaced or deleted may still stay on Hearth when
  the act was one Hearth refuses (read-only mount, a refused path), and
  when a device lost its database while Hearth held it: without its
  history the device cannot know of a delete it had not yet sent, so
  the file comes back.
- At every settled point, every live content is on Hearth or listed on
  a device (I1).
- Hearth holds no content that is not live: a person's delete or edit
  against what they saw is never undone (I7).
- No content is on Hearth more often than people made it (I9), allowing
  for what a resync without history brings back.
- Every device equals Hearth in NFC except for listed items (I8), and
  every listing is true (I10).

Every interleaving the documents allow satisfies these, and losing,
duplicating or resurrecting a version breaks them.

## The checkers

`check::check_trace` runs them all; each returns violations with the
step, device, path, what was expected and what was found.

| Guarantee | What is checked | Where |
| --- | --- | --- |
| I1 | At every engine step: content leaving a device is on that device under another name, on Hearth (files, versions, trash) or in its holding area; no engine step removes a content's last copy anywhere (except a delete on a mount without trash or a replace without versions, which the contract makes final). At every settled point: every live version is visible. | `check_i1`, `outcome::check_versions` |
| I2 | At every engine step: a file replaced or removed whose content is not the synced tree's keeps that content visible on the device or on Hearth (a conflict copy; the holding area is not enough). | `check_i2` |
| I3 | Every write Hearth carried out had its operation's condition (sync-api 8.1), a file condition named the version Hearth held, and that version was in the device's view of Hearth or is content the device itself sent (a hash condition on its own bytes). | `check_i3` |
| I4, I5 | At every step, every file at a real name on any disk and on Hearth holds a content some person saved: on Hearth, otherwise I5; on a device, I5 after an injected corruption, else I4. | `check_i4_i5` |
| I6 | While a root is missing or replaced, its engine deletes and moves nothing on Hearth. After a restore, no engine removes from its disk a content Hearth lacks. | `check_i6` |
| I7 | After `Settled`, only `/sync/wait`, and no disk change; the settle took at most the bound of engine steps. The exact outcome where fixed; no superseded content back on Hearth. | `check_i7_quiescence`, `outcome` |
| I8 | At `Settled`: Hearth's names in NFC; every device equal to Hearth in NFC except listed items; the synced tree agreeing. | `check_i8` |
| I9 | At `Settled`: no content more often than made; engine-made conflict names well formed with a device of the run. | `check_i9` |
| I10 | At every settled point every listing's reason is one the documents give and true of that item, and every name on Hearth is one a person gave or a conflict copy name (the engine invents no names); at every step an engine never changes an item that stays listed through its run. | `check_i10` |
| I11 | No upload repeated after one was answered and Hearth holds it; no download repeated after one is in place. | `check_i11` |
| I12 | After a lost database, until settled: no transfer of a content both sides hold at one path at that moment. | `check_i12` |
| I13 | Two runs of one seed have byte-identical logs. | `check_i13` |
| 3.11 | No engine run deletes more than 50 files and a quarter of a mount without the person's confirmation. | `check_mass_change` |

Where a guarantee speaks of something no observer sees, the checker
checks its observable consequence: I4 and I5 as "only saved contents at
real names", I11 as "no completed transfer redone", the mass-change
guard by engine runs between outside events (Q1 in
docs/contract-questions.md).

### The proof that they bite

tests/common is a naive "perfect sync" that plays a scenario by taking
the model's own settled outcome after every act, one request and one
disk step at a time. tests/properties.rs runs it over random seeds and
requires zero violations. tests/mutants.rs breaks it on purpose, one
defect each, and requires the checker meant for that defect to report
it on a short scenario the unbroken sync passes:

| Mutant | Caught by |
| --- | --- |
| unlinks without holding | I1 |
| empties the holding area early | I1 |
| drops the conflict copy, overwrites the local edit | I2, and I1 (the version is gone) |
| writes without conditions | I3 |
| shows a download half written | I4 |
| puts an altered download in place | I5 |
| syncs a missing root as empty | I6 |
| reads a restore as deletions | I6 |
| keeps uploading after settling | I7 |
| sends names not in NFC | I8 |
| makes two conflict copies | I9 |
| renames a refused local file to send it | I10 |
| uploads again after a crash | I11 |
| downloads everything again after a lost database | I12 |
| leaks a run counter into its log | I13 |
| deletes 60 files without pausing | mass change |
| decides conflicts by modification time | I1 |

## The scenario format

Plain text, one op per line, so a failing seed can be saved, read,
edited and replayed. Names are always quoted, with `\\`, `\"` and
`\u{hex}` escapes for anything outside printable ASCII.

    hearth-sync-scenario 1
    seed 42
    device "Lap" ntfs +3600
    device "Desk" ext4 +0
    mount "Team" trash=no versions=yes perms=1 at-start=yes
    ---
    web mkdir "d1"
    web create "d1/f2.txt" c1
    settle
    fault "Lap" offline
    "Lap" edit "d1/f2.txt" c2
    web edit "d1/f2.txt" c3
    fault "Lap" online
    settle

Header lines: `seed <n>`, `device <name> <volume> <utc offset seconds>`
(volumes `ext4`, `apfs`, `hfsplus`, `ntfs`, `fat`), `mount <path>
trash= versions= perms= at-start=` (every key required). After `---`:

- `<device or web> create|edit <path> <content>`, `mkdir <path>`,
  `rename <from> <to>`, `delete <path>`, `copy <from> <to>`, `retype
  <path> <content>`;
- `admin share-arrive|share-leave <path>`, `admin restore` (to the
  previous `settle`, new epoch), `admin restart`, `admin cursor-expire`,
  `admin empty-trash`, `admin refuse <path> <locked|quota|virus|forbidden>`;
- `fault <device> offline|online|root-vanish|root-back|root-replaced|db-deleted`,
  `fault <device> drop|stall|lose-answer|crash|disk-full|corrupt <n>`
  (at the device's n-th request or disk call from then), `fault <device>
  deny <path>`, `fault <device> skew <seconds>`;
- `confirm <device>`: the person says "go on" to a pause;
- `settle`: faults stop, everything settles, the convergence checks run.

An act whose target is missing when it runs is skipped and recorded so;
shrinking and timing may remove what an act needed.

## The generator

`generate(seed, config)` makes one to three devices on random volumes
and time zones, sometimes a second mount (a share, possibly read-only,
possibly without trash or versions, possibly arriving later), a few web
folders and files, then one to four rounds, each ending in `settle`.
A round may start with some devices offline (so their changes meet the
others' at once) and holds up to ten patterns, chosen with these
weights:

| Pattern | Weight | What |
| --- | --- | --- |
| create | 12 | a new file |
| edit | 12 | new content |
| concurrent edit | 8 | two sides edit (or one deletes) one file |
| delete | 7 | a file or a folder |
| rename | 6 | in place |
| move | 5 | to another folder |
| mkdir | 5 | a new folder |
| delete then recreate | 4 | a delete, then a file or folder at the name |
| case-only rename | 3 | `readme` to `README` |
| retype | 3 | file to folder or back |
| edit during upload | 3 | edit, a lost answer, stall or crash, edit again |
| clash pair | 3 | two names one volume holds as one |
| deep rename | 2 | a chain of 5 to 28 folders, renamed at the top |
| copy | 2 | a second file with one content |
| fault | 12 | drop, stall, lost answer, crash, disk full, corruption, clock skew of an hour to three years |
| heavy fault | 3 | root vanishing (and back), root replaced (and confirmed), lost database, denied path |
| admin | 4 | restart, cursor expiry, empty trash, restore, a refused path, a share leaving or arriving |
| mass delete | 1 | 60 files made on the web, then their folder deleted, then confirmed |

New names come from the corpus of sync.md 7.7 (crates/model/src/corpus.rs)
35 percent of the time: composed and decomposed pairs, case pairs
including sharp s, Turkish i and final sigma, every character Windows
refuses, trailing dots and spaces, reserved device names in all forms,
emoji, bidi and zero-width marks, names Hearth refuses, the engine's own
names and names that look like conflict copies, and long names at and
past 250 and 255 bytes.

`op_histogram` counts what a batch of scenarios holds; the property test
prints it.

## Shrinking and the seed archive

`shrink(scenario, still_fails, budget)` removes runs of ops (halving),
drops devices and mounts, replaces corpus names by plain ones and moves
fault positions to the first request, keeping each change only while the
failure stays. The shrunk scenario's text is what the archive keeps.

## Adding a guarantee

1. Write the guarantee's sentence and its source section into the doc
   comment of a new `check_*` function in check.rs (or outcome.rs if it
   needs the settled outcome), reading only the `Trace`.
2. If it needs something not yet observed, add the field to trace.rs
   and say how to fill it in docs/model-needs.md.
3. Call it from `check_trace`.
4. Add a mutant to tests/common that breaks exactly that rule, and a
   test in tests/mutants.rs on the shortest scenario that provokes it;
   the unbroken sync must pass that scenario.
5. Add its row to the tables above.

## Running

    /usr/local/bin/loadwait cargo test -p hearth-sync-model -j 4
    MODEL_SEEDS=5000 MODEL_FIRST_SEED=0 cargo test -p hearth-sync-model --release --test properties

## Open findings (2026-11-24)

The model's self-test (the naive sync over random seeds) is clean for
seeds 0 to 999. A run over seeds 1000 to 3999 (2,999 scenarios, 617,227
steps checked, 11,858 settled points, 4,491 of them against the exact
outcome) still reports the cases below. Each is a rare combination; for
each it is not yet settled whether the test stand-in, the merge or a
checker's reading is at fault. Replay with
`MODEL_SEEDS=1 MODEL_FIRST_SEED=<seed>`; the shrunk scripts:

- seed 1359, the stand-in panics: in a read-only share, a device turns a
  file into a folder, then the share leaves.
- seed 1383 (I1, I2, I10): Hearth refuses writes to `n1`; the device
  edits `n1`; the web turns `n1` into a folder. The local edit is lost
  in the stand-in; whether the merge or the stand-in's handling of a
  refused path is at fault is open.
- seed 1883 (I6): after a restore, on a FAT device, a file renamed on the
  web before the backup is removed locally.
- seed 1300 (I7): Hearth refuses writes to `n1`; the device renames it
  and then deletes it; the delete never reaches Hearth, and the
  checker's "refused" exemption follows paths, not items.
- seed 1439 (I8): in a read-only share, a device turns a file into a
  folder and moves another folder into it.
- seed 1311 (I10): two offline devices and the web create `README` and
  `readme` in one folder on case-insensitive volumes.

Fixed on the way, by the same run: a conflict copy made again every
round for an item a device keeps at an old local name (seeds 1313 and
3033, 499 and 150 copies; I9 caught both).
