//! L0c pure helpers: pin a run's executed closure (repo snapshot + toolchain
//! digest), deterministically verify a replay, and project an external OTel-GenAI
//! trace into a captured event tape (`docs/designs/trust-substrate.md` §L0c / §L5).
//!
//! Everything here is a pure function of its arguments — no IO, no wall-clock, no
//! randomness — so the same inputs yield byte-identical digests/manifests (INV-R2).
//! SHA-256 via `sha2`, mirroring [`crate::provenance`].

use crate::provenance::{EventKind, ReplayManifest, Run, RunInputs, ToolchainPin, build_ledger};
use crate::snapshot::CatalogSnapshot;
use sha2::{Digest, Sha256};

/// ASCII unit / record separators, so a field/row boundary can never be forged by
/// crafted path contents (the digest is unambiguous).
const US: u8 = 0x1f;
const RS: u8 = 0x1e;

/// Content-address a repo snapshot: SHA-256 over its entries sorted by
/// `(root_id, rel_path)`, each bound to its byte `size`. A path-set + size **proxy**
/// — the kernel keeps no per-file body hash today, so a same-shape edit that
/// preserves sizes is not distinguished; the per-byte / OCI-digest upgrade is the
/// deferred `EnvironmentPinner` seam. Deterministic: entries are sorted first.
#[must_use]
pub fn hash_repo_snapshot(snapshot: &CatalogSnapshot) -> String {
    let mut rows: Vec<(&str, &str, u64)> = snapshot
        .entries
        .iter()
        .map(|e| (e.root_id.as_str(), e.rel_path.as_str(), e.size))
        .collect();
    rows.sort_unstable();
    let mut hasher = Sha256::new();
    for (root_id, rel_path, size) in rows {
        hasher.update(root_id.as_bytes());
        hasher.update([US]);
        hasher.update(rel_path.as_bytes());
        hasher.update([US]);
        hasher.update(size.to_le_bytes());
        hasher.update([RS]);
    }
    hex(&hasher.finalize())
}

/// Content-address a resolved toolchain: SHA-256 over its sorted tool→version and
/// lockfile→content-hash maps. `BTreeMap` iterates sorted, so the digest is
/// byte-stable regardless of insertion order.
#[must_use]
pub fn hash_toolchain(pin: &ToolchainPin) -> String {
    let mut hasher = Sha256::new();
    for (key, value) in &pin.tools {
        hasher.update(b"tool");
        hasher.update([US]);
        hasher.update(key.as_bytes());
        hasher.update([US]);
        hasher.update(value.as_bytes());
        hasher.update([RS]);
    }
    for (key, value) in &pin.lockfiles {
        hasher.update(b"lock");
        hasher.update([US]);
        hasher.update(key.as_bytes());
        hasher.update([US]);
        hasher.update(value.as_bytes());
        hasher.update([RS]);
    }
    hex(&hasher.finalize())
}

/// Assemble the pinned [`RunInputs`] from the computed digests (the closure a run
/// executed in). `image_digest` is `None` until the strong-isolation seam lands.
#[must_use]
pub fn build_run_inputs(
    repo_snapshot_hash: String,
    toolchain_digest: String,
    image_digest: Option<String>,
) -> RunInputs {
    RunInputs {
        repo_snapshot_hash,
        toolchain_digest,
        image_digest,
        // The honest containment fact is stamped by the impure capture/verify path
        // (the launcher's probed tier); this pure assembler leaves the weak default.
        ..RunInputs::default()
    }
}

/// Deterministically verify a replay: re-derive the content-addressed spine over the
/// run's recorded events and compare to the recorded `root_hash`. `matched` is the
/// byte-for-byte equality (the CI gate); `diverged_at_seq` is the first event whose
/// re-derived chained hash differs from the recorded ledger (best-effort). A
/// mismatch is a *verdict*, never an error.
#[must_use]
pub fn verify_replay(run: &Run) -> ReplayManifest {
    let (ledger, replayed_root_hash) = build_ledger(&run.events);
    let matched = replayed_root_hash == run.root_hash;
    let diverged_at_seq = if matched {
        None
    } else {
        ledger
            .iter()
            .zip(run.ledger.iter())
            .find(|(re_derived, recorded)| re_derived.chained_hash != recorded.chained_hash)
            .map(|(re_derived, _)| re_derived.seq)
            .or_else(|| run.events.first().map(|event| event.seq))
    };
    ReplayManifest {
        run_id: run.run_id.clone(),
        recorded_root_hash: run.root_hash.clone(),
        replayed_root_hash,
        matched,
        event_count: run.events.len() as u64,
        diverged_at_seq,
    }
}

/// One OTel-GenAI span projected to the fields L0c needs to reconstruct a partial run.
#[derive(Debug, Clone)]
pub struct SpanView {
    pub start_unix_nano: u64,
    pub span_id: String,
    pub gen_ai_operation: Option<String>,
    pub gen_ai_system: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub content_text: Option<String>,
}

