//! The proof that the checkers bite: each deliberately broken sync of
//! tests/common must be reported by the checker meant for it, on a short
//! scenario that provokes its defect, while the unbroken sync passes the
//! same scenario clean.

mod common;

use common::{Mutant, run};
use hearth_sync_model::check::{CheckConfig, Guarantee, check_i13, check_trace};
use hearth_sync_model::scenario::Scenario;

/// The scenario text with the usual header.
fn scenario(devices: &str, mounts: &str, ops: &str) -> Scenario {
    let text = format!("hearth-sync-scenario 1\nseed 0\n{devices}\n{mounts}\n---\n{ops}\n");
    Scenario::parse(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

const ONE: &str = "device \"A\" ext4 +0";

/// The unbroken sync passes `sc`; the mutant is caught by `g`.
fn bites(mutant: Mutant, g: Guarantee, sc: &Scenario) {
    let cfg = CheckConfig::plan_defaults();
    let clean = check_trace(&run(sc, Mutant::None), &cfg);
    assert!(
        clean.is_empty(),
        "the unbroken sync fails the scenario for {mutant:?}:\n{}",
        clean
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let found = check_trace(&run(sc, mutant), &cfg);
    assert!(
        found.iter().any(|v| v.guarantee == g),
        "{mutant:?} not caught by {g}; found:\n{}",
        found
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    );
    eprintln!(
        "{mutant:?} caught by {g}: {}",
        found
            .iter()
            .find(|v| v.guarantee == g)
            .map(|v| v.to_string())
            .unwrap_or_default()
    );
}

fn conflict() -> Scenario {
    scenario(
        ONE,
        "",
        "web create \"f.txt\" c1\nsettle\nfault \"A\" offline\n\"A\" edit \"f.txt\" c2\nweb edit \"f.txt\" c3\nfault \"A\" online\nsettle",
    )
}

fn no_trash() -> Scenario {
    scenario(
        ONE,
        "mount \"Team\" trash=no versions=no perms=31 at-start=yes",
        "web create \"Team/a.txt\" c1\nsettle\nweb delete \"Team/a.txt\"\nsettle",
    )
}

#[test]
fn i1_unlink_without_holding() {
    bites(Mutant::UnlinkWithoutHolding, Guarantee::I1, &no_trash());
}

#[test]
fn i1_purge_holding_early() {
    bites(Mutant::PurgeHoldingEarly, Guarantee::I1, &no_trash());
}

#[test]
fn i2_drop_conflict_copy() {
    bites(Mutant::DropConflictCopy, Guarantee::I2, &conflict());
}

#[test]
fn i3_blind_writes() {
    bites(
        Mutant::BlindWrites,
        Guarantee::I3,
        &scenario(ONE, "", "\"A\" create \"f.txt\" c1\nsettle"),
    );
}

#[test]
fn i4_partial_visible() {
    bites(
        Mutant::PartialVisible,
        Guarantee::I4,
        &scenario(ONE, "", "web create \"f.txt\" c1\nsettle"),
    );
}

#[test]
fn i5_trust_corrupt_download() {
    bites(
        Mutant::TrustCorruptDownload,
        Guarantee::I5,
        &scenario(
            ONE,
            "",
            "fault \"A\" corrupt 1\nweb create \"f.txt\" c1\nsettle",
        ),
    );
}

#[test]
fn i6_delete_on_missing_root() {
    bites(
        Mutant::DeleteOnMissingRoot,
        Guarantee::I6,
        &scenario(
            ONE,
            "",
            "web create \"f.txt\" c1\nsettle\nfault \"A\" root-vanish\nweb create \"g.txt\" c2\nfault \"A\" root-back\nsettle",
        ),
    );
}

#[test]
fn i6_epoch_read_as_retention() {
    bites(
        Mutant::EpochAsRetention,
        Guarantee::I6,
        &scenario(
            ONE,
            "",
            "web create \"f.txt\" c1\nsettle\n\"A\" create \"new.txt\" c2\nadmin restore\nsettle",
        ),
    );
}

#[test]
fn i7_sync_loop() {
    bites(
        Mutant::SyncLoop,
        Guarantee::I7,
        &scenario(ONE, "", "web create \"f.txt\" c1\nsettle"),
    );
}

#[test]
fn i8_names_not_nfc() {
    bites(
        Mutant::NoNfc,
        Guarantee::I8,
        &scenario(
            "device \"A\" hfsplus +0",
            "",
            "\"A\" create \"Caf\\u{e9}.txt\" c1\nsettle",
        ),
    );
}

#[test]
fn i9_duplicate_copy() {
    bites(Mutant::DuplicateCopy, Guarantee::I9, &conflict());
}

#[test]
fn i9_dropped_copy_loses_a_version() {
    // Dropping the copy is also caught as the losing version gone (I1).
    bites(Mutant::DropConflictCopy, Guarantee::I1, &conflict());
}

#[test]
fn i10_refused_name_touched() {
    bites(
        Mutant::SanitizeRefusedNames,
        Guarantee::I10,
        &scenario(
            ONE,
            "",
            "\"A\" create \"a\\\\b\" c1\n\"A\" create \"x.txt\" c2\nsettle",
        ),
    );
}

#[test]
fn i11_reupload_after_crash() {
    bites(
        Mutant::ReuploadAfterCrash,
        Guarantee::I11,
        &scenario(
            ONE,
            "",
            "fault \"A\" crash 1\n\"A\" create \"f.txt\" c1\nsettle",
        ),
    );
}

#[test]
fn i12_redownload_after_db_loss() {
    bites(
        Mutant::RedownloadAfterDbLoss,
        Guarantee::I12,
        &scenario(
            ONE,
            "",
            "web create \"f.txt\" c1\nsettle\nfault \"A\" db-deleted\nweb create \"g.txt\" c2\nsettle",
        ),
    );
}

#[test]
fn i13_nondeterministic_log() {
    let sc = scenario(ONE, "", "web create \"f.txt\" c1\nsettle");
    assert_eq!(
        check_i13(&run(&sc, Mutant::None).log, &run(&sc, Mutant::None).log),
        None
    );
    let a = run(&sc, Mutant::NondeterministicLog);
    let b = run(&sc, Mutant::NondeterministicLog);
    let v = check_i13(&a.log, &b.log).expect("caught");
    assert_eq!(v.guarantee, Guarantee::I13);
}

fn mass() -> Scenario {
    let mut ops = String::from("web mkdir \"m\"\n");
    for i in 0..60 {
        ops.push_str(&format!("web create \"m/f{i}.txt\" c{i}\n"));
    }
    ops.push_str("settle\n\"A\" delete \"m\"\nconfirm \"A\"\nsettle");
    scenario(ONE, "", &ops)
}

#[test]
fn mass_change_without_pause() {
    bites(Mutant::NoMassGuard, Guarantee::MassChange, &mass());
}

#[test]
fn trusting_mtimes_loses_a_version() {
    let sc = scenario(
        ONE,
        "",
        "web create \"f.txt\" c1\nsettle\nfault \"A\" offline\nfault \"A\" skew -94608000\n\"A\" edit \"f.txt\" c2\nweb edit \"f.txt\" c3\nfault \"A\" online\nsettle",
    );
    bites(Mutant::TrustMtimes, Guarantee::I1, &sc);
}
