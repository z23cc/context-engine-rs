//! Pure, golden-tested content-addressing for the L0 run-capture tape
//! (`docs/designs/trust-substrate.md` §3 L0/L1, INV-R2). Given the
//! [`nerve_proto::provenance`] shapes, this seals an ordered [`Event`] tape into a
//! content-addressed [`Run`]: each event is SHA-256 hashed over its canonical JSON,
//! and the per-event digests are folded into a linear hash chain whose head
//! ([`Run::root_hash`]) is the single content address committing to the whole
//! ordered sequence.
//!
//! **This is the determinism boundary's L0 brick:** every function here is a pure
//! function of its arguments — no IO, no wall-clock, no randomness — so the same
//! tape in yields a byte-identical [`Run`] out, and a replay reproduces identical
//! hashes. That property is regression-locked by `tests/golden_provenance.rs`.
//! Capture and persistence (which DO touch the world) live above the kernel in
//! `nerve-workstation` (INV-R2).
//!
//! SHA-256 (not the FNV-1a in [`crate::edit`]) is used deliberately: the FNV hash
//! is non-cryptographic and whitespace-lossy — fine for stale-edit detection, wrong
//! for an audit trail that portable receipts (L4) will sign.

// Re-export the shapes this module content-addresses so a consumer of the kernel
// builds an event tape (and reads back a sealed `Run`) through `nerve_core` alone,
// without taking its own `nerve-proto` dependency.
pub use nerve_proto::provenance::{
    Attestation, Event, EventKind, LedgerEntry, RUN_SCHEMA_VERSION, ReplayManifest, Run, RunInputs,
    ToolchainPin,
};
use sha2::{Digest, Sha256};

/// Lowercase-hex SHA-256 of one event's canonical JSON. Deterministic: every
/// hashed type is a fixed-field struct or an internally-tagged enum with **no
/// maps and no floats**, so `serde_json` emits byte-stable bytes (INV-R2).
#[must_use]
pub fn hash_event(event: &Event) -> String {
    let bytes = serde_json::to_vec(event).expect("Event serializes infallibly");
    hex(Sha256::digest(bytes).as_slice())
}

/// Lowercase-hex SHA-256 over a JSON value's canonical bytes — the digest that
/// fills the `args_hash` / `output_hash` fields of the tool-lifecycle
/// [`EventKind`]s. `serde_json` emits object keys in sorted order (no
/// `preserve_order` feature), so the bytes are canonical and the digest is
/// deterministic; capture (in `nerve-workstation`) calls this so the hashing stays
/// pure and above-the-boundary code never reimplements it (INV-R2).
#[must_use]
pub fn hash_canonical_json(value: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(value).expect("Value serializes infallibly");
    hex(Sha256::digest(bytes).as_slice())
}

/// Fold an ordered tape into its content-addressed spine. For each event,
/// `chained[n] = sha256(chained[n-1] || event_hash[n])` with `chained[-1] = ""`;
/// the returned root is `chained[last]` (`""` for an empty tape). Each
/// [`LedgerEntry`] records both digests so a verifier can re-derive — and thus
/// detect tampering of — the spine from [`Run::events`] alone.
#[must_use]
pub fn build_ledger(events: &[Event]) -> (Vec<LedgerEntry>, String) {
    let mut ledger = Vec::with_capacity(events.len());
    let mut prev = String::new();
    for event in events {
        let event_hash = hash_event(event);
        let mut hasher = Sha256::new();
        hasher.update(prev.as_bytes());
        hasher.update(event_hash.as_bytes());
        let chained_hash = hex(hasher.finalize().as_slice());
        ledger.push(LedgerEntry {
            seq: event.seq,
            event_hash,
            chained_hash: chained_hash.clone(),
        });
        prev = chained_hash;
    }
    (ledger, prev)
}

