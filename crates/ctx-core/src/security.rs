//! Fail-closed root policy and path containment checks.

use crate::models::{CtxError, RootRef};
use std::path::{Component, Path, PathBuf};

/// Allow-list policy for filesystem access.
#[derive(Debug, Clone)]
pub struct RootPolicy {
    roots: Vec<RootRef>,
}

impl RootPolicy {
    /// Build a policy from canonicalized allowed roots.
    pub fn new(paths: Vec<PathBuf>) -> Result<Self, CtxError> {
        if paths.is_empty() {
            return Ok(Self { roots: Vec::new() });
        }

        let mut roots = Vec::with_capacity(paths.len());
        for (idx, path) in paths.into_iter().enumerate() {
            reject_traversal(&path)?;
            let canonical = path.canonicalize().map_err(|err| CtxError::io(path, err))?;
            roots.push(RootRef {
                id: format!("root-{idx}"),
                path: canonical,
            });
        }
        Ok(Self { roots })
    }

    #[must_use]
    pub fn roots(&self) -> &[RootRef] {
        &self.roots
    }

    /// Resolve an input path and ensure it is contained in a configured root.
    pub fn resolve_allowed(&self, path: &Path) -> Result<PathBuf, CtxError> {
        if self.roots.is_empty() {
            return Err(CtxError::NoRoots);
        }
        reject_traversal(path)?;

        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.roots[0].path.join(path)
        };
        let canonical = candidate
            .canonicalize()
            .map_err(|err| CtxError::io(candidate.clone(), err))?;

        if self
            .roots
            .iter()
            .any(|root| canonical.starts_with(&root.path))
        {
            Ok(canonical)
        } else {
            Err(CtxError::OutsideRoots(canonical))
        }
    }

    /// Resolve a path for writing. If it already exists this matches
    /// [`Self::resolve_allowed`]; otherwise the parent directory must exist and
    /// be within an allowed root, and the would-be canonical path is returned.
    /// Used for create and move-destination targets.
    pub fn resolve_for_write(&self, path: &Path) -> Result<PathBuf, CtxError> {
        if self.roots.is_empty() {
            return Err(CtxError::NoRoots);
        }
        reject_traversal(path)?;

        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.roots[0].path.join(path)
        };

        if let Ok(canonical) = candidate.canonicalize() {
            return if self
                .roots
                .iter()
                .any(|root| canonical.starts_with(&root.path))
            {
                Ok(canonical)
            } else {
                Err(CtxError::OutsideRoots(canonical))
            };
        }

        let parent = candidate
            .parent()
            .ok_or_else(|| CtxError::OutsideRoots(candidate.clone()))?;
        let file_name = candidate
            .file_name()
            .ok_or_else(|| CtxError::OutsideRoots(candidate.clone()))?;
        let canonical_parent = parent
            .canonicalize()
            .map_err(|err| CtxError::io(parent.to_path_buf(), err))?;
        if !canonical_parent.is_dir() {
            return Err(CtxError::io(
                canonical_parent,
                std::io::Error::new(
                    std::io::ErrorKind::NotADirectory,
                    "parent is not a directory",
                ),
            ));
        }
        let resolved = canonical_parent.join(file_name);
        if self
            .roots
            .iter()
            .any(|root| resolved.starts_with(&root.path))
        {
            Ok(resolved)
        } else {
            Err(CtxError::OutsideRoots(resolved))
        }
    }
}

fn reject_traversal(path: &Path) -> Result<(), CtxError> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CtxError::PathTraversal(path.display().to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_roots_refuses_resolution() {
        let policy = RootPolicy::new(Vec::new()).expect("policy");
        let err = policy.resolve_allowed(Path::new("src/lib.rs")).unwrap_err();
        assert!(matches!(err, CtxError::NoRoots));
    }

    #[test]
    fn traversal_is_rejected() {
        let err = RootPolicy::new(vec![PathBuf::from("..")]).unwrap_err();
        assert!(matches!(err, CtxError::PathTraversal(_)));
    }
}
