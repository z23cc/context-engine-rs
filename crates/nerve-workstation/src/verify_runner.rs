//! L2 **execution-grounded verify** runner (`docs/designs/trust-substrate.md`
//! §3 L2, INV-R1) — the impure half of the re-verifier. Given a captured
//! [`Run`](nerve_core::provenance::Run), it loads the org's declared checks from
//! `<root>/.nerve/checks.json`, re-runs each one inside the recorded closure via the
//! [`SandboxLauncher`](crate::sandbox::SandboxLauncher) seam, folds the per-check
//! [`CheckResult`](nerve_core::verdict::CheckResult)s, and seals the outcome with the
//! pure [`nerve_core::verdict::build_verdict`].
//!
//! **Court reporter, not judge (INV-R1).** The runner never decides "correct"; it
//! re-runs the org's own bar and reports whether it cleared. Flakiness is observed by
//! re-running each check `reruns` times: a check that both passes and fails is
//! `Flaky`, which a *required* check turns into [`VerdictStatus::Inconclusive`]. The
//! aggregation, hashing, and content-addressing are all in the pure kernel
//! (`nerve_core::verdict`); this module only drives processes and records what it saw.
//!
//! **Deferred infra.** Strong hermetic isolation (Landlock/seccomp/microVM, a pinned
//! closure digest) is the deferred `SandboxLauncher` backend — today the best-effort
//! `ProcessLauncher` (cwd-forced, env-scrubbed) is wired. The checkspec source is the
//! seam: `CheckSpec::load` reads JSON now; a richer/TOML source can slot in behind it.

use crate::sandbox::{CommandSpec, SandboxLauncher, SandboxPolicy};
use crate::verify_store::VerifyStore;
use anyhow::{Context, Result};
use nerve_core::CancelToken;
use nerve_core::provenance::Run;
use nerve_core::verdict::{
    CheckKind, CheckResult, CheckStatus, Verdict, build_verdict, hash_checkspec,
};
use nerve_runtime::RuntimeError;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// Default number of times each check is re-run to detect flakiness when the caller
/// does not specify. One extra run beyond the first is enough to surface a flake.
const DEFAULT_RERUNS: u32 = 1;

/// One declared check from `<root>/.nerve/checks.json`. The org states *what* to run
/// (a program + argv — never a shell string, mirroring [`CommandSpec`]), how to
/// classify it ([`CheckKind`]), and whether it gates the verdict (`required`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CheckDecl {
    /// Human-facing label (e.g. `"cargo test"`).
    pub(crate) name: String,
    /// Which org-bar dimension this check exercises.
    #[serde(default = "default_kind")]
    pub(crate) kind: CheckKind,
    /// Program to execute (resolved via the child `PATH`, or an absolute path).
    pub(crate) command: String,
    /// Arguments passed verbatim (no shell, no interpolation).
    #[serde(default)]
    pub(crate) args: Vec<String>,
    /// Whether this check gates the verdict. Advisory (`false`) checks are recorded
    /// but can never fail the verdict (INV-R1).
    #[serde(default = "default_required")]
    pub(crate) required: bool,
}

fn default_kind() -> CheckKind {
    CheckKind::Test
}

fn default_required() -> bool {
    true
}

/// The org's full checkspec: the ordered checks plus the parsed JSON the
/// `checkspec_hash` commits to (so the verdict pins exactly *which* checks ran).
#[derive(Debug, Clone)]
pub(crate) struct CheckSpec {
    pub(crate) checks: Vec<CheckDecl>,
    spec_json: serde_json::Value,
}

