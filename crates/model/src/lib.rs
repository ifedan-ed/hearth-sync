//! The reference model and the invariant checkers of Hearth's sync
//! engine.
//!
//! What it is: a second, much simpler statement of what correct sync
//! means, written from the plan (docs/sync.md in the Hearth repository:
//! section 2, the guarantees; 3.5, the decision table; 3.8, names) and
//! the contract (docs/sync-api.md) only, by someone who has not read the
//! engine. The engine is checked against it over many random scenarios;
//! where the two disagree, one of them is wrong, and the plan decides.
//!
//! Why its own crate: independence. It depends on no other crate of this
//! workspace, so it cannot borrow the planner's mistakes, and nobody who
//! works on the planner or the simulator needs to edit a file here.
//!
//! It favours clarity over speed everywhere: a reader should be able to
//! check each rule against the plan line by line. Each rule's comment
//! names the section it comes from.
//!
//! The parts:
//!
//! - [`name`]: what names Hearth and each kind of volume accept, when two
//!   names are one, and conflict copy names.
//! - [`tree`]: trees by identity and by path.
//! - [`reconcile`]: the three-way merge, the decision table as code.
//! - [`apply`]: what a person's act does to a tree.
//! - [`scenario`]: scenarios as data and as text.
//! - [`trace`]: the observations a run produces, which checkers read.
//! - [`check`]: one checker per guarantee.
//! - [`outcome`]: the settled outcome, exact where the plan fixes it and
//!   as a set where order is free.
//! - [`generate`], [`corpus`], [`rng`]: random scenarios from a seed.
//! - [`shrink`]: a failing scenario reduced to a short one.
//!
//! docs/model.md (in this repository) states the rules in prose, the
//! scenario format, and how to add a guarantee; docs/model-needs.md says
//! what a runner must observe.
#![forbid(unsafe_code)]

pub mod apply;
pub mod check;
pub mod corpus;
pub mod generate;
pub mod name;
pub mod outcome;
pub mod reconcile;
pub mod rng;
pub mod scenario;
pub mod shrink;
pub mod trace;
pub mod tree;
