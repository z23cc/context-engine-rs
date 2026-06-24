//! L5 OTel-GenAI ingest (`docs/designs/trust-substrate.md` §8 L5) — the third face of
//! the integration seam. It reconstructs a **`Partial`-attestation** [`Run`] from an
//! external OpenTelemetry **GenAI** trace (`gen_ai.*` semantic conventions), so even
//! an agent Nerve never instrumented is partially attested from the trace its host
//! already emits.
//!
//! **Partial, and honestly so (INV-R1/INV-R2).** A trace is a witness, not the live
//! tape: it cannot prove the exact byte-for-byte outputs Nerve captures on its own
//! path. So the reconstructed run is sealed with [`Attestation::Partial`] — the same
//! content-addressing (a SHA-256 hash chain over the ordered events) but flagged as
//! second-hand, never conflated with a `Full` Nerve-captured run.
//!
//! The trace → events mapping is **pure** and lives in the kernel
//! ([`nerve_core::runpin::otel_genai_to_events`]): a deterministic function of the
//! parsed [`SpanView`]s, ordered by `start_unix_nano` then `span_id` so a reordered
//! export yields the byte-identical run. Wall-clock from the trace is display metadata
//! only and is never hashed. Parsing the OTLP dialect into [`SpanView`]s and persisting
//! (the impure acts) live here above the determinism boundary; the seal
//! ([`build_run_attested`]) and the span→event map are the kernel's pure helpers
//! (INV-R2).
//!
//! The [`OtelIngestor`] trait is the deferred-infra seam: [`GenAiOtelIngestor`] handles
//! the `gen_ai.*` semconv today; a Langfuse/Arize dialect can implement the trait
//! without touching the run-build path.

use nerve_core::provenance::{Attestation, Event, RunInputs, build_run_attested};
use nerve_core::runpin::{SpanView, otel_genai_to_events};
use nerve_runtime::RuntimeError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

/// Where an OTel trace to ingest comes from. Mirrors the protocol's
/// `nerve_proto::command::OtelSource` (untagged: an inline trace object or a
/// filesystem path). Defined locally so this file compiles standalone and is
/// unit-testable; the integrator maps the protocol enum into this at the call site.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum OtelSource {
    /// An inline OTel trace payload (a spans document).
    Inline { trace: Value },
    /// A filesystem path to an OTel trace export.
    Path { trace_path: String },
}

/// Reconstruct a Partial-attestation run from an OTel source. Parses the trace into
/// ordered [`SpanView`]s (via the trait), maps them to a deterministic event tape via
/// the kernel's pure [`otel_genai_to_events`], seals it with [`Attestation::Partial`],
/// and persists it best-effort. Returns `{"run_id","events","attestation":"partial"}`.
/// A missing/unreadable source is an adapter error; a store with no served root (or a
/// write failure) degrades gracefully — the run is still returned.
pub(crate) fn handle_otel_ingest(
    source: &OtelSource,
    store: Option<&crate::run_store::RunStore>,
    _root: Option<&Path>,
) -> Result<Value, RuntimeError> {
    let trace = load_trace(source)?;
    let ingestor = GenAiOtelIngestor;
    let spans = ingestor.parse_spans(&trace);
    let agent = ingestor.infer_agent(&spans);

    // The span → event tape map is the kernel's pure helper (INV-R2): deterministic
    // and reorder-invariant. We assign the logical `seq` (the hashed clock) here.
    let tape: Vec<Event> = otel_genai_to_events(&spans)
        .into_iter()
        .enumerate()
        .map(|(seq, kind)| Event {
            seq: seq as u64,
            kind,
        })
        .collect();
    let event_count = tape.len() as u64;

    // Wall-clock from the trace is display metadata only (never hashed). The session
    // id is the trace's origin; root is unset for an externally-sourced run.
    let started = spans
        .first()
        .map(|s| s.start_unix_nano / 1_000_000)
        .unwrap_or(0);
    let run = build_run_attested(
        format!("otel:{agent}"),
        agent,
        None,
        started,
        None,
        true,
        tape,
        RunInputs::default(),
        Attestation::Partial,
    );

    // Best-effort persistence: a write failure NEVER fails the ingest (the run, with
    // its content address, is still returned for the caller to surface).
    if let Some(store) = store {
        let _ = store.write_record(&run);
    }

    Ok(json!({
        "run_id": run.run_id,
        "events": event_count,
        "attestation": "partial",
    }))
}

/// Parses an OTel trace document into ordered [`SpanView`]s. The deferred-infra seam:
/// the default [`GenAiOtelIngestor`] reads the `gen_ai.*` semconv; another dialect
/// (Langfuse/Arize) implements this trait without touching the run-build path.
pub(crate) trait OtelIngestor {
    /// Extract the GenAI spans from a trace document, ordered deterministically by
    /// `start_unix_nano` then `span_id` so a reordered export yields the same run.
    fn parse_spans(&self, trace: &Value) -> Vec<SpanView>;

