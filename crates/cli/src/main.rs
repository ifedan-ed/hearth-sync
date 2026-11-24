//! `hearth-sync`: the headless client.
//!
//! Owns: the commands (connect, add a folder, run, status, problems and
//! conflicts, check the database, pause, a diagnostic bundle) and the
//! wiring of the core to the real file system and the real server.
//!
//! It is the client for servers, and the driver of every test on real
//! file systems. What the desktop tray does later goes through the same
//! code as these commands.
#![forbid(unsafe_code)]

/// Nothing is built yet: the core, its simulator and its model tests come
/// first, and no client touches a real folder before them.
fn main() {}
