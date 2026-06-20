//! [`WorkerRegistry`] — worker-as-data (Wave C6, the P3 "loaded, not compiled" seam).
//!
//! C0's [`WorkerFactory`](super::WorkerFactory) maps a concrete [`WorkerKind`] to a
//! boxed [`AgentWorker`]. C6 adds the missing data layer above it: a declarative
//! [`WorkerDef`] discovered from disk and a [`WorkerRegistry`] that resolves a
//! [`WorkerRef::Named`](nerve_runtime::WorkerRef) through it. So "add any worker" is
//! a DATA change — drop a `<name>.json` in `.nerve/workers/` — with no recompile,
//! exactly the discipline [`Capabilities`](crate::capabilities) already uses for
//! agent-defs + skills (architecture north star P3, design §3/§6).
//!
//! ## Discovery precedence (project > global > built-in)
//!
//! [`WorkerRegistry::discover`] mirrors [`Capabilities::discover`] verbatim: a
//! precedence-ordered set of base directories (project `<root>/.nerve` before global
//! `config_home()`), each holding a `workers/` subdirectory of `<name>.json` files,
//! falling back to embedded built-in defaults (the bare `cli{codex|claude|gemini}`
//! refs, so existing inline workflows keep working unchanged). The first match wins;
//! a project def shadows a global one shadows a built-in.
//!
//! ## JSON only (offline-safe, matching `Capabilities`)
//!
//! Defs are JSON (`<name>.json`), the same choice [`Capabilities`] makes "to avoid
//! adding a YAML dependency (the build stays offline-safe)". The C6 brief mentioned
//! `{json,toml}`; we deliberately keep the existing JSON-only convention rather than
//! pull in a `toml` dependency the workspace does not otherwise carry — the loader
//! pattern is the load-bearing reuse, not the file extension.
//!
//! ## Security before openness (design §6/§9)
//!
//! A def may declare an exec-tier `remote` / `mcp` worker (design §10 C6). Resolving
//! such a def is allowed, but the [`WorkerFactory`](super::factory) refuses to MINT
//! it unless the fleet was explicitly opened (the same `--allow-delegate` posture a
//! CLI worker passes) — the registry never widens authority, it only names workers.

use super::WorkerError;
use crate::discovery::{CapabilitySource, DiscoveryBases};
use nerve_runtime::{DelegateAutonomy, WorkerRef};
use serde::Deserialize;
use std::path::Path;

/// The embedded built-in worker catalog: the three bare CLI agents, so a fresh
/// install resolves `cli{codex|claude|gemini}` named refs (and inline refs already
/// work without the registry). Each is `(name, raw-json)`, consulted only after the
/// project + global directories — a project may shadow any of them.
const BUILTIN_WORKERS: &[(&str, &str)] = &[
    ("codex", r#"{ "kind": { "type": "cli", "name": "codex" } }"#),
    (
        "claude",
        r#"{ "kind": { "type": "cli", "name": "claude" } }"#,
    ),
    (
        "gemini",
        r#"{ "kind": { "type": "cli", "name": "gemini" } }"#,
    ),
];

/// A declarative worker description (worker-as-data, design §3/§6). The file name
/// (without `.json`) is the worker's name; [`WorkerRef::Named`] resolves to it. Every
/// field beyond `kind` is optional and narrows/decorates the resolved worker.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct WorkerDef {
    /// What concrete worker this names (CLI / provider / remote / MCP).
    pub(crate) kind: WorkerDefKind,
    /// Default autonomy posture for this worker (a `Step`'s own autonomy still
    /// applies per call; this is the def's declared default). Optional.
    #[serde(default)]
    pub(crate) autonomy: Option<DelegateAutonomy>,
    /// Default tool allowlist (a provider/remote/MCP worker may narrow its tools).
    #[serde(default)]
    pub(crate) tool_filter: Option<Vec<String>>,
    /// Model override (a provider/remote worker's model when the kind omits it).
    #[serde(default)]
    pub(crate) model: Option<String>,
}

