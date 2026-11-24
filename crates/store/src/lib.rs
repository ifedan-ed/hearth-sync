//! The engine's state database: SQLite behind the core's `Store`
//! interface.
//!
//! Owns: the schema and its forward-only migrations, the pragmas (WAL,
//! full synchronous writes, full fsync on Apple systems), every query, and
//! the consistency checker that runs after each simulated crash.
//!
//! Why its own crate: the choice of SQLite is then one crate, not the
//! engine; and a phone's file provider extension, which has a few
//! megabytes and a few milliseconds, can link the store and its read
//! functions without the planner or the scanner.
//!
//! The database is a cache of facts, never the only copy of anything a
//! person saved: deleting it must lose no data.
#![forbid(unsafe_code)]
