//! [`WorkerFactory`] — the registry the flow engine calls to mint workers.
//!
//! Holds the shared deps both substrates need (the delegate launcher + codex MCP
//! allowlist for [`CliWorker`]s; the runtime / provider registry / gate / max-depth
//! for [`ProviderWorker`]s) and maps a [`WorkerRef`] to a boxed [`AgentWorker`].
//! This is the single seam the conductor resolves workers through.
//!
//! ## C6: worker-as-data + remote/MCP adapters
//!
//! The factory consults a [`WorkerRegistry`] so a [`WorkerRef::Named`] resolves to a
//! discovered [`WorkerDef`] (project > global > built-in, "loaded, not compiled") —
//! the engine still only ever sees an [`AgentWorker`]. A def may name a
//! [`RemoteWorker`] or [`McpWorker`]; those are EXEC-TIER and **refused-by-default**:
//! the factory mints them only when the fleet was explicitly opened (`allow_remote`,
//! mirroring `--allow-delegate`) — security before openness (design §6/§9).

use super::registry::{ResolvedWorker, WorkerDefKind, WorkerRegistry};
use super::remote::{McpEndpoint, McpWorker, RemoteEndpoint, RemoteWorker};
use super::{AgentWorker, CliWorker, ProviderWorker, WorkerError, WorkerKind};
use crate::delegate_codex_mcp::delegate_disable_flags;
use crate::delegate_runtime::DelegateAgent;
use crate::policy::ToolGate;
use crate::providers::ProviderRegistry;
use crate::sandbox::SandboxLauncher;
use crate::tools::NerveRuntime;
use nerve_runtime::WorkerRef;
use std::sync::Arc;

/// How the factory connects a `remote`/`mcp` [`WorkerDef`] to a live transport. The
/// production transports (spawn `nerve daemon --stdio` + the protocol client; an
/// MCP-client connection) are the documented follow-on; a host injects a connector
/// so the factory stays composition-only and the adapters stay hermetically testable.
pub(crate) trait RemoteConnector: Send + Sync {
    /// Build the transport for a `remote` def's `endpoint`.
    fn remote(&self, endpoint: &str) -> Result<Arc<dyn RemoteEndpoint>, WorkerError>;
    /// Build the transport for an `mcp` def's `server`.
    fn mcp(&self, server: &str) -> Result<Arc<dyn McpEndpoint>, WorkerError>;
}

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
    /// Worker-as-data catalog: resolves a [`WorkerRef::Named`] to a [`WorkerDef`]
    /// (C6). Defaults to a built-ins-only registry, so a bare `cli{claude}` still
    /// works without any data files.
    workers: WorkerRegistry,
    /// Whether registry-driven `remote`/`mcp` (exec-tier) workers may be minted.
    /// OFF by default — security before openness (design §9). A host flips it on
    /// alongside the connector when the fleet is explicitly opened.
    allow_remote: bool,
    /// The connector for `remote`/`mcp` defs, present only when `allow_remote`.
    connector: Option<Arc<dyn RemoteConnector>>,
}

