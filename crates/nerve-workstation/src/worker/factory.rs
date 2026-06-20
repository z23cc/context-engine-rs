//! [`WorkerFactory`] — the registry the C1 engine will call to mint workers.
//!
//! Holds the shared deps both substrates need (the delegate launcher + codex MCP
//! allowlist for [`CliWorker`]s; the runtime / provider registry / gate / max-depth
//! for [`ProviderWorker`]s) and maps a [`WorkerKind`] to a boxed [`AgentWorker`].
//! This is the single seam the conductor (C1) resolves workers through.
//!
//! C0 is ADDITIVE: the factory exists and is unit-tested, but the shipped
//! `delegate_agent` / `spawn_agent` tools are NOT rewritten to call it in this wave
//! (that collapse is design §2's bonus, landed alongside the engine).
#![allow(
    dead_code,
    reason = "C0 worker port awaits its C1 engine caller (mirrors subagent::bounded_fan_out)"
)]

use super::{AgentWorker, CliWorker, ProviderWorker, WorkerError, WorkerKind};
use crate::delegate_codex_mcp::delegate_disable_flags;
use crate::delegate_runtime::DelegateAgent;
use crate::policy::ToolGate;
use crate::providers::ProviderRegistry;
use crate::sandbox::SandboxLauncher;
use crate::tools::NerveRuntime;
use std::sync::Arc;

/// The shared-deps registry that mints workers. Cloneable (its deps are all `Arc`/
/// cheap clones) so the engine can hand it to fan-out workers.
#[derive(Clone)]
pub(crate) struct WorkerFactory {
    /// Trust-bound launcher for CLI workers (a refusing launcher when delegation is
    /// off — defence in depth, exactly like the daemon's `delegate_launcher`).
    delegate_launcher: Arc<dyn SandboxLauncher>,
    /// The runtime the provider workers reach tools through (the shared snapshot).
    runtime: Arc<NerveRuntime>,
    registry: ProviderRegistry,
    gate: ToolGate,
    max_depth: usize,
}

impl WorkerFactory {
    /// Build the factory over the shared deps.
    pub(crate) fn new(
        delegate_launcher: Arc<dyn SandboxLauncher>,
        runtime: Arc<NerveRuntime>,
        registry: ProviderRegistry,
        gate: ToolGate,
        max_depth: usize,
    ) -> Self {
        Self {
            delegate_launcher,
            runtime,
            registry,
            gate,
            max_depth,
        }
    }

    /// Mint a worker for `kind`. A `Cli` kind resolves its [`DelegateAgent`] (an
    /// unknown name errors before any spawn) and pre-computes the codex MCP-disable
    /// flags from the effective allowlist; a `Provider` kind builds a
    /// [`ProviderWorker`] over the shared runtime/registry/gate.
    pub(crate) fn create(&self, kind: WorkerKind) -> Result<Box<dyn AgentWorker>, WorkerError> {
        match kind {
            WorkerKind::Cli(name) => {
                let agent = DelegateAgent::from_name(name)
                    .map_err(|err| WorkerError::Start(err.to_string()))?;
                // Effective codex allowlist from config (per-call override is a C1
                // concern once the engine carries per-node MCP enables).
                let flags = delegate_disable_flags(agent, None);
                Ok(Box::new(CliWorker::new(
                    agent,
                    Arc::clone(&self.delegate_launcher),
                    flags,
                )))
            }
            WorkerKind::Provider { provider, model } => Ok(Box::new(ProviderWorker::new(
                Arc::clone(&self.runtime),
                self.registry.clone(),
                self.gate.clone(),
                self.max_depth,
                provider,
                model,
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    fn factory() -> WorkerFactory {
        let registry = ProviderRegistry::default();
        let runtime = Arc::new(crate::tools::runtime(
            nerve_core::WorkspaceRegistry::default(),
        ));
        WorkerFactory::new(
            crate::sandbox::refuse_launcher(),
            runtime,
            registry,
            ToolGate::deny(Policy::default()),
            crate::subagent::DEFAULT_MAX_DEPTH,
        )
    }

    #[test]
    fn create_cli_worker_for_known_agent() {
        let worker = factory()
            .create(WorkerKind::Cli("claude"))
            .expect("claude is a known agent");
        assert_eq!(worker.kind(), WorkerKind::Cli("claude"));
        assert_eq!(worker.capability(), nerve_runtime::RiskTier::Exec);
    }

    #[test]
    fn create_cli_worker_rejects_unknown_agent() {
        // `Box<dyn AgentWorker>` is not `Debug`, so match rather than `expect_err`.
        match factory().create(WorkerKind::Cli("rovo")) {
            Ok(_) => panic!("unknown agent must be rejected before any spawn"),
            Err(err) => assert!(err.to_string().contains("rovo"), "{err}"),
        }
    }

    #[test]
    fn create_provider_worker_carries_provider_and_model() {
        let worker = factory()
            .create(WorkerKind::Provider {
                provider: "anthropic".into(),
                model: "claude-opus-4-8".into(),
            })
            .expect("provider worker builds");
        assert_eq!(
            worker.kind(),
            WorkerKind::Provider {
                provider: "anthropic".into(),
                model: "claude-opus-4-8".into(),
            }
        );
        // A provider worker reaches the edit tier at worst (exec/delegate refused).
        assert_eq!(worker.capability(), nerve_runtime::RiskTier::Edit);
    }
}
