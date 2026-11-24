//! A small harness for the core's own tests: a fake disk, a fake Hearth, a
//! clock that only moves when told, seeded randomness, and a runner that
//! steps an engine until it is idle. The simulator (hearth-sync-sim) is
//! the full harness; this one is just enough to test the decision table
//! end to end and to property-test pieces of the engine.

pub mod fs;
pub mod remote;

use std::cell::Cell;

use crate::config::Config;
use crate::engine::{Engine, EngineResult, StepOutcome};
use crate::env::{Clock, Env, Random, Scheduler};
use crate::names::VolumeRules;
use crate::store::Store;

pub use fs::FakeFs;
pub use remote::{FakeRemote, Fault};

/// A clock that moves only when told (or when the runner jumps to a timer).
#[derive(Debug)]
pub struct FakeClock {
    now: Cell<i64>,
    /// The local offset from UTC it reports, in seconds.
    pub offset_s: i32,
}

impl FakeClock {
    /// A clock at a moment.
    pub fn at(ns: i64) -> FakeClock {
        FakeClock {
            now: Cell::new(ns),
            offset_s: 0,
        }
    }

    /// Moves time on.
    pub fn advance(&self, ns: i64) {
        self.now.set(self.now.get().saturating_add(ns));
    }

    /// Sets the time.
    pub fn set(&self, ns: i64) {
        self.now.set(ns);
    }
}

impl Clock for FakeClock {
    fn now_ns(&self) -> i64 {
        self.now.get()
    }

    fn utc_offset_s(&self, _at_ns: i64) -> i32 {
        self.offset_s
    }
}

/// splitmix64 as `Random` and as a `Scheduler` that explores orders.
#[derive(Clone, Debug)]
pub struct SeededRandom(pub u64);

impl SeededRandom {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

impl Random for SeededRandom {
    fn next_u64(&mut self) -> u64 {
        self.next()
    }
}

impl Scheduler for SeededRandom {
    fn pick(&mut self, ready: usize) -> usize {
        (self.next() % ready.max(1) as u64) as usize
    }
}

/// Everything outside one engine.
pub struct World {
    /// The disk.
    pub fs: FakeFs,
    /// Hearth.
    pub remote: FakeRemote,
    /// Time.
    pub clock: FakeClock,
    /// Randomness.
    pub random: SeededRandom,
    /// The order of work.
    pub scheduler: SeededRandom,
}

impl World {
    /// A world with an empty disk and an empty Hearth, from a seed.
    pub fn new(seed: u64, rules: VolumeRules) -> World {
        World {
            fs: FakeFs::new(rules),
            remote: FakeRemote::new(),
            clock: FakeClock::at(1_795_615_402_000_000_000),
            random: SeededRandom(seed),
            scheduler: SeededRandom(seed ^ 0x5EED),
        }
    }

    /// The environment to lend to one call of the engine.
    pub fn env(&mut self) -> Env<'_> {
        Env {
            fs: &mut self.fs,
            remote: &mut self.remote,
            clock: &self.clock,
            random: &mut self.random,
            scheduler: &mut self.scheduler,
        }
    }

    /// Settings for tests: the plan's values, a short device name.
    pub fn config(rules: VolumeRules) -> Config {
        Config::recommended("Laptop", rules)
    }
}

/// How a run to idle ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    /// Idle, with nothing new on Hearth: converged as far as it can.
    Idle {
        /// Steps taken.
        steps: usize,
    },
    /// A guard paused it.
    Paused(crate::engine::PauseReason),
    /// It did not settle within the bound (a sync loop: I7).
    Unbounded,
}

/// Steps an engine until it is idle and Hearth's wait says nothing is new
/// (or a guard pauses it), jumping the clock to the engine's timers so
/// retries happen. Bounded by `max_steps`.
pub fn run_to_idle<S: Store>(
    engine: &mut Engine<S>,
    world: &mut World,
    max_steps: usize,
) -> EngineResult<Settled> {
    let mut steps = 0;
    let mut idle_rounds = 0;
    while steps < max_steps {
        steps += 1;
        match engine.step(&mut world.env())? {
            StepOutcome::Worked => idle_rounds = 0,
            StepOutcome::Paused(p) => return Ok(Settled::Paused(p)),
            StepOutcome::Idle { wake_at } => {
                engine.wait_remote(&mut world.env(), 0)?;
                // Did the wait report anything? Step once more to find out.
                match engine.step(&mut world.env())? {
                    StepOutcome::Worked => {
                        idle_rounds = 0;
                        continue;
                    }
                    StepOutcome::Paused(p) => return Ok(Settled::Paused(p)),
                    StepOutcome::Idle { wake_at: w2 } => {
                        let next = w2.or(wake_at);
                        // Timers far away (the rescan, the slow retry) are
                        // not waited for; near ones (backoff) are.
                        match next {
                            Some(t) if t - world.clock.now_ns() < 3_600_000_000_000 => {
                                if t > world.clock.now_ns() {
                                    world.clock.set(t);
                                }
                                idle_rounds += 1;
                                if idle_rounds > 50 {
                                    return Ok(Settled::Unbounded);
                                }
                            }
                            _ => return Ok(Settled::Idle { steps }),
                        }
                    }
                }
            }
        }
    }
    Ok(Settled::Unbounded)
}
