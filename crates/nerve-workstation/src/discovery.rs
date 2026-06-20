//! Precedence-ordered capability discovery — the shared "loaded, not compiled" seam.
//!
//! Architecture north star P3 (`docs/designs/architecture-north-star.md` §6.3,
//! "capabilities — data as plugin"): named capabilities are *data*, discovered from
//! directories with no recompile, with precedence **project > global > built-in**.
//!
//! [`Capabilities`](crate::capabilities) discovers agent-defs + skills this way; the
//! C6 [`WorkerRegistry`](crate::worker::WorkerRegistry) +
//! [`WorkflowRegistry`](crate::flow::WorkflowRegistry) discover workers + workflow
//! defs the SAME way. This module factors the precedence walk + the source labelling
//! out of `capabilities.rs` so all three share one loader (the design's "loaded by
//! `Capabilities::discover`" instruction, made literal).
//!
//! Discovery base directories, highest precedence first — each holds capability-type
//! subdirectories (`agents/`, `skills/`, `workers/`, `workflows/`):
//! - project: `<root>/.nerve/`
//! - global:  `config_home()` (`$NERVE_HOME` / `$XDG_CONFIG_HOME/nerve` / OS config dir)
//! - built-in: embedded defaults (per capability type).

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use std::path::{Path, PathBuf};

/// Where a resolved capability came from, surfaced so a client (and a human reading
/// `list_agents`) can see which source won. `Inline` is the C6 worker case: an
/// inline `cli`/`provider` ref that never touched the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CapabilitySource {
    Project,
    Global,
    BuiltIn,
    /// A worker named inline in a `WorkflowDef`, not discovered from a file.
    Inline,
}

impl std::fmt::Display for CapabilitySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Project => "project",
            Self::Global => "global",
            Self::BuiltIn => "built-in",
            Self::Inline => "inline",
        })
    }
}

/// The precedence-ordered discovery bases (project before global), each paired with
/// the source it represents. Built-ins are consulted last by the load helpers.
///
/// The source is tracked alongside the path (not inferred from the array index)
/// because `discover` only pushes the project base when a project root exists — so
/// with no project root, `bases[0]` is the *global* config home and must be labelled
/// as such.
#[derive(Clone)]
pub(crate) struct DiscoveryBases {
    bases: Vec<(CapabilitySource, PathBuf)>,
}

impl DiscoveryBases {
    /// The standard chain: project (`<root>/.nerve`) then global (`config_home()`).
    /// A missing config home is skipped rather than failing — built-ins still resolve.
    pub(crate) fn discover(project_dir: Option<&Path>) -> Self {
        let mut bases = Vec::new();
        if let Some(root) = project_dir {
            bases.push((CapabilitySource::Project, root.join(".nerve")));
        }
        if let Ok(home) = nerve_agent::auth::config_home() {
            bases.push((CapabilitySource::Global, home));
        }
        Self { bases }
    }

    /// Construct from explicit project/global base directories (each optional),
    /// bypassing environment-derived discovery. Test-only.
    #[cfg(test)]
    pub(crate) fn from_sources(project: Option<PathBuf>, global: Option<PathBuf>) -> Self {
        let mut bases = Vec::new();
        if let Some(project) = project {
            bases.push((CapabilitySource::Project, project));
        }
        if let Some(global) = global {
            bases.push((CapabilitySource::Global, global));
        }
        Self { bases }
    }

    /// The discovery bases, highest precedence first (each with its real source).
    pub(crate) fn bases(&self) -> &[(CapabilitySource, PathBuf)] {
        &self.bases
    }

    /// Load + parse the JSON capability `name` of `kind` (the `<kind>/` subdirectory),
    /// honoring precedence: project then global file, else an embedded built-in.
    /// Returns the parsed value + the winning source.
    pub(crate) fn load_json<T: DeserializeOwned>(
        &self,
        kind: &str,
        name: &str,
        builtins: &[(&'static str, &'static str)],
    ) -> Result<(T, CapabilitySource)> {
        validate_name(name)?;
        for (source, base) in &self.bases {
            let path = base.join(kind).join(format!("{name}.json"));
            if path.is_file() {
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read {kind} def: {}", path.display()))?;
                let value = parse_json(&raw, &path.display().to_string())?;
                return Ok((value, *source));
            }
        }
        if let Some(raw) = builtin(builtins, name) {
            let value = parse_json(raw, &format!("<built-in {kind} {name}>"))?;
            return Ok((value, CapabilitySource::BuiltIn));
        }
        bail!(
            "unknown {kind} '{name}': not found in project (.nerve/{kind}), global, or built-ins"
        );
    }

    /// Every capability name of `kind` across all bases + built-ins, de-duplicated
    /// (a higher-precedence file shadows a lower one of the same name). `valid` filters
    /// candidate files by content (e.g. parses-as-the-def-type), so a stray file does
    /// not pollute the catalog. Sorted for deterministic `list_agents` output.
    pub(crate) fn names(
        &self,
        kind: &str,
        builtins: &[(&'static str, &'static str)],
        valid: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        for (_source, base) in &self.bases {
            collect_dir_names(&base.join(kind), &valid, &mut names);
        }
        for (name, raw) in builtins {
            if valid(raw) {
                names.insert((*name).to_string());
            }
        }
        names.into_iter().collect()
    }
}

