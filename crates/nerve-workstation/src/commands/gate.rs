//! L5 merge-gate CLI (`docs/designs/trust-substrate.md` §8 L5, INV-R1) — the
//! distribution body's CI face. `nerve verify` **re-runs the org's own checks** for a
//! captured run in-process (via [`crate::commands::verify::run_verify_flow`], the same
//! L2 engine the daemon uses), seals + signs a fresh Receipt, then reports it; `nerve
//! gate` borrows a sealed [`Receipt`]'s verdict and translates it — via the pure
//! [`gate_outcome`](nerve_core::receipt_gate::gate_outcome) — into the tri-state a
//! CI/merge surface consumes: a process **exit code** (authoritative), a stable
//! **conclusion** label, and a one-line **summary**.
//!
//! **Court reporter, not judge (INV-R1).** Neither subcommand decides correctness;
//! the verdict is the receipt's already-sealed verdict (itself borrowed from the
//! org's own tests). The decision is a pure function of the receipt — emission (a
//! GitHub check run via [`GhCheckRunEmitter`], the process exit) is the only impure
//! act, and it lives here above the determinism boundary (INV-R2).
//!
//! The exit code is the source of truth: even with no merge App deployed, a CI step
//! that runs `nerve gate` and honours its exit code is a complete merge gate. The
//! [`CheckRunEmitter`] seam is the deferred-infra hook for auto-posting a check run.
//!
//! Wired into `cli.rs` as `nerve verify` / `nerve gate`; each returns its raw exit
//! code (`i32`) so the CLI arm can `std::process::exit` with it — the exit code is the
//! authoritative gate output.

use anyhow::{Context, Result, anyhow};
use clap::Args;
use nerve_core::receipt::Receipt;
use nerve_core::receipt_gate::{GateOutcome, gate_outcome};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

/// `nerve verify <run_id>` — re-verify a captured run by re-running the org's own
/// checks (`<root>/.nerve/checks.json`) in the recorded closure, sealing a borrowed
/// Verdict, and signing + persisting a fresh Verification Receipt. The exit code is the
/// gate (0=Passed, 1=Failed, 2=Inconclusive/Error); a missing `checks.json` is honestly
/// Inconclusive — never a fabricated pass (INV-R1).
#[derive(Debug, Args)]
pub(crate) struct VerifyArgs {
    /// The captured run id (its content address) to verify.
    run_id: String,
    /// Workspace root holding `.nerve/` (defaults to the current directory).
    #[arg(long = "root")]
    root: Option<PathBuf>,
    /// How many times to re-run each check to surface flakiness (default: the L2
    /// runner's own default of one extra run).
    #[arg(long = "reruns")]
    reruns: Option<u32>,
    /// Print the resolved Receipt / outcome as JSON instead of a one-line summary.
    #[arg(long)]
    json: bool,
}

/// `nerve gate (--run <id> | --receipt <path>)` — translate a sealed Receipt into a
/// merge-gate decision (exit code + conclusion), optionally posting a check run. The
/// receipt is located either by path (`--receipt`) or by captured run id (`--run`,
/// scanning `<root>/.nerve/receipts/`).
#[derive(Debug, Args)]
pub(crate) struct GateArgs {
    /// Path to a sealed Receipt JSON (as produced by the receipt store / `nerve verify`).
    #[arg(long = "receipt", conflicts_with = "run")]
    receipt: Option<PathBuf>,
    /// A captured run id whose sealed Receipt to load from `<root>/.nerve/receipts/`.
    #[arg(long = "run", conflicts_with = "receipt")]
    run: Option<String>,
    /// Workspace root holding `.nerve/` (defaults to the current directory); used to
    /// resolve `--run`.
    #[arg(long = "root")]
    root: Option<PathBuf>,
    /// Where to post the resulting check run: `none` (default — exit code only),
    /// `gh` (shell `gh api` to the GitHub Checks API), or `gitlab` (reserved).
    #[arg(long = "emit", default_value = "none")]
    emit: String,
    /// The commit SHA the check run attaches to (required for `--emit gh`).
    #[arg(long)]
    sha: Option<String>,
    /// `owner/repo` slug for the check run (required for `--emit gh`).
    #[arg(long)]
    repo: Option<String>,
    /// Print the [`GateOutcome`] as JSON in addition to setting the exit code.
    #[arg(long)]
    json: bool,
}