/// The concrete worker a [`WorkerDef`] names. Tagged the same way [`WorkerRef`] is,
/// so a def reads like the inline form plus the two C6 adapters. `remote` and `mcp`
/// are exec-tier and refused-by-default at mint time (security before openness, §9).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WorkerDefKind {
    /// An external agentic CLI by catalog name (`codex` | `claude` | `gemini`).
    Cli { name: String },
    /// An in-process provider loop by provider + model.
    Provider { provider: String, model: String },
    /// A worker that drives ANOTHER `nerve daemon` over the runtime protocol
    /// (design §10 C6). Exec-tier; refused unless the fleet is explicitly opened.
    Remote {
        /// How to reach the remote daemon (a `nerve daemon --stdio` argv, or a
        /// future socket address — the production transport is documented as a
        /// follow-on; C6 ships the adapter shape + a hermetic fake-endpoint test).
        endpoint: String,
    },
    /// A worker backed by an MCP-server tool (consume an MCP server as a worker,
    /// design §10 C6). Exec-tier; refused unless the fleet is explicitly opened.
    Mcp {
        /// The MCP server to consume (a command/URL the MCP client connects to).
        server: String,
    },
}

/// The discovered worker catalog, resolving a [`WorkerRef::Named`] to a concrete
/// [`WorkerRef`] (or the named def for the factory to mint). Built from a
/// precedence-ordered set of base directories + embedded built-ins, exactly like
/// [`Capabilities`](crate::capabilities).
#[derive(Clone)]
pub(crate) struct WorkerRegistry {
    bases: DiscoveryBases,
}

impl WorkerRegistry {
    /// Build the standard discovery chain (project `<root>/.nerve` then global
    /// `config_home()`), mirroring [`Capabilities::discover`].
    pub(crate) fn discover(project_dir: Option<&Path>) -> Self {
        Self {
            bases: DiscoveryBases::discover(project_dir),
        }
    }

    /// Construct from explicit base directories (test-only), bypassing
    /// environment-derived discovery — the [`Capabilities::from_sources`] analogue.
    #[cfg(test)]
    pub(crate) fn from_sources(
        project: Option<std::path::PathBuf>,
        global: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            bases: DiscoveryBases::from_sources(project, global),
        }
    }

    /// Resolve a [`WorkerRef`]: an inline `Cli`/`Provider` ref is returned verbatim
    /// (the registry never rewrites an inline ref); a `Named` ref is looked up in the
    /// catalog and mapped onto the concrete ref its def names. An unknown name is a
    /// clear error (the C5 "reject Named" message becomes "resolve, else error").
    pub(crate) fn resolve(&self, worker_ref: &WorkerRef) -> Result<ResolvedWorker, WorkerError> {
        match worker_ref {
            WorkerRef::Cli { name } => Ok(ResolvedWorker::inline(WorkerRef::Cli {
                name: name.clone(),
            })),
            WorkerRef::Provider { provider, model } => {
                Ok(ResolvedWorker::inline(WorkerRef::Provider {
                    provider: provider.clone(),
                    model: model.clone(),
                }))
            }
            WorkerRef::Named { name } => self.resolve_named(name),
        }
    }

    /// Resolve a named worker to its def + source, or a clear error if no def exists.
    fn resolve_named(&self, name: &str) -> Result<ResolvedWorker, WorkerError> {
        let (def, source) = self.load_def(name)?;
        Ok(ResolvedWorker {
            def,
            source,
            name: name.to_string(),
        })
    }

    /// Load + parse the worker def `name`, honoring precedence (project > global >
    /// built-in). Returns the def and the source it won from (for `list_agents`).
    fn load_def(&self, name: &str) -> Result<(WorkerDef, CapabilitySource), WorkerError> {
        self.bases
            .load_json::<WorkerDef>("workers", name, BUILTIN_WORKERS)
            .map_err(|err| WorkerError::Start(err.to_string()))
    }

    /// Every discovered worker name + its winning source, for `list_agents` (so the
    /// catalog reflects worker-as-data). Built-ins are always present; a project or
    /// global file shadows a built-in of the same name (reported under its source).
    pub(crate) fn catalog(&self) -> Vec<DiscoveredWorker> {
        let names = self.bases.names("workers", BUILTIN_WORKERS, |raw| {
            serde_json::from_str::<WorkerDef>(raw).is_ok()
        });
        names
            .into_iter()
            .filter_map(|name| {
                self.load_def(&name)
                    .ok()
                    .map(|(def, source)| DiscoveredWorker { name, def, source })
            })
            .collect()
    }
}

