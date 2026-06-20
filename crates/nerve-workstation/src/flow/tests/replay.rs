//! REPLAY (byte-identical) + CONTRACT (declared-order fold) tests.
//!
//! These reuse the scripted-worker harness in the parent [`super`] module and
//! pin the two load-bearing determinism properties (design §3): a recorded run
//! re-emits byte-identically under replay, and the fold is a function of declared
//! order, never completion order.

use super::{
    NeverApprover, ReplayResolver, Script, def, ok, parallel_out_of_order, prompt_to_node, record,
    render_outcome,
};
use crate::delegate_proxy::DelegateApprover;
use crate::flow::Driver;
use crate::worker::WorkerLedger;
use nerve_core::CancelToken;
use nerve_runtime::{Join, Strategy};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

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
                },
            ),
            (
                "second".to_string(),
                Script {
                    result: ok("R2"),
                    delay: Duration::from_millis(db),
                },
            ),
            (
                "third".to_string(),
                Script {
                    result: ok("R3"),
                    delay: Duration::from_millis(dc),
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
fn replay_is_byte_identical_to_the_recorded_run() {
    // RECORD a parallel run (with out-of-order completion), then REPLAY from the
    // recorded ledger and assert: (a) the engine's outcome is identical, and
    // (b) the replayed tape is byte-identical to the recorded tape (the audit
    // moat — design §3, the byte-identical replay gate that C4 promotes to CI).
    let (workflow, scripts) = parallel_out_of_order(Join::All);
    let (recorded_outcome, recorded_ledger) = record(&workflow, scripts);
    let recorded_jsonl = recorded_ledger.to_jsonl();
    let recorded_tape = Arc::new(recorded_ledger.snapshot());

    let map = Arc::new(prompt_to_node(&workflow, &recorded_tape));
    let resolver = ReplayResolver {
        recorded: Arc::clone(&recorded_tape),
        prompt_to_node: Arc::clone(&map),
    };
    let replay_ledger = Arc::new(WorkerLedger::new());
    let approver: Arc<dyn DelegateApprover> = Arc::new(NeverApprover);
    let driver =
        Driver::new(&resolver, Arc::clone(&replay_ledger), approver, None).with_concurrency(8);
    let replay_outcome = driver.run(&workflow, &CancelToken::never());

    // (a) identical engine output.
    assert_eq!(
        render_outcome(&replay_outcome),
        render_outcome(&recorded_outcome),
        "replay must reproduce the recorded outcome exactly"
    );
    // (b) byte-identical tape (the audit gate).
    assert_eq!(
        replay_ledger.to_jsonl(),
        recorded_jsonl,
        "replayed ledger must be byte-identical to the recorded ledger"
    );
}

#[test]
fn replay_reconstructed_from_jsonl_matches() {
    // The ledger reconstructed from its own JSONL replays identically — proving
    // the on-disk record (job 3) is a faithful resume source (design §5).
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
        },
    )]);
    let (_, ledger) = record(&workflow, scripts);
    let jsonl = ledger.to_jsonl();
    let restored = WorkerLedger::from_jsonl(&jsonl).expect("reconstruct");
    assert_eq!(restored.to_jsonl(), jsonl);
    assert_eq!(restored.output("node-0"), Some("done".to_string()));
}
