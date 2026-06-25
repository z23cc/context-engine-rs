//! Run-capture provenance vocabulary — the **L0 flight-recorder** data shapes
//! (`docs/designs/trust-substrate.md` §3 L0/L1, §5). A delegated agent run is
//! captured as an ordered, content-addressed tape of [`Event`]s; the tape is
//! sealed into a [`Run`] whose [`Run::root_hash`] is the single content address
//! committing to the whole ordered sequence (a linear hash chain at L0, upgraded
//! to a Merkle tree only when receipts need partial-inclusion proofs).
//!
//! These are **pure, transport-neutral serde data** (INV-R5: receipts & ledger are
//! portable, signed, append-only, additive protocol data) with **no behavior** —
//! every hash field is a plain `String`. The pure canonicalization + SHA-256
//! hashing that *fills* the hash fields lives in `nerve-core::provenance`
//! (INV-R2: the hashing is pure and golden-tested), never here. Hosts (the daemon)
//! capture and persist runs; `nerve-core` seals them; this crate only names the
//! shapes so they are wasm-shareable and appear in the exported protocol schema.
//!
//! **No floats** appear in any hashed type ([`Event`] / [`EventKind`]): token
//! counts are `u64` and cost is integer micro-USD, so the canonical JSON is
//! byte-stable and the types derive `Eq` — exact golden snapshots, no precision or
//! `-0.0`/NaN nondeterminism (INV-R2).

#[cfg(feature = "schema")]
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// On-disk + on-wire provenance schema version. Bumped only for additive,
/// backward-compatible changes to the [`Run`] shape (mirrors `WorkflowDef`'s
/// `schema_version`); a reader rejects a record from a newer major it cannot
/// understand rather than silently dropping fields.
pub const RUN_SCHEMA_VERSION: u32 = 2;

/// One typed, replayable step in a captured run's tape (`trust-substrate.md`
/// §5 `Event.kind`). Internally tagged (`{"kind": "...", ...}`), mirroring
/// [`crate::FlowDecisionKind`] / [`crate::AgentEventKind`] so the audit trail is
/// golden-diffable. The set is intentionally small and **execution-grounded** —
/// only events a real delegated CLI run actually produces today. The tool-lifecycle
/// kinds (`tool_started` / `tool_finished`) lift the structured tool calls the agent
/// streams DO carry — claude `tool_use` / `tool_result` content blocks, codex
/// `command_execution` / `file_change` items — into a queryable index of *which*
/// tools ran, files were edited, and commands executed (the full inputs/outputs also
/// remain verbatim in the raw `Output` lines). They are appended AFTER the
/// pre-existing variants, so a run that uses none of them serializes — and
/// content-addresses — byte-for-byte as before. Additive: new kinds may be appended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    /// The run began: which agent, the delegated task text, and the working dir.
    /// `inputs` is the L0c pinned closure (repo snapshot + toolchain digest) hashed
    /// in-band; absent on a legacy/unpinned run, so an omitted `inputs` reproduces
    /// the pre-L0c content address byte-for-byte.
    RunStarted {
        agent: String,
        task: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inputs: Option<RunInputs>,
    },
    /// A turn of the delegated agent began. `turn` is a 0-based logical index.
    TurnStarted { turn: u64 },
    /// A raw stdout/stderr line from the delegated CLI — the tape unit. `turn`
    /// attributes it to the turn that produced it.
    Output { turn: u64, text: String },
    /// Per-turn token/cost rollup, emitted only when the agent reported usage.
    /// All counts are `u64`; cost is integer micro-USD (never a float) so the
    /// hashed canonical bytes are stable.
    UsageUpdated {
        turn: u64,
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        cache_read_tokens: u64,
        #[serde(default)]
        cache_creation_tokens: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_micro_usd: Option<u64>,
    },
    /// A turn finished. `ok` is the turn's success as the host observed it.
    TurnFinished { turn: u64, ok: bool },
    /// The run finished: overall success, the process exit code when known, and
    /// whether it was killed by the wall-clock timeout.
    RunFinished {
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(default)]
        timed_out: bool,
    },
    /// A tool / command the delegated agent invoked, lifted from the agent's own
    /// structured stream (claude `tool_use` content blocks; codex
    /// `command_execution` / `file_change` items). `tool` is the vendor tool name
    /// (e.g. "Bash", "Edit", "Read"); `title` is a bounded human-identifying summary
    /// — the file path for file tools, the command for a shell tool — truncated to
    /// <= 200 chars (never a float); `args_hash` is the SHA-256 of the canonical args
    /// JSON (the full inputs also remain verbatim in the raw `Output` lines, so this
    /// is a queryable index, not the only copy). Appended AFTER the pre-existing
    /// variants so a run using none of them serializes — and content-addresses —
    /// byte-for-byte as before (L0 granularity, additive; INV-R2/INV-R5).
    ToolStarted {
        turn: u64,
        tool: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        args_hash: String,
    },
    /// The result of a tool the agent invoked. `ok` is the tool's success as the
    /// agent stream reported it; `output_hash` is the SHA-256 of the tool output.
    ToolFinished {
        turn: u64,
        tool: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        output_hash: String,
    },
}

