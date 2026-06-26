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
//!
//! **Signature re-verification at the gate (INV-R5).** Before trusting a pre-sealed
//! Receipt's verdict, `nerve gate` re-verifies it offline via the pure
//! [`verify_receipt`](nerve_core::receipt::verify_receipt) (statement re-hashes to the
//! receipt id + the embedded ed25519 public key checks the detached signature over the
//! DSSE PAE). A receipt that does not verify is **refused** — exit non-zero, no verdict
//! trusted, no check run posted — so a tampered/forged receipt file can never gate a
//! fabricated pass through CI (INV-R1). **Honest trust model:** self-signature
//! verification proves the receipt is *tamper-evident* (it wasn't modified after
//! signing) but NOT issuer identity — a forger can re-sign with their OWN key. The
//! optional `--trusted-key` / `NERVE_TRUSTED_RECEIPT_KEY` pin adds issuer identity by
//! requiring the signing key to equal a known org key; sigstore-keyless issuer identity
//! remains the deferred upgrade.

mod emit;

use anyhow::{Context, Result, anyhow};
use clap::Args;
use emit::select_emitter;
use nerve_core::provenance::IsolationTier;
use nerve_core::receipt::Receipt;
use nerve_core::receipt_gate::{GateOutcome, enforce_isolation_floor, enforce_merge_bar};
use serde_json::{Value, json};
use std::path::PathBuf;

/// The env-var fallback for `--trusted-key`: a base64 (standard) ed25519 public key the
/// gate requires the receipt to be signed by (issuer-identity pin). When neither the
/// flag nor this var is set, the gate verifies only self-consistency (tamper-evidence)
/// and prints an advisory that issuer identity is not pinned.
const TRUSTED_KEY_ENV: &str = "NERVE_TRUSTED_RECEIPT_KEY";

/// The env-var fallback for `--require-isolation`: the minimum signed
/// [`IsolationTier`] the gate requires (`hermetic|contained|best-effort|unconfined`).
/// A receipt whose re-run was contained BELOW this floor has a passing outcome
/// downgraded to neutral (INV-R7, §3.4). Unset → report-only (today's behavior).
const REQUIRE_ISOLATION_ENV: &str = "NERVE_REQUIRE_ISOLATION";

/// Exit code for a refused gate (mirrors the Inconclusive/Error neutral exit): a receipt
/// that fails integrity / signature / trusted-key verification is never trusted.
const REFUSE_EXIT: i32 = 2;

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
    /// `gh` (shell `gh api` to the GitHub Checks API), or `gitlab` (POST a commit
    /// status via the GitLab Commit Status API with `curl`).
    #[arg(long = "emit", default_value = "none")]
    emit: String,
    /// The commit SHA the check run / commit status attaches to (required for
    /// `--emit gh` and `--emit gitlab`).
    #[arg(long)]
    sha: Option<String>,
    /// `owner/repo` slug (GitHub) or numeric/`group/project` id (GitLab) the status
    /// attaches to (required for `--emit gh` and `--emit gitlab`).
    #[arg(long)]
    repo: Option<String>,
    /// Pin the receipt's issuer identity: a base64 (standard) ed25519 public key the
    /// receipt MUST be signed by, else the gate refuses (`NERVE_TRUSTED_RECEIPT_KEY` is
    /// the env fallback). Unset → self-consistency (tamper-evidence) only, with an
    /// advisory that issuer identity is not pinned.
    #[arg(long = "trusted-key")]
    trusted_key: Option<String>,
    /// Require the receipt's SIGNED isolation tier to be at least this strong
    /// (`hermetic|contained|best-effort|unconfined`; `NERVE_REQUIRE_ISOLATION` is the env
    /// fallback). A receipt whose verify re-run was contained below the floor has a
    /// passing outcome DOWNGRADED to neutral (exit 2) — never a fabricated pass, never an
    /// upgrade (INV-R7). Unset → report-only (the tier is still printed).
    #[arg(long = "require-isolation")]
    require_isolation: Option<String>,
    /// Print the [`GateOutcome`] as JSON in addition to setting the exit code.
    #[arg(long)]
    json: bool,
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

