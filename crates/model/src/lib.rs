//! The reference model and the invariant checkers.
//!
//! Owns: a small, slow, obviously-right function that computes, from a
//! base tree, a local tree and a remote tree, the trees that should
//! result; and the checkers for the engine's guarantees (no saved bytes
//! lost, nothing overwritten unseen, convergence, exactly the expected
//! conflict copies, and the rest), which the test runners call after every
//! step of every random run.
//!
//! Why its own crate: independence. It is written from the plan's
//! decision tables and the contract, by someone who does not read the
//! planner in hearth-sync-core. It depends on no other crate of this
//! workspace, so it cannot borrow the planner's mistakes, and nobody who
//! works on the planner or the simulator needs to edit a file here.
//!
//! It favours clarity over speed everywhere: a reader should be able to
//! check each rule against the plan line by line.
#![forbid(unsafe_code)]