impl CheckSpec {
    /// Load the checkspec for `root` from `<root>/.nerve/checks.json`. A missing file
    /// or an empty `checks` array yields an empty spec (the verdict will then be
    /// [`VerdictStatus::Error`] — the org has no bar to clear). A malformed file is an
    /// error so the operator is told their config is broken rather than silently
    /// getting an empty (and thus `Error`) verdict.
    pub(crate) fn load(root: &Path) -> Result<Self> {
        let path = root.join(".nerve").join("checks.json");
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    checks: Vec::new(),
                    spec_json: serde_json::json!({ "checks": [] }),
                });
            }
            Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
        };
        let spec_json: serde_json::Value =
            serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
        let checks: Vec<CheckDecl> = serde_json::from_value(
            spec_json
                .get("checks")
                .cloned()
                .unwrap_or_else(|| serde_json::json!([])),
        )
        .with_context(|| format!("parse `checks` in {}", path.display()))?;
        Ok(Self { checks, spec_json })
    }

    /// Filter to the kinds in `only` (when given), preserving declaration order.
    fn selected(&self, only: Option<&[CheckKind]>) -> Vec<CheckDecl> {
        match only {
            None => self.checks.clone(),
            Some(kinds) => self
                .checks
                .iter()
                .filter(|c| kinds.contains(&c.kind))
                .cloned()
                .collect(),
        }
    }
}

/// Run `verify.start`: load the captured run, re-run the org's declared checks in the
/// recorded closure `reruns` times each, fold them into a sealed [`Verdict`], persist
/// it (best-effort), and return it. An unknown `run_id` (or no served run store) is a
/// transport error; an empty checkspec seals an honest [`VerdictStatus::Error`] verdict
/// (no bar to clear) rather than erroring.
#[allow(
    clippy::too_many_arguments,
    reason = "host-supplied verify inputs are irreducible"
)]
pub(crate) fn handle_verify_start(
    run_store: Option<&crate::run_store::RunStore>,
    verify_store: Option<&VerifyStore>,
    launcher: &Arc<dyn SandboxLauncher>,
    root: &Path,
    run_id: &str,
    reruns: Option<u32>,
    only: Option<&[CheckKind]>,
    cancel: &CancelToken,
    verified_at_ms: u64,
) -> Result<Verdict, RuntimeError> {
    let run = load_run(run_store, run_id)?;
    let spec = CheckSpec::load(root)
        .map_err(|err| RuntimeError::adapter(format!("load checkspec: {err}")))?;
    let decls = spec.selected(only);
    let reruns = reruns.unwrap_or(DEFAULT_RERUNS).max(1);
    let policy = SandboxPolicy::for_root(Some(root));

    let mut checks = Vec::with_capacity(decls.len());
    let mut required = Vec::with_capacity(decls.len());
    for decl in &decls {
        if cancel.is_cancelled() {
            return Err(RuntimeError::adapter("verify cancelled"));
        }
        checks.push(run_check(launcher.as_ref(), &policy, decl, reruns, cancel));
        required.push(decl.required);
    }

    let checkspec_hash = hash_checkspec(&spec.spec_json);
    let closure_digest = closure_digest_for(&run);
    let verdict = build_verdict(
        run.run_id.clone(),
        None,
        checkspec_hash,
        closure_digest,
        checks,
        &required,
        verified_at_ms,
    );
    // Best-effort: a persistence failure never fails the verify turn.
    if let Some(store) = verify_store {
        let _ = store.write_record(&verdict);
    }
    Ok(verdict)
}

/// `verify.get` — fetch a sealed verdict by id (delegates to the store handler).
pub(crate) fn handle_verify_get(
    verdict_id: &str,
    store: Option<&VerifyStore>,
) -> Result<serde_json::Value, RuntimeError> {
    crate::verify_store::run_verify_get(verdict_id, store)
}

/// `verify.list` — enumerate sealed verdicts (optionally for one run).
pub(crate) fn handle_verify_list(
    store: Option<&VerifyStore>,
    run_id: Option<&str>,
) -> serde_json::Value {
    crate::verify_store::run_verify_list(store, run_id)
}