/// Scan one `<base>/<kind>` directory for `<name>.json` files whose content passes
/// `valid`, inserting each `name` into `names`. A missing/unreadable dir is skipped.
fn collect_dir_names(
    dir: &Path,
    valid: &impl Fn(&str) -> bool,
    names: &mut std::collections::BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if validate_name(stem).is_err() {
            continue;
        }
        if let Ok(raw) = std::fs::read_to_string(&path)
            && valid(&raw)
        {
            names.insert(stem.to_string());
        }
    }
}

/// Parse a JSON capability document, tagging errors with `source`.
fn parse_json<T: DeserializeOwned>(raw: &str, source: &str) -> Result<T> {
    serde_json::from_str(raw).with_context(|| format!("failed to parse {source}"))
}

/// Look up an embedded built-in by name.
fn builtin(table: &[(&'static str, &'static str)], name: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(entry, _)| *entry == name)
        .map(|(_, raw)| *raw)
}

/// Reject names that could escape the discovery directories or are empty. Names are
/// simple identifiers — ASCII alphanumerics plus `-` and `_` (no path separators or
/// dots) — so `<name>.json` always stays in-dir.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid {
        bail!("invalid capability name '{name}': use only letters, digits, '-' and '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::fs;
    use tempfile::tempdir;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Thing {
        v: u32,
    }

    const BUILTINS: &[(&str, &str)] = &[("base", r#"{ "v": 0 }"#)];

    fn write_thing(base: &Path, name: &str, json: &str) {
        let path = base.join("things").join(format!("{name}.json"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, json).unwrap();
    }

    #[test]
    fn builtin_resolves_when_no_files() {
        let bases = DiscoveryBases::from_sources(None, None);
        let (thing, source) = bases
            .load_json::<Thing>("things", "base", BUILTINS)
            .unwrap();
        assert_eq!(thing, Thing { v: 0 });
        assert_eq!(source, CapabilitySource::BuiltIn);
    }

    #[test]
    fn project_shadows_global_shadows_builtin() {
        let project = tempdir().unwrap();
        let global = tempdir().unwrap();
        write_thing(global.path(), "base", r#"{ "v": 1 }"#);
        let bases = DiscoveryBases::from_sources(
            Some(project.path().to_path_buf()),
            Some(global.path().to_path_buf()),
        );
        // global shadows built-in
        assert_eq!(
            bases
                .load_json::<Thing>("things", "base", BUILTINS)
                .unwrap(),
            (Thing { v: 1 }, CapabilitySource::Global)
        );
        // project shadows global
        write_thing(project.path(), "base", r#"{ "v": 2 }"#);
        assert_eq!(
            bases
                .load_json::<Thing>("things", "base", BUILTINS)
                .unwrap(),
            (Thing { v: 2 }, CapabilitySource::Project)
        );
    }

    #[test]
    fn names_merge_builtins_and_files_filtered_and_sorted() {
        let dir = tempdir().unwrap();
        write_thing(dir.path(), "zeta", r#"{ "v": 9 }"#);
        write_thing(dir.path(), "alpha", r#"{ "v": 1 }"#);
        // An invalid (non-parsing) file is filtered out.
        write_thing(dir.path(), "broken", "not json");
        let bases = DiscoveryBases::from_sources(Some(dir.path().to_path_buf()), None);
        let names = bases.names("things", BUILTINS, |raw| {
            serde_json::from_str::<Thing>(raw).is_ok()
        });
        assert_eq!(names, vec!["alpha", "base", "zeta"]);
    }

    #[test]
    fn unknown_name_errors() {
        let bases = DiscoveryBases::from_sources(None, None);
        let err = bases
            .load_json::<Thing>("things", "ghost", BUILTINS)
            .expect_err("ghost not found");
        assert!(err.to_string().contains("unknown things 'ghost'"));
    }

    #[test]
    fn invalid_names_rejected() {
        for bad in ["../evil", "a/b", "", "dots.here", "back\\slash"] {
            assert!(validate_name(bad).is_err(), "expected `{bad}` rejected");
        }
        for good in ["claude", "my-worker", "w_1"] {
            assert!(validate_name(good).is_ok(), "expected `{good}` ok");
        }
    }

    #[test]
    fn no_project_root_labels_only_base_as_global() {
        let global = tempdir().unwrap();
        write_thing(global.path(), "base", r#"{ "v": 7 }"#);
        let bases = DiscoveryBases::from_sources(None, Some(global.path().to_path_buf()));
        let (_, source) = bases
            .load_json::<Thing>("things", "base", BUILTINS)
            .unwrap();
        assert_eq!(source, CapabilitySource::Global);
    }
}