    /// Infer the agent/system label for the reconstructed run from the spans.
    fn infer_agent(&self, spans: &[SpanView]) -> String;
}

/// Ingestor for the OpenTelemetry **GenAI** semantic conventions (`gen_ai.*`). Reads
/// the standard OTLP-ish JSON shape: `resourceSpans[].scopeSpans[].spans[]` with span
/// `attributes[] = {key, value:{stringValue|intValue}}`. Tolerant of either OTLP
/// `attributes` arrays or a flat `attributes` object.
pub(crate) struct GenAiOtelIngestor;

impl OtelIngestor for GenAiOtelIngestor {
    fn parse_spans(&self, trace: &Value) -> Vec<SpanView> {
        let mut spans: Vec<SpanView> = collect_raw_spans(trace)
            .iter()
            .filter_map(parse_genai_span)
            .collect();
        spans.sort_by(|a, b| {
            a.start_unix_nano
                .cmp(&b.start_unix_nano)
                .then_with(|| a.span_id.cmp(&b.span_id))
        });
        spans
    }

    fn infer_agent(&self, spans: &[SpanView]) -> String {
        spans
            .iter()
            .find_map(|s| s.gen_ai_system.clone())
            .unwrap_or_else(|| "otel".to_string())
    }
}

/// Resolve the trace `Value` from the source (inline, or read from a path).
fn load_trace(source: &OtelSource) -> Result<Value, RuntimeError> {
    match source {
        OtelSource::Inline { trace } => Ok(trace.clone()),
        OtelSource::Path { trace_path } => {
            let raw = std::fs::read_to_string(trace_path).map_err(|err| {
                RuntimeError::adapter(format!("failed to read trace `{trace_path}`: {err}"))
            })?;
            serde_json::from_str(&raw)
                .map_err(|err| RuntimeError::adapter(format!("invalid OTel trace JSON: {err}")))
        }
    }
}

