//! REPLAY (byte-identical) + CONTRACT (declared-order fold) tests.
//!
//! These reuse the scripted-worker harness in the parent [`super`] module and
//! pin the two load-bearing determinism properties (design §3): a recorded run
//! re-emits byte-identically under replay, and the fold is a function of declared
//! order, never completion order.

use super::{NeverApprover, Script, def, ok, parallel_out_of_order, record, render_outcome};
use crate::delegate_proxy::DelegateApprover;
use crate::flow::{Driver, ReplayResolver, replay_generation_provider};
use crate::worker::WorkerLedger;
use nerve_core::CancelToken;
use nerve_runtime::{Join, Strategy};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Replay a recorded ledger through the PRODUCTION [`ReplayResolver`] + recorded-
/// generation provider, returning the replayed outcome + the replayed ledger's
/// JSONL. The shared helper behind the byte-identical gate (design §3): the same
/// engine, the same def, the self-contained recorded tape.
fn replay(
    workflow: &nerve_runtime::WorkflowDef,
    recorded: &WorkerLedger,
    concurrency: usize,
) -> (crate::flow::FlowOutcome, String) {
    let resolver = ReplayResolver::from_ledger(recorded);
    let generation = replay_generation_provider(recorded);
    let replay_ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver = Driver::new(&resolver, Arc::clone(&replay_ledger), approver, None)
        .with_concurrency(concurrency)
        .with_generation(&generation);
    let outcome = driver.run(workflow, &CancelToken::never());
    let jsonl = replay_ledger.to_jsonl();
    (outcome, jsonl)
}

// ---- CONTRACT: declared-order fold (the load-bearing invariant) ---------------

#[test]
fn contract_declared_order_fold_is_independent_of_completion_order() {
    // Run the SAME def twice with INVERTED delays (so completion order flips), and
    // assert the folded outcome is byte-identical both times — the determinism
    // contract (design §3): orchestration depends on declared order, never on
    // which worker finished first.
    let workflow = def(
        "contract",
        Strategy::Parallel {
            branches: vec![
                super::cli_step("first"),
                super::cli_step("second"),
                super::cli_step("third"),
            ],
            join: Join::All,
        },
    );
    let make = |da: u64, db: u64, dc: u64| {
        BTreeMap::from([
            (
                "first".to_string(),
                Script {
                    result: ok("R1"),
                    delay: Duration::from_millis(da),
                    steerable: false,
                },
            ),
            (
                "second".to_string(),
                Script {
                    result: ok("R2"),
                    delay: Duration::from_millis(db),
                    steerable: false,
                },
            ),
            (
                "third".to_string(),
                Script {
                    result: ok("R3"),
                    delay: Duration::from_millis(dc),
                    steerable: false,
                },
            ),
        ])
    };
    let (forward, _) = record(&workflow, make(0, 30, 60));
    let (inverted, _) = record(&workflow, make(60, 30, 0));
    assert_eq!(
        render_outcome(&forward),
        render_outcome(&inverted),
        "completion order must not change the folded outcome"
    );
    assert_eq!(
        forward
            .results
            .iter()
            .map(|r| r.text.clone())
            .collect::<Vec<_>>(),
        vec!["R1", "R2", "R3"]
    );
}

// ---- REPLAY: byte-identical re-emission ----------------------------------------

#[test]
fn replay_is_byte_identical_parallel_out_of_order() {
    // THE GATE (design §3): RECORD a `Parallel` run whose branches finish OUT OF
    // ORDER, then REPLAY through the production resolver and assert the replayed
    // Flow* tape is BYTE-IDENTICAL to the recorded one (and the outcome matches).
    // Out-of-order completion is the load-bearing case: the declared-order fold must
    // make the replay identical regardless of which branch finished first.
    let (workflow, scripts) = parallel_out_of_order(Join::All);
    let (recorded_outcome, recorded_ledger) = record(&workflow, scripts);
    let recorded_jsonl = recorded_ledger.to_jsonl();

    let (replay_outcome, replay_jsonl) = replay(&workflow, &recorded_ledger, 8);

    assert_eq!(
        render_outcome(&replay_outcome),
        render_outcome(&recorded_outcome),
        "replay must reproduce the recorded outcome exactly"
    );
    assert_eq!(
        replay_jsonl, recorded_jsonl,
        "replayed ledger must be byte-identical to the recorded ledger (the audit gate)"
    );
}

#[test]
fn replay_is_byte_identical_single() {
    // The simplest gate case + a reconstruct-from-JSONL round-trip, proving the
    // on-disk record (FlowStore's ledger.jsonl) is a faithful replay source (§5).
    let workflow = def(
        "single",
        Strategy::Single {
            step: super::cli_step("only"),
        },
    );
    let scripts = BTreeMap::from([(
        "only".to_string(),
        Script {
            result: ok("done"),
            delay: Duration::ZERO,
            steerable: false,
        },
    )]);
    let (recorded_outcome, recorded_ledger) = record(&workflow, scripts);
    let recorded_jsonl = recorded_ledger.to_jsonl();

    // Replay directly...
    let (replay_outcome, replay_jsonl) = replay(&workflow, &recorded_ledger, 1);
    assert_eq!(
        render_outcome(&replay_outcome),
        render_outcome(&recorded_outcome)
    );
    assert_eq!(replay_jsonl, recorded_jsonl);

    // ...and from the ledger RECONSTRUCTED from its own JSONL (the resume source).
    let restored = WorkerLedger::from_jsonl(&recorded_jsonl).expect("reconstruct from jsonl");
    assert_eq!(restored.to_jsonl(), recorded_jsonl);
    assert_eq!(restored.output("node-0"), Some("done".to_string()));
    let (_, from_restored_jsonl) = replay(&workflow, &restored, 1);
    assert_eq!(
        from_restored_jsonl, recorded_jsonl,
        "replay from a reconstructed ledger is byte-identical too"
    );
}
