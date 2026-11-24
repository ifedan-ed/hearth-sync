//! The desktop client: a background service and a tray.
//!
//! Owns: the per-person service that holds the core, the watcher and the
//! transfers; the local socket through which the tray talks to it; the
//! tray itself (state, activity, problems and conflicts, settings), built
//! with Tauri 2; packaging and updates.
//!
//! The two are separate programs so the tray can close, crash or update
//! without syncing stopping. The engine does not depend on this crate.
#![forbid(unsafe_code)]
