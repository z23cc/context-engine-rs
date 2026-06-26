//! The L6 **outcome-corpus ingestion rail** (`docs/designs/trust-substrate.md`
//! §3 L6, INV-R1/R3/R4) — the offline, daemon-free twin of the daemon's
//! `outcome.label` command, symmetric with `nerve ledger verify`/`query` and
//! `nerve verify`/`gate`. A post-merge CI hook (or a human) records what the
//! org **actually observed happen** to a captured change — was it `merged`,
//! later `reverted`, implicated in an `incident`, or `shipped-no-regress` — by
//! constructing the outcome + ledger stores straight from `--root` (no daemon)
//! and appending the REAL outcome to the run's append-only corpus.
//!
//! **Honest ingestion, not invention (INV-R1).** This records what the CALLER
//! asserts happened: a post-merge hook asserts `merged` because the platform
//! merged the PR; a revert pipeline asserts `reverted`. Nerve never derives an
//! outcome from a verify verdict — an [`Outcome`] is a post-merge real-world
//! disposition, NOT a verify pass/fail. The corpus is **advisory and
//! non-load-bearing**: it is the data calibration (deferred) reads, never an
//! input to a verdict, gate, or receipt (INV-R1/R3/R4).
//!
//! Each recorded outcome is also mirrored onto the L1 cross-run evidence ledger
//! as a [`LedgerKind::OutcomeRecorded`] — the same seam the daemon's
//! `record_outcome_on_ledger` uses ([`crate::ledger_store::append_evidence`]) —
//! so the observation joins the tamper-evident chain (`trust-substrate.md` §8).
//! The chaining/hashing it calls is pure in `nerve-core`; only the read + append
//! + print live here above the determinism boundary (INV-R2).

use crate::ledger_store::LedgerStore;
use crate::outcome_store::{LabeledOutcome, OutcomeStore, handle_outcome_label};
use anyhow::{Context, Result, anyhow};
use clap::{Args, ValueEnum};
use nerve_core::ledger::{LedgerKind, LedgerRecord};
use nerve_core::outcome::{LabelSource, Outcome};
use serde_json::{Value, json};
use std::path::PathBuf;

/// The REAL post-merge disposition the caller observed (`trust-substrate.md` §3 L6).
/// These are **real-world** outcomes — NOT a verify pass/fail — so a verdict is never
/// laundered into one (INV-R1).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Disposition {
    /// The change landed on the target branch (e.g. a post-merge CI hook).
    Merged,
    /// A previously merged change was rolled back.
    Reverted,
    /// The change was implicated in a production incident.
    Incident,
    /// The change shipped and produced no observed regression.
    #[value(name = "shipped-no-regress")]
    ShippedNoRegress,
}

impl Disposition {
    /// Map the CLI subcommand onto the protocol [`Outcome`] vocabulary.
    fn to_outcome(self) -> Outcome {
        match self {
            Self::Merged => Outcome::Merged,
            Self::Reverted => Outcome::Reverted,
            Self::Incident => Outcome::Incident,
            Self::ShippedNoRegress => Outcome::ShippedNoRegress,
        }
    }

    /// The serde tag for human/JSON output (`merged` / `reverted` / …).
    fn tag(self) -> &'static str {
        match self {
            Self::Merged => "merged",
            Self::Reverted => "reverted",
            Self::Incident => "incident",
            Self::ShippedNoRegress => "shipped_no_regress",
        }
    }
}

/// Who witnessed the outcome — the **provenance of the label**, distinct from the
/// disposition it asserts. Defaults to `ci` (the headline post-merge-hook use).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum WitnessSource {
    /// A person recorded the disposition.
    Human,
    /// A CI signal recorded it (a merge / revert pipeline).
    Ci,
    /// Passive telemetry / monitoring inferred it (an incident alert).
    Observation,
}

impl WitnessSource {
    fn to_source(self) -> LabelSource {
        match self {
            Self::Human => LabelSource::Human,
            Self::Ci => LabelSource::Ci,
            Self::Observation => LabelSource::Observation,
        }
    }
}

