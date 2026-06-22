//! [`WorkflowRegistry`] — named workflow-defs as data (Wave C6, the P3 close-out).
//!
//! C2 wired the `flow.start { workflow_ref: String }` variant but refused it until
//! the loader landed. C6 lands it: a [`WorkflowDef`] discovered from disk
//! (`.nerve/workflows/<name>.json`, project > global > built-in), resolved by the
//! same [`DiscoveryBases`](crate::discovery) loader that backs agent-defs, skills,
//! and the worker registry — "loaded, not compiled" (design §3/§6, north-star P3).
//!
//! There are NO built-in workflows (a workflow is inherently project-specific); the
//! built-in table is empty, so an unknown `workflow_ref` errors clearly.
//!
//! Named workflows can reference each other (a `Named` worker resolved from the
//! worker registry, or — looking ahead — a nested workflow). [`reachable_refs`]
//! exposes the named references a def transitively reaches, which
//! [`validate_workflow_refs`](super::safety::validate_workflow_refs) walks for cycles
//! at `flow.start` (the genuine reference-cycle check C6 adds, design §8).

use crate::discovery::DiscoveryBases;
use nerve_runtime::{Strategy, WorkerRef, WorkflowDef};
use std::path::Path;

/// No built-in workflows: a workflow is project-specific, so the embedded table is
/// empty (an unknown `workflow_ref` is a clear "not found" error). Kept as a named
/// const so the loader call reads the same as the worker/agent ones.
const BUILTIN_WORKFLOWS: &[(&str, &str)] = &[];

/// The discovered named-workflow catalog, resolving a `workflow_ref` to a
/// [`WorkflowDef`]. Built from the shared precedence-ordered bases, exactly like the
/// worker registry + capabilities loader.
#[derive(Clone)]
pub(crate) struct WorkflowRegistry {
    bases: DiscoveryBases,
}

impl WorkflowRegistry {
    /// Build the standard discovery chain (project `<root>/.nerve` then global
    /// `config_home()`), mirroring [`Capabilities::discover`](crate::capabilities).
    pub(crate) fn discover(project_dir: Option<&Path>) -> Self {
        Self {
            bases: DiscoveryBases::discover(project_dir),
        }
    }

    /// Construct from explicit base directories (test-only).
    #[cfg(test)]
    pub(crate) fn from_sources(
        project: Option<std::path::PathBuf>,
        global: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            bases: DiscoveryBases::from_sources(project, global),
        }
    }

    /// Resolve a `workflow_ref` to its [`WorkflowDef`], honoring precedence. An
    /// unknown ref is a clear error (no built-in workflows exist).
    pub(crate) fn resolve(&self, workflow_ref: &str) -> anyhow::Result<WorkflowDef> {
        let (def, _source) =
            self.bases
                .load_json::<WorkflowDef>("workflows", workflow_ref, BUILTIN_WORKFLOWS)?;
        Ok(def)
    }

    /// Resolve `workflow_ref` only when a workflow definition is actually present.
    ///
    /// Missing refs return `Ok(None)`: the same name may still be a valid plain
    /// worker leaf. Invalid names and load/read/parse failures return `Err`, so
    /// broken workflow data cannot be silently treated as a leaf during cycle
    /// validation.
    pub(crate) fn resolve_if_present(
        &self,
        workflow_ref: &str,
    ) -> anyhow::Result<Option<WorkflowDef>> {
        if !self.has_definition(workflow_ref)? {
            return Ok(None);
        }
        self.resolve(workflow_ref).map(Some)
    }

    /// Whether a named workflow `workflow_ref` exists and parses. Used only for
    /// catalog-style probes where malformed files should behave as absent.
    pub(crate) fn contains(&self, workflow_ref: &str) -> bool {
        self.resolve_if_present(workflow_ref)
            .is_ok_and(|def| def.is_some())
    }

    fn has_definition(&self, workflow_ref: &str) -> anyhow::Result<bool> {
        crate::discovery::validate_name(workflow_ref)?;
        for (_source, base) in self.bases.bases() {
            if base
                .join("workflows")
                .join(format!("{workflow_ref}.json"))
                .is_file()
            {
                return Ok(true);
            }
        }
        Ok(BUILTIN_WORKFLOWS
            .iter()
            .any(|(name, _raw)| *name == workflow_ref))
    }
}

/// The named references a [`WorkflowDef`] directly reaches: every `Named` worker in
/// its strategy tree (resolved through the worker registry) plus — defined-ahead —
/// any nested named workflow reference. C6's data shape carries named workers; a
/// future nested-`workflow_ref` step would extend this list, and the cycle check
/// already handles it (it walks whatever this returns). Pure + deterministic.
pub(crate) fn reachable_named_workers(def: &WorkflowDef) -> Vec<String> {
    let mut names = Vec::new();
    collect_named(&def.strategy, &mut names);
    names
}

/// Walk a strategy tree, collecting every `Named` worker name (recursing into a
/// `Hierarchical` child). Total over the closed strategy enum.
fn collect_named(strategy: &Strategy, names: &mut Vec<String>) {
    for step in steps_of(strategy) {
        if let WorkerRef::Named { name } = &step.worker {
            names.push(name.clone());
        }
    }
    if let Strategy::Hierarchical { child, .. } = strategy {
        collect_named(child, names);
    }
}