impl WorkerFactory {
    /// Build the factory over the shared deps. The worker registry defaults to
    /// built-ins-only and remote/MCP minting is OFF; use [`Self::with_registry`] /
    /// [`Self::with_remote`] to opt into worker-as-data + remote/MCP.
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
            workers: WorkerRegistry::discover(None),
            allow_remote: false,
            connector: None,
        }
    }

    /// Use a specific worker-as-data [`WorkerRegistry`] (C6) — the daemon/CLI build
    /// it from the resolved project root so `.nerve/workers/*.json` defs resolve.
    pub(crate) fn with_registry(mut self, workers: WorkerRegistry) -> Self {
        self.workers = workers;
        self
    }

    /// Open exec-tier registry workers: enable minting of `remote`/`mcp` defs over
    /// `connector`. OFF by default (security before openness). A host calls this only
    /// when the operator explicitly opted the fleet in (mirroring `--allow-delegate`).
    pub(crate) fn with_remote(mut self, connector: Arc<dyn RemoteConnector>) -> Self {
        self.allow_remote = true;
        self.connector = Some(connector);
        self
    }

    /// Mint the worker for `worker_ref`, resolving a `Named` ref through the C6
    /// [`WorkerRegistry`] first. The engine only ever sees the resulting
    /// [`AgentWorker`]; an unknown name, an unknown CLI agent, or a refused exec-tier
    /// remote/MCP worker all fail before any spawn.
    pub(crate) fn create_ref(
        &self,
        worker_ref: &WorkerRef,
    ) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let resolved = self.workers.resolve(worker_ref)?;
        self.create_resolved(&resolved)
    }

    /// Mint a worker for an already-resolved [`WorkerDef`]. CLI/provider defs mint the
    /// C0 workers; remote/MCP defs are gated by `allow_remote` (refused-by-default).
    fn create_resolved(
        &self,
        resolved: &ResolvedWorker,
    ) -> Result<Box<dyn AgentWorker>, WorkerError> {
        match &resolved.def.kind {
            WorkerDefKind::Cli { name } => self.create_cli(name),
            WorkerDefKind::Provider { provider, model } => {
                Ok(self.create_provider(provider.clone(), resolved.model_or(model)))
            }
            WorkerDefKind::Remote { endpoint } => self.create_remote(endpoint),
            WorkerDefKind::Mcp { server } => self.create_mcp(server),
        }
    }

    /// Mint a [`CliWorker`] for catalog name `name` (an unknown name errors before
    /// any spawn) with its pre-computed codex MCP-disable flags.
    fn create_cli(&self, name: &str) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let agent =
            DelegateAgent::from_name(name).map_err(|err| WorkerError::Start(err.to_string()))?;
        let flags = delegate_disable_flags(agent, None);
        Ok(Box::new(CliWorker::new(
            agent,
            Arc::clone(&self.delegate_launcher),
            flags,
        )))
    }

    /// Mint a [`ProviderWorker`] over the shared runtime/registry/gate.
    fn create_provider(&self, provider: String, model: String) -> Box<dyn AgentWorker> {
        Box::new(ProviderWorker::new(
            Arc::clone(&self.runtime),
            self.registry.clone(),
            self.gate.clone(),
            self.max_depth,
            provider,
            model,
        ))
    }

    /// Mint a [`RemoteWorker`] — refused unless the fleet was explicitly opened
    /// (security before openness, design §9). The refusal message names the missing
    /// authorization so an operator knows the exact knob.
    fn create_remote(&self, endpoint: &str) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let connector = self.remote_connector("remote")?;
        let transport = connector.remote(endpoint)?;
        Ok(Box::new(RemoteWorker::new(transport, endpoint.to_string())))
    }

    /// Mint an [`McpWorker`] — refused unless the fleet was explicitly opened.
    fn create_mcp(&self, server: &str) -> Result<Box<dyn AgentWorker>, WorkerError> {
        let connector = self.remote_connector("mcp")?;
        let transport = connector.mcp(server)?;
        Ok(Box::new(McpWorker::new(transport, server.to_string())))
    }

    /// The exec-tier gate for remote/MCP workers: returns the connector only when the
    /// fleet was explicitly opened, else a clear refusal. `what` names the worker kind.
    fn remote_connector(&self, what: &str) -> Result<&Arc<dyn RemoteConnector>, WorkerError> {
        if !self.allow_remote {
            return Err(WorkerError::Start(format!(
                "{what} workers are exec-tier and OFF by default; enable them explicitly \
                 (the fleet must be opened with policy + budget configured, like \
                 --allow-delegate) — security before openness"
            )));
        }
        self.connector.as_ref().ok_or_else(|| {
            WorkerError::Start(format!(
                "{what} workers enabled but no connector configured"
            ))
        })
    }

    /// Mint a worker for a concrete [`WorkerKind`] (the pre-C6 entry point, kept for
    /// callers that already hold a kind). `Named` is not a `WorkerKind`, so this never
    /// resolves the registry; use [`Self::create_ref`] for that.
    pub(crate) fn create(&self, kind: WorkerKind) -> Result<Box<dyn AgentWorker>, WorkerError> {
        match kind {
            WorkerKind::Cli(name) => self.create_cli(name),
            WorkerKind::Provider { provider, model } => Ok(self.create_provider(provider, model)),
        }
    }
}