/// Seal a captured tape into a content-addressed [`Run`]. `run_id` is set to the
/// tape's `root_hash` — at L0 the run's identity *is* the content address of its
/// event sequence (which, via the `RunStarted` event, already commits to the agent,
/// task, and every output line), so the id is reproducible on replay (a later brick
/// folds in the pinned toolchain digest). `started_at_ms` / `finished_at_ms` are
/// host metadata carried for display and are **never** hashed (only `events` are),
/// so wall-clock never perturbs the content address.
#[must_use]
#[allow(clippy::too_many_arguments)] // reason: one cohesive seal call (delegates to
// build_run_attested); the run identity, host timestamps, the tape, and the pinned
// inputs are independent — bundling them adds indirection without isolating a concern.
pub fn build_run(
    session_id: impl Into<String>,
    agent: impl Into<String>,
    root: Option<String>,
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    finished: bool,
    events: Vec<Event>,
    inputs: RunInputs,
) -> Run {
    build_run_attested(
        session_id,
        agent,
        root,
        started_at_ms,
        finished_at_ms,
        finished,
        events,
        inputs,
        Attestation::Full,
    )
}

/// Seal a tape into a [`Run`] with an explicit [`Attestation`] — `Full` for a
/// Nerve-captured run, `Partial` for one reconstructed from an external OTel trace
/// (L5). The `inputs` mirror and `attestation` are stored for display/query and are
/// **not** hashed, so they never perturb the content address (only `events` are
/// hashed). [`build_run`] delegates here with `Attestation::Full`.
#[must_use]
#[allow(clippy::too_many_arguments)] // reason: one cohesive seal call; the run
// identity (session/agent/root), host timestamps, the tape, the pinned inputs, and
// the attestation tier are independent inputs — bundling them adds indirection
// without isolating a separate responsibility.
pub fn build_run_attested(
    session_id: impl Into<String>,
    agent: impl Into<String>,
    root: Option<String>,
    started_at_ms: u64,
    finished_at_ms: Option<u64>,
    finished: bool,
    events: Vec<Event>,
    inputs: RunInputs,
    attestation: Attestation,
) -> Run {
    let (ledger, root_hash) = build_ledger(&events);
    Run {
        schema_version: RUN_SCHEMA_VERSION,
        run_id: root_hash.clone(),
        session_id: session_id.into(),
        agent: agent.into(),
        root,
        started_at_ms,
        finished_at_ms,
        events,
        ledger,
        root_hash,
        finished,
        inputs,
        attestation,
    }
}