/// Project a GenAI OTel trace into an L0c event tape (L5 partial attestation). Spans
/// are sorted by `(start_unix_nano, span_id)` for reorder-invariance, then emitted as
/// one `RunStarted`, per-span `Output` (if any content) + `UsageUpdated` (if any token
/// count), closed by one `RunFinished`. Empty input yields an empty tape.
#[must_use]
pub fn otel_genai_to_events(spans: &[SpanView]) -> Vec<EventKind> {
    if spans.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&SpanView> = spans.iter().collect();
    sorted.sort_by(|a, b| (a.start_unix_nano, &a.span_id).cmp(&(b.start_unix_nano, &b.span_id)));
    let agent = sorted[0]
        .gen_ai_system
        .clone()
        .unwrap_or_else(|| "otel".to_string());
    let task = sorted[0]
        .gen_ai_operation
        .clone()
        .unwrap_or_else(|| "chat".to_string());
    let mut events = vec![EventKind::RunStarted {
        agent,
        task,
        cwd: None,
        inputs: None,
    }];
    for (index, span) in sorted.iter().enumerate() {
        let turn = index as u64;
        if let Some(text) = &span.content_text {
            events.push(EventKind::Output {
                turn,
                text: text.clone(),
            });
        }
        if span.input_tokens.is_some() || span.output_tokens.is_some() {
            events.push(EventKind::UsageUpdated {
                turn,
                input_tokens: span.input_tokens.unwrap_or(0),
                output_tokens: span.output_tokens.unwrap_or(0),
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                cost_micro_usd: None,
            });
        }
    }
    events.push(EventKind::RunFinished {
        ok: true,
        exit_code: None,
        timed_out: false,
    });
    events
}

/// Lowercase-hex encode bytes (mirrors `provenance::hex`).
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
    use crate::provenance::build_run;

    #[test]
    fn toolchain_hash_is_order_independent_and_stable() {
        let mut a = ToolchainPin::default();
        a.tools.insert("rustc".into(), "1.95".into());
        a.tools.insert("cargo".into(), "1.95".into());
        a.lockfiles.insert("Cargo.lock".into(), "abc".into());
        let mut b = ToolchainPin::default();
        // Different insertion order, same content -> same digest (BTreeMap sorts).
        b.lockfiles.insert("Cargo.lock".into(), "abc".into());
        b.tools.insert("cargo".into(), "1.95".into());
        b.tools.insert("rustc".into(), "1.95".into());
        assert_eq!(hash_toolchain(&a), hash_toolchain(&b));
        assert_eq!(hash_toolchain(&a).len(), 64);
        // A changed version flips the digest.
        a.tools.insert("rustc".into(), "1.96".into());
        assert_ne!(hash_toolchain(&a), hash_toolchain(&b));
    }

    #[test]
    fn verify_replay_matches_a_freshly_built_run() {
        let events = vec![
            crate::provenance::Event {
                seq: 0,
                kind: EventKind::RunStarted {
                    agent: "codex".into(),
                    task: "t".into(),
                    cwd: None,
                    inputs: None,
                },
            },
            crate::provenance::Event {
                seq: 1,
                kind: EventKind::Output {
                    turn: 0,
                    text: "x".into(),
                },
            },
        ];
        let run = build_run(
            "s",
            "codex",
            None,
            1,
            Some(2),
            true,
            events,
            RunInputs::default(),
        );
        let manifest = verify_replay(&run);
        assert!(manifest.matched);
        assert_eq!(manifest.recorded_root_hash, manifest.replayed_root_hash);
        assert_eq!(manifest.event_count, 2);
        assert_eq!(manifest.diverged_at_seq, None);

        // Tamper with the recorded root: the replay no longer matches.
        let mut tampered = run.clone();
        tampered.root_hash = "deadbeef".into();
        let bad = verify_replay(&tampered);
        assert!(!bad.matched);
    }

    #[test]
    fn otel_genai_projection_is_reorder_invariant() {
        let span = |nano: u64, id: &str, content: &str| SpanView {
            start_unix_nano: nano,
            span_id: id.into(),
            gen_ai_operation: Some("chat".into()),
            gen_ai_system: Some("openai".into()),
            input_tokens: Some(10),
            output_tokens: Some(5),
            content_text: Some(content.into()),
        };
        let forward = otel_genai_to_events(&[span(1, "a", "first"), span(2, "b", "second")]);
        let reversed = otel_genai_to_events(&[span(2, "b", "second"), span(1, "a", "first")]);
        assert_eq!(forward, reversed);
        // RunStarted + (Output + UsageUpdated) * 2 + RunFinished = 6 events.
        assert_eq!(forward.len(), 6);
        assert!(otel_genai_to_events(&[]).is_empty());
    }
}
