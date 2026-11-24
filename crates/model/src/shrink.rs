//! Shrinking: a failing scenario reduced to a small one that still fails
//! (sync.md 7.2 "Shrinking"), so the saved regression reads as a short
//! script.
//!
//! Plain delta debugging over the scenario's ops, then simpler pieces:
//! fewer devices and mounts, plain names instead of corpus names, fault
//! positions moved to the first request. Each step is kept only if the
//! caller's test still fails. It is not proptest's shrinker: the
//! scenario's own text form is what is shrunk and saved, so the archive
//! needs nothing but this crate to replay.

use crate::scenario::{Act, AdminOp, FaultOp, Op, Scenario, Side};

/// Reduces `scenario` while `fails` stays true; returns the smallest
/// found. `fails` must be true for the input (else it is returned as
/// is). The number of tries is bounded by `budget`.
pub fn shrink(
    scenario: &Scenario,
    mut fails: impl FnMut(&Scenario) -> bool,
    budget: usize,
) -> Scenario {
    let mut best = scenario.clone();
    if !fails(&best) {
        return best;
    }
    let mut tries = 0usize;
    let mut try_one = |candidate: Scenario, best: &mut Scenario, tries: &mut usize| -> bool {
        if *tries >= budget || candidate == *best {
            return false;
        }
        *tries += 1;
        if fails(&candidate) {
            *best = candidate;
            true
        } else {
            false
        }
    };
    loop {
        let mut progress = false;
        // 1. Remove runs of ops, halving the run length (ddmin).
        let mut chunk = best.ops.len().div_ceil(2).max(1);
        while chunk >= 1 {
            let mut start = 0;
            while start < best.ops.len() {
                let mut c = best.clone();
                let end = (start + chunk).min(c.ops.len());
                c.ops.drain(start..end);
                if !c.ops.ends_with(&[Op::Settle]) {
                    c.ops.push(Op::Settle);
                }
                if try_one(c, &mut best, &mut tries) {
                    progress = true;
                } else {
                    start += chunk;
                }
            }
            if chunk == 1 {
                break;
            }
            chunk /= 2;
        }
        // 2. Drop a device and everything it does.
        for d in best.devices.clone() {
            if best.devices.len() <= 1 {
                break;
            }
            let mut c = best.clone();
            c.devices.retain(|x| x.name != d.name);
            c.ops.retain(|op| !mentions_device(op, &d.name));
            progress |= try_one(c, &mut best, &mut tries);
        }
        // 3. Drop the mounts.
        if !best.mounts.is_empty() {
            let mut c = best.clone();
            c.mounts.clear();
            c.ops.retain(|op| {
                !matches!(
                    op,
                    Op::Admin(AdminOp::ShareArrive { .. } | AdminOp::ShareLeave { .. })
                )
            });
            progress |= try_one(c, &mut best, &mut tries);
        }
        // 4. Plain names: each distinct name becomes n1, n2, ... in every
        //    path where it occurs, one name at a time.
        for (i, name) in names_in(&best).into_iter().enumerate() {
            let plain = format!("n{}", i + 1);
            if name == plain || name.chars().all(|c| c.is_ascii_alphanumeric()) {
                continue;
            }
            let c = rename_everywhere(&best, &name, &plain);
            progress |= try_one(c, &mut best, &mut tries);
        }
        // 5. Fault positions to 1, skews to one hour.
        for i in 0..best.ops.len() {
            let mut c = best.clone();
            if let Some(Op::Fault { fault, .. }) = c.ops.get_mut(i) {
                let simpler = match fault {
                    FaultOp::Drop(n) if *n > 1 => FaultOp::Drop(1),
                    FaultOp::Stall(n) if *n > 1 => FaultOp::Stall(1),
                    FaultOp::LoseAnswer(n) if *n > 1 => FaultOp::LoseAnswer(1),
                    FaultOp::Crash(n) if *n > 1 => FaultOp::Crash(1),
                    FaultOp::DiskFull(n) if *n > 1 => FaultOp::DiskFull(1),
                    FaultOp::Corrupt(n) if *n > 1 => FaultOp::Corrupt(1),
                    FaultOp::Skew(s) if s.abs() > 3600 => FaultOp::Skew(3600 * s.signum()),
                    _ => continue,
                };
                *fault = simpler;
                progress |= try_one(c, &mut best, &mut tries);
            }
        }
        if !progress || tries >= budget {
            return best;
        }
    }
}

fn mentions_device(op: &Op, device: &str) -> bool {
    match op {
        Op::Person {
            side: Side::Device(d),
            ..
        } => d == device,
        Op::Fault { device: d, .. } | Op::Confirm { device: d } => d == device,
        _ => false,
    }
}

fn act_paths(act: &Act) -> Vec<&String> {
    match act {
        Act::Create { path, .. }
        | Act::Edit { path, .. }
        | Act::Mkdir { path }
        | Act::Delete { path }
        | Act::Retype { path, .. } => {
            vec![path]
        }
        Act::Rename { from, to } | Act::Copy { from, to } => vec![from, to],
    }
}

/// Every distinct single name used in the scenario's paths, in order of
/// first use.
fn names_in(s: &Scenario) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for op in &s.ops {
        if let Op::Person { act, .. } = op {
            for p in act_paths(act) {
                for n in p.split('/') {
                    if !out.iter().any(|x| x == n) {
                        out.push(n.to_string());
                    }
                }
            }
        }
    }
    out
}

/// The scenario with the name `from` replaced by `to` in every path.
fn rename_everywhere(s: &Scenario, from: &str, to: &str) -> Scenario {
    let fix = |p: &mut String| {
        *p = p
            .split('/')
            .map(|n| if n == from { to } else { n })
            .collect::<Vec<_>>()
            .join("/");
    };
    let mut c = s.clone();
    for op in &mut c.ops {
        match op {
            Op::Person { act, .. } => match act {
                Act::Create { path, .. }
                | Act::Edit { path, .. }
                | Act::Mkdir { path }
                | Act::Delete { path }
                | Act::Retype { path, .. } => fix(path),
                Act::Rename { from, to } | Act::Copy { from, to } => {
                    fix(from);
                    fix(to);
                }
            },
            Op::Fault {
                fault: FaultOp::Deny(p),
                ..
            } => fix(p),
            Op::Admin(AdminOp::Refuse { path, .. }) => fix(path),
            _ => {}
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::{GenConfig, generate};

    /// A failure that needs exactly one op: shrinking finds a scenario
    /// with that op and the final settle.
    #[test]
    fn shrinks_to_the_op_that_matters() {
        let big = generate(5, &GenConfig::small());
        let needle = Op::Admin(AdminOp::EmptyTrash);
        let mut with = big.clone();
        with.ops.insert(with.ops.len() / 2, needle.clone());
        let small = shrink(&with, |s| s.ops.contains(&needle), 10_000);
        assert_eq!(small.ops, vec![needle, Op::Settle]);
        assert_eq!(small.devices.len(), 1);
    }
}