/// One node in a run's append-only tape: a logical sequence number plus the typed
/// step. `seq` is a monotonic logical clock (0,1,2,…) assigned at capture, *not* a
/// wall-clock — so a replay reproduces byte-identical ordering and hashes. The
/// `kind` is a nested object (`{"seq":N,"kind":{"kind":"...",...}}`) rather than
/// flattened: a stable, schemars-clean `$ref` to [`EventKind`] that keeps the
/// exported protocol schema deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
}

/// One entry on the content-addressed spine: this event's own digest plus the
/// chained digest committing to it and every prior event
/// (`chained[n] = sha256(chained[n-1] || event_hash[n])`). A verifier re-derives
/// the spine from the [`Run::events`] to confirm the tape is untampered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct LedgerEntry {
    pub seq: u64,
    pub event_hash: String,
    pub chained_hash: String,
}

/// A captured, replayable agent run — the L0 unit of trust. The ordered
/// [`Self::events`] tape is the record; [`Self::ledger`] is its content-addressed
/// spine; [`Self::root_hash`] (the spine head, `""` for an empty tape) is the
/// single content address. `started_at_ms` / `finished_at_ms` are host wall-clock
/// metadata for display and are **excluded from the hashed bytes** (only `events`
/// are hashed), so they never perturb the content address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct Run {
    pub schema_version: u32,
    pub run_id: String,
    pub session_id: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub events: Vec<Event>,
    #[serde(default)]
    pub ledger: Vec<LedgerEntry>,
    #[serde(default)]
    pub root_hash: String,
    #[serde(default)]
    pub finished: bool,
    /// Denormalized mirror of the run's pinned closure (also carried in-band on the
    /// `RunStarted` event, which IS hashed). This top-level copy is for display/query
    /// and is **not** hashed — so adding it never perturbs an existing run's
    /// `root_hash` (L0c, additive).
    #[serde(default)]
    pub inputs: RunInputs,
    /// How completely the run was attested: `Full` = captured by Nerve's recorder;
    /// `Partial` = reconstructed from an external OTel trace (L5 ingest). Skipped
    /// when `Full`, so existing serialized runs round-trip byte-identically.
    #[serde(default, skip_serializing_if = "is_full_attestation")]
    pub attestation: Attestation,
}

/// The pinned closure a run executed in (`trust-substrate.md` §5 inputs): the
/// content address of the repo snapshot at start plus a digest over the resolved
/// toolchain/lockfiles. Hashed in-band on the `RunStarted` event so the run's
/// content address commits to *what ran*, not just the agent's output — the basis
/// of the "bit-for-bit replayable from recorded inputs" claim (L0c).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct RunInputs {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo_snapshot_hash: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub toolchain_digest: String,
    /// OCI image digest of a fully-reproduced environment, when available. `None`
    /// today (the strong-isolation `EnvironmentPinner` seam is deferred infra).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_digest: Option<String>,
}

/// The resolved toolchain a [`RunInputs::toolchain_digest`] is computed over:
/// tool→version and lockfile→content-hash maps. `BTreeMap` for deterministic
/// (sorted) iteration so the digest is byte-stable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ToolchainPin {
    #[serde(default)]
    pub tools: BTreeMap<String, String>,
    #[serde(default)]
    pub lockfiles: BTreeMap<String, String>,
}

