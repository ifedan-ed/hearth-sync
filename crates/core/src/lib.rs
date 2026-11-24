//! The core of Hearth's sync engine.
//!
//! Owns: the three trees (remote: what Hearth last reported; local: what
//! was last seen on disk; synced: the last state both agreed on), the
//! planner, the executor, the operation journal's logic, and the four
//! interfaces through which everything outside is reached: `Fs`, `Remote`,
//! `Clock` and `Random`, plus `Store` for its own state.
//!
//! Why it is shaped this way:
//!
//! - The planner is a pure function: three trees in, operations out. It
//!   reads no clock, no disk and no network, so every case of the decision
//!   table can be tested by the million and replayed from a seed.
//! - The core does no I/O of its own. Real clients plug in the real disk
//!   and Hearth's API; the simulator plugs in fakes it controls. The same
//!   code runs in both, which is what makes a simulated run evidence.
//! - One control task owns all state; results of I/O come back to it as
//!   events. Nothing here starts a thread or an async runtime.
//! - Determinism is a rule: no iteration over randomly seeded maps where
//!   order can reach a decision, no time or randomness except through the
//!   interfaces.
//!
//! Must not: call the operating system, open sockets, read the system
//! clock, or contain `unsafe`. The reference model (hearth-sync-model) is
//! written without reading this crate's planner, and this crate's planner
//! is written without reading the model: when they disagree, one of them
//! has a bug, and that is the point of having two.
#![forbid(unsafe_code)]
