//! L0c deterministic replay (`replay.start`, `docs/designs/trust-substrate.md` §3
//! L0c) — the host-side handler that re-drives a captured [`Run`](nerve_core::provenance::Run)'s
//! event tape, re-deriving its content-addressed spine step-by-step, and emits a
//! recorded-vs-replayed [`ReplayManifest`](nerve_core::provenance::ReplayManifest)
//! verdict. This is the "bit-for-bit replayable" proof the trust substrate rests on:
//! a verifier re-folds the recorded events and confirms the head hash matches.
//!
//! The replay itself is pure — [`nerve_core::build_ledger`] re-folds the spine and
//! [`nerve_core::runpin::verify_replay`] produces the verdict (INV-R2). This module
//! only orchestrates: it loads the [`Run`] from the [`RunStore`], streams a
//! [`RuntimeEvent::ReplayProgress`] per re-derived step (so a cockpit can render the
//! re-verification live), and announces the verdict via
//! [`RuntimeEvent::ReplayFinished`].
//!
//! **`matched == false` is a verdict, not an error.** A divergence (the recorded
//! tape does not re-derive to the recorded `root_hash`) is a real, recorded outcome
//! returned as `Ok({"manifest": ...})`. Only an *unknown* run id or the absence of a
//! served root (no store) is an adapter error — mirroring `run.get`
//! ([`crate::run_store::run_run_get`]).

use crate::run_store::RunStore;
use nerve_core::CancelToken;
use nerve_runtime::{RuntimeError, RuntimeEvent};
use serde_json::{Value, json};

