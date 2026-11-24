//! The simulator: fakes of everything outside the core, under the control
//! of one seeded random generator.
//!
//! Owns: an in-memory file system with each operating system's behaviour
//! (case, Unicode forms, refused names, file ids and their reuse, what a
//! crash keeps and loses), an in-memory Hearth built from the contract
//! alone, a clock that moves only when told, the fault injector (network,
//! process, disk, time, server), and the runner that drives several
//! devices through a random scenario and then calls the checkers of
//! hearth-sync-model.
//!
//! One thread and no real time, so a seed is a complete description of a
//! run: the same seed gives the same log, byte for byte, and a failing
//! seed is replayed as often as wanted. Every failing seed is kept as a
//! named regression test.
//!
//! The simulated server must not drift from the real one: the same
//! scripted request sequences run against both and must agree.
#![forbid(unsafe_code)]