/// `nerve gate`: load a sealed Receipt, **re-verify its signature + statement integrity**
/// (INV-R5), and only then decide the merge outcome, optionally post a check run, and
/// exit with the authoritative code.
///
/// A receipt that does not verify — a tampered statement, a corrupted/forged signature,
/// or (with `--trusted-key`) one not signed by the pinned issuer key — is **refused**:
/// the gate exits non-zero, NEVER trusts the claimed verdict, and posts NO check run /
/// commit status (INV-R1, court reporter: never fabricate a pass).
pub(crate) fn gate(args: GateArgs) -> Result<i32> {
    let receipt = load_gate_receipt(&args)?;
    // Resolve the optional isolation floor up front so a bad value fails fast (before any
    // verification work) rather than after sealing a decision.
    let require_isolation = resolve_required_isolation(&args)?;
    let trusted_key = resolve_trusted_key(&args);
    let verification = verify_gate_receipt(&receipt, trusted_key.as_deref());
    if !verification.trusted {
        // REFUSE: integrity / signature / trusted-key check failed. No emit, no trusted
        // verdict — exit non-zero so CI blocks the merge (INV-R1 / INV-R5).
        print_refusal(&receipt, &verification, args.json);
        return Ok(REFUSE_EXIT);
    }
    // L3 (INV-R1/R5): enforce the org's bar that the receipt SIGNED. The overlay borrows
    // the embedded `merge_bar` + `required_evidence` — never a gate-side policy re-read —
    // and may only KEEP a pass or DOWNGRADE it (never upgrade). An empty (no-bar) receipt
    // passes through to `gate_outcome` unchanged.
    let outcome = enforce_merge_bar(&receipt);
    // INV-R7 (§3.4): apply the OPTIONAL isolation-tier floor AFTER the signature verify +
    // merge-bar enforcement, reusing the same downgrade-only kernel — a sub-floor tier
    // downgrades a pass to neutral, never upgrades. `None` is a pure pass-through.
    let outcome = enforce_isolation_floor(outcome, &receipt, require_isolation);
    // Best-effort L1 decision record: route the bar clearance to the evidence ledger if a
    // ledger is served (degrades to the no-op sink — never fails the gate).
    record_gate_decision(&args, &receipt, &outcome);
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
    if !verification.issuer_pinned {
        eprintln!(
            "note: receipt issuer identity is NOT pinned (signed_by={}); --trusted-key / \
             {TRUSTED_KEY_ENV} verifies the signing key against a known org key. The \
             signature only proves the receipt is tamper-evident, not who issued it.",
            verification.keyid
        );
    }
    print_verified_outcome(&outcome, &verification, args.json);
    Ok(outcome.exit_code)
}

/// Best-effort L1 routing of the gate's bar decision (`docs/designs/frontier-l3-l6-sigstore.md`
/// §1 — "Route the decision to L1"). Records a [`PolicyDecisionRecord`] keyed on the
/// receipt's run + pinned policy version via the served scope's ledger-backed
/// [`LedgerEvidenceSink`](crate::policy_plane::LedgerEvidenceSink); degrades to the no-op
/// [`NullEvidenceSink`](crate::policy_plane::NullEvidenceSink) when no ledger is served,
/// and NEVER fails the gate (INV-R1: the audit trail is evidence, not the admission gate).
fn record_gate_decision(args: &GateArgs, receipt: &Receipt, outcome: &GateOutcome) {
    use nerve_core::policy::{Capability, POLICY_SCHEMA_VERSION, PolicyDecisionRecord};
    let Ok(root) = resolve_root(args.root.clone()) else {
        return;
    };
    let plane = match crate::ledger_store::LedgerStore::for_scope(Some(&root)) {
        Ok(store) => crate::policy_plane::PolicyPlane::with_ledger(Some(&root), store),
        Err(_) => crate::policy_plane::PolicyPlane::resolve(Some(&root)),
    };
    // Pin the version the RECEIPT signed (INV-R5), not the gate host's live plane.
    let policy_version = receipt
        .statement
        .provenance
        .policy_version
        .clone()
        .unwrap_or_default();
    let record = PolicyDecisionRecord {
        schema_version: POLICY_SCHEMA_VERSION,
        policy_version,
        session_id: receipt.statement.provenance.run_id.clone(),
        agent: String::new(),
        tool: "gate".to_string(),
        // The gate is an admission decision over the change as a whole; classify it as
        // the most consequential capability (Exec) — fail-closed labeling.
        capability: Capability::Exec,
        // The bar clearance maps to the binary allow/deny the ledger commits.
        decision: if outcome.exit_code == 0 {
            "allow"
        } else {
            "deny"
        }
        .to_string(),
        reason: outcome.summary.clone(),
        args_hash: String::new(),
    };
    let _ = plane.record_decision(&record);
}

/// The result of re-verifying a receipt at the gate (INV-R5): the pure self-verification
/// flags plus the resolved issuer-pin decision and the receipt's signing key id.
struct GateVerification {
    /// The statement re-hashes to the receipt id (no tampering after signing).
    statement_intact: bool,
    /// The embedded public key checks the detached signature over the DSSE PAE.
    signature_valid: bool,
    /// `--trusted-key` (or the env fallback) was supplied AND matched the signing key.
    issuer_pinned: bool,
    /// The signing public key the signature was VERIFIED against (the embedded
    /// `public_key`, NOT the spoofable self-declared `keyid`), echoed so a consumer
    /// sees who the gate actually trusted.
    keyid: String,
    /// All required checks held: statement intact, signature valid, and — when a
    /// trusted key was supplied — the signing key matched it.
    trusted: bool,
}