/// A resolved [`WorkerRef::Named`] → its def + winning source + the name it was
/// looked up under. An inline ref resolves to a synthetic built-in def so the
/// factory has one uniform path.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedWorker {
    pub(crate) def: WorkerDef,
    pub(crate) source: CapabilitySource,
    pub(crate) name: String,
}

impl ResolvedWorker {
    /// Wrap an inline `Cli`/`Provider` ref as a resolved worker with a synthetic
    /// def (no autonomy/filter/model overrides), tagged as the inline source.
    fn inline(worker_ref: WorkerRef) -> Self {
        let (kind, name) = match worker_ref {
            WorkerRef::Cli { name } => (WorkerDefKind::Cli { name: name.clone() }, name),
            WorkerRef::Provider { provider, model } => (
                WorkerDefKind::Provider {
                    provider: provider.clone(),
                    model,
                },
                provider,
            ),
            // `inline` is only ever called with an inline ref (the Named arm goes
            // through `resolve_named`), so this is unreachable in practice.
            WorkerRef::Named { name } => (WorkerDefKind::Cli { name: name.clone() }, name),
        };
        Self {
            def: WorkerDef {
                kind,
                autonomy: None,
                tool_filter: None,
                model: None,
            },
            source: CapabilitySource::Inline,
            name,
        }
    }
}

/// One catalog entry for `list_agents`: a discovered worker name, its def, and the
/// source it won from (built-in / global / project).
#[derive(Debug, Clone)]
pub(crate) struct DiscoveredWorker {
    pub(crate) name: String,
    pub(crate) def: WorkerDef,
    pub(crate) source: CapabilitySource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn worker_file(base: &Path, name: &str, json: &str) {
        let path = base.join("workers").join(format!("{name}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json).unwrap();
    }

    #[test]
    fn builtin_cli_workers_resolve_without_any_files() {
        let reg = WorkerRegistry::from_sources(None, None);
        for name in ["codex", "claude", "gemini"] {
            let resolved = reg
                .resolve(&WorkerRef::Named { name: name.into() })
                .expect("built-in resolves");
            assert_eq!(resolved.source, CapabilitySource::BuiltIn);
            assert_eq!(resolved.def.kind, WorkerDefKind::Cli { name: name.into() });
        }
    }

    #[test]
    fn inline_refs_pass_through_unchanged() {
        let reg = WorkerRegistry::from_sources(None, None);
        let cli = reg
            .resolve(&WorkerRef::Cli {
                name: "claude".into(),
            })
            .expect("inline cli");
        assert_eq!(cli.source, CapabilitySource::Inline);
        assert_eq!(
            cli.def.kind,
            WorkerDefKind::Cli {
                name: "claude".into()
            }
        );
    }

    #[test]
    fn project_worker_def_resolves_a_named_ref() {
        let dir = tempdir().unwrap();
        worker_file(
            dir.path(),
            "reviewer",
            r#"{ "kind": { "type": "provider", "provider": "xai", "model": "grok" }, "tool_filter": ["read_file"] }"#,
        );
        let reg = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let resolved = reg
            .resolve(&WorkerRef::Named {
                name: "reviewer".into(),
            })
            .expect("named resolves to project def");
        assert_eq!(resolved.source, CapabilitySource::Project);
        assert_eq!(
            resolved.def.kind,
            WorkerDefKind::Provider {
                provider: "xai".into(),
                model: "grok".into()
            }
        );
        assert_eq!(
            resolved.def.tool_filter,
            Some(vec!["read_file".to_string()])
        );
    }

    #[test]
    fn project_overrides_global_overrides_builtin() {
        let project = tempdir().unwrap();
        let global = tempdir().unwrap();
        // Both shadow the built-in `claude`, project wins.
        worker_file(
            project.path(),
            "claude",
            r#"{ "kind": { "type": "provider", "provider": "p", "model": "m" } }"#,
        );
        worker_file(
            global.path(),
            "claude",
            r#"{ "kind": { "type": "cli", "name": "claude" }, "model": "global" }"#,
        );
        let reg = WorkerRegistry::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        );
        let resolved = reg
            .resolve(&WorkerRef::Named {
                name: "claude".into(),
            })
            .expect("named claude");
        assert_eq!(resolved.source, CapabilitySource::Project);
        assert!(matches!(resolved.def.kind, WorkerDefKind::Provider { .. }));

        // Drop the project file: global now wins.
        fs::remove_file(project.path().join("workers").join("claude.json")).unwrap();
        let reg = WorkerRegistry::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        );
        let resolved = reg
            .resolve(&WorkerRef::Named {
                name: "claude".into(),
            })
            .expect("named claude from global");
        assert_eq!(resolved.source, CapabilitySource::Global);
        assert_eq!(resolved.def.model.as_deref(), Some("global"));
    }