/// Re-drive a captured run's tape and verify its content-addressed spine.
///
/// Loads the [`Run`] named `run_id` from `store`, emits a job-scoped
/// [`RuntimeEvent::ReplayProgress`] for every re-derived spine entry (carrying the
/// re-folded `chained_hash`), then emits [`RuntimeEvent::ReplayFinished`] with the
/// verdict and returns `{"manifest": <ReplayManifest>}`. A `matched == false`
/// manifest is returned as `Ok` (a recorded divergence verdict). An unknown id or a
/// `None` store (no served root) is an [`RuntimeError::adapter`] error.
pub(crate) fn handle_replay_start(
    run_id: &str,
    job_id: &str,
    store: Option<&RunStore>,
    emit: &dyn Fn(RuntimeEvent),
    token: &CancelToken,
) -> Result<Value, RuntimeError> {
    let store =
        store.ok_or_else(|| RuntimeError::adapter(format!("no captured run `{run_id}`")))?;
    let run = store
        .load_record(run_id)
        .map_err(|err| RuntimeError::adapter(format!("no captured run `{run_id}`: {err}")))?;

    // Re-fold the spine purely from the recorded events. Each re-derived entry is
    // streamed as job-scoped progress so a cockpit can render the re-verification
    // step-by-step; the head of this chain is the replayed root hash.
    let (ledger, _replayed_root) = nerve_core::build_ledger(&run.events);
    for entry in &ledger {
        if token.is_cancelled() {
            return Err(RuntimeError::adapter(format!(
                "replay of `{run_id}` cancelled"
            )));
        }
        emit(RuntimeEvent::ReplayProgress {
            job_id: job_id.to_string(),
            seq: entry.seq,
            chained_hash: entry.chained_hash.clone(),
        });
    }

    // The pure verdict: recorded vs. re-derived head, matched, divergence point.
    let manifest = nerve_core::runpin::verify_replay(&run);
    let manifest_json = serde_json::to_value(&manifest).map_err(|err| {
        RuntimeError::adapter(format!(
            "failed to render replay manifest for `{run_id}`: {err}"
        ))
    })?;

    emit(RuntimeEvent::ReplayFinished {
        job_id: job_id.to_string(),
        manifest,
    });
    Ok(json!({ "manifest": manifest_json }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::provenance::{Event, EventKind, RunInputs};
    use std::cell::RefCell;
    use std::path::Path;
    use tempfile::tempdir;

    fn sample_run_id(store: &RunStore) -> String {
        let run = nerve_core::build_run(
            "job-1",
            "codex",
            Some("/repo".into()),
            10,
            Some(20),
            true,
            vec![
                Event {
                    seq: 0,
                    kind: EventKind::RunStarted {
                        agent: "codex".into(),
                        task: "do a thing".into(),
                        cwd: Some("/repo".into()),
                        inputs: None,
                    },
                },
                Event {
                    seq: 1,
                    kind: EventKind::Output {
                        turn: 0,
                        text: "working".into(),
                    },
                },
                Event {
                    seq: 2,
                    kind: EventKind::RunFinished {
                        ok: true,
                        exit_code: Some(0),
                        timed_out: false,
                    },
                },
            ],
            RunInputs::default(),
        );
        store.write_record(&run).unwrap();
        run.run_id
    }

    fn collector() -> (RefCell<Vec<RuntimeEvent>>,) {
        (RefCell::new(Vec::new()),)
    }

    #[test]
    fn replay_matches_recorded_run() {
        let dir = tempdir().unwrap();
        let store = RunStore::new(dir.path().join("runs"));
        let run_id = sample_run_id(&store);

        let (events,) = collector();
        let emit = |e: RuntimeEvent| events.borrow_mut().push(e);
        let out = handle_replay_start(&run_id, "job-1", Some(&store), &emit, &CancelToken::never())
            .expect("replay of a captured run succeeds");

        // Verdict: matched, against the recorded root hash, over every event.
        assert_eq!(out["manifest"]["matched"], json!(true));
        assert_eq!(out["manifest"]["run_id"], json!(run_id));
        assert_eq!(out["manifest"]["event_count"], json!(3));
        assert_eq!(
            out["manifest"]["recorded_root_hash"],
            out["manifest"]["replayed_root_hash"]
        );

        // One ReplayProgress per event, then exactly one ReplayFinished, all
        // job-scoped to "job-1".
        let evts = events.borrow();
        let progress = evts
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::ReplayProgress { .. }))
            .count();
        let finished = evts
            .iter()
            .filter(|e| matches!(e, RuntimeEvent::ReplayFinished { .. }))
            .count();
        assert_eq!(progress, 3);
        assert_eq!(finished, 1);
        // Replay events carry their `job_id` and are broadcast fleet-wide (the
        // landed contract routes them through the `None`/broadcast arm, like
        // `RunRecorded`), so a flight-recorder dashboard sees every step.
        for e in evts.iter() {
            assert_eq!(e.session_id(), None, "replay events broadcast fleet-wide");
            match e {
                RuntimeEvent::ReplayProgress { job_id, .. }
                | RuntimeEvent::ReplayFinished { job_id, .. } => assert_eq!(job_id, "job-1"),
                other => panic!("unexpected event {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_id_and_no_store_are_errors() {
        let dir = tempdir().unwrap();
        let store = RunStore::new(dir.path().join("runs"));
        let (events,) = collector();
        let emit = |e: RuntimeEvent| events.borrow_mut().push(e);

        assert!(
            handle_replay_start("nope", "j", Some(&store), &emit, &CancelToken::never()).is_err(),
            "unknown id is an adapter error"
        );
        assert!(
            handle_replay_start("anything", "j", None, &emit, &CancelToken::never()).is_err(),
            "no served root (None store) is an adapter error"
        );
        assert!(
            events.borrow().is_empty(),
            "no events on a not-found replay"
        );
    }

    #[test]
    fn cancellation_aborts_the_replay() {
        let dir = tempdir().unwrap();
        let store = RunStore::new(dir.path().join("runs"));
        let run_id = sample_run_id(&store);

        let token = CancelToken::never();
        token.cancel();
        let (events,) = collector();
        let emit = |e: RuntimeEvent| events.borrow_mut().push(e);
        assert!(
            handle_replay_start(&run_id, "j", Some(&store), &emit, &token).is_err(),
            "a cancelled token aborts the replay"
        );
        assert!(
            !events
                .borrow()
                .iter()
                .any(|e| matches!(e, RuntimeEvent::ReplayFinished { .. })),
            "no finished event after cancellation"
        );
    }

    #[test]
    fn for_scope_resolves_under_project_root() {
        // Sanity: replay reads the same store run.get/list does.
        let store = RunStore::for_scope(Some(Path::new("/tmp/proj"))).unwrap();
        assert!(store.dir().ends_with(".nerve/runs"));
    }
}