/// Load the captured run for `run_id`, mapping an absent store / unknown id to a
/// transport error (mirrors `run_store::run_run_get`).
fn load_run(store: Option<&crate::run_store::RunStore>, run_id: &str) -> Result<Run, RuntimeError> {
    let store =
        store.ok_or_else(|| RuntimeError::adapter(format!("no captured run `{run_id}`")))?;
    store
        .load_record(run_id)
        .map_err(|err| RuntimeError::adapter(format!("no captured run `{run_id}`: {err}")))
}

/// The closure digest a verdict pins: the recorded run's pinned closure (repo
/// snapshot + toolchain digest from L0c `RunInputs`). Empty when the run was
/// captured before closure pinning — the verdict then commits to an empty closure,
/// which a verifier can detect.
fn closure_digest_for(run: &Run) -> String {
    let repo = run.inputs.repo_snapshot_hash.as_str();
    let toolchain = run.inputs.toolchain_digest.as_str();
    if repo.is_empty() && toolchain.is_empty() {
        String::new()
    } else {
        format!("{repo}:{toolchain}")
    }
}

/// Re-run one declared check `reruns` times under containment and fold the attempts
/// into a single [`CheckResult`]. Flakiness is derived from disagreement across
/// attempts: all-pass → `Pass`, all-fail → `Fail`, mixed → `Flaky`, a launch error
/// on any attempt → `Error` (the check could not be run). The reported `exit_code` /
/// `duration_ms` / `output_hash` are from the *last* attempt.
fn run_check(
    launcher: &dyn SandboxLauncher,
    policy: &SandboxPolicy,
    decl: &CheckDecl,
    reruns: u32,
    cancel: &CancelToken,
) -> CheckResult {
    let spec = CommandSpec {
        command: decl.command.clone(),
        args: decl.args.clone(),
    };
    let mut passed = 0u32;
    let mut attempts = 0u32;
    let mut errored = false;
    let mut last_exit: Option<i32> = None;
    let mut last_timed_out = false;
    let mut last_duration_ms = 0u64;
    let mut last_output_hash = String::new();

    for _ in 0..reruns {
        if cancel.is_cancelled() {
            errored = true;
            break;
        }
        attempts += 1;
        let started = Instant::now();
        match launcher.launch(&spec, policy, cancel) {
            Ok(output) => {
                last_duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
                last_exit = output.exit_code;
                last_timed_out = output.timed_out;
                last_output_hash = hash_output(&output.stdout, &output.stderr);
                if output.exit_code == Some(0) && !output.timed_out {
                    passed += 1;
                }
            }
            Err(_) => {
                errored = true;
                break;
            }
        }
    }

    let status = fold_status(errored, attempts, passed);
    CheckResult {
        name: decl.name.clone(),
        kind: decl.kind,
        status,
        reproducible: matches!(status, CheckStatus::Pass | CheckStatus::Fail),
        exit_code: last_exit,
        timed_out: last_timed_out,
        duration_ms: last_duration_ms,
        output_hash: last_output_hash,
        runs: attempts,
        passed,
    }
}

/// Fold the per-attempt tally into a [`CheckStatus`]: a launch error dominates; with
/// no completed attempt it is `Error`; all-pass → `Pass`, none-pass → `Fail`, a
/// partial split → `Flaky` (not reproducible).
fn fold_status(errored: bool, attempts: u32, passed: u32) -> CheckStatus {
    if errored || attempts == 0 {
        CheckStatus::Error
    } else if passed == attempts {
        CheckStatus::Pass
    } else if passed == 0 {
        CheckStatus::Fail
    } else {
        CheckStatus::Flaky
    }
}

