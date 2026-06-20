//! Advisory tool risk/capability vocabulary — the protocol-data half of the
//! adapter surface (the `RuntimeToolAdapter` trait itself stays in
//! `nerve-runtime`, as it references engine ports). Transport-neutral data a
//! permission/UI layer can reason about; it does not gate execution today.

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Coarse risk classification for a tool, ordered least-to-most privileged.
/// Advisory protocol data consumed by a future permission engine (P4); it does
/// not gate execution today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RiskTier {
    /// Pure reads: catalog/search/navigation that never mutate state.
    ReadOnly,
    /// Mutates workspace files (writes, patches, moves, deletes).
    Edit,
    /// Runs arbitrary commands or otherwise escapes the file sandbox.
    Exec,
}

/// Declared capabilities and risk surface of a runtime tool. Advisory only:
/// transport-neutral data a permission/UI layer can reason about. The default
/// is intentionally the *most permissive* so adapters that don't declare a
/// capability are never silently restricted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ToolCapability {
    pub risk: RiskTier,
    pub reads_fs: bool,
    pub writes_fs: bool,
    pub network: bool,
}

impl Default for ToolCapability {
    /// Most permissive default: highest risk, all surfaces enabled, so an
    /// adapter that hasn't opted into a narrower descriptor is treated as
    /// fully capable (non-breaking for existing adapters).
    fn default() -> Self {
        Self {
            risk: RiskTier::Exec,
            reads_fs: true,
            writes_fs: true,
            network: true,
        }
    }
}