/// Side-effecting sink that posts a merge-gate decision to a code-host check surface.
/// The default impl ([`NoopEmitter`]) does nothing — the exit code is authoritative —
/// so a deployed merge App or a CI step both work without code change. This is the
/// deferred-infra seam (trust-substrate §8): a GitHub App / GitLab status can replace
/// the shelled `gh` path without touching the gate logic.
pub(crate) trait CheckRunEmitter {
    /// Post (or skip) a check run for `outcome` against `sha` in `repo`. Best-effort:
    /// a posting failure is reported but never overrides the authoritative exit code.
    fn emit(&self, repo: &str, sha: &str, outcome: &GateOutcome) -> Result<()>;
}

/// The no-op emitter: the exit code alone is the gate. Used when `--emit none`.
pub(crate) struct NoopEmitter;

impl CheckRunEmitter for NoopEmitter {
    fn emit(&self, _repo: &str, _sha: &str, _outcome: &GateOutcome) -> Result<()> {
        Ok(())
    }
}

/// Posts a GitHub check run by shelling `gh api` (the deferred-infra default until a
/// first-party GitHub App is deployed). The `gh` CLI carries the auth; we only build
/// the Checks-API request body from the pure [`GateOutcome`].
pub(crate) struct GhCheckRunEmitter {
    /// The check run's display name (the row shown on the PR).
    pub(crate) name: String,
}

impl Default for GhCheckRunEmitter {
    fn default() -> Self {
        Self {
            name: "nerve/verification-receipt".to_string(),
        }
    }
}

impl GhCheckRunEmitter {
    /// The `gh api` argument vector that POSTs a check run for `outcome`. Pure (no IO)
    /// so it is unit-testable without invoking `gh`.
    pub(crate) fn gh_args(&self, repo: &str, sha: &str, outcome: &GateOutcome) -> Vec<String> {
        vec![
            "api".to_string(),
            "--method".to_string(),
            "POST".to_string(),
            format!("repos/{repo}/check-runs"),
            "-f".to_string(),
            format!("name={}", self.name),
            "-f".to_string(),
            format!("head_sha={sha}"),
            "-f".to_string(),
            "status=completed".to_string(),
            "-f".to_string(),
            format!("conclusion={}", outcome.conclusion),
            "-f".to_string(),
            format!(
                "output[title]=Nerve verification receipt: {}",
                outcome.conclusion
            ),
            "-f".to_string(),
            format!("output[summary]={}", outcome.summary),
        ]
    }
}

impl CheckRunEmitter for GhCheckRunEmitter {
    fn emit(&self, repo: &str, sha: &str, outcome: &GateOutcome) -> Result<()> {
        let status = Command::new("gh")
            .args(self.gh_args(repo, sha, outcome))
            .status()
            .context("failed to spawn `gh` (is the GitHub CLI installed and authed?)")?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("`gh api` exited with status {status}"))
        }
    }
}

/// `nerve verify`: re-run the org's own checks for a captured run in-process, seal +
/// sign a fresh Verification Receipt, and report its gate decision. Returns the gate's
/// exit code (0=Passed, 1=Failed, 2=Inconclusive/Error) so the calling CLI arm can
/// propagate it to CI. A missing `<root>/.nerve/checks.json` yields an honest
/// Inconclusive receipt (exit 2) — never a fabricated pass (INV-R1).
pub(crate) fn verify(args: VerifyArgs) -> Result<i32> {
    let root = resolve_root(args.root)?;
    let receipt = crate::commands::verify::run_verify_flow(&root, &args.run_id, args.reruns)?;
    report_receipt(&receipt, args.json)
}

/// `nerve gate`: load a sealed Receipt, decide the merge outcome, optionally post a
/// check run, and exit with the authoritative code.
pub(crate) fn gate(args: GateArgs) -> Result<i32> {
    let receipt = load_gate_receipt(&args)?;
    let outcome = gate_outcome(&receipt);
    let emitter = select_emitter(&args.emit)?;
    if let (Some(repo), Some(sha)) = (args.repo.as_deref(), args.sha.as_deref()) {
        if let Err(err) = emitter.emit(repo, sha, &outcome) {
            // Best-effort: a posting failure never overrides the exit code (INV-R1).
            eprintln!("warning: failed to post check run: {err}");
        }
    } else if args.emit != "none" {
        eprintln!(
            "warning: --emit {} ignored (needs --repo and --sha)",
            args.emit
        );
    }
    print_outcome(&outcome, args.json);
    Ok(outcome.exit_code)
}

