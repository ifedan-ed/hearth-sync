# Hearth sync

The sync engine for Hearth: one core, written in Rust, that keeps a folder
on a device and a person's files on a Hearth server the same. It is meant
for the desktop client (Windows, macOS, Linux), a command-line client for
servers, and the phone app with its file providers.

Status: early. This repository holds the layout and the rules; the engine
is being built, tests first. It does not sync anything yet.

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

## Building

    cargo build --workspace
    cargo test --workspace

The compiler version is pinned in `rust-toolchain.toml`.

## Licence

AGPL-3.0-or-later. See `LICENSE`.