/// The declared steps directly in a strategy (not recursing into a `Hierarchical`
/// child — [`collect_named`] recurses). Total over the closed enum.
fn steps_of(strategy: &Strategy) -> Vec<&nerve_runtime::Step> {
    match strategy {
        Strategy::Single { step } => vec![step],
        Strategy::Parallel { branches, .. } => branches.iter().collect(),
        Strategy::Pipeline { stages } => stages.iter().collect(),
        Strategy::VoteJudge {
            candidates, judge, ..
        } => candidates.iter().chain([judge]).collect(),
        Strategy::MapReduce { map, reduce, .. } => vec![map, reduce],
        Strategy::Debate { sides, judge, .. } => sides.iter().chain([judge]).collect(),
        Strategy::Hierarchical { planner, .. } => vec![planner],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::{DelegateAutonomy, FailPolicy, Join, Step, TaskTemplate};
    use std::fs;
    use tempfile::tempdir;

    fn workflow_file(base: &Path, name: &str, json: &str) {
        let path = base.join("workflows").join(format!("{name}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json).unwrap();
    }

    #[test]
    fn resolves_a_project_named_workflow() {
        let dir = tempdir().unwrap();
        workflow_file(
            dir.path(),
            "review",
            r#"{
                "schema_version": 1,
                "name": "review",
                "strategy": {
                    "type": "single",
                    "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "review" }
                }
            }"#,
        );
        let reg = WorkflowRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let def = reg.resolve("review").expect("named workflow resolves");
        assert_eq!(def.name, "review");
        assert!(reg.contains("review"));
        assert!(!reg.contains("missing"));
    }

    #[test]
    fn unknown_workflow_errors() {
        let reg = WorkflowRegistry::from_sources(None, None);
        let err = reg.resolve("nope").expect_err("no such workflow");
        assert!(err.to_string().contains("unknown workflows 'nope'"));
        assert!(reg.resolve_if_present("nope").unwrap().is_none());
    }

    #[test]
    fn resolve_if_present_errors_on_malformed_existing_workflow() {
        let dir = tempdir().unwrap();
        workflow_file(
            dir.path(),
            "bad",
            r#"{ "schema_version": 1, "name": "bad" }"#,
        );
        let reg = WorkflowRegistry::from_sources(Some(dir.path().to_path_buf()), None);
        let err = reg
            .resolve_if_present("bad")
            .expect_err("malformed workflow should not be hidden as absent");
        assert!(err.to_string().contains("failed to parse"), "{err}");
        assert!(!reg.contains("bad"));
    }

    #[test]
    fn resolve_if_present_keeps_discovery_precedence_visible() {
        let project = tempdir().unwrap();
        let global = tempdir().unwrap();
        workflow_file(
            project.path(),
            "shadowed",
            r#"{ "schema_version": 1, "name": "shadowed" }"#,
        );
        workflow_file(
            global.path(),
            "shadowed",
            r#"{
                "schema_version": 1,
                "name": "shadowed",
                "strategy": {
                    "type": "single",
                    "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "review" }
                }
            }"#,
        );
        let reg = WorkflowRegistry::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        );
        let err = reg
            .resolve_if_present("shadowed")
            .expect_err("malformed project workflow should shadow valid global workflow");
        assert!(err.to_string().contains("failed to parse"), "{err}");
    }

    #[test]
    fn resolve_if_present_ignores_malformed_lower_priority_workflow() {
        let project = tempdir().unwrap();
        let global = tempdir().unwrap();
        workflow_file(
            project.path(),
            "shadowed",
            r#"{
                "schema_version": 1,
                "name": "shadowed",
                "strategy": {
                    "type": "single",
                    "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "project" }
                }
            }"#,
        );
        workflow_file(
            global.path(),
            "shadowed",
            r#"{ "schema_version": 1, "name": "shadowed" }"#,
        );
        let reg = WorkflowRegistry::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        );
        let def = reg
            .resolve_if_present("shadowed")
            .expect("valid project workflow should win")
            .expect("workflow present");
        assert_eq!(def.name, "shadowed");
    }

    #[test]
    fn resolve_if_present_reports_malformed_global_when_no_project_override() {
        let global = tempdir().unwrap();
        workflow_file(
            global.path(),
            "bad_global",
            r#"{ "schema_version": 1, "name": "bad_global" }"#,
        );
        let reg = WorkflowRegistry::from_sources(None, Some(global.path().to_path_buf()));
        let err = reg
            .resolve_if_present("bad_global")
            .expect_err("malformed global workflow should be reported when visible");
        assert!(err.to_string().contains("failed to parse"), "{err}");
    }

    #[test]
    fn reachable_named_workers_collects_across_the_tree() {
        let named = |n: &str| Step {
            worker: WorkerRef::Named { name: n.into() },
            task: TaskTemplate::new("t"),
            autonomy: DelegateAutonomy::ReadOnly,
            on_fail: FailPolicy::Continue,
        };
        let def = WorkflowDef {
            schema_version: 1,
            name: "w".into(),
            strategy: Strategy::Parallel {
                branches: vec![named("a"), named("b")],
                join: Join::All,
            },
            budget: nerve_runtime::BudgetSpec::default(),
            max_depth: 2,
        };
        assert_eq!(reachable_named_workers(&def), vec!["a", "b"]);
    }
}