/// Resolve the sealed Receipt a `gate` invocation acts on: by explicit `--receipt`
/// path, or by `--run <id>` (scanning `<root>/.nerve/receipts/` via the shared
/// [`load_receipt_for_run`]). Exactly one is required; `--run` with no sealed receipt is
/// an error (run `nerve verify <id>` first to seal one).
fn load_gate_receipt(args: &GateArgs) -> Result<Receipt> {
    match (&args.receipt, &args.run) {
        (Some(path), _) => read_receipt(path),
        (None, Some(run_id)) => {
            let root = resolve_root(args.root.clone())?;
            load_receipt_for_run(&root, run_id)?.ok_or_else(|| {
                anyhow!("no sealed receipt for run `{run_id}` (run `nerve verify {run_id}` first)")
            })
        }
        (None, None) => Err(anyhow!(
            "provide exactly one of --receipt <path> or --run <id>"
        )),
    }
}

/// Pick the [`CheckRunEmitter`] for `--emit`. `gitlab` is reserved (the seam exists;
/// the gh path proves it) and falls back to the no-op so the exit code still gates.
fn select_emitter(emit: &str) -> Result<Box<dyn CheckRunEmitter>> {
    match emit {
        "none" | "gitlab" => Ok(Box::new(NoopEmitter)),
        "gh" => Ok(Box::new(GhCheckRunEmitter::default())),
        other => Err(anyhow!(
            "unknown --emit `{other}` (expected: none, gh, gitlab)"
        )),
    }
}

/// Render a receipt's gate decision and return its exit code (shared by `verify`).
fn report_receipt(receipt: &Receipt, as_json: bool) -> Result<i32> {
    let outcome = gate_outcome(receipt);
    print_outcome(&outcome, as_json);
    Ok(outcome.exit_code)
}