impl ResolvedWorker {
    /// The effective model for a provider def: the def's `model` override wins over
    /// the kind's inline model (so a named def can retune a provider worker).
    fn model_or(&self, kind_model: &str) -> String {
        self.def
            .model
            .clone()
            .unwrap_or_else(|| kind_model.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use nerve_core::CancelToken;

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
        assert_eq!(worker.capability(), nerve_runtime::RiskTier::Edit);
    }

    #[test]
    fn create_ref_resolves_a_builtin_named_cli_worker() {
        // A bare Named ref to a built-in cli worker resolves (worker-as-data, C6).
        let worker = factory()
            .create_ref(&WorkerRef::Named {
                name: "claude".into(),
            })
            .expect("built-in named claude resolves");
        assert_eq!(worker.kind(), WorkerKind::Cli("claude"));
    }

    #[test]
    fn create_ref_passes_through_inline_provider() {
        let worker = factory()
            .create_ref(&WorkerRef::Provider {
                provider: "xai".into(),
                model: "grok".into(),
            })
            .expect("inline provider");
        assert!(matches!(worker.kind(), WorkerKind::Provider { .. }));
    }

    #[test]
    fn create_ref_resolves_a_project_provider_def() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers").join("judge.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{ "kind": { "type": "provider", "provider": "xai", "model": "grok" }, "model": "grok-2" }"#,
        )
        .unwrap();
        let workers = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let worker = factory()
            .with_registry(workers)
            .create_ref(&WorkerRef::Named {
                name: "judge".into(),
            })
            .expect("named judge resolves to project def");
        // The def's model override (`grok-2`) wins over the kind's inline model.
        assert_eq!(
            worker.kind(),
            WorkerKind::Provider {
                provider: "xai".into(),
                model: "grok-2".into()
            }
        );
    }

    #[test]
    fn remote_worker_refused_by_default() {
        // A registry def naming a remote worker is REFUSED unless the fleet is opened.
        // `Box<dyn AgentWorker>` is not Debug, so match rather than `expect_err`.
        let dir = tempfile::tempdir().unwrap();
        write_worker(
            &dir,
            "peer",
            r#"{ "kind": { "type": "remote", "endpoint": "x" } }"#,
        );
        let workers = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        match factory()
            .with_registry(workers)
            .create_ref(&WorkerRef::Named {
                name: "peer".into(),
            }) {
            Ok(_) => panic!("remote worker must be off by default"),
            Err(err) => assert!(
                err.to_string().contains("security before openness"),
                "refusal must explain the gate: {err}"
            ),
        }
    }

    #[test]
    fn mcp_worker_refused_by_default() {
        let dir = tempfile::tempdir().unwrap();
        write_worker(
            &dir,
            "srv",
            r#"{ "kind": { "type": "mcp", "server": "s" } }"#,
        );
        let workers = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        match factory()
            .with_registry(workers)
            .create_ref(&WorkerRef::Named { name: "srv".into() })
        {
            Ok(_) => panic!("mcp worker must be off by default"),
            Err(err) => assert!(
                err.to_string().contains("security before openness"),
                "{err}"
            ),
        }
    }

    #[test]
    fn remote_worker_mints_when_fleet_opened() {
        // With the fleet explicitly opened (allow_remote + a connector), the same def
        // mints — proving the gate is the ONLY thing standing between off and on.
        let dir = tempfile::tempdir().unwrap();
        write_worker(
            &dir,
            "peer",
            r#"{ "kind": { "type": "remote", "endpoint": "x" } }"#,
        );
        let workers = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let worker = factory()
            .with_registry(workers)
            .with_remote(Arc::new(EchoConnector))
            .create_ref(&WorkerRef::Named {
                name: "peer".into(),
            })
            .expect("remote worker mints when opened");
        assert_eq!(worker.kind(), WorkerKind::Cli("remote"));
        assert_eq!(worker.capability(), nerve_runtime::RiskTier::Exec);
    }

    fn write_worker(dir: &tempfile::TempDir, name: &str, json: &str) {
        let path = dir.path().join("workers").join(format!("{name}.json"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json).unwrap();
    }

    /// A connector whose transports echo (only reachable once the fleet is opened).
    struct EchoConnector;
    impl RemoteConnector for EchoConnector {
        fn remote(&self, _endpoint: &str) -> Result<Arc<dyn RemoteEndpoint>, WorkerError> {
            Ok(Arc::new(EchoRemote))
        }
        fn mcp(&self, _server: &str) -> Result<Arc<dyn McpEndpoint>, WorkerError> {
            Ok(Arc::new(EchoMcp))
        }
    }
    struct EchoRemote;
    impl RemoteEndpoint for EchoRemote {
        fn turn(
            &self,
            prompt: &str,
            _cancel: &CancelToken,
            _on_event: &mut dyn FnMut(super::super::WorkerEvent),
        ) -> Result<super::super::TurnResult, WorkerError> {
            Ok(super::super::TurnResult {
                ok: true,
                text: prompt.to_string(),
                usage: nerve_agent::Usage::default(),
                cost_usd: None,
                timed_out: false,
            })
        }
    }
    struct EchoMcp;
    impl McpEndpoint for EchoMcp {
        fn call(&self, prompt: &str, _cancel: &CancelToken) -> Result<String, WorkerError> {
            Ok(prompt.to_string())
        }
    }
}
