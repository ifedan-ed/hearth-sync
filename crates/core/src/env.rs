//! Time, randomness and scheduling as interfaces, and `Env`, the bundle of
//! every outside thing one step of the engine may touch.
//!
//! Why: a run must replay exactly from one seed (I13). The engine reads no
//! clock, draws no random number and chooses no order of work except
//! through these, so the simulator controls all three. The real hosts pass
//! the system clock, the system's random source and a first-come scheduler.

use crate::fs::Fs;
use crate::remote::Remote;

/// The clock. Read only for the uses docs/sync.md 3.15 allows: backoff
/// timers, the holding area's days, the name of a conflict copy, the
/// mtime written for a file Hearth dated zero. No decision about what to
/// sync reads it.
pub trait Clock {
    /// Now, in nanoseconds since 1970 (UTC).
    fn now_ns(&self) -> i64;
    /// The local time zone's offset from UTC in seconds at a moment (for
    /// conflict copies' names, which use local time).
    fn utc_offset_s(&self, at_ns: i64) -> i32;
}

/// Randomness: the root marker's id and backoff jitter.
pub trait Random {
    /// A uniformly random 64-bit number.
    fn next_u64(&mut self) -> u64;
}

/// The choice of which runnable piece of work goes next. The simulator
/// draws it from its seed to explore orders; real hosts take the first.
pub trait Scheduler {
    /// Picks one of `ready` runnable items (0 .. ready). Called only with
    /// `ready > 1`; an answer out of range is taken modulo `ready`.
    fn pick(&mut self, ready: usize) -> usize;
}

/// A scheduler that always takes the oldest runnable item: what a real host
/// uses until parallel transfers arrive (step 3).
#[derive(Clone, Copy, Debug, Default)]
pub struct FirstReady;

impl Scheduler for FirstReady {
    fn pick(&mut self, _ready: usize) -> usize {
        0
    }
}

/// Everything outside the engine, lent to it for one call.
pub struct Env<'a> {
    /// The disk under the sync root.
    pub fs: &'a mut dyn Fs,
    /// Hearth.
    pub remote: &'a mut dyn Remote,
    /// The clock.
    pub clock: &'a dyn Clock,
    /// Randomness.
    pub random: &'a mut dyn Random,
    /// The order of work.
    pub scheduler: &'a mut dyn Scheduler,
}