/// Emit the outcome (JSON or one human line) to stdout.
fn print_outcome(outcome: &GateOutcome, as_json: bool) {
    if as_json {
        println!(
            "{}",
            serde_json::to_string(outcome).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!(
            "{} (exit {}): {}",
            outcome.conclusion, outcome.exit_code, outcome.summary
        );
    }
}

/// Read + parse a sealed Receipt from a JSON file.
fn read_receipt(path: &std::path::Path) -> Result<Receipt> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read receipt {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse receipt {}", path.display()))
}

/// Find a sealed Receipt for `run_id` under `<root>/.nerve/receipts/`, matching by the
/// statement's `provenance.run_id`. Tolerant: skips unreadable/bad files; a missing
/// dir yields `None` (the `verify_not_available` path).
fn load_receipt_for_run(root: &std::path::Path, run_id: &str) -> Result<Option<Receipt>> {
    let dir = root.join(".nerve").join("receipts");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(anyhow!("failed to read {}: {err}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let matches = value
            .pointer("/statement/provenance/run_id")
            .and_then(Value::as_str)
            == Some(run_id);
        if matches && let Ok(receipt) = serde_json::from_value::<Receipt>(value) {
            return Ok(Some(receipt));
        }
    }
    Ok(None)
}

/// Resolve the workspace root: the flag, else the current directory.
fn resolve_root(root: Option<PathBuf>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(root),
        None => std::env::current_dir().context("failed to resolve current directory"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::receipt::{
        RECEIPT_PREDICATE_TYPE, RECEIPT_SCHEMA_VERSION, Receipt, ReceiptProvenance,
        ReceiptSignature, ReceiptStatement, ReplayManifest,
    };
    use nerve_core::verdict::VerdictStatus;
    use std::fs;
    use tempfile::tempdir;

    fn receipt_for(run_id: &str, verdict: VerdictStatus) -> Receipt {
        Receipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            receipt_id: format!("rcpt-{run_id}"),
            statement: ReceiptStatement {
                predicate_type: RECEIPT_PREDICATE_TYPE.to_string(),
                provenance: ReceiptProvenance {
                    run_id: run_id.to_string(),
                    inputs_hash: "h".to_string(),
                    toolchain_digest: None,
                    policy_version: None,
                    ledger_ref: None,
                },
                checks: vec![],
                verdict,
                replay_manifest: ReplayManifest {
                    run_schema_version: 2,
                    root_hash: "root".to_string(),
                    event_count: 0,
                    command: None,
                },
                issued_at_ms: 1,
            },
            signature: ReceiptSignature {
                payload_type: "application/vnd.in-toto+json".to_string(),
                backend: "local-ed25519".to_string(),
                keyid: "k1".to_string(),
                sig: "s".to_string(),
                public_key: None,
                bundle: None,
            },
        }
    }

    fn write_receipt(root: &std::path::Path, receipt: &Receipt) {
        let dir = root.join(".nerve").join("receipts");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", receipt.receipt_id));
        fs::write(path, serde_json::to_string_pretty(receipt).unwrap()).unwrap();
    }

    #[test]
    fn gate_reads_receipt_and_maps_passed_to_exit_zero() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("receipt.json");
        let receipt = receipt_for("run-a", VerdictStatus::Passed);
        fs::write(&path, serde_json::to_string(&receipt).unwrap()).unwrap();

        let code = gate(GateArgs {
            receipt: Some(path),
            run: None,
            root: None,
            emit: "none".to_string(),
            sha: None,
            repo: None,
            json: false,
        })
        .unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn gate_maps_failed_to_exit_one() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("r.json");
        fs::write(
            &path,
            serde_json::to_string(&receipt_for("r", VerdictStatus::Failed)).unwrap(),
        )
        .unwrap();
        let outcome = gate_outcome(&read_receipt(&path).unwrap());
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(outcome.conclusion, "failure");
    }

    #[test]
    fn gate_by_run_loads_sealed_receipt_and_gates() {
        let dir = tempdir().unwrap();
        // A sealed receipt for `run-x` is found by id under <root>/.nerve/receipts.
        write_receipt(dir.path(), &receipt_for("run-x", VerdictStatus::Passed));
        let code = gate(GateArgs {
            receipt: None,
            run: Some("run-x".to_string()),
            root: Some(dir.path().to_path_buf()),
            emit: "none".to_string(),
            sha: None,
            repo: None,
            json: false,
        })
        .unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn gate_by_run_errors_when_no_receipt_sealed() {
        let dir = tempdir().unwrap();
        // No receipt for the run -> a hard error (never a fabricated pass — INV-R1).
        let err = gate(GateArgs {
            receipt: None,
            run: Some("absent".to_string()),
            root: Some(dir.path().to_path_buf()),
            emit: "none".to_string(),
            sha: None,
            repo: None,
            json: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("absent"), "{err}");
    }

    #[test]
    fn verify_re_runs_checks_and_gates_on_a_fresh_receipt() {
        // End-to-end: a seeded run + a passing `true` check seals a Passed receipt and
        // `nerve verify` exits 0; the deep re-verify coverage lives in commands::verify.
        use nerve_core::provenance::{Event, EventKind, RunInputs};
        let dir = tempdir().unwrap();
        let store = crate::run_store::RunStore::for_scope(Some(dir.path())).unwrap();
        let run = nerve_core::build_run(
            "job",
            "codex",
            None,
            1,
            Some(2),
            true,
            vec![Event {
                seq: 0,
                kind: EventKind::RunStarted {
                    agent: "codex".into(),
                    task: "t".into(),
                    cwd: None,
                    inputs: None,
                },
            }],
            RunInputs::default(),
        );
        store.write_record(&run).unwrap();
        let checks = dir.path().join(".nerve");
        fs::create_dir_all(&checks).unwrap();
        fs::write(
            checks.join("checks.json"),
            serde_json::json!({"checks":[{"name":"smoke","command":"true","required":true}]})
                .to_string(),
        )
        .unwrap();

        let code = verify(VerifyArgs {
            run_id: run.run_id.clone(),
            root: Some(dir.path().to_path_buf()),
            reruns: Some(1),
            json: false,
        })
        .unwrap();
        assert_eq!(code, 0, "passing check gates 0");
    }

    #[test]
    fn gh_args_build_a_check_run_post() {
        let outcome = gate_outcome(&receipt_for("r", VerdictStatus::Passed));
        let args = GhCheckRunEmitter::default().gh_args("o/r", "deadbeef", &outcome);
        assert_eq!(args[0], "api");
        assert!(args.iter().any(|a| a == "repos/o/r/check-runs"));
        assert!(args.iter().any(|a| a == "head_sha=deadbeef"));
        assert!(args.iter().any(|a| a == "conclusion=success"));
        assert!(args.iter().any(|a| a == "status=completed"));
    }

    #[test]
    fn select_emitter_knows_its_three_modes() {
        assert!(select_emitter("none").is_ok());
        assert!(select_emitter("gh").is_ok());
        assert!(select_emitter("gitlab").is_ok());
        assert!(select_emitter("bogus").is_err());
    }

    #[test]
    fn noop_emitter_is_inert() {
        let outcome = gate_outcome(&receipt_for("r", VerdictStatus::Failed));
        assert!(NoopEmitter.emit("o/r", "sha", &outcome).is_ok());
    }
}