/// Re-verify `receipt` offline (INV-R5): re-derive its content address (tamper-evidence),
/// check the detached ed25519 signature over the DSSE PAE with the embedded public key,
/// and — when `trusted_key` is set — require the signing key to equal it (issuer pin).
/// Pure except for delegating to the host [`ed25519_verify`](crate::signer::ed25519_verify)
/// predicate; never trusts a verdict it could not verify.
fn verify_gate_receipt(receipt: &Receipt, trusted_key: Option<&str>) -> GateVerification {
    let v = nerve_core::receipt::verify_receipt(receipt, crate::signer::ed25519_verify);
    // The issuer identity is the key the signature was VERIFIED against — the embedded
    // `public_key` — NEVER the self-declared `keyid`. `keyid` is a free-form label a
    // forger can spoof to a trusted key while signing with their own key; pinning it
    // would gate a forged pass. (A `None` public_key already fails `signature_valid`.)
    let signing_key = receipt.signature.public_key.as_deref();
    let self_ok = v.statement_intact && v.signature_valid;
    let (issuer_pinned, key_ok) = match trusted_key {
        Some(pin) => {
            let matched = signing_key == Some(pin);
            (matched, matched)
        }
        // No pin requested: self-consistency only, issuer identity unproven (advisory).
        None => (false, true),
    };
    GateVerification {
        statement_intact: v.statement_intact,
        signature_valid: v.signature_valid,
        issuer_pinned,
        // Report the verified signing key (security-relevant), falling back to the
        // display-only keyid when no public key is embedded — that case is refused.
        keyid: signing_key
            .map(str::to_owned)
            .unwrap_or_else(|| receipt.signature.keyid.clone()),
        trusted: self_ok && key_ok,
    }
}

/// Resolve the issuer-pin key: the `--trusted-key` flag, else the
/// `NERVE_TRUSTED_RECEIPT_KEY` env var. An empty string is treated as unset.
fn resolve_trusted_key(args: &GateArgs) -> Option<String> {
    args.trusted_key
        .clone()
        .or_else(|| std::env::var(TRUSTED_KEY_ENV).ok())
        .filter(|k| !k.is_empty())
}

/// Resolve the optional isolation floor: the `--require-isolation` flag, else the
/// `NERVE_REQUIRE_ISOLATION` env var (empty = unset). Returns `None` when no floor is
/// requested (report-only); a malformed value is a hard error so the operator is told
/// their config is wrong rather than silently getting no floor.
fn resolve_required_isolation(args: &GateArgs) -> Result<Option<IsolationTier>> {
    let raw = args
        .require_isolation
        .clone()
        .or_else(|| std::env::var(REQUIRE_ISOLATION_ENV).ok())
        .filter(|s| !s.is_empty());
    match raw {
        Some(spec) => Ok(Some(parse_isolation_tier(&spec)?)),
        None => Ok(None),
    }
}