/// The verdict of a deterministic replay (L0c `replay.start`): the recorded vs.
/// re-derived spine head and whether they matched. `matched == false` is a real
/// (recorded) divergence verdict, not a transport error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct ReplayManifest {
    pub run_id: String,
    pub recorded_root_hash: String,
    pub replayed_root_hash: String,
    pub matched: bool,
    pub event_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diverged_at_seq: Option<u64>,
}

/// How completely a run was attested. `Full` (the default) is a Nerve-captured run;
/// `Partial` is reconstructed from an external OTel trace and cannot be replayed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Attestation {
    #[default]
    Full,
    Partial,
}

/// Whether an [`Attestation`] is the default `Full` — the `skip_serializing_if`
/// predicate that keeps existing serialized runs byte-identical.
fn is_full_attestation(attestation: &Attestation) -> bool {
    *attestation == Attestation::Full
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_tags_are_snake_case() {
        let cases = [
            (
                EventKind::RunStarted {
                    agent: "codex".into(),
                    task: "t".into(),
                    cwd: None,
                    inputs: None,
                },
                "run_started",
            ),
            (EventKind::TurnStarted { turn: 0 }, "turn_started"),
            (
                EventKind::Output {
                    turn: 0,
                    text: "x".into(),
                },
                "output",
            ),
            (
                EventKind::TurnFinished { turn: 0, ok: true },
                "turn_finished",
            ),
            (
                EventKind::RunFinished {
                    ok: true,
                    exit_code: Some(0),
                    timed_out: false,
                },
                "run_finished",
            ),
        ];
        for (kind, tag) in cases {
            let value = serde_json::to_value(&kind).expect("kind json");
            assert_eq!(value["kind"], tag);
        }
    }

    #[test]
    fn event_flattens_kind_and_round_trips() {
        // `seq` sits beside the flattened internally-tagged kind on the wire.
        let event = Event {
            seq: 3,
            kind: EventKind::Output {
                turn: 1,
                text: "hello".into(),
            },
        };
        let value = serde_json::to_value(&event).expect("event json");
        assert_eq!(value["seq"], 3);
        assert_eq!(value["kind"]["kind"], "output");
        assert_eq!(value["kind"]["turn"], 1);
        assert_eq!(value["kind"]["text"], "hello");
        let back: Event = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, event);
    }

    #[test]
    fn usage_updated_omits_optional_cost_and_round_trips() {
        let event = Event {
            seq: 9,
            kind: EventKind::UsageUpdated {
                turn: 0,
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_micro_usd: None,
            },
        };
        let value = serde_json::to_value(&event).expect("usage json");
        assert_eq!(value["kind"]["kind"], "usage_updated");
        assert_eq!(value["kind"]["input_tokens"], 100);
        assert!(value["kind"].get("cost_micro_usd").is_none());
        let back: Event = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, event);
    }

    #[test]
    fn run_round_trips_and_defaults_are_tolerant() {
        let run = Run {
            schema_version: RUN_SCHEMA_VERSION,
            run_id: "abc123".into(),
            session_id: "job-7".into(),
            agent: "codex".into(),
            root: Some("/repo".into()),
            started_at_ms: 1000,
            finished_at_ms: Some(2000),
            events: vec![Event {
                seq: 0,
                kind: EventKind::TurnStarted { turn: 0 },
            }],
            ledger: vec![LedgerEntry {
                seq: 0,
                event_hash: "ff".into(),
                chained_hash: "ee".into(),
            }],
            root_hash: "ee".into(),
            finished: true,
            inputs: RunInputs::default(),
            attestation: Attestation::Full,
        };
        let value = serde_json::to_value(&run).expect("run json");
        let back: Run = serde_json::from_value(value).expect("round-trip");
        assert_eq!(back, run);

        // A minimal record (only the non-default fields) deserializes, with the
        // additive fields falling back to their defaults — forward tolerance.
        let minimal: Run = serde_json::from_value(serde_json::json!({
            "schema_version": RUN_SCHEMA_VERSION,
            "run_id": "x",
            "session_id": "s",
            "agent": "claude",
            "started_at_ms": 5,
            "events": [],
        }))
        .expect("minimal run");
        assert_eq!(minimal.root, None);
        assert_eq!(minimal.finished_at_ms, None);
        assert!(minimal.ledger.is_empty());
        assert_eq!(minimal.root_hash, "");
        assert!(!minimal.finished);
    }
}
