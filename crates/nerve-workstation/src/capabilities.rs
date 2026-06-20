//! Capabilities-as-data: named agent definitions and skills loaded from disk.
//!
//! Architecture north star P3 (`docs/designs/architecture-north-star.md` §6.3
//! "capabilities — data as plugin", §7.5 skill/agent-def contract): named agents
//! and skills are *data*, discovered from directories with no recompile.
//! Precedence is **project > global > built-in**, so a project can shadow a
//! global or built-in definition.
//!
//! - **Agent definitions** are JSON files (`<name>.json`) mapping to the same
//!   fields the orchestrator's `AgentDef` exposes, plus a `skills` list. JSON is
//!   used for consistency with `--mcp-config` / `--provider-config` and to avoid
//!   adding a YAML dependency (the build stays offline-safe).
//! - **Skills** are plain markdown files (`<name>.md`). Skills are
//!   *progressively disclosed*: only each skill's name + one-line description is
//!   injected into the system prompt (a compact footer), not its full body. This
//!   keeps the prompt small as the skill library grows; the body is fetched
//!   on demand (out of scope here). The footer also surfaces which source won
//!   (project / global / built-in) for each disclosed skill.
//!
//! Discovery base directories, highest precedence first — each holds `agents/`
//! and `skills/` subdirectories:
//! - project: `<root>/.nerve/`
//! - global:  `config_home()` (`$NERVE_HOME` / `$XDG_CONFIG_HOME/nerve` / OS config dir)
//! - built-in: embedded defaults (`BUILTIN_AGENTS` / `BUILTIN_SKILLS`).
//!
//! Composition happens here, at the binary (the composition root); the
//! orchestrator stays unaware of skills/agents.
//!
//! The precedence walk + source labelling are shared with the C6
//! [`WorkerRegistry`](crate::worker::WorkerRegistry) /
//! [`WorkflowRegistry`](crate::flow::WorkflowRegistry) through
//! [`crate::discovery::DiscoveryBases`] (the one "loaded, not compiled" loader).

use crate::discovery::{CapabilitySource, DiscoveryBases};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::Path;

/// Built-in agent definitions, embedded so a fresh install has a working
/// `--agent`. Each entry is `(name, raw-json)`; consulted only after the project
/// and global directories.
const BUILTIN_AGENTS: &[(&str, &str)] = &[("coder", include_str!("../assets/agents/coder.json"))];

/// Built-in skills, embedded markdown. Consulted only after the project and
/// global directories.
const BUILTIN_SKILLS: &[(&str, &str)] = &[(
    "nerve-tools",
    include_str!("../assets/skills/nerve-tools.md"),
)];

/// On-disk shape of an agent-definition JSON file. Every field is optional: an
/// agent may rely entirely on CLI flags and/or skills. The file name (without
/// `.json`) is the agent's name.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AgentDefFile {
    /// Base system prompt; a skills *metadata* footer is appended after it.
    pub(crate) system_prompt: Option<String>,
    /// Model id (overridable by `--model`).
    pub(crate) model: Option<String>,
    /// Provider name (overridable by `--provider`).
    pub(crate) provider: Option<String>,
    /// Maximum agent turns (overridable by `--max-turns`).
    pub(crate) max_turns: Option<u32>,
    /// Sampling temperature (overridable by `--temperature`).
    pub(crate) temperature: Option<f32>,
    /// Reasoning-effort hint (overridable by `--reasoning-effort`).
    pub(crate) reasoning_effort: Option<String>,
    /// Optional tool allowlist passed through to the orchestrator.
    pub(crate) tool_filter: Option<Vec<String>>,
    /// Skill names to compose into the system prompt, in order.
    #[serde(default)]
    pub(crate) skills: Vec<String>,
}

/// A fully resolved agent: its definition with all referenced skills composed
/// into `system_prompt`. The CLI applies explicit flag overrides on top of this.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedAgent {
    /// Composed system prompt (base prompt followed by each skill body), or
    /// `None` when the definition supplies neither — leaving the caller's default.
    pub(crate) system_prompt: Option<String>,
    /// Model id from the definition, if any.
    pub(crate) model: Option<String>,
    /// Provider name from the definition, if any.
    pub(crate) provider: Option<String>,
    /// Turn cap from the definition, if any.
    pub(crate) max_turns: Option<u32>,
    /// Sampling temperature from the definition, if any.
    pub(crate) temperature: Option<f32>,
    /// Reasoning-effort hint from the definition, if any.
    pub(crate) reasoning_effort: Option<String>,
    /// Tool allowlist from the definition, if any.
    pub(crate) tool_filter: Option<Vec<String>>,
}