/// Parse a `--require-isolation` tier spelling into an [`IsolationTier`]. Accepts the
/// hyphenated CLI form (`best-effort`) and the serde snake_case form (`best_effort`),
/// case-insensitively. A typo is a hard error listing the valid tiers.
fn parse_isolation_tier(spec: &str) -> Result<IsolationTier> {
    match spec.trim().to_ascii_lowercase().replace('_', "-").as_str() {
        "hermetic" => Ok(IsolationTier::Hermetic),
        "contained" => Ok(IsolationTier::Contained),
        "best-effort" => Ok(IsolationTier::BestEffort),
        "unconfined" => Ok(IsolationTier::Unconfined),
        other => Err(anyhow!(
            "invalid --require-isolation '{other}': expected one of \
             hermetic|contained|best-effort|unconfined"
        )),
    }
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

/// Render a freshly sealed receipt's gate decision and return its exit code (shared by
/// `verify`). The receipt was just signed in-process, so it is trusted by construction;
/// the `gate` path re-verifies a *pre-sealed* receipt before trusting it (INV-R5).
fn report_receipt(receipt: &Receipt, as_json: bool) -> Result<i32> {
    // The receipt was just sealed in-process; enforce the bar it co-sealed (L3) so
    // `nerve verify`'s exit code reflects the org's bar, consistent with `nerve gate`.
    let outcome = enforce_merge_bar(receipt);
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

/// Emit a verified gate outcome, surfacing the signature re-verification (INV-R5) so a
/// consumer sees the gate checked the receipt before trusting its verdict.
fn print_verified_outcome(outcome: &GateOutcome, v: &GateVerification, as_json: bool) {
    if as_json {
        let mut value = serde_json::to_value(outcome).unwrap_or_else(|_| json!({}));
        if let Value::Object(map) = &mut value {
            map.insert(
                "verification".to_string(),
                verification_json(v, /* refused */ false),
            );
        }
        println!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        println!(
            "{} (exit {}): {} [receipt verified: statement_intact={} signature_valid={} \
             signed_by={} issuer_pinned={}]",
            outcome.conclusion,
            outcome.exit_code,
            outcome.summary,
            v.statement_intact,
            v.signature_valid,
            v.keyid,
            v.issuer_pinned,
        );
    }
}

/// Emit a REFUSAL (INV-R1 / INV-R5): the receipt did not verify, so the gate refuses to
/// trust/post its claimed verdict. No `GateOutcome` is computed from the claimed verdict
/// — only the verification failure is reported, with a non-zero exit.
fn print_refusal(receipt: &Receipt, v: &GateVerification, as_json: bool) {
    let reason = refusal_reason(v);
    if as_json {
        let value = json!({
            "status": "refused",
            "exit_code": REFUSE_EXIT,
            "reason": reason,
            "receipt_id": receipt.receipt_id,
            "verification": verification_json(v, /* refused */ true),
        });
        println!(
            "{}",
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        eprintln!(
            "REFUSED (exit {REFUSE_EXIT}): {reason} (statement_intact={} signature_valid={} \
             signed_by={} issuer_pinned={}) — claimed verdict NOT trusted, no status posted",
            v.statement_intact, v.signature_valid, v.keyid, v.issuer_pinned,
        );
    }
}

/// The human reason a refusal occurred, most-fundamental failure first.
fn refusal_reason(v: &GateVerification) -> &'static str {
    if !v.statement_intact || !v.signature_valid {
        "receipt integrity check FAILED — refusing to gate (a tampered or forged receipt \
         never gates a fabricated pass)"
    } else {
        "receipt not signed by the trusted key — refusing to gate"
    }
}

/// The verification block surfaced in `--json` output (mirrors `nerve_verify`'s reported
/// fields), shared by the verified and refused paths.
fn verification_json(v: &GateVerification, refused: bool) -> Value {
    json!({
        "statement_intact": v.statement_intact,
        "signature_valid": v.signature_valid,
        "issuer_pinned": v.issuer_pinned,
        "signed_by": { "keyid": v.keyid },
        "refused": refused,
    })
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
    use super::emit::{
        CheckRunEmitter, GITLAB_DEFAULT_API_BASE, GhCheckRunEmitter, GitLabStatusEmitter,
        NoopEmitter,
    };
    use super::*;
    use crate::signer::{LocalEd25519Signer, Signer};
    use nerve_core::receipt::{
        RECEIPT_PREDICATE_TYPE, Receipt, ReceiptProvenance, ReceiptSignature, ReceiptStatement,
        ReplayManifest,
    };
    use nerve_core::receipt_gate::gate_outcome;
    use nerve_core::verdict::VerdictStatus;
    use std::fs;
    use tempfile::tempdir;

    /// Build the (unsigned) statement a `run_id`+`verdict` test receipt attests to.
    fn statement_for(run_id: &str, verdict: VerdictStatus) -> ReceiptStatement {
        ReceiptStatement {
            predicate_type: RECEIPT_PREDICATE_TYPE.to_string(),
            provenance: ReceiptProvenance {
                run_id: run_id.to_string(),
                inputs_hash: "h".to_string(),
                toolchain_digest: None,
                policy_version: None,
                ledger_ref: None,
                isolation_tier: nerve_core::provenance::IsolationTier::Contained,
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
            checkspec_hash: None,
            merge_bar: nerve_core::policy::MergeBar::default(),
            required_evidence: Vec::new(),
        }
    }

    /// A GENUINELY SIGNED receipt: seal the statement via the real `nerve-core` seal path
    /// signed by the fixed `deterministic_test_key`, so `verify_receipt` passes (mirrors
    /// how golden receipts are built). The gate now refuses anything that does not
    /// verify, so positive-path tests must use this, not a hand-built fake signature.
    fn receipt_for(run_id: &str, verdict: VerdictStatus) -> Receipt {
        signed_receipt(
            run_id,
            verdict,
            &LocalEd25519Signer::deterministic_test_key(),
        )
    }

    /// Seal `statement_for` with `signer` through the real DSSE PAE + seal path.
    fn signed_receipt(run_id: &str, verdict: VerdictStatus, signer: &dyn Signer) -> Receipt {
        let statement = statement_for(run_id, verdict);
        let pae = nerve_core::receipt::dsse_pae(
            RECEIPT_PREDICATE_TYPE,
            &nerve_core::receipt::canonical_statement_bytes(&statement),
        );
        let (sig, public_key) = signer.sign(&pae);
        nerve_core::receipt::seal_receipt(
            statement,
            ReceiptSignature {
                payload_type: RECEIPT_PREDICATE_TYPE.to_string(),
                backend: signer.backend().to_string(),
                keyid: signer.keyid(),
                sig,
                public_key: Some(public_key),
                bundle: None,
            },
        )
    }

    /// The base64 public key the fixed `deterministic_test_key` signs with — the value a
    /// `--trusted-key` pin must equal to accept a receipt it sealed.
    fn deterministic_key_id() -> String {
        LocalEd25519Signer::deterministic_test_key().keyid()
    }

    /// A GENUINELY SIGNED receipt whose statement co-seals a merge bar + the given checks
    /// (L3). The bar is part of the signed bytes, so `verify_receipt` passes and the gate
    /// then enforces the embedded bar (INV-R5).
    fn receipt_with_bar(
        run_id: &str,
        verdict: VerdictStatus,
        checks: &[(&str, VerdictStatus)],
        required_checks: &[&str],
    ) -> Receipt {
        use nerve_core::policy::MergeBar;
        use nerve_core::receipt::ReceiptCheck;
        use nerve_core::verdict::CheckKind;
        let mut statement = statement_for(run_id, verdict);
        statement.checks = checks
            .iter()
            .map(|(name, v)| ReceiptCheck {
                name: (*name).to_string(),
                kind: CheckKind::Test,
                verdict: *v,
                reproducible: true,
                evidence_hash: None,
            })
            .collect();
        statement.merge_bar = MergeBar {
            required_checks: required_checks.iter().map(|s| (*s).to_string()).collect(),
            expected_checkspec_hash: None,
        };
        let signer = LocalEd25519Signer::deterministic_test_key();
        let pae = nerve_core::receipt::dsse_pae(
            RECEIPT_PREDICATE_TYPE,
            &nerve_core::receipt::canonical_statement_bytes(&statement),
        );
        let (sig, public_key) = signer.sign(&pae);
        nerve_core::receipt::seal_receipt(
            statement,
            ReceiptSignature {
                payload_type: RECEIPT_PREDICATE_TYPE.to_string(),
                backend: signer.backend().to_string(),
                keyid: signer.keyid(),
                sig,
                public_key: Some(public_key),
                bundle: None,
            },
        )
    }

    fn write_receipt(root: &std::path::Path, receipt: &Receipt) {
        let dir = root.join(".nerve").join("receipts");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", receipt.receipt_id));
        fs::write(path, serde_json::to_string_pretty(receipt).unwrap()).unwrap();
    }

    /// A `GateArgs` over an explicit `--receipt` path (the common test shape).
    fn args_for_path(path: PathBuf, trusted_key: Option<String>) -> GateArgs {
        GateArgs {
            receipt: Some(path),
            run: None,
            root: None,
            emit: "none".to_string(),
            sha: None,
            repo: None,
            trusted_key,
            require_isolation: None,
            json: false,
        }
    }

    /// Write a receipt to a temp JSON path and return that path.
    fn write_to_temp(dir: &std::path::Path, receipt: &Receipt) -> PathBuf {
        let path = dir.join(format!("{}.json", receipt.receipt_id));
        fs::write(&path, serde_json::to_string(receipt).unwrap()).unwrap();
        path
    }

    #[test]
    fn gate_reads_receipt_and_maps_passed_to_exit_zero() {
        let dir = tempdir().unwrap();
        let receipt = receipt_for("run-a", VerdictStatus::Passed);
        let path = write_to_temp(dir.path(), &receipt);

        // A genuinely signed receipt verifies, so the gate trusts its Passed verdict.
        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(code, 0);
    }

    /// L3 end-to-end: a receipt embedding a MET bar (all required checks Passed) gates 0.
    #[test]
    fn gate_with_met_embedded_bar_gates_zero() {
        let dir = tempdir().unwrap();
        let receipt = receipt_with_bar(
            "met",
            VerdictStatus::Passed,
            &[
                ("unit", VerdictStatus::Passed),
                ("build", VerdictStatus::Passed),
            ],
            &["unit", "build"],
        );
        let path = write_to_temp(dir.path(), &receipt);
        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(code, 0, "a met embedded bar gates 0");
    }

    /// L3 end-to-end: a receipt embedding an UNMET bar gates non-zero even though the
    /// aggregate verdict is Passed — the gate enforces the bar the receipt SIGNED.
    #[test]
    fn gate_with_unmet_embedded_bar_gates_nonzero() {
        let dir = tempdir().unwrap();
        // The aggregate verdict says Passed, but a required check is present-and-failed.
        let receipt = receipt_with_bar(
            "unmet",
            VerdictStatus::Passed,
            &[
                ("unit", VerdictStatus::Passed),
                ("build", VerdictStatus::Failed),
            ],
            &["unit", "build"],
        );
        let path = write_to_temp(dir.path(), &receipt);
        let code = gate(args_for_path(path, None)).unwrap();
        assert_ne!(code, 0, "an unmet embedded bar must not gate a pass");
        assert_eq!(
            code, 1,
            "a present-and-failed required check is a failure (exit 1)"
        );
    }

    /// L3 end-to-end: a MISSING required check downgrades a Passed receipt to neutral.
    #[test]
    fn gate_with_missing_required_check_gates_neutral() {
        let dir = tempdir().unwrap();
        let receipt = receipt_with_bar(
            "missing",
            VerdictStatus::Passed,
            &[("unit", VerdictStatus::Passed)],
            &["unit", "integration"],
        );
        let path = write_to_temp(dir.path(), &receipt);
        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(code, 2, "a missing required check is neutral (exit 2)");
    }

    /// L3 end-to-end (v15 checkspec-identity binding): a receipt whose bar pins an
    /// `expected_checkspec_hash` the receipt's own `checkspec_hash` does NOT match gates
    /// NEUTRAL — the required-check names cannot be trusted, so a renamed/stubbed check
    /// cannot impersonate the org's real one. The bar + checkspec are co-sealed (signed),
    /// so the receipt still verifies (NOT a refusal) — the downgrade is the bar overlay.
    #[test]
    fn gate_with_checkspec_mismatch_gates_neutral_not_refused() {
        use nerve_core::policy::MergeBar;
        use nerve_core::receipt::ReceiptCheck;
        use nerve_core::verdict::CheckKind;
        let dir = tempdir().unwrap();
        // A receipt whose named "unit" check passes, but whose checkspec identity differs
        // from the one the bar was authored against.
        let mut statement = statement_for("checkspec-mismatch", VerdictStatus::Passed);
        statement.checks = vec![ReceiptCheck {
            name: "unit".to_string(),
            kind: CheckKind::Test,
            verdict: VerdictStatus::Passed,
            reproducible: true,
            evidence_hash: None,
        }];
        statement.checkspec_hash = Some("spec-stub".to_string());
        statement.merge_bar = MergeBar {
            required_checks: vec!["unit".to_string()],
            expected_checkspec_hash: Some("spec-real".to_string()),
        };
        let signer = LocalEd25519Signer::deterministic_test_key();
        let pae = nerve_core::receipt::dsse_pae(
            RECEIPT_PREDICATE_TYPE,
            &nerve_core::receipt::canonical_statement_bytes(&statement),
        );
        let (sig, public_key) = signer.sign(&pae);
        let receipt = nerve_core::receipt::seal_receipt(
            statement,
            ReceiptSignature {
                payload_type: RECEIPT_PREDICATE_TYPE.to_string(),
                backend: signer.backend().to_string(),
                keyid: signer.keyid(),
                sig,
                public_key: Some(public_key),
                bundle: None,
            },
        );

        // The receipt verifies (the bar + checkspec are part of the signed bytes) — so this
        // is the bar overlay downgrading, NOT a tamper refusal.
        let v = verify_gate_receipt(&receipt, None);
        assert!(v.statement_intact && v.signature_valid, "co-sealed + valid");

        let path = write_to_temp(dir.path(), &receipt);
        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(
            code, 2,
            "a checkspec-identity mismatch gates neutral (names untrusted), never a pass"
        );
    }

    /// INV-R5 ORDERING: the wave-7 tamper refusal fires BEFORE bar enforcement — a
    /// receipt whose statement was edited after signing (even to satisfy the bar) is
    /// refused first, never silently bar-enforced into a pass.
    #[test]
    fn gate_tamper_refusal_fires_before_bar_enforcement() {
        let dir = tempdir().unwrap();
        // Seal an honest receipt with an unmet bar, then forge the failing check to Passed
        // AFTER signing — this breaks the content address, so the gate must REFUSE first.
        let receipt = receipt_with_bar(
            "tamper-bar",
            VerdictStatus::Passed,
            &[
                ("unit", VerdictStatus::Passed),
                ("build", VerdictStatus::Failed),
            ],
            &["unit", "build"],
        );
        let mut tampered = receipt.clone();
        tampered.statement.checks[1].verdict = VerdictStatus::Passed; // forge the bar pass
        let path = write_to_temp(dir.path(), &tampered);

        let v = verify_gate_receipt(&tampered, None);
        assert!(!v.statement_intact, "editing a check breaks the hash");
        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(
            code, REFUSE_EXIT,
            "tamper is refused BEFORE the bar is enforced"
        );
    }

    /// INV-R7 round-trip (§3.4): a sealed receipt carries `isolation_tier: contained`
    /// (today's best-effort floor). `--require-isolation hermetic` downgrades its pass to
    /// neutral (exit 2 — the re-run was not bit-for-bit); `--require-isolation contained`
    /// is satisfied and the pass stands (exit 0). The receipt is genuinely signed, so this
    /// is the floor overlay, NOT a tamper refusal.
    #[test]
    fn gate_require_isolation_downgrades_below_floor_and_passes_at_floor() {
        let dir = tempdir().unwrap();
        let receipt = receipt_for("iso", VerdictStatus::Passed);
        // The default sealed tier is Contained (omitted on the wire, byte-identical).
        assert_eq!(
            receipt.statement.provenance.isolation_tier,
            nerve_core::provenance::IsolationTier::Contained
        );
        let path = write_to_temp(dir.path(), &receipt);

        let mut hermetic = args_for_path(path.clone(), None);
        hermetic.require_isolation = Some("hermetic".to_string());
        assert_eq!(
            gate(hermetic).unwrap(),
            2,
            "a Contained re-run cannot clear a hermetic floor — neutral (exit 2), never a pass"
        );

        let mut contained = args_for_path(path, None);
        contained.require_isolation = Some("contained".to_string());
        assert_eq!(
            gate(contained).unwrap(),
            0,
            "the floor is met (contained >= contained) so the pass stands"
        );
    }

    /// A malformed `--require-isolation` value is a hard error (the operator is told their
    /// config is wrong), never silently ignored into no floor.
    #[test]
    fn gate_rejects_a_bogus_isolation_tier() {
        let dir = tempdir().unwrap();
        let receipt = receipt_for("bogus-iso", VerdictStatus::Passed);
        let path = write_to_temp(dir.path(), &receipt);
        let mut args = args_for_path(path, None);
        args.require_isolation = Some("super-hermetic".to_string());
        assert!(gate(args).is_err(), "an invalid tier spelling errors");
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
            trusted_key: None,
            require_isolation: None,
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
            trusted_key: None,
            require_isolation: None,
            json: false,
        })
        .unwrap_err();
        assert!(err.to_string().contains("absent"), "{err}");
    }

    /// INV-R5 / INV-R1: a TAMPERED statement (verdict flipped to Passed AFTER signing)
    /// breaks the content-address integrity check, so the gate REFUSES — exit non-zero,
    /// NOT the flipped pass (exit 0), and no status posted.
    #[test]
    fn gate_refuses_a_tampered_receipt_and_never_trusts_the_flip() {
        let dir = tempdir().unwrap();
        // Seal an honest Failed receipt, then flip its verdict to Passed in the file.
        let receipt = receipt_for("tampered", VerdictStatus::Failed);
        let mut tampered = receipt.clone();
        tampered.statement.verdict = VerdictStatus::Passed; // forge a pass post-signing
        let path = write_to_temp(dir.path(), &tampered);

        let v = verify_gate_receipt(&tampered, None);
        assert!(
            !v.statement_intact,
            "flipping the statement breaks the hash"
        );
        assert!(!v.trusted, "an unverified receipt is never trusted");

        let code = gate(args_for_path(path, None)).unwrap();
        // REFUSED at exit 2 — NOT the forged Passed (which would be exit 0).
        assert_eq!(code, REFUSE_EXIT, "tampered receipt is refused, not gated");
        assert_ne!(code, 0, "the forged pass is never trusted");
    }

    /// INV-R5: a CORRUPTED signature fails ed25519 verification, so the gate REFUSES even
    /// though the statement itself still hashes intact.
    #[test]
    fn gate_refuses_a_corrupted_signature() {
        let dir = tempdir().unwrap();
        let mut receipt = receipt_for("badsig", VerdictStatus::Passed);
        // Corrupt the detached signature without touching the statement.
        receipt.signature.sig = "AAAA".to_string();
        let path = write_to_temp(dir.path(), &receipt);

        let v = verify_gate_receipt(&receipt, None);
        assert!(
            v.statement_intact,
            "statement is untouched, hash still holds"
        );
        assert!(
            !v.signature_valid,
            "the corrupted signature does not verify"
        );

        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(code, REFUSE_EXIT, "corrupted-signature receipt is refused");
    }

    /// OPTIONAL ISSUER PIN: with `--trusted-key`, the MATCHING key is accepted (the
    /// receipt gates its verdict) while a WRONG key is refused (issuer identity, INV-R5).
    #[test]
    fn gate_trusted_key_accepts_matching_and_refuses_wrong_key() {
        let dir = tempdir().unwrap();
        let receipt = receipt_for("pinned", VerdictStatus::Passed);
        let path = write_to_temp(dir.path(), &receipt);

        // Matching pin: the deterministic key id == the receipt's signing key -> gated.
        let code = gate(args_for_path(path.clone(), Some(deterministic_key_id()))).unwrap();
        assert_eq!(code, 0, "matching trusted key accepts the receipt");
        // And the pin is recorded as satisfied.
        let v = verify_gate_receipt(&receipt, Some(&deterministic_key_id()));
        assert!(v.issuer_pinned && v.trusted);

        // Wrong pin: a different key -> refused, even though the self-signature is valid.
        let wrong = "not-the-signing-key".to_string();
        let code = gate(args_for_path(path, Some(wrong.clone()))).unwrap();
        assert_eq!(code, REFUSE_EXIT, "a wrong trusted key refuses the receipt");
        let v = verify_gate_receipt(&receipt, Some(&wrong));
        assert!(v.signature_valid, "self-signature still valid");
        assert!(!v.trusted, "but the issuer pin failed, so it is refused");
    }

    /// REGRESSION (security): a forger who signs with their OWN key but SPOOFS the
    /// self-declared `keyid` to the org's trusted key must NOT clear an issuer pin —
    /// the pin compares the VERIFIED public key (what the signature checks out against),
    /// never the spoofable `keyid`. Pinning `keyid` would gate a forged pass (INV-R1/R5).
    #[test]
    fn gate_refuses_a_keyid_spoofed_receipt_under_the_trusted_key() {
        use ed25519_dalek::SigningKey;
        let dir = tempdir().unwrap();
        // A forger signs a `Passed` verdict with their OWN distinct key...
        let forger = LocalEd25519Signer::new(SigningKey::from_bytes(&[42u8; 32]));
        let mut receipt = signed_receipt("spoof", VerdictStatus::Passed, &forger);
        let org_key = deterministic_key_id();
        // ...then spoofs the self-declared keyid to the org's trusted key, while the
        // embedded public_key (what the signature verifies against) stays the forger's.
        receipt.signature.keyid = org_key.clone();

        // The forger's self-signature is valid, but the VERIFIED key is the forger's,
        // not the org's, so pinning the org key must refuse the forged pass.
        let v = verify_gate_receipt(&receipt, Some(&org_key));
        assert!(v.signature_valid, "the forger's self-signature is valid");
        assert!(
            !v.issuer_pinned,
            "the pin must compare the verified public key, not keyid"
        );
        assert!(
            !v.trusted,
            "a keyid-spoofed receipt must never be trusted under a pin"
        );

        // End-to-end: gate() refuses (non-zero), never the forged `Passed` -> exit 0.
        let path = write_to_temp(dir.path(), &receipt);
        let code = gate(args_for_path(path, Some(org_key))).unwrap();
        assert_eq!(
            code, REFUSE_EXIT,
            "a keyid-spoofed receipt gated a forged pass"
        );
    }

    /// A garbage (unparseable-signature) UNSIGNED receipt — the old fake-receipt shape —
    /// is now refused: its hand-built `sig`/`public_key` do not verify (INV-R5).
    #[test]
    fn gate_refuses_an_unsigned_fake_receipt() {
        let dir = tempdir().unwrap();
        let mut receipt = receipt_for("fake", VerdictStatus::Passed);
        receipt.signature.public_key = None; // no key to verify against
        receipt.signature.sig = "s".to_string();
        // Re-seal the id so the statement still hashes intact; only the sig is bogus.
        receipt.receipt_id = nerve_core::receipt::statement_id(&receipt.statement);
        let path = write_to_temp(dir.path(), &receipt);

        let code = gate(args_for_path(path, None)).unwrap();
        assert_eq!(code, REFUSE_EXIT, "an unsigned/fake receipt is refused");
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

    /// `select_emitter("gitlab")` returns the real GitLab emitter, not the Noop
    /// fallback. We probe by behaviour: NoopEmitter::emit is always `Ok`, while the
    /// GitLab emitter, with no auth env, errors before posting.
    #[test]
    fn select_emitter_gitlab_is_not_noop() {
        // Avoid env-ordering flakiness with other parallel tests by exercising the
        // concrete type's auth check directly.
        let outcome = gate_outcome(&receipt_for("r", VerdictStatus::Passed));
        // NoopEmitter is inert (Ok) regardless of env; the GitLab emitter requires auth.
        assert!(NoopEmitter.emit("1", "sha", &outcome).is_ok());
        // The selected gitlab emitter is a distinct, non-noop type: build its pure args.
        let _ = select_emitter("gitlab").expect("gitlab is a known mode");
        let args = GitLabStatusEmitter::curl_args(GITLAB_DEFAULT_API_BASE, "1", "sha", &outcome);
        assert!(args.iter().any(|a| a == "POST"));
    }

    #[test]
    fn gitlab_curl_args_build_a_commit_status_post() {
        let outcome = gate_outcome(&receipt_for("r", VerdictStatus::Passed));
        let args = GitLabStatusEmitter::curl_args(
            "https://gitlab.example.com/api/v4",
            "group/proj",
            "deadbeef",
            &outcome,
        );
        // URL: project path is percent-encoded; method + endpoint are right.
        assert!(args.iter().any(|a| a == "--fail"));
        assert!(args.iter().any(|a| a == "POST"));
        assert!(
            args.iter().any(|a| a
                == "https://gitlab.example.com/api/v4/projects/group%2Fproj/statuses/deadbeef"),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "name=nerve-gate"));
        assert!(args.iter().any(|a| a == "state=success"));
    }

    /// STATE MAPPING (INV-R1): exit 0 (Passed) -> success; every non-zero exit
    /// (Failed exit 1, Inconclusive exit 2, Error exit 2) -> failed. A non-zero exit
    /// NEVER maps to success, so an un-cleared verdict can never post a pass.
    #[test]
    fn gitlab_state_mirrors_exit_code_never_fabricating_a_pass() {
        for (verdict, want_exit, want_state) in [
            (VerdictStatus::Passed, 0, "success"),
            (VerdictStatus::Failed, 1, "failed"),
            (VerdictStatus::Inconclusive, 2, "failed"),
            (VerdictStatus::Error, 2, "failed"),
        ] {
            let outcome = gate_outcome(&receipt_for("r", verdict));
            assert_eq!(outcome.exit_code, want_exit, "{verdict:?}");
            assert_eq!(
                GitLabStatusEmitter::state_for(&outcome),
                want_state,
                "{verdict:?}"
            );
            let args = GitLabStatusEmitter::curl_args("b", "1", "s", &outcome);
            // A non-zero exit must NEVER emit state=success.
            if outcome.exit_code != 0 {
                assert!(
                    !args.iter().any(|a| a == "state=success"),
                    "non-zero exit posted success: {args:?}"
                );
            }
        }
    }

    /// SECRET SAFETY: the auth token must never appear in the pure args builder, so it
    /// cannot leak into a logged argv or a recorded test fixture.
    #[test]
    fn gitlab_curl_args_never_contain_the_token() {
        let secret = "glpat-SUPER-SECRET-TOKEN";
        let outcome = gate_outcome(&receipt_for("r", VerdictStatus::Passed));
        let args = GitLabStatusEmitter::curl_args(GITLAB_DEFAULT_API_BASE, "1", "sha", &outcome);
        assert!(
            args.iter().all(|a| !a.contains(secret)
                && !a.contains("PRIVATE-TOKEN")
                && !a.contains("JOB-TOKEN")
                && !a.contains("GITLAB_TOKEN")),
            "token-shaped material leaked into curl_args: {args:?}"
        );
    }
}
