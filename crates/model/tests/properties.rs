//! The model against itself: random scenarios played by the naive sync
//! (tests/common) must satisfy every checker. A failure is shrunk and
//! printed as scenario text, ready for the seed archive.
//!
//! The number of seeds is one explicit setting: MODEL_SEEDS (the CI run
//! keeps the default small; longer runs set it).

mod common;

use std::collections::BTreeMap;

use common::{Mutant, run};
use hearth_sync_model::check::{CheckConfig, Guarantee, Violation, check_i13, check_trace};
use hearth_sync_model::generate::{GenConfig, generate, op_histogram};
use hearth_sync_model::scenario::Scenario;
use hearth_sync_model::shrink::shrink;

/// How many seeds this run tries: MODEL_SEEDS, else 100.
fn seeds() -> u64 {
    match std::env::var("MODEL_SEEDS") {
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| panic!("MODEL_SEEDS must be a number, got {v:?}")),
        Err(_) => 100,
    }
}

/// The first seed: MODEL_FIRST_SEED, else 0 (to spread long runs).
fn first_seed() -> u64 {
    match std::env::var("MODEL_FIRST_SEED") {
        Ok(v) => v
            .parse()
            .unwrap_or_else(|_| panic!("MODEL_FIRST_SEED must be a number, got {v:?}")),
        Err(_) => 0,
    }
}

fn violations(sc: &Scenario) -> Vec<Violation> {
    match run_caught(sc) {
        Some(t) => check_trace(&t, &CheckConfig::plan_defaults()),
        None => Vec::new(),
    }
}

/// The run's trace, or `None` when the stand-in panicked (a fault of the
/// test code or of the model's merge, shrunk and shown like any other).
fn run_caught(sc: &Scenario) -> Option<hearth_sync_model::trace::Trace> {
    std::panic::catch_unwind(|| run(sc, Mutant::None)).ok()
}

#[test]
fn naive_sync_satisfies_every_checker() {
    let cfg = GenConfig::small();
    let mut scenarios = Vec::new();
    let mut steps = 0usize;
    let (mut settles, mut exact) = (0usize, 0usize);
    // The first failing seed of each guarantee, shrunk: one run shows
    // every kind of failure at once.
    let mut failures: BTreeMap<Option<Guarantee>, String> = BTreeMap::new();
    for seed in first_seed()..first_seed() + seeds() {
        let sc = generate(seed, &cfg);
        let Some(trace) = run_caught(&sc) else {
            failures.entry(None).or_insert_with(|| {
                let small = shrink(&sc, |s| run_caught(s).is_none(), 3000);
                format!(
                    "seed {seed}: the run panicked\nshrunk scenario:\n{}",
                    small.to_text()
                )
            });
            continue;
        };
        steps += trace.steps.len();
        settles += trace
            .steps
            .iter()
            .filter(|s| matches!(s.event, hearth_sync_model::trace::Event::Settled))
            .count();
        exact += hearth_sync_model::outcome::exact_windows(&trace).1;
        let found = check_trace(&trace, &CheckConfig::plan_defaults());
        for v in &found {
            if failures.contains_key(&Some(v.guarantee)) {
                continue;
            }
            let g = v.guarantee;
            // A shrunk scenario must still be one a runner may play (no
            // pause left unconfirmed), or it shows another failure.
            let small = shrink(
                &sc,
                |s| {
                    let v = violations(s);
                    v.iter().any(|v| v.guarantee == g)
                        && (g == Guarantee::Runner
                            || v.iter().all(|v| v.guarantee != Guarantee::Runner))
                },
                3000,
            );
            let again = violations(&small);
            failures.insert(
                Some(g),
                format!(
                    "seed {seed}: {v}\nshrunk scenario:\n{}its violations:\n{}\n",
                    small.to_text(),
                    again
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            );
        }
        scenarios.push(sc);
    }
    eprintln!(
        "{} scenarios, {steps} steps checked, {settles} settled points, {exact} of them against the exact outcome",
        scenarios.len()
    );
    for (kind, n) in op_histogram(&scenarios) {
        eprintln!("  {n:6}  {kind}");
    }
    assert!(
        failures.is_empty(),
        "\n{}",
        failures.into_values().collect::<Vec<_>>().join("\n")
    );
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
