//! The real file system, per operating system, behind the core's `Fs`
//! interface.
//!
//! Owns: scanning (the fastest listing each system has), watching
//! (inotify, FSEvents, ReadDirectoryChangesW, with their resume points),
//! file ids, the atomic swap that puts a download in place while keeping
//! the replaced file, flushing, the probe of a volume's case and Unicode
//! behaviour, and each system's rules for names.
//!
//! One sub-module per operating system. This is the only crate allowed
//! `unsafe`, for system calls with no safe wrapper; each such block is
//! small, wrapped in a safe function, and carries a comment saying why it
//! is sound.
//!
//! A watcher event is a hint that a folder needs looking at, never a
//! fact: this crate reports what it then reads from disk.
