# Hearth sync

The sync engine for Hearth: one core, written in Rust, that keeps a folder
on a device and a person's files on a Hearth server the same. It is meant
for the desktop client (Windows, macOS, Linux), a command-line client for
servers, and the phone app with its file providers.

Status: early. The core (the engine's state machine, planner and
executor) and its state database exist and are tested against a fake disk
and a fake Hearth. There is no real file system or HTTP code yet, so it
does not sync real folders yet.

## What it is built to do

- Sync both ways between a local folder and Hearth, continuously.
- Know files by identity, not by path, so a rename or a move is one cheap
  operation on any size of folder.
- Never remove or overwrite anything without the replaced bytes existing
  somewhere else first: on the server (trash, versions) or in a holding
  area on the device.
- Keep both versions when two devices change the same file, the second
  as a clearly named conflict copy.
- Verify every transfer by SHA-256 from end to end.
- Survive a crash or a power cut at any moment and carry on where it was.
- Decide nothing by a file's time: a device with a wrong clock still syncs
  correctly.
- Work with large trees (a million files) within stated limits of memory
  and time.

## How it is put together

A Cargo workspace, one job per crate:

| Crate | What it owns |
| --- | --- |
| `crates/core` | The three trees (remote, local, last synced), the planner, the executor. No I/O of its own. |
| `crates/store` | The state database (SQLite). |
| `crates/fs` | The real file system on each operating system: scanning, watching, atomic replace, names. |
| `crates/remote` | The server's sync API over HTTP. |
| `crates/model` | A small reference model of what sync should do, and the checkers for the engine's guarantees. |
| `crates/sim` | A simulated disk, server, clock and network, driven by one seed, with faults. |
| `crates/cli` | `hearth-sync`, the headless client. |
| `crates/ffi` | Bindings for the phone app. |
| `crates/desktop` | The desktop background service and tray. |

The core reaches disk, network, time and randomness only through
interfaces. Real clients plug in the real thing; the tests plug in a
simulation that replays exactly from one seed number, with crashes and
network faults at any point.

The protocol between the engine and the server is one written contract,
kept with the server. `CONTRACT` names the version this code implements.

## What exists

- `crates/core`: the engine. Three pictures of every item (Hearth's, the
  disk's, and the last agreed base); the decision table of every pair of
  local and remote change as data; a planner that checks names, rights,
  free places and cycles before any step; an executor with resumable
  downloads and upload sessions, SHA-256 checks end to end, swaps that
  keep what they replace, conflict copies, and a holding area for
  everything sync removes. Guards for a missing root, mass deletions, a
  changed or restored server. Every outside thing (disk, Hearth, clock,
  randomness, scheduling) is an interface, so a run replays from a seed.
- `crates/store`: the state database on SQLite (WAL, full fsync), with
  the same contract as the in-memory store the simulator uses.
- `docs/observation.md`: what checkers and the simulator can observe.
- `docs/core-design.md`: how the core is arranged and why.
- `docs/performance.md`: measured figures against the budgets.
- `docs/contract-questions.md`: open questions about the sync API.

## Checking it

The engine is tested against a reference model (`crates/model`): a short,
independent statement of what correct sync means, written from the plan
and the protocol alone.

- The three-way merge as a table, one test per cell.
- One checker per guarantee (nothing lost, nothing overwritten unseen,
  nothing half written, convergence, one conflict copy per losing
  version, and the rest), run after every step of every simulated run.
- Random scenarios from a seed: several devices on different file
  systems, nasty names, concurrent edits, crashes, network faults, clock
  skew, server restores. A failing run shrinks to a short plain-text
  script that is kept and replayed for ever.
- Deliberately broken variants prove that each checker catches what it
  is meant to.

See `docs/model.md`.

## Building

    cargo build --workspace
    cargo test --workspace

The compiler version is pinned in `rust-toolchain.toml`. `ci/cargo.sh`
runs any cargo command in a container with that compiler, with capped
CPUs and memory and core dumps off. `tools/check-large-files.sh` refuses
files over 5 MB; `git config core.hooksPath tools/hooks` runs it before
every commit.

## Licence

AGPL-3.0-or-later. See `LICENSE`.