/// Flatten OTLP `resourceSpans[].scopeSpans[].spans[]` (tolerating `instrumentation
/// LibrarySpans` / a top-level `spans` array) into raw span values.
fn collect_raw_spans(trace: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(top) = trace.get("spans").and_then(Value::as_array) {
        out.extend(top.iter().cloned());
    }
    let resource_spans = trace
        .get("resourceSpans")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for rs in &resource_spans {
        let scopes = rs
            .get("scopeSpans")
            .or_else(|| rs.get("instrumentationLibrarySpans"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for scope in &scopes {
            if let Some(spans) = scope.get("spans").and_then(Value::as_array) {
                out.extend(spans.iter().cloned());
            }
        }
    }
    out
}

/// Parse one raw OTLP span into a [`SpanView`], reading `gen_ai.*` attributes.
fn parse_genai_span(span: &Value) -> Option<SpanView> {
    let attrs = span_attributes(span);
    let span_id = span
        .get("spanId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let start_unix_nano = span
        .get("startTimeUnixNano")
        .and_then(read_u64)
        .unwrap_or(0);
    Some(SpanView {
        start_unix_nano,
        span_id,
        gen_ai_operation: attr_string(&attrs, "gen_ai.operation.name"),
        gen_ai_system: attr_string(&attrs, "gen_ai.system"),
        input_tokens: attr_u64(&attrs, "gen_ai.usage.input_tokens"),
        output_tokens: attr_u64(&attrs, "gen_ai.usage.output_tokens"),
        content_text: attr_string(&attrs, "gen_ai.completion")
            .or_else(|| attr_string(&attrs, "gen_ai.response.content")),
    })
}

/// Normalize a span's attributes into a flat `key -> Value` map, accepting either the
/// OTLP `attributes` array (`[{key, value:{...}}]`) or a flat `attributes` object.
fn span_attributes(span: &Value) -> serde_json::Map<String, Value> {
    let mut map = serde_json::Map::new();
    match span.get("attributes") {
        Some(Value::Array(items)) => {
            for item in items {
                if let Some(key) = item.get("key").and_then(Value::as_str) {
                    let value = item.get("value").cloned().unwrap_or(Value::Null);
                    map.insert(key.to_string(), value);
                }
            }
        }
        Some(Value::Object(obj)) => {
            for (k, v) in obj {
                map.insert(k.clone(), v.clone());
            }
        }
        _ => {}
    }
    map
}

/// Read a string attribute, unwrapping an OTLP `{stringValue}` wrapper or a bare string.
fn attr_string(attrs: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let value = attrs.get(key)?;
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    value
        .get("stringValue")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Read a u64 attribute, unwrapping an OTLP `{intValue}` (which may be a JSON string).
fn attr_u64(attrs: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    let value = attrs.get(key)?;
    read_u64(value).or_else(|| value.get("intValue").and_then(read_u64))
}

/// Read a u64 from a JSON number or a numeric string (OTLP int64s ship as strings).
fn read_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::provenance::EventKind;
    use tempfile::tempdir;

    fn sample_trace() -> Value {
        json!({
            "resourceSpans": [{
                "scopeSpans": [{
                    "spans": [
                        {
                            "spanId": "s2", "startTimeUnixNano": "2000",
                            "attributes": [
                                {"key": "gen_ai.system", "value": {"stringValue": "anthropic"}},
                                {"key": "gen_ai.operation.name", "value": {"stringValue": "chat"}},
                                {"key": "gen_ai.usage.input_tokens", "value": {"intValue": "10"}},
                                {"key": "gen_ai.usage.output_tokens", "value": {"intValue": "20"}},
                                {"key": "gen_ai.completion", "value": {"stringValue": "second"}}
                            ]
                        },
                        {
                            "spanId": "s1", "startTimeUnixNano": "1000",
                            "attributes": {
                                "gen_ai.system": "anthropic",
                                "gen_ai.completion": "first"
                            }
                        }
                    ]
                }]
            }]
        })
    }

    #[test]
    fn parse_orders_by_start_then_span_id() {
        let spans = GenAiOtelIngestor.parse_spans(&sample_trace());
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].span_id, "s1"); // start 1000 first
        assert_eq!(spans[1].span_id, "s2");
        assert_eq!(spans[1].input_tokens, Some(10));
        assert_eq!(spans[1].output_tokens, Some(20));
        assert_eq!(spans[0].content_text.as_deref(), Some("first"));
    }

    #[test]
    fn events_bracket_turns_with_run_started_and_finished() {
        let spans = GenAiOtelIngestor.parse_spans(&sample_trace());
        let events = otel_genai_to_events(&spans);
        assert!(matches!(events.first(), Some(EventKind::RunStarted { .. })));
        assert!(matches!(events.last(), Some(EventKind::RunFinished { .. })));
        // s1 has content but no usage; s2 has both -> deterministic event mix.
        let outputs = events
            .iter()
            .filter(|e| matches!(e, EventKind::Output { .. }))
            .count();
        let usages = events
            .iter()
            .filter(|e| matches!(e, EventKind::UsageUpdated { .. }))
            .count();
        assert_eq!(outputs, 2);
        assert_eq!(usages, 1);
    }

    #[test]
    fn ingest_inline_persists_partial_run_and_returns_shape() {
        let dir = tempdir().unwrap();
        let store = crate::run_store::RunStore::new(dir.path().join("runs"));
        let out = handle_otel_ingest(
            &OtelSource::Inline {
                trace: sample_trace(),
            },
            Some(&store),
            Some(dir.path()),
        )
        .unwrap();

        assert_eq!(out["attestation"], "partial");
        assert!(out["events"].as_u64().unwrap() > 0);
        let run_id = out["run_id"].as_str().unwrap().to_string();
        assert_eq!(run_id.len(), 64, "content address is a SHA-256 hex");

        // The persisted run reloads and is flagged Partial.
        let loaded = store.load_record(&run_id).unwrap();
        assert_eq!(loaded.attestation, Attestation::Partial);
        assert_eq!(loaded.run_id, run_id);
    }

    #[test]
    fn reordered_trace_yields_identical_run_id() {
        let mut reordered = sample_trace();
        // Reverse the span array; ordering by start_unix_nano must make this a no-op.
        let spans = reordered["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        reordered["resourceSpans"][0]["scopeSpans"][0]["spans"] = json!(spans);

        let a = handle_otel_ingest(
            &OtelSource::Inline {
                trace: sample_trace(),
            },
            None,
            None,
        )
        .unwrap();
        let b = handle_otel_ingest(&OtelSource::Inline { trace: reordered }, None, None).unwrap();
        assert_eq!(
            a["run_id"], b["run_id"],
            "reorder-invariant content address"
        );
    }

    #[test]
    fn missing_path_source_is_an_adapter_error_not_a_panic() {
        let err = handle_otel_ingest(
            &OtelSource::Path {
                trace_path: "/no/such/trace.json".to_string(),
            },
            None,
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn empty_trace_yields_an_empty_partial_run_without_panicking() {
        let out = handle_otel_ingest(&OtelSource::Inline { trace: json!({}) }, None, None).unwrap();
        // No spans -> the kernel mapper yields an empty tape (no synthetic frame),
        // so the run carries zero events. The handler still returns the contract
        // shape (honest: a witness with nothing to attest), never a panic.
        assert_eq!(out["events"], 0);
        assert_eq!(out["attestation"], "partial");
        assert!(out["run_id"].is_string());
    }

    #[test]
    fn agent_inferred_from_gen_ai_system() {
        let spans = GenAiOtelIngestor.parse_spans(&sample_trace());
        assert_eq!(GenAiOtelIngestor.infer_agent(&spans), "anthropic");
        assert_eq!(GenAiOtelIngestor.infer_agent(&[]), "otel");
    }
}