/// Content address of a check's captured output (stdout + stderr), so the verdict can
/// reference the evidence without inlining it. Delegates to the kernel's pure
/// SHA-256-over-canonical-JSON helper ([`hash_checkspec`]) so this module needs no
/// crypto dep of its own; the `{stdout,stderr}` object is byte-stable (no floats,
/// sorted keys), so the digest is reproducible.
fn hash_output(stdout: &str, stderr: &str) -> String {
    hash_checkspec(&serde_json::json!({ "stdout": stdout, "stderr": stderr }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Output;
    use nerve_core::provenance::{Event, EventKind};
    use nerve_core::verdict::VerdictStatus;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// A scripted launcher: returns canned [`Output`]s in sequence per check name, so
    /// flakiness and failure can be exercised without real subprocesses.
    struct ScriptedLauncher {
        // command -> queue of (exit_code) outcomes, consumed per attempt.
        script: Mutex<std::collections::HashMap<String, Vec<Option<i32>>>>,
        error_on: Option<String>,
    }

    impl ScriptedLauncher {
        fn new() -> Self {
            Self {
                script: Mutex::new(std::collections::HashMap::new()),
                error_on: None,
            }
        }
        fn with(self, command: &str, codes: Vec<Option<i32>>) -> Self {
            self.script.lock().unwrap().insert(command.into(), codes);
            self
        }
    }

    impl SandboxLauncher for ScriptedLauncher {
        fn launch(
            &self,
            spec: &CommandSpec,
            _policy: &SandboxPolicy,
            _cancel: &CancelToken,
        ) -> Result<Output> {
            if self.error_on.as_deref() == Some(spec.command.as_str()) {
                return Err(anyhow::anyhow!("launch failed"));
            }
            let mut guard = self.script.lock().unwrap();
            let queue = guard.entry(spec.command.clone()).or_default();
            let code = if queue.is_empty() {
                Some(0)
            } else {
                queue.remove(0)
            };
            Ok(Output {
                exit_code: code,
                stdout: format!("ran {}", spec.command),
                stderr: String::new(),
                timed_out: false,
            })
        }
    }

    fn write_checks(root: &Path, body: serde_json::Value) {
        let dir = root.join(".nerve");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("checks.json"), body.to_string()).unwrap();
    }

    fn captured_run(store: &crate::run_store::RunStore, run_id_out: &mut String) {
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
            nerve_core::provenance::RunInputs::default(),
        );
        *run_id_out = run.run_id.clone();
        store.write_record(&run).unwrap();
    }

    fn fixtures() -> (
        tempfile::TempDir,
        crate::run_store::RunStore,
        VerifyStore,
        String,
    ) {
        let dir = tempdir().unwrap();
        let runs = crate::run_store::RunStore::new(dir.path().join("runs"));
        let verdicts = VerifyStore::new(dir.path().join("verdicts"));
        let mut run_id = String::new();
        captured_run(&runs, &mut run_id);
        (dir, runs, verdicts, run_id)
    }

    #[test]
    fn all_required_pass_is_passed_and_persists() {
        let (dir, runs, verdicts, run_id) = fixtures();
        write_checks(
            dir.path(),
            serde_json::json!({"checks":[
                {"name":"test","kind":"test","command":"t","required":true},
                {"name":"build","kind":"build","command":"b","required":true}
            ]}),
        );
        let launcher: Arc<dyn SandboxLauncher> = Arc::new(ScriptedLauncher::new());
        let v = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            &run_id,
            Some(1),
            None,
            &CancelToken::never(),
            1000,
        )
        .unwrap();
        assert_eq!(v.status, VerdictStatus::Passed);
        assert_eq!(v.checks.len(), 2);
        // Sealed verdict was persisted and round-trips.
        let loaded = verdicts.load_record(&v.verdict_id).unwrap();
        assert_eq!(loaded.verdict_id, v.verdict_id);
    }

    #[test]
    fn required_failure_is_failed_advisory_ignored() {
        let (dir, runs, verdicts, run_id) = fixtures();
        write_checks(
            dir.path(),
            serde_json::json!({"checks":[
                {"name":"test","command":"t","required":true},
                {"name":"lint","kind":"lint","command":"l","required":false}
            ]}),
        );
        let launcher: Arc<dyn SandboxLauncher> = Arc::new(
            ScriptedLauncher::new()
                .with("t", vec![Some(1)])
                .with("l", vec![Some(1)]),
        );
        let v = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            &run_id,
            Some(1),
            None,
            &CancelToken::never(),
            1,
        )
        .unwrap();
        assert_eq!(v.status, VerdictStatus::Failed);
    }

    #[test]
    fn flaky_required_is_inconclusive() {
        let (dir, runs, verdicts, run_id) = fixtures();
        write_checks(
            dir.path(),
            serde_json::json!({"checks":[{"name":"test","command":"t","required":true}]}),
        );
        // pass then fail across two reruns -> Flaky -> Inconclusive.
        let launcher: Arc<dyn SandboxLauncher> =
            Arc::new(ScriptedLauncher::new().with("t", vec![Some(0), Some(1)]));
        let v = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            &run_id,
            Some(2),
            None,
            &CancelToken::never(),
            1,
        )
        .unwrap();
        assert_eq!(v.status, VerdictStatus::Inconclusive);
        assert_eq!(v.checks[0].status, CheckStatus::Flaky);
        assert_eq!(v.checks[0].runs, 2);
        assert_eq!(v.checks[0].passed, 1);
    }

    #[test]
    fn empty_checkspec_is_error_not_a_transport_error() {
        let (dir, runs, verdicts, run_id) = fixtures();
        // No checks.json at all.
        let launcher: Arc<dyn SandboxLauncher> = Arc::new(ScriptedLauncher::new());
        let v = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            &run_id,
            None,
            None,
            &CancelToken::never(),
            1,
        )
        .unwrap();
        // No required bar exercised -> Inconclusive per aggregate_status, sealed honestly.
        assert_eq!(v.status, VerdictStatus::Inconclusive);
        assert!(v.checks.is_empty());
    }

    #[test]
    fn launch_error_marks_check_error() {
        let (dir, runs, verdicts, run_id) = fixtures();
        write_checks(
            dir.path(),
            serde_json::json!({"checks":[{"name":"test","command":"t","required":true}]}),
        );
        let mut l = ScriptedLauncher::new();
        l.error_on = Some("t".into());
        let launcher: Arc<dyn SandboxLauncher> = Arc::new(l);
        let v = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            &run_id,
            Some(1),
            None,
            &CancelToken::never(),
            1,
        )
        .unwrap();
        assert_eq!(v.checks[0].status, CheckStatus::Error);
        assert_eq!(v.status, VerdictStatus::Error);
    }

    #[test]
    fn unknown_run_is_a_transport_error() {
        let (dir, runs, verdicts, _run_id) = fixtures();
        let launcher: Arc<dyn SandboxLauncher> = Arc::new(ScriptedLauncher::new());
        let err = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            "nope",
            Some(1),
            None,
            &CancelToken::never(),
            1,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no captured run"), "{err}");
    }

    #[test]
    fn only_filter_selects_kinds_and_sealed_id_matches_pure_rebuild() {
        let (dir, runs, verdicts, run_id) = fixtures();
        write_checks(
            dir.path(),
            serde_json::json!({"checks":[
                {"name":"test","kind":"test","command":"t","required":true},
                {"name":"lint","kind":"lint","command":"l","required":true}
            ]}),
        );
        let launcher: Arc<dyn SandboxLauncher> = Arc::new(ScriptedLauncher::new());
        let v = handle_verify_start(
            Some(&runs),
            Some(&verdicts),
            &launcher,
            dir.path(),
            &run_id,
            Some(1),
            Some(&[CheckKind::Lint]),
            &CancelToken::never(),
            1,
        )
        .unwrap();
        assert_eq!(v.checks.len(), 1, "only lint selected");
        assert_eq!(v.checks[0].name, "lint");
        // The sealed id is the pure content address over the same components.
        let rebuilt = nerve_core::verdict::verdict_content_id(
            &v.run_id,
            v.diff_hash.as_deref(),
            &v.checkspec_hash,
            &v.closure_digest,
            &v.checks,
        );
        assert_eq!(v.verdict_id, rebuilt);
    }
}