/// Resolves named agents and skills from a precedence-ordered set of base
/// directories, falling back to embedded built-ins.
///
/// Each base directory holds `agents/` and `skills/` subdirectories. The
/// precedence walk + source labelling are shared with the C6 worker/workflow
/// registries through [`DiscoveryBases`]: agents are JSON (loaded via
/// [`DiscoveryBases::load_json`]), skills are markdown (resolved here, since the
/// shared loader is JSON-only).
pub(crate) struct Capabilities {
    bases: DiscoveryBases,
}

impl Capabilities {
    /// Build the standard discovery chain: project (`<root>/.nerve`) then global
    /// (`config_home()`). A missing config home is skipped rather than failing —
    /// built-ins still resolve.
    pub(crate) fn discover(project_dir: Option<&Path>) -> Self {
        Self {
            bases: DiscoveryBases::discover(project_dir),
        }
    }

    /// Construct from explicit project/global base directories (each optional),
    /// bypassing environment-derived discovery. Test-only.
    #[cfg(test)]
    fn from_sources(
        project: Option<std::path::PathBuf>,
        global: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            bases: DiscoveryBases::from_sources(project, global),
        }
    }

    /// Resolve `name` to a [`ResolvedAgent`], composing its skills into the
    /// system prompt. Errors if the agent or any referenced skill is missing.
    pub(crate) fn resolve_agent(&self, name: &str) -> Result<ResolvedAgent> {
        let def = self.load_agent_def(name)?;
        let system_prompt = self.compose_system_prompt(&def)?;
        Ok(ResolvedAgent {
            system_prompt,
            model: def.model,
            provider: def.provider,
            max_turns: def.max_turns,
            temperature: def.temperature,
            reasoning_effort: def.reasoning_effort,
            tool_filter: def.tool_filter,
        })
    }

    /// Compose the base prompt followed by a compact *skills metadata* footer
    /// (progressive disclosure): one `- name (source): description` line per
    /// referenced skill, in listed order. The full body is deliberately *not*
    /// inlined — it is fetched on demand. Returns `None` when nothing contributes.
    fn compose_system_prompt(&self, def: &AgentDefFile) -> Result<Option<String>> {
        let mut sections: Vec<String> = Vec::new();
        if let Some(base) = def.system_prompt.as_deref() {
            let base = base.trim();
            if !base.is_empty() {
                sections.push(base.to_string());
            }
        }
        if let Some(footer) = self.compose_skills_footer(&def.skills)? {
            sections.push(footer);
        }
        if sections.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sections.join("\n\n")))
        }
    }

    /// Build the metadata footer for the referenced skills, or `None` if the
    /// agent lists none. Resolving each skill still honors precedence and still
    /// errors on a missing skill (an agent referencing a ghost skill is a config
    /// error), but only its name/description/source reach the prompt.
    fn compose_skills_footer(&self, skills: &[String]) -> Result<Option<String>> {
        if skills.is_empty() {
            return Ok(None);
        }
        let mut lines = vec!["## Available skills".to_string()];
        for skill in skills {
            let meta = self.load_skill_meta(skill)?;
            lines.push(format!(
                "- {} ({}): {}",
                meta.name, meta.source, meta.description
            ));
        }
        Ok(Some(lines.join("\n")))
    }

    /// Load and parse the agent definition `name`, honoring precedence — via the
    /// shared JSON loader (agents are JSON, like worker/workflow defs).
    fn load_agent_def(&self, name: &str) -> Result<AgentDefFile> {
        let (def, _source) =
            self.bases
                .load_json::<AgentDefFile>("agents", name, BUILTIN_AGENTS)?;
        Ok(def)
    }

    /// Resolve skill `name` to its disclosure metadata (name, one-line
    /// description, winning source), honoring precedence. Reads the body only to
    /// extract the description — the body itself is not retained for the prompt.
    /// Skills are markdown (not JSON), so this walks the bases directly rather than
    /// through the shared JSON loader.
    fn load_skill_meta(&self, name: &str) -> Result<SkillMeta> {
        crate::discovery::validate_name(name)?;
        // Each base carries its real source (project / global), so labelling
        // reflects what the base *is*, not its array position — important when no
        // project root exists and the only base is the global config home.
        for (source, base) in self.bases.bases() {
            let path = base.join("skills").join(format!("{name}.md"));
            if path.is_file() {
                let body = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read skill: {}", path.display()))?;
                return Ok(SkillMeta::from_body(name, &body, *source));
            }
        }
        if let Some(raw) = builtin(BUILTIN_SKILLS, name) {
            return Ok(SkillMeta::from_body(name, raw, CapabilitySource::BuiltIn));
        }
        bail!("skill '{name}' not found in project (.nerve/skills), global, or built-ins");
    }
}