/// `nerve outcome <merged|reverted|incident|shipped-no-regress> --run <id>` — record a
/// REAL post-merge disposition into a captured run's L6 outcome corpus and mirror it onto
/// the L1 evidence ledger. Daemon-free (builds the stores from `--root`), exit 0 on
/// success. **Honesty:** Nerve records what the CALLER asserts happened (the platform
/// merged it, a pipeline reverted it) — it never invents an outcome from a verify verdict,
/// and the corpus is advisory, never a gate (INV-R1/R3/R4).
#[derive(Debug, Args)]
pub(crate) struct OutcomeArgs {
    /// The REAL disposition observed: `merged`, `reverted`, `incident`, or
    /// `shipped-no-regress`. A post-merge real-world outcome — never a verify verdict.
    #[arg(value_enum)]
    disposition: Disposition,
    /// The captured run id (its content address) this outcome is about.
    #[arg(long = "run")]
    run: String,
    /// Optional receipt id this outcome relates to (denormalized onto the corpus for
    /// query); recorded when first labelling the run.
    #[arg(long = "receipt")]
    receipt: Option<String>,
    /// Optional originating session id (denormalized onto the corpus for query);
    /// recorded when first labelling the run.
    #[arg(long = "session")]
    session: Option<String>,
    /// Who witnessed the outcome: `human`, `ci` (default), or `observation`.
    #[arg(long = "source", value_enum, default_value = "ci")]
    source: WitnessSource,
    /// Optional free-text note attached to the label.
    #[arg(long = "note")]
    note: Option<String>,
    /// Workspace root holding `.nerve/` (defaults to the current directory).
    #[arg(long = "root")]
    root: Option<PathBuf>,
    /// Emit the appended label + ledger ref as JSON instead of the human line.
    #[arg(long = "json")]
    json: bool,
}

/// Record the asserted outcome: append the label to `<root>/.nerve/outcomes/<run>.json`
/// via [`handle_outcome_label`] and mirror it onto `<root>/.nerve/ledger` as an
/// [`LedgerKind::OutcomeRecorded`]. Returns the process exit code (`0` on success). The
/// label write is the load-bearing step; the ledger mirror is best-effort (a persistence
/// failure warns but never fails the recording — INV-R1/R3, mirroring the daemon).
pub(crate) fn record(args: OutcomeArgs) -> Result<i32> {
    let root = resolve_root(args.root.clone())?;
    let outcome = args.disposition.to_outcome();
    let source = args.source.to_source();
    let store = OutcomeStore::for_scope(Some(&root)).context("open outcome store")?;
    let ledger = LedgerStore::for_scope(Some(&root)).context("open evidence ledger")?;

    // Denormalize the asserted receipt/session identity onto a fresh corpus so it is
    // queryable; first-writer-wins, never clobbering an existing run's record.
    seed_identity(
        &store,
        &args.run,
        args.session.clone(),
        args.receipt.clone(),
    )?;

    let labeled = handle_outcome_label(
        &args.run,
        outcome,
        source,
        None, // actor — the CLI records source-of-witness, not a per-user actor
        args.note.clone(),
        None, // verdict_ref — an outcome is not a verdict reference
        Some(&store),
    )
    .map_err(|err| anyhow!("failed to record outcome for run `{}`: {err}", args.run))?;

    // L6→L1: mirror the observation onto the tamper-evident ledger (the same seam the
    // daemon's `record_outcome_on_ledger` uses). Best-effort: an OBSERVATION, never a
    // verdict input (INV-R1/R3/R4).
    let kind = LedgerKind::OutcomeRecorded {
        run_id: labeled.run_id.clone(),
        outcome,
        source,
        label_hash: labeled.label_hash.clone(),
    };
    let ledger_record = crate::ledger_store::append_evidence(Some(&ledger), kind);
    report(&args, &labeled, ledger_record.as_ref());
    Ok(0)
}

/// Stamp the asserted `session`/`receipt` identity onto a NOT-YET-EXISTING corpus so the
/// first label carries them (the `OutcomeRecord` denormalizes them for query). A no-op
/// when neither is supplied, or when a corpus already exists (first-writer-wins — a later
/// label never rewrites the run's denormalized identity).
fn seed_identity(
    store: &OutcomeStore,
    run: &str,
    session: Option<String>,
    receipt: Option<String>,
) -> Result<()> {
    if session.is_none() && receipt.is_none() {
        return Ok(());
    }
    if store.load_record(run).is_ok() {
        return Ok(());
    }
    let mut record = nerve_core::outcome::empty_record(run, session, None);
    record.receipt_id = receipt;
    store
        .write_record(&record)
        .with_context(|| format!("seed outcome corpus for run `{run}`"))
}

/// Print the recorded outcome: the appended label + ledger ref as JSON (`--json`), else a
/// single human line. The appended label is the tail of the corpus the label write sealed.
fn report(args: &OutcomeArgs, labeled: &LabeledOutcome, ledger: Option<&LedgerRecord>) {
    let label = labeled
        .payload
        .pointer("/record/labels")
        .and_then(Value::as_array)
        .and_then(|labels| labels.last())
        .cloned()
        .unwrap_or(Value::Null);
    let ledger_ref =
        ledger.map(|record| json!({ "seq": record.seq, "record_hash": record.record_hash }));
    if args.json {
        println!(
            "{}",
            json!({
                "run_id": labeled.run_id,
                "outcome": args.disposition.tag(),
                "label": label,
                "labels_root": labeled.labels_root,
                "label_count": labeled.label_count,
                "ledger": ledger_ref,
            })
        );
        return;
    }
    let tail = match ledger {
        Some(record) => format!("ledger seq {} ({})", record.seq, short(&record.record_hash)),
        None => "ledger append skipped (best-effort)".to_string(),
    };
    println!(
        "recorded outcome `{}` for run `{}` ({} label(s), root {}); {tail}",
        args.disposition.tag(),
        labeled.run_id,
        labeled.label_count,
        short(&labeled.labels_root),
    );
}

