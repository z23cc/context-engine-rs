//! Context recipes: named presets that fix WHICH sections an assembled
//! `workspace_context` contains — modeled on RepoPrompt CE's copy presets
//! (Standard / Plan / Review / Diff Follow-Up / Manual). Pure data; the assembly
//! itself stays deterministic in the `workspace_context` module.

use crate::workspace_context::WorkspaceContextInclude;

/// A reusable instruction block, rendered as a numbered `<meta prompt>` section.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MetaPrompt {
    pub title: String,
    pub body: String,
}

/// A named context recipe: the set of sections to assemble plus any default
/// meta-prompts (used when the caller supplies none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextRecipe {
    pub name: &'static str,
    pub label: &'static str,
    pub sections: &'static [WorkspaceContextInclude],
    pub meta_prompts: &'static [(&'static str, &'static str)],
}

const ARCHITECT: &str = "Act as a software architect. Before writing any code, produce a \
precise, step-by-step implementation plan: the files to change, the order of changes, and \
the risks. Do not write the final code yet.";

const REVIEWER: &str = "Act as a meticulous code reviewer. Review the changes for \
correctness, edge cases, security, and consistency with the surrounding code. List concrete \
issues with file:line references and a short rationale for each.";

/// Built-in recipes (RepoPrompt-style). `manual` keeps the caller's explicit
/// `include` set; the others fix an ordered section list.
pub fn built_in_recipes() -> &'static [ContextRecipe] {
    use WorkspaceContextInclude::{Contents, FileMap, GitDiff, MetaPrompts};
    &[
        ContextRecipe {
            name: "standard",
            label: "Standard",
            sections: &[FileMap, Contents],
            meta_prompts: &[],
        },
        ContextRecipe {
            name: "plan",
            label: "Plan",
            sections: &[FileMap, Contents, MetaPrompts],
            meta_prompts: &[("Architect", ARCHITECT)],
        },
        ContextRecipe {
            name: "review",
            label: "Review",
            sections: &[FileMap, Contents, GitDiff, MetaPrompts],
            meta_prompts: &[("Review", REVIEWER)],
        },
        ContextRecipe {
            name: "diff",
            label: "Diff Follow-Up",
            sections: &[GitDiff],
            meta_prompts: &[],
        },
        ContextRecipe {
            name: "manual",
            label: "Manual",
            sections: &[],
            meta_prompts: &[],
        },
    ]
}

/// Look up a built-in recipe by name.
pub fn recipe_by_name(name: &str) -> Option<&'static ContextRecipe> {
    built_in_recipes().iter().find(|r| r.name == name)
}