    #[test]
    fn unresolved_named_worker_errors_clearly() {
        let reg = WorkerRegistry::from_sources(None, None);
        let err = reg
            .resolve(&WorkerRef::Named {
                name: "ghost".into(),
            })
            .expect_err("ghost has no def");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn catalog_includes_builtins_and_project_with_sources() {
        let dir = tempdir().unwrap();
        worker_file(
            dir.path(),
            "reviewer",
            r#"{ "kind": { "type": "provider", "provider": "xai", "model": "grok" } }"#,
        );
        // Shadow a built-in so the source is reported as project.
        worker_file(
            dir.path(),
            "claude",
            r#"{ "kind": { "type": "cli", "name": "claude" } }"#,
        );
        let reg = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let catalog = reg.catalog();
        let by_name: std::collections::BTreeMap<_, _> = catalog
            .iter()
            .map(|w| (w.name.as_str(), w.source))
            .collect();
        assert_eq!(by_name.get("reviewer"), Some(&CapabilitySource::Project));
        assert_eq!(by_name.get("claude"), Some(&CapabilitySource::Project));
        assert_eq!(by_name.get("codex"), Some(&CapabilitySource::BuiltIn));
        assert_eq!(by_name.get("gemini"), Some(&CapabilitySource::BuiltIn));
    }

    #[test]
    fn remote_and_mcp_defs_parse() {
        let dir = tempdir().unwrap();
        worker_file(
            dir.path(),
            "peer",
            r#"{ "kind": { "type": "remote", "endpoint": "nerve daemon --stdio" } }"#,
        );
        worker_file(
            dir.path(),
            "tool-srv",
            r#"{ "kind": { "type": "mcp", "server": "some-mcp" } }"#,
        );
        let reg = WorkerRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        assert!(matches!(
            reg.resolve(&WorkerRef::Named {
                name: "peer".into()
            })
            .unwrap()
            .def
            .kind,
            WorkerDefKind::Remote { .. }
        ));
        assert!(matches!(
            reg.resolve(&WorkerRef::Named {
                name: "tool-srv".into()
            })
            .unwrap()
            .def
            .kind,
            WorkerDefKind::Mcp { .. }
        ));
    }

    #[test]
    fn invalid_name_is_rejected() {
        let reg = WorkerRegistry::from_sources(None, None);
        for bad in ["../evil", "a/b", "", "dots.here"] {
            assert!(
                reg.resolve(&WorkerRef::Named { name: bad.into() }).is_err(),
                "expected `{bad}` rejected"
            );
        }
    }

    // Helper so the unused PathBuf import is exercised on all cfgs.
    #[allow(dead_code)]
    fn _path(p: &str) -> PathBuf {
        PathBuf::from(p)
    }
}
