//! Golden lock on the L0 run-capture canonicalization + content-addressing
//! (`docs/designs/trust-substrate.md` §3/§5, INV-R2). The input tape is hardcoded
//! with fixed logical `seq`s and fixed host timestamps — no wall-clock, no
//! randomness — so the snapshotted `root_hash` / `ledger` / `events` are a pure
//! function of the algorithm. Any change that perturbs the digest (a serde-version
//! bump, a field reorder, a canonicalization change) flips this snapshot and fails
//! CI: the content address that portable receipts (L4) will sign cannot drift
//! silently.

use nerve_core::provenance::{Event, EventKind, build_run};
use serde_json::json;

fn ev(seq: u64, kind: EventKind) -> Event {
    Event { seq, kind }
}

#[test]
fn golden_run_hash() {
    let events = vec![
        ev(
            0,
            EventKind::RunStarted {
                agent: "codex".into(),
                task: "add a regression test for the parser".into(),
                cwd: Some("/repo".into()),
                // None -> skip_serialized, so the locked content address is unchanged
                // by the L0c additive field (RISK #8 regression lock).
                inputs: None,
            },
        ),
        ev(1, EventKind::TurnStarted { turn: 0 }),
        ev(
            2,
            EventKind::Output {
                turn: 0,
                text: "reading src/parser.rs".into(),
            },
        ),
        ev(
            3,
            EventKind::Output {
                turn: 0,
                text: "applied edit to tests/parser.rs".into(),
            },
        ),
        ev(
            4,
            EventKind::UsageUpdated {
                turn: 0,
                input_tokens: 1200,
                output_tokens: 340,
                cache_read_tokens: 800,
                cache_creation_tokens: 0,
                cost_micro_usd: Some(4200),
            },
        ),
        ev(5, EventKind::TurnFinished { turn: 0, ok: true }),
        ev(
            6,
            EventKind::RunFinished {
                ok: true,
                exit_code: Some(0),
                timed_out: false,
            },
        ),
    ];

    let run = build_run(
        "delegate-session-fixed",
        "codex",
        Some("/repo".into()),
        1_000,
        Some(2_000),
        true,
        events,
        nerve_core::provenance::RunInputs::default(),
    );

    // run_id IS the content address of the tape (== root_hash) at L0.
    assert_eq!(run.run_id, run.root_hash);

    insta::assert_json_snapshot!(json!({
        "run_id": run.run_id,
        "root_hash": run.root_hash,
        "ledger": run.ledger,
        "events": run.events,
    }));
}

/// THE additive-invariance lock (Wave 2): a tape of ONLY the pre-existing variants
/// (RunStarted/TurnStarted/Output/UsageUpdated/TurnFinished/RunFinished) must
/// content-address to the EXACT same hex it produced before the `tool_started` /
/// `tool_finished` `EventKind`s were appended. The literal below was computed from
/// the code at this commit and pasted verbatim — so a future change that perturbs
/// the canonical bytes of any old variant (a field reorder, a serde-tag change, a
/// new field that isn't `skip`ped) flips this and fails CI loudly, proving every
/// already-recorded `run_id` is unperturbed (INV-R2: the content address can't drift).
#[test]
fn additive_invariance_pre_existing_tape_root_hash_is_locked() {
    let events = vec![
        ev(
            0,
            EventKind::RunStarted {
                agent: "claude".into(),
                task: "fix the flaky test".into(),
                cwd: Some("/repo".into()),
                inputs: None,
            },
        ),
        ev(1, EventKind::TurnStarted { turn: 0 }),
        ev(
            2,
            EventKind::Output {
                turn: 0,
                text: "editing src/lib.rs".into(),
            },
        ),
        ev(
            3,
            EventKind::UsageUpdated {
                turn: 0,
                input_tokens: 500,
                output_tokens: 120,
                cache_read_tokens: 64,
                cache_creation_tokens: 0,
                cost_micro_usd: Some(1500),
            },
        ),
        ev(4, EventKind::TurnFinished { turn: 0, ok: true }),
        ev(
            5,
            EventKind::RunFinished {
                ok: true,
                exit_code: Some(0),
                timed_out: false,
            },
        ),
    ];
    let run = build_run(
        "fixed-session",
        "claude",
        Some("/repo".into()),
        1_000,
        Some(2_000),
        true,
        events,
        nerve_core::provenance::RunInputs::default(),
    );
    assert_eq!(
        run.root_hash, "5d1bffb6c386eea10b5b65bdc5bb794e56325db018e59ceb099b6ccb1904a4af",
        "appending tool-lifecycle EventKinds must NOT perturb a tape that uses none \
         of them — every existing run_id stays byte-stable"
    );
}