/// Lowercase-hex encode bytes (no allocation per byte beyond the result string).
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64, kind: EventKind) -> Event {
        Event { seq, kind }
    }

    fn sample_tape() -> Vec<Event> {
        vec![
            ev(
                0,
                EventKind::RunStarted {
                    agent: "codex".into(),
                    task: "add a test".into(),
                    cwd: Some("/repo".into()),
                    inputs: None,
                },
            ),
            ev(1, EventKind::TurnStarted { turn: 0 }),
            ev(
                2,
                EventKind::Output {
                    turn: 0,
                    text: "running".into(),
                },
            ),
            ev(3, EventKind::TurnFinished { turn: 0, ok: true }),
            ev(
                4,
                EventKind::RunFinished {
                    ok: true,
                    exit_code: Some(0),
                    timed_out: false,
                },
            ),
        ]
    }

    #[test]
    fn hash_event_is_stable_and_distinguishes_content() {
        let a = ev(0, EventKind::TurnStarted { turn: 0 });
        assert_eq!(hash_event(&a), hash_event(&a), "same event -> same hash");
        // 64 lowercase hex chars (SHA-256).
        assert_eq!(hash_event(&a).len(), 64);
        assert!(hash_event(&a).chars().all(|c| c.is_ascii_hexdigit()));
        // A different payload yields a different digest.
        let b = ev(0, EventKind::TurnStarted { turn: 1 });
        assert_ne!(hash_event(&a), hash_event(&b));
        // Even the same kind at a different seq differs (seq is hashed).
        let c = ev(1, EventKind::TurnStarted { turn: 0 });
        assert_ne!(hash_event(&a), hash_event(&c));
    }

    #[test]
    fn ledger_chains_and_is_tamper_evident() {
        let tape = sample_tape();
        let (ledger, root) = build_ledger(&tape);
        assert_eq!(ledger.len(), tape.len());
        assert!(!root.is_empty());
        assert_eq!(ledger.last().unwrap().chained_hash, root);
        // The build is deterministic.
        let (_, root_again) = build_ledger(&tape);
        assert_eq!(root, root_again);
        // Mutating the FIRST event perturbs the head (and thus every chained hash
        // after it) — the chain is tamper-evident.
        let mut tampered = tape.clone();
        tampered[0] = ev(
            0,
            EventKind::RunStarted {
                agent: "codex".into(),
                task: "DIFFERENT".into(),
                cwd: Some("/repo".into()),
                inputs: None,
            },
        );
        let (_, tampered_root) = build_ledger(&tampered);
        assert_ne!(root, tampered_root);
    }

    #[test]
    fn empty_tape_yields_empty_root() {
        let (ledger, root) = build_ledger(&[]);
        assert!(ledger.is_empty());
        assert_eq!(root, "");
    }

    #[test]
    fn hash_canonical_json_is_stable_and_key_order_independent() {
        use serde_json::json;
        // Same logical value built in two key orders -> same canonical digest
        // (serde_json sorts object keys when `preserve_order` is off).
        let a = json!({ "command": "cargo test", "cwd": "/repo" });
        let b = json!({ "cwd": "/repo", "command": "cargo test" });
        assert_eq!(hash_canonical_json(&a), hash_canonical_json(&b));
        assert_eq!(hash_canonical_json(&a).len(), 64);
        // A different payload differs.
        let c = json!({ "command": "cargo build", "cwd": "/repo" });
        assert_ne!(hash_canonical_json(&a), hash_canonical_json(&c));
    }

    #[test]
    fn tool_lifecycle_events_hash_deterministically_and_distinguish_content() {
        // A tape that USES the new ToolStarted/ToolFinished variants seals
        // deterministically (same tape -> same content address) and a different
        // tool/title/hash yields a different address.
        let tape_with_tools = |title: &str, args_hash: &str| {
            vec![
                ev(0, EventKind::TurnStarted { turn: 0 }),
                ev(
                    1,
                    EventKind::ToolStarted {
                        turn: 0,
                        tool: "Edit".into(),
                        title: Some(title.into()),
                        args_hash: args_hash.into(),
                    },
                ),
                ev(
                    2,
                    EventKind::ToolFinished {
                        turn: 0,
                        tool: "Edit".into(),
                        ok: true,
                        title: Some(title.into()),
                        output_hash: "ff".into(),
                    },
                ),
            ]
        };
        let (_, root_a) = build_ledger(&tape_with_tools("src/a.rs", "aa"));
        let (_, root_again) = build_ledger(&tape_with_tools("src/a.rs", "aa"));
        assert_eq!(root_a, root_again, "same tool tape -> same content address");
        // A different title distinguishes content.
        let (_, root_b) = build_ledger(&tape_with_tools("src/b.rs", "aa"));
        assert_ne!(root_a, root_b);
        // A different args_hash distinguishes content.
        let (_, root_c) = build_ledger(&tape_with_tools("src/a.rs", "bb"));
        assert_ne!(root_a, root_c);
    }

    #[test]
    fn build_run_addresses_by_root_and_excludes_wallclock() {
        let tape = sample_tape();
        let run_a = build_run(
            "job-7",
            "codex",
            Some("/repo".into()),
            1000,
            Some(2000),
            true,
            tape.clone(),
            RunInputs::default(),
        );
        assert_eq!(run_a.run_id, run_a.root_hash);
        assert_eq!(run_a.schema_version, RUN_SCHEMA_VERSION);
        assert_eq!(run_a.events.len(), 5);
        // Same tape, DIFFERENT wall-clock + session id -> identical content address
        // (timestamps and the session id are not part of the hashed bytes).
        let run_b = build_run(
            "job-99",
            "codex",
            Some("/repo".into()),
            9999,
            Some(123_456),
            true,
            tape,
            RunInputs::default(),
        );
        assert_eq!(run_a.root_hash, run_b.root_hash);
        assert_eq!(run_a.run_id, run_b.run_id);
    }
}