/// A short prefix of a content hash for the human one-liner (`-` for an empty hash).
fn short(hash: &str) -> String {
    if hash.is_empty() {
        "-".to_string()
    } else {
        hash.chars().take(12).collect()
    }
}

/// Resolve the workspace root (defaults to the current directory; mirrors `ledger.rs` /
/// `gate.rs`).
fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root),
        None => std::env::current_dir().context("failed to resolve current directory"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::ledger::verify_chain;
    use tempfile::tempdir;

    fn args(disposition: Disposition, run: &str, root: &std::path::Path) -> OutcomeArgs {
        OutcomeArgs {
            disposition,
            run: run.to_string(),
            receipt: None,
            session: None,
            source: WitnessSource::Ci,
            note: None,
            root: Some(root.to_path_buf()),
            json: false,
        }
    }

    #[test]
    fn record_appends_real_label_and_keeps_the_ledger_intact() {
        let dir = tempdir().unwrap();

        // A post-merge hook asserts the REAL outcome (merged) for a captured run.
        let code = record(args(Disposition::Merged, "run-merge", dir.path())).unwrap();
        assert_eq!(code, 0);

        // The outcome store reads back the appended Merged label (round-trip).
        let store = OutcomeStore::for_scope(Some(dir.path())).unwrap();
        let record = store.load_record("run-merge").unwrap();
        assert_eq!(record.labels.len(), 1);
        assert_eq!(record.labels[0].outcome, Outcome::Merged);
        assert_eq!(record.labels[0].source, LabelSource::Ci);
        assert!(!record.labels_root.is_empty());

        // The L1 evidence ledger carries an OutcomeRecorded fact AND its chain re-derives
        // intact (what `nerve ledger verify` checks).
        let ledger = LedgerStore::for_scope(Some(dir.path())).unwrap();
        let records = ledger.read_all();
        assert_eq!(records.len(), 1);
        assert!(matches!(
            records[0].kind,
            LedgerKind::OutcomeRecorded {
                outcome: Outcome::Merged,
                ..
            }
        ));
        // The ledger fact's label_hash equals the appended label's content digest.
        if let LedgerKind::OutcomeRecorded { label_hash, .. } = &records[0].kind {
            assert_eq!(label_hash, &record.labels[0].label_hash);
        }
        assert!(
            verify_chain(&records).is_ok(),
            "ledger chain must be intact"
        );
    }

    #[test]
    fn second_outcome_appends_and_advances_the_chain() {
        let dir = tempdir().unwrap();
        record(args(Disposition::Merged, "run-x", dir.path())).unwrap();
        // Later, the same run is reverted — the corpus accrues a second REAL label.
        record(args(Disposition::Reverted, "run-x", dir.path())).unwrap();

        let store = OutcomeStore::for_scope(Some(dir.path())).unwrap();
        let record = store.load_record("run-x").unwrap();
        assert_eq!(record.labels.len(), 2);
        assert_eq!(record.labels[1].outcome, Outcome::Reverted);

        // Two ledger facts, chain still intact.
        let ledger = LedgerStore::for_scope(Some(dir.path())).unwrap();
        let records = ledger.read_all();
        assert_eq!(records.len(), 2);
        assert!(verify_chain(&records).is_ok());
    }

    #[test]
    fn receipt_and_session_are_denormalized_onto_a_fresh_corpus() {
        let dir = tempdir().unwrap();
        let mut a = args(Disposition::ShippedNoRegress, "run-id", dir.path());
        a.receipt = Some("rcpt-7".into());
        a.session = Some("job-3".into());
        a.source = WitnessSource::Observation;
        record(a).unwrap();

        let store = OutcomeStore::for_scope(Some(dir.path())).unwrap();
        let record = store.load_record("run-id").unwrap();
        assert_eq!(record.receipt_id.as_deref(), Some("rcpt-7"));
        assert_eq!(record.session_id.as_deref(), Some("job-3"));
        assert_eq!(record.labels.len(), 1);
        assert_eq!(record.labels[0].source, LabelSource::Observation);
    }

    #[test]
    fn disposition_maps_to_the_real_outcome_vocabulary() {
        assert_eq!(Disposition::Merged.to_outcome(), Outcome::Merged);
        assert_eq!(Disposition::Reverted.to_outcome(), Outcome::Reverted);
        assert_eq!(Disposition::Incident.to_outcome(), Outcome::Incident);
        assert_eq!(
            Disposition::ShippedNoRegress.to_outcome(),
            Outcome::ShippedNoRegress
        );
    }
}
