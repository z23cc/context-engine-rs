//! Trust-substrate (L-series) command executors for the [`JobManager`].
//!
//! These are the `run.*` / `replay.*` / `ledger.*` / `verify.*` / `policy.*` /
//! `receipt.*` / `outcome.*` families plus the per-scope store resolvers and the
//! authorization/attestation helpers. Each is an `impl JobManager` method so it
//! reaches the manager's private deps (served root, launcher, emit) directly; the
//! child module sees those private fields because privacy is visible-to-descendants.

use super::{JobManager, now_ms};
use nerve_core::CancelToken;
use nerve_runtime::{DelegateAutonomy, DelegateRole, RuntimeCommand, RuntimeEvent};
use serde_json::{Value, json};

impl JobManager {
    /// Execute a `run.*` command (L0 flight-recorder, read-only): enumerate or fetch
    /// captured Runs from the persisted [`RunStore`](crate::run_store) for the served
    /// root. No served root => an empty list / not-found, mirroring `delegate.*`.
    pub(super) fn run_run_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::RunList => {
                Ok(crate::run_store::run_run_list(self.run_store().as_ref()))
            }
            RuntimeCommand::RunGet { run_id } => {
                crate::run_store::run_run_get(&run_id, self.run_store().as_ref())
            }
            RuntimeCommand::OtelIngest { source, .. } => {
                let local = match source {
                    nerve_runtime::OtelSource::Inline { trace } => {
                        crate::otel_ingest::OtelSource::Inline { trace }
                    }
                    nerve_runtime::OtelSource::Path { trace_path } => {
                        crate::otel_ingest::OtelSource::Path { trace_path }
                    }
                };
                crate::otel_ingest::handle_otel_ingest(
                    &local,
                    self.run_store().as_ref(),
                    self.delegate_root().ok().as_deref(),
                )
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected a run.* command",
            )),
        }
    }

    /// L0c — `replay.start`: re-drive a captured Run's tape and verify its content
    /// address (handler in `crate::replay`).
    pub(super) fn run_replay_command(
        &self,
        job_id: &str,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::ReplayStart { run_id } => {
                let emit = |event: RuntimeEvent| self.emit(event);
                crate::replay::handle_replay_start(
                    &run_id,
                    job_id,
                    self.run_store().as_ref(),
                    &emit,
                    token,
                )
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected replay.* command",
            )),
        }
    }

    /// L1 — `ledger.query`: read the append-only evidence ledger (handler in
    /// `crate::ledger_store`).
    pub(super) fn run_ledger_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::LedgerQuery {
                run_id,
                agent,
                diff_hash,
                outcome,
                record_kind,
                limit,
            } => Ok(crate::ledger_store::run_ledger_query(
                self.ledger_store().as_ref(),
                run_id.as_deref(),
                agent.as_deref(),
                diff_hash.as_deref(),
                outcome,
                record_kind.as_deref(),
                limit.unwrap_or(200),
            )),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected ledger.* command",
            )),
        }
    }

    /// L2 — `verify.*`: re-run the org's checks in the closure and seal/fetch the
    /// borrowed verdict (handlers in `crate::verify_runner`). On a fresh verify it
    /// also announces `VerificationCompleted` and appends the verdict to the ledger.
    pub(super) fn run_verify_command(
        &self,
        command: RuntimeCommand,
        token: &CancelToken,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::VerifyStart {
                run_id,
                reruns,
                only,
            } => {
                let root = self.delegate_root()?;
                let verdict = crate::verify_runner::handle_verify_start(
                    self.run_store().as_ref(),
                    self.verify_store().as_ref(),
                    &self.delegate_launcher,
                    &root,
                    &run_id,
                    reruns,
                    only.as_deref(),
                    token,
                    now_ms(),
                )?;
                self.emit(RuntimeEvent::VerificationCompleted {
                    run_id: verdict.run_id.clone(),
                    verdict_id: verdict.verdict_id.clone(),
                    status: verdict.status,
                    check_count: verdict.checks.len() as u64,
                });
                self.attest_verdict(&run_id, &verdict);
                serde_json::to_value(&verdict)
                    .map(|verdict| json!({ "verdict": verdict }))
                    .map_err(|err| nerve_runtime::RuntimeError::adapter(err.to_string()))
            }
            RuntimeCommand::VerifyGet { verdict_id } => {
                crate::verify_runner::handle_verify_get(&verdict_id, self.verify_store().as_ref())
            }
            RuntimeCommand::VerifyList { run_id } => Ok(crate::verify_runner::handle_verify_list(
                self.verify_store().as_ref(),
                run_id.as_deref(),
            )),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected verify.* command",
            )),
        }
    }

    /// L3 — `policy.*`: serve the sealed policy doc + decision evidence (handlers in
    /// `crate::policy_plane`).
    pub(super) fn run_policy_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        let plane = self.policy_plane();
        match command {
            RuntimeCommand::PolicyGet => Ok(crate::policy_plane::run_policy_get(plane.as_ref())),
            RuntimeCommand::PolicyDecisions { session_id } => {
                Ok(crate::policy_plane::run_policy_decisions(
                    session_id.as_deref(),
                    plane.as_ref(),
                    self.ledger_store().as_ref(),
                ))
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected policy.* command",
            )),
        }
    }

    /// L4 — `receipt.get`: fetch a signed Verification Receipt (handler in
    /// `crate::receipt_store`).
    pub(super) fn run_receipt_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::ReceiptGet { receipt_id } => {
                crate::receipt_store::run_receipt_get(&receipt_id, self.receipt_store().as_ref())
            }
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected receipt.* command",
            )),
        }
    }

    /// L6 — `outcome.*`: append/get/query human/CI outcome labels (handlers in
    /// `crate::outcome_store`); a label append announces `OutcomeLabeled`.
    pub(super) fn run_outcome_command(
        &self,
        command: RuntimeCommand,
    ) -> Result<Value, nerve_runtime::RuntimeError> {
        match command {
            RuntimeCommand::OutcomeLabel {
                run_id,
                outcome,
                source,
                actor,
                note,
                verdict_ref,
            } => {
                let (payload, run_id, labels_root, label_count) =
                    crate::outcome_store::handle_outcome_label(
                        &run_id,
                        outcome,
                        source,
                        actor,
                        note,
                        verdict_ref,
                        self.outcome_store().as_ref(),
                    )?;
                self.emit(RuntimeEvent::OutcomeLabeled {
                    run_id,
                    session_id: None,
                    outcome,
                    labels_root,
                    label_count,
                });
                Ok(payload)
            }
            RuntimeCommand::OutcomeGet { run_id } => {
                crate::outcome_store::handle_outcome_get(&run_id, self.outcome_store().as_ref())
            }
            RuntimeCommand::OutcomeQuery {
                agent,
                outcome,
                limit,
            } => Ok(crate::outcome_store::handle_outcome_query(
                agent.as_deref(),
                outcome,
                limit.unwrap_or(200),
                self.outcome_store().as_ref(),
            )),
            _ => Err(nerve_runtime::RuntimeError::adapter(
                "expected outcome.* command",
            )),
        }
    }

    /// L1 evidence ledger store for the served root (mirrors `run_store`).
    pub(super) fn ledger_store(&self) -> Option<crate::ledger_store::LedgerStore> {
        crate::ledger_store::LedgerStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L2 verdict store for the served root.
    pub(super) fn verify_store(&self) -> Option<crate::verify_store::VerifyStore> {
        crate::verify_store::VerifyStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L4 receipt store for the served root.
    fn receipt_store(&self) -> Option<crate::receipt_store::ReceiptStore> {
        crate::receipt_store::ReceiptStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L6 outcome corpus store for the served root.
    fn outcome_store(&self) -> Option<crate::outcome_store::OutcomeStore> {
        crate::outcome_store::OutcomeStore::for_scope(self.delegate_root().ok().as_deref()).ok()
    }

    /// L3 policy plane for the served root (sealed policy doc + a null evidence sink;
    /// the ledger-backed sink is wired when L3↔L1 is promoted).
    fn policy_plane(&self) -> Option<crate::policy_plane::PolicyPlane> {
        let root = self.delegate_root().ok();
        // Wire the L1-backed evidence sink when a served scope resolves a ledger, so
        // recorded policy decisions land in the ledger (the live L3↔L1 link); fall back
        // to the no-op sink when there is no served root.
        Some(match self.ledger_store() {
            Some(store) => crate::policy_plane::PolicyPlane::with_ledger(root.as_deref(), store),
            None => crate::policy_plane::PolicyPlane::resolve(root.as_deref()),
        })
    }

    /// L3↔L1 — record what Nerve authorized a delegated agent to do (fs/exec ceiling +
    /// always-on egress) to the L1 evidence ledger via the policy plane, announcing each
    /// recorded decision. The posture mapping + record building is the shared
    /// [`crate::policy_plane::record_delegate_authorization`] (also used by the in-chat
    /// `delegate_agent` ToolBox path); this method adds the protocol event emission a
    /// `delegate.start` job has. Best-effort — never fails the start.
    pub(super) fn record_delegate_authorization(
        &self,
        job_id: &str,
        agent: &str,
        role: DelegateRole,
        autonomy: DelegateAutonomy,
    ) {
        let Some(plane) = self.policy_plane() else {
            return;
        };
        for (record, ledger_seq) in crate::policy_plane::record_delegate_authorization(
            &plane, job_id, agent, role, autonomy,
        ) {
            self.emit(RuntimeEvent::PolicyDecisionRecorded { record, ledger_seq });
        }
    }

    /// L1+L4 — attest a sealed verdict: append it to the evidence ledger and issue a
    /// signed Verification Receipt (best-effort), then announce `ReceiptIssued`. The
    /// append+issue+sign+persist tail is the SINGLE canonical
    /// [`crate::verify_runner::seal_and_attest`] (shared verbatim with the `nerve verify`
    /// CLI, INV-R1); this method adds only the daemon's run reload + event emission. A
    /// missing run/store is a silent no-op — attesting never fails the verify turn.
    fn attest_verdict(&self, run_id: &str, verdict: &nerve_core::verdict::Verdict) {
        let Some(run) = self
            .run_store()
            .and_then(|store| store.load_record(run_id).ok())
        else {
            return;
        };
        let ledger = self.ledger_store();
        let receipt = self.receipt_store();
        let stores = crate::verify_runner::AttestStores {
            ledger: ledger.as_ref(),
            receipt: receipt.as_ref(),
        };
        if let Some(sealed) =
            crate::verify_runner::seal_and_attest(&run, verdict, &stores, &self.signer(), now_ms())
        {
            self.emit(RuntimeEvent::ReceiptIssued {
                session_id: run.session_id.clone(),
                run_id: sealed.statement.provenance.run_id.clone(),
                receipt_id: sealed.receipt_id.clone(),
                verdict: sealed.statement.verdict,
            });
        }
    }

    /// The local ed25519 receipt signer, keyed under `config_home()/keys` (stable
    /// across projects), falling back to the served root's `.nerve/keys`. Delegates to
    /// the shared [`crate::signer::local_signer`] so the CLI re-verify path signs with
    /// the same per-host key.
    fn signer(&self) -> crate::signer::LocalEd25519Signer {
        crate::signer::local_signer(self.delegate_root().ok().as_deref())
    }
}