/// One skill's progressive-disclosure metadata: just enough to advertise it in
/// the system prompt without inlining its body.
#[derive(Debug, Clone)]
struct SkillMeta {
    name: String,
    description: String,
    source: CapabilitySource,
}

impl SkillMeta {
    /// Derive metadata from a skill's markdown body: the description is the first
    /// non-empty line, with a leading markdown heading marker stripped, truncated
    /// to a single compact line. Empty bodies yield a placeholder description.
    fn from_body(name: &str, body: &str, source: CapabilitySource) -> Self {
        let description = skill_description(body);
        Self {
            name: name.to_string(),
            description,
            source,
        }
    }
}

/// Extract a one-line description from a skill markdown body: the first non-blank
/// line, sans a leading `#`/heading marker. Falls back to a placeholder when the
/// body has no text.
fn skill_description(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.trim_start_matches('#').trim().to_string())
        .filter(|line| !line.is_empty())
        .unwrap_or_else(|| "(no description)".to_string())
}

/// Look up an embedded built-in skill by name (skills are markdown, resolved here
/// rather than through the shared JSON loader).
fn builtin(table: &[(&'static str, &'static str)], name: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(entry, _)| *entry == name)
        .map(|(_, raw)| *raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn write(path: PathBuf, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn agent_file(base: &Path, name: &str, json: &str) {
        write(base.join("agents").join(format!("{name}.json")), json);
    }

    fn skill_file(base: &Path, name: &str, md: &str) {
        write(base.join("skills").join(format!("{name}.md")), md);
    }

    #[test]
    fn builtin_agent_discloses_skill_metadata_only() {
        let caps = Capabilities::from_sources(None, None);
        let resolved = caps.resolve_agent("coder").expect("resolve coder");
        let prompt = resolved.system_prompt.expect("system prompt");
        // Base prompt is present, plus a compact metadata footer naming the skill
        // and its source — but NOT the full skill body.
        assert!(prompt.contains("You are Coder"));
        assert!(prompt.contains("## Available skills"));
        assert!(prompt.contains("- nerve-tools (built-in): Nerve tools"));
        // Progressive disclosure: the body's tool list must not be inlined.
        assert!(
            !prompt.contains("`file_search`"),
            "skill body should not be inlined:\n{prompt}"
        );
        // Built-in coder leaves model/provider to the CLI.
        assert!(resolved.model.is_none());
        assert!(resolved.provider.is_none());
    }

    #[test]
    fn unknown_agent_errors() {
        let err = Capabilities::from_sources(None, None)
            .resolve_agent("does-not-exist")
            .expect_err("should fail");
        assert!(err.to_string().contains("unknown agent"));
    }

    #[test]
    fn project_overrides_builtin() {
        let dir = tempdir().unwrap();
        agent_file(dir.path(), "coder", r#"{"system_prompt":"PROJECT CODER"}"#);
        let caps = Capabilities::from_sources(Some(dir.path().to_path_buf()), None);
        let prompt = caps.resolve_agent("coder").unwrap().system_prompt.unwrap();
        // Project file fully shadows the built-in (it lists no skills, so no
        // footer is added).
        assert_eq!(prompt, "PROJECT CODER");
        assert!(!prompt.contains("Available skills"));
    }

    #[test]
    fn project_base_overrides_global_base() {
        let project = tempdir().unwrap();
        let global = tempdir().unwrap();
        agent_file(project.path(), "foo", r#"{"system_prompt":"FROM PROJECT"}"#);
        agent_file(global.path(), "foo", r#"{"system_prompt":"FROM GLOBAL"}"#);
        let caps = Capabilities::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        );
        assert_eq!(
            caps.resolve_agent("foo").unwrap().system_prompt.unwrap(),
            "FROM PROJECT"
        );
    }

    #[test]
    fn skill_metadata_footer_orders_and_labels_source() {
        let dir = tempdir().unwrap();
        agent_file(
            dir.path(),
            "multi",
            r#"{"system_prompt":"HEAD","skills":["nerve-tools","extra"]}"#,
        );
        // Project skill shadows the built-in nerve-tools skill; its first line is
        // the disclosed description.
        skill_file(
            dir.path(),
            "nerve-tools",
            "# Override tools\n\nbody not inlined",
        );
        skill_file(dir.path(), "extra", "Extra skill summary\n\nmore body");
        let prompt = Capabilities::from_sources(Some(dir.path().to_path_buf()), None)
            .resolve_agent("multi")
            .unwrap()
            .system_prompt
            .unwrap();
        // Base prompt, then a metadata footer with skills in listed order, each
        // labelled with the winning source (project, here). Bodies are not inlined.
        assert_eq!(
            prompt,
            "HEAD\n\n## Available skills\n- nerve-tools (project): Override tools\n- extra (project): Extra skill summary"
        );
        assert!(!prompt.contains("body not inlined"));
        assert!(!prompt.contains("more body"));
    }

    #[test]
    fn footer_surfaces_global_vs_builtin_source() {
        let project = tempdir().unwrap();
        let global = tempdir().unwrap();
        agent_file(
            project.path(),
            "mix",
            r#"{"skills":["from-global","nerve-tools"]}"#,
        );
        // `from-global` only exists in the global base; `nerve-tools` falls all
        // the way through to the built-in.
        skill_file(global.path(), "from-global", "# Global skill\n\nx");
        let prompt = Capabilities::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        )
        .resolve_agent("mix")
        .unwrap()
        .system_prompt
        .unwrap();
        assert!(prompt.contains("- from-global (global): Global skill"));
        assert!(prompt.contains("- nerve-tools (built-in): Nerve tools"));
    }

    #[test]
    fn skill_description_strips_heading_and_handles_empty() {
        assert_eq!(skill_description("# Title here\n\nbody"), "Title here");
        assert_eq!(
            skill_description("plain first line\nsecond"),
            "plain first line"
        );
        assert_eq!(skill_description("\n\n   \n## Deep\n"), "Deep");
        assert_eq!(skill_description(""), "(no description)");
        assert_eq!(skill_description("###\n"), "(no description)");
    }

    #[test]
    fn missing_skill_errors() {
        let dir = tempdir().unwrap();
        agent_file(dir.path(), "broken", r#"{"skills":["ghost"]}"#);
        let err = Capabilities::from_sources(Some(dir.path().to_path_buf()), None)
            .resolve_agent("broken")
            .expect_err("should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn def_fields_pass_through() {
        let dir = tempdir().unwrap();
        agent_file(
            dir.path(),
            "tuned",
            r#"{"model":"m1","provider":"p1","max_turns":7,"temperature":0.25,"reasoning_effort":"high","tool_filter":["read_file","edit"]}"#,
        );
        let resolved = Capabilities::from_sources(Some(dir.path().to_path_buf()), None)
            .resolve_agent("tuned")
            .unwrap();
        assert_eq!(resolved.model.as_deref(), Some("m1"));
        assert_eq!(resolved.provider.as_deref(), Some("p1"));
        assert_eq!(resolved.max_turns, Some(7));
        assert!((resolved.temperature.unwrap() - 0.25).abs() < 1e-6);
        assert_eq!(resolved.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            resolved.tool_filter,
            Some(vec!["read_file".to_string(), "edit".to_string()])
        );
        // No system_prompt / skills -> default left to the caller.
        assert!(resolved.system_prompt.is_none());
    }

    #[test]
    fn invalid_names_are_rejected() {
        let caps = Capabilities::from_sources(None, None);
        for bad in ["../evil", "a/b", "", "dots.here", "back\\slash"] {
            assert!(
                caps.resolve_agent(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn empty_def_yields_no_prompt() {
        let dir = tempdir().unwrap();
        agent_file(dir.path(), "blank", "{}");
        let resolved = Capabilities::from_sources(Some(dir.path().to_path_buf()), None)
            .resolve_agent("blank")
            .unwrap();
        assert!(resolved.system_prompt.is_none());
        assert!(resolved.model.is_none());
    }

    #[test]
    fn no_project_root_labels_only_base_as_global() {
        // Regression: with no project root, the single base is the global config
        // home. It must be labelled "global", not "project" (the old index-based
        // inference mislabelled `bases[0]` as project regardless of what it was).
        let global = tempdir().unwrap();
        agent_file(global.path(), "g", r#"{"skills":["only-global"]}"#);
        skill_file(global.path(), "only-global", "# Global only\n\nbody");
        let prompt = Capabilities::from_sources(None, Some(global.path().to_path_buf()))
            .resolve_agent("g")
            .unwrap()
            .system_prompt
            .unwrap();
        assert!(
            prompt.contains("- only-global (global): Global only"),
            "global-only base must be labelled global, got:\n{prompt}"
        );
        assert!(!prompt.contains("(project)"));
    }
}
