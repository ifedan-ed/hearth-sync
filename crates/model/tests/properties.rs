//! The model against itself: random scenarios played by the naive sync
//! (tests/common) must satisfy every checker. A failure is shrunk and
//! printed as scenario text, ready for the seed archive.
//!
//! The number of seeds is one explicit setting: MODEL_SEEDS (the CI run
//! keeps the default small; longer runs set it).

mod common;

use std::collections::BTreeSet;

use common::{Mutant, run};
use hearth_sync_model::check::{CheckConfig, Violation, check_i13, check_trace};
use hearth_sync_model::generate::{GenConfig, generate, op_histogram};
use hearth_sync_model::scenario::Scenario;
use hearth_sync_model::shrink::shrink;

/// How many seeds this run tries: MODEL_SEEDS, else 200.
fn seeds() -> u64 {
    match std::env::var("MODEL_SEEDS") {
        Ok(v) => v.parse().unwrap_or_else(|_| panic!("MODEL_SEEDS must be a number, got {v:?}")),
        Err(_) => 200,
    }
}

/// The first seed: MODEL_FIRST_SEED, else 0 (to spread long runs).
fn first_seed() -> u64 {
    match std::env::var("MODEL_FIRST_SEED") {
        Ok(v) => v.parse().unwrap_or_else(|_| panic!("MODEL_FIRST_SEED must be a number, got {v:?}")),
        Err(_) => 0,
    }
}

fn violations(sc: &Scenario) -> Vec<Violation> {
    check_trace(&run(sc, Mutant::None), &CheckConfig::plan_defaults())
}

#[test]
fn naive_sync_satisfies_every_checker() {
    let cfg = GenConfig::small();
    let mut scenarios = Vec::new();
    let mut steps = 0usize;
    for seed in first_seed()..first_seed() + seeds() {
        let sc = generate(seed, &cfg);
        let trace = run(&sc, Mutant::None);
        steps += trace.steps.len();
        let found = check_trace(&trace, &CheckConfig::plan_defaults());
        if let Some(first) = found.first() {
            let kinds: BTreeSet<_> = found.iter().map(|v| v.guarantee).collect();
            let small = shrink(
                &sc,
                |s| violations(s).iter().any(|v| kinds.contains(&v.guarantee)),
                3000,
            );
            let again = violations(&small);
            panic!(
                "seed {seed}: {} violations, first: {first}\nshrunk scenario:\n{}\nits violations:\n{}",
                found.len(),
                small.to_text(),
                again.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")
            );
        }
        scenarios.push(sc);
    }
    eprintln!("{} scenarios, {steps} steps checked", scenarios.len());
    for (kind, n) in op_histogram(&scenarios) {
        eprintln!("  {n:6}  {kind}");
    }
}

#[test]
fn same_scenario_same_log() {
    for seed in 0..20 {
        let sc = generate(seed, &GenConfig::small());
        let a = run(&sc, Mutant::None);
        let b = run(&sc, Mutant::None);
        assert_eq!(check_i13(&a.log, &b.log), None, "seed {seed}");
    }
}
