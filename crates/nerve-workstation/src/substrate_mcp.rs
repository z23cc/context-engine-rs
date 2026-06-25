//! L5 MCP face of the trust substrate (`docs/designs/trust-substrate.md` §8 L5) — the
//! four read-mostly `nerve_*` tools an *external* coding agent (Claude Code, Codex,
//! Gemini CLI) calls over MCP to inspect its own provenance: enumerate captured runs
//! (`nerve_runs`), read a captured run's tape for replay (`nerve_replay`), fetch a
//! signed Receipt (`nerve_receipt`), and ask for a verification verdict
//! (`nerve_verify`).
//!
//! **Court reporter, not judge (INV-R1).** `nerve_verify` NEVER fabricates a verdict:
//! it re-verifies an already-sealed Receipt offline (the statement re-hashes to the
//! receipt id, and the detached ed25519 signature checks out over the PAE), reports the
//! receipt's OWN borrowed verdict, or — when none exists — `verify_not_available`. It
//! reports what cleared the org's bar; it cannot invent that it did. It also surfaces
//! `signed_by` + a `trust_note`: `signature_valid` proves consistency with the receipt's
//! embedded key, NOT issuer trust — pinning `keyid` against a known org key (or the
//! deferred sigstore-keyless backend) is what defends against a forged receipt.
//!
//! This adapter is read-only over the served `<root>/.nerve/` flat-file stores. It is
//! deliberately decoupled from the sibling `RunStore`/`ReceiptStore` types (it reads
//! the same on-disk shapes directly) so it composes with the integrator's wiring
//! without coupling the MCP face to a particular store struct. The integrator
//! registers it in `tools.rs` as a `RuntimeToolAdapter`, mapping `handle_tool_call`'s
//! result into the runtime's `Result<Option<Value>, RuntimeError>` contract.

use nerve_runtime::RuntimeError;
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// The four MCP tools this adapter owns. Stable names an external agent binds to.
const TOOL_NAMES: [&str; 4] = [
    "nerve_runs",
    "nerve_replay",
    "nerve_receipt",
    "nerve_verify",
];

/// Read-only MCP adapter exposing the trust-substrate's `nerve_*` tools to delegated
/// agents. Stateless: every call resolves the served root passed by the host.
pub(crate) struct SubstrateToolAdapter;

impl SubstrateToolAdapter {
    /// The MCP `tools/list` specs for the four `nerve_*` tools.
    pub(crate) fn tool_specs(&self) -> Vec<Value> {
        vec![
            tool_spec::<RunsArgs>(
                "nerve_runs",
                "List the captured, content-addressed runs in this workspace (newest first). \
                 Each run is a replayable agent tape; returns run ids, agents, and root hashes.",
            ),
            tool_spec::<ReplayArgs>(
                "nerve_replay",
                "Fetch a captured run's full event tape + content-addressed ledger by run id, \
                 so its execution can be deterministically replayed and its root hash re-derived.",
            ),
            tool_spec::<ReceiptArgs>(
                "nerve_receipt",
                "Fetch a signed Verification Receipt by receipt id (or by run id), or list \
                 receipts. The Receipt is a portable, offline-re-verifiable attestation of what \
                 cleared the org's own checks on replay.",
            ),
            tool_spec::<VerifyArgs>(
                "nerve_verify",
                "Re-verify a captured run's sealed Receipt offline: confirms the statement \
                 re-hashes to the receipt id (untampered) and the detached ed25519 signature \
                 checks out, then reports statement_intact + signature_valid + the receipt's \
                 own verdict. Returns {\"status\":\"verify_not_available\"} when no receipt \
                 exists — it never fabricates a verdict (court reporter, not judge).",
            ),
        ]
    }

    /// Whether `name` is one of this adapter's `nerve_*` tools.
    pub(crate) fn owns(&self, name: &str) -> bool {
        TOOL_NAMES.contains(&name)
    }

    /// Dispatch a `tools/call` for one of the `nerve_*` tools. `root` is the served
    /// workspace root (`None` => no served root => empty lists / not-found, never a
    /// crash). `params` is the `arguments` object of the MCP call.
    pub(crate) fn handle_tool_call(
        &self,
        name: &str,
        params: &Value,
        root: Option<&Path>,
    ) -> Result<Value, RuntimeError> {
        match name {
            "nerve_runs" => Ok(handle_runs(root)),
            "nerve_replay" => handle_replay(parse::<ReplayArgs>(params)?.run_id, root),
            "nerve_receipt" => handle_receipt(parse::<ReceiptArgs>(params)?, root),
            "nerve_verify" => handle_verify(parse::<VerifyArgs>(params)?.run_id, root),
            other => Err(RuntimeError::adapter(format!(
                "unknown substrate tool `{other}`"
            ))),
        }
    }
}

/// `nerve_runs`: enumerate captured runs (newest first). No served root => empty.
fn handle_runs(root: Option<&Path>) -> Value {
    let mut runs: Vec<Value> = read_dir_json(&runs_dir(root))
        .into_iter()
        .filter_map(|(_, value)| run_summary(&value))
        .collect();
    runs.sort_by(|a, b| {
        b["started_at_ms"]
            .as_u64()
            .cmp(&a["started_at_ms"].as_u64())
            .then_with(|| {
                b["run_id"]
                    .as_str()
                    .unwrap_or_default()
                    .cmp(a["run_id"].as_str().unwrap_or_default())
            })
    });
    json!({ "runs": runs })
}

/// `nerve_replay`: the full captured run (tape + ledger) by id, for deterministic
/// replay. Unknown id (or no served root) is a not-found adapter error.
fn handle_replay(run_id: String, root: Option<&Path>) -> Result<Value, RuntimeError> {
    let run = load_json_by_id(&runs_dir(root), &run_id)
        .ok_or_else(|| RuntimeError::adapter(format!("no captured run `{run_id}`")))?;
    Ok(json!({ "run": run }))
}

/// `nerve_receipt`: by receipt id, by run id, or list. Read-only over `.nerve/receipts`.
fn handle_receipt(args: ReceiptArgs, root: Option<&Path>) -> Result<Value, RuntimeError> {
    let dir = receipts_dir(root);
    if let Some(receipt_id) = args.receipt_id {
        let receipt = load_json_by_id(&dir, &receipt_id)
            .ok_or_else(|| RuntimeError::adapter(format!("no receipt `{receipt_id}`")))?;
        return Ok(json!({ "receipt": receipt }));
    }
    if let Some(run_id) = args.run_id {
        return match find_receipt_for_run(&dir, &run_id) {
            Some(receipt) => Ok(json!({ "receipt": receipt })),
            None => Err(RuntimeError::adapter(format!(
                "no receipt for run `{run_id}`"
            ))),
        };
    }
    let receipts: Vec<Value> = read_dir_json(&dir).into_iter().map(|(_, v)| v).collect();
    Ok(json!({ "receipts": receipts }))
}

/// `nerve_verify`: the run's sealed Receipt re-verified — its statement re-hashes to
/// the receipt id (no tampering) and its ed25519 signature checks out over the PAE —
/// or `verify_not_available`. NEVER a fabricated verdict (INV-R1); the reported verdict
/// is the receipt's own, borrowed from the org's tests. The synchronous re-run is the
/// L2 handle (deferred).
fn handle_verify(run_id: String, root: Option<&Path>) -> Result<Value, RuntimeError> {
    let Some(receipt_value) = find_receipt_for_run(&receipts_dir(root), &run_id) else {
        return Ok(json!({ "run_id": run_id, "status": "verify_not_available" }));
    };
    // Re-verify the receipt offline: content address + detached ed25519 signature.
    let verification =
        serde_json::from_value::<nerve_core::receipt::Receipt>(receipt_value.clone())
            .ok()
            .map(|receipt| {
                nerve_core::receipt::verify_receipt(&receipt, crate::signer::ed25519_verify)
            });
    let (statement_intact, signature_valid) = verification
        .as_ref()
        .map_or((false, false), |v| (v.statement_intact, v.signature_valid));
    // Surface the signing identity so a consumer can decide trust. CRUCIAL HONESTY
    // (INV-R1): `signature_valid` only proves the signature is consistent with the key
    // the receipt CARRIES — a receipt forged under an attacker's own key still validates.
    // Establishing issuer trust requires pinning `keyid` against a known org key (or the
    // deferred sigstore-keyless backend); without that pin, treat a receipt found in an
    // untrusted location as unproven provenance.
    let keyid = receipt_value
        .pointer("/signature/keyid")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let backend = receipt_value
        .pointer("/signature/backend")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(json!({
        "run_id": run_id,
        "status": "verified",
        "statement_intact": statement_intact,
        "signature_valid": signature_valid,
        "signed_by": { "keyid": keyid, "backend": backend },
        "trust_note": "signature_valid proves consistency with the receipt's OWN embedded \
            key, not issuer trust — pin keyid against a known org key (or use sigstore) to \
            defend against a forged receipt.",
        "receipt": receipt_value,
    }))
}

/// Project a full captured-run value into the `nerve_runs` summary shape.
fn run_summary(run: &Value) -> Option<Value> {
    let run_id = run.get("run_id").and_then(Value::as_str)?;
    Some(json!({
        "run_id": run_id,
        "agent": run.get("agent").and_then(Value::as_str).unwrap_or_default(),
        "root_hash": run.get("root_hash").and_then(Value::as_str).unwrap_or_default(),
        "attestation": run.get("attestation").cloned().unwrap_or(json!("full")),
        "event_count": run.get("events").and_then(Value::as_array).map(Vec::len).unwrap_or(0),
        "started_at_ms": run.get("started_at_ms").and_then(Value::as_u64).unwrap_or(0),
        "finished": run.get("finished").and_then(Value::as_bool).unwrap_or(false),
    }))
}

/// Scan `dir` for a receipt whose `statement.provenance.run_id` matches `run_id`.
fn find_receipt_for_run(dir: &Path, run_id: &str) -> Option<Value> {
    read_dir_json(dir).into_iter().find_map(|(_, value)| {
        let matches = value
            .pointer("/statement/provenance/run_id")
            .and_then(Value::as_str)
            == Some(run_id);
        matches.then_some(value)
    })
}

/// `<root>/.nerve/runs` for a served root, else `None`-rooted (empty results).
fn runs_dir(root: Option<&Path>) -> PathBuf {
    nerve_subdir(root, "runs")
}

fn receipts_dir(root: Option<&Path>) -> PathBuf {
    nerve_subdir(root, "receipts")
}

/// Resolve `<root>/.nerve/<sub>`; with no served root, a non-existent sentinel path
/// (read tolerantly => empty), so the adapter is fail-closed-to-empty, never panics.
fn nerve_subdir(root: Option<&Path>, sub: &str) -> PathBuf {
    match root {
        Some(root) => root.join(".nerve").join(sub),
        None => PathBuf::from(".nerve-unserved").join(sub),
    }
}

/// Load one `<dir>/<id>.json` as a `Value`, validating the id stays in-dir. `None` on
/// a bad id, a missing file, or a parse error (tolerant).
fn load_json_by_id(dir: &Path, id: &str) -> Option<Value> {
    if !valid_id(id) {
        return None;
    }
    let raw = std::fs::read_to_string(dir.join(format!("{id}.json"))).ok()?;
    serde_json::from_str(&raw).ok()
}

/// All `*.json` in `dir` parsed as `(stem, Value)`, tolerating a missing dir and
/// skipping unreadable/unparseable files.
fn read_dir_json(dir: &Path) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        out.push((stem, value));
    }
    out
}

/// Reject ids that could escape the directory (same token rule as the stores).
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Parse the MCP `arguments` object into a typed args struct.
fn parse<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params.clone())
        .map_err(|err| RuntimeError::adapter(format!("invalid arguments: {err}")))
}

/// Build one MCP tool spec (`{name, description, inputSchema}`) — mirrors `xai::specs`.
fn tool_spec<T: JsonSchema>(name: &str, description: &str) -> Value {
    let mut spec = Map::new();
    spec.insert("name".to_string(), Value::String(name.to_string()));
    spec.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    spec.insert("inputSchema".to_string(), input_schema::<T>());
    Value::Object(spec)
}

fn input_schema<T: JsonSchema>() -> Value {
    let mut schema = serde_json::to_value(schema_for!(T)).expect("schema serializes");
    if let Value::Object(object) = &mut schema {
        object.remove("$schema");
    }
    schema
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RunsArgs {}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReplayArgs {
    /// The captured run id (its content address) to fetch for replay.
    run_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReceiptArgs {
    /// Fetch one receipt by its id.
    #[serde(default)]
    receipt_id: Option<String>,
    /// Or fetch the receipt issued for this run id.
    #[serde(default)]
    run_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {
    /// The captured run id to report a verification verdict for.
    run_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write(dir: &Path, name: &str, value: &Value) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), serde_json::to_string_pretty(value).unwrap()).unwrap();
    }

    fn seed(root: &Path) {
        let runs = root.join(".nerve").join("runs");
        write(
            &runs,
            "aaaa.json",
            &json!({
                "run_id": "aaaa", "agent": "codex", "root_hash": "aaaa",
                "started_at_ms": 100, "finished": true, "attestation": "full",
                "events": [{"seq":0,"kind":{"kind":"turn_started","turn":0}}]
            }),
        );
        write(
            &runs,
            "bbbb.json",
            &json!({
                "run_id": "bbbb", "agent": "claude", "root_hash": "bbbb",
                "started_at_ms": 300, "finished": false, "events": []
            }),
        );
        let receipts = root.join(".nerve").join("receipts");
        write(
            &receipts,
            "rcpt1.json",
            &json!({
                "schema_version": 1, "receipt_id": "rcpt1",
                "statement": { "provenance": { "run_id": "aaaa" }, "verdict": "passed" }
            }),
        );
    }

    #[test]
    fn owns_only_the_four_nerve_tools() {
        let a = SubstrateToolAdapter;
        for name in TOOL_NAMES {
            assert!(a.owns(name), "{name}");
        }
        assert!(!a.owns("file_search"));
        assert!(!a.owns("xai_responses"));
    }

    #[test]
    fn tool_specs_expose_the_four_named_tools_with_schemas() {
        let specs = SubstrateToolAdapter.tool_specs();
        let names: Vec<&str> = specs.iter().filter_map(|s| s["name"].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "nerve_runs",
                "nerve_replay",
                "nerve_receipt",
                "nerve_verify"
            ]
        );
        for spec in &specs {
            assert!(spec["inputSchema"].is_object(), "spec has inputSchema");
            assert!(spec["description"].as_str().is_some());
        }
    }

    #[test]
    fn runs_lists_newest_first_and_no_root_is_empty() {
        let dir = tempdir().unwrap();
        seed(dir.path());
        let a = SubstrateToolAdapter;
        let listed = a
            .handle_tool_call("nerve_runs", &json!({}), Some(dir.path()))
            .unwrap();
        let runs = listed["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["run_id"], "bbbb"); // started_at_ms 300 first
        assert_eq!(runs[0]["event_count"], 0);
        assert_eq!(runs[1]["event_count"], 1);

        let empty = a.handle_tool_call("nerve_runs", &json!({}), None).unwrap();
        assert_eq!(empty["runs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn replay_fetches_full_run_and_errors_on_unknown() {
        let dir = tempdir().unwrap();
        seed(dir.path());
        let a = SubstrateToolAdapter;
        let got = a
            .handle_tool_call("nerve_replay", &json!({"run_id":"aaaa"}), Some(dir.path()))
            .unwrap();
        assert_eq!(got["run"]["run_id"], "aaaa");
        assert!(
            a.handle_tool_call("nerve_replay", &json!({"run_id":"nope"}), Some(dir.path()))
                .is_err()
        );
        // Path-escape id is rejected (treated as not-found).
        assert!(
            a.handle_tool_call("nerve_replay", &json!({"run_id":"../x"}), Some(dir.path()))
                .is_err()
        );
    }

    #[test]
    fn receipt_by_id_by_run_and_list() {
        let dir = tempdir().unwrap();
        seed(dir.path());
        let a = SubstrateToolAdapter;
        let by_id = a
            .handle_tool_call(
                "nerve_receipt",
                &json!({"receipt_id":"rcpt1"}),
                Some(dir.path()),
            )
            .unwrap();
        assert_eq!(by_id["receipt"]["receipt_id"], "rcpt1");

        let by_run = a
            .handle_tool_call("nerve_receipt", &json!({"run_id":"aaaa"}), Some(dir.path()))
            .unwrap();
        assert_eq!(by_run["receipt"]["receipt_id"], "rcpt1");

        let listed = a
            .handle_tool_call("nerve_receipt", &json!({}), Some(dir.path()))
            .unwrap();
        assert_eq!(listed["receipts"].as_array().unwrap().len(), 1);

        assert!(
            a.handle_tool_call(
                "nerve_receipt",
                &json!({"run_id":"missing"}),
                Some(dir.path())
            )
            .is_err()
        );
    }

    #[test]
    fn verify_returns_receipt_or_not_available_never_fabricates() {
        let dir = tempdir().unwrap();
        seed(dir.path());
        let a = SubstrateToolAdapter;
        let verified = a
            .handle_tool_call("nerve_verify", &json!({"run_id":"aaaa"}), Some(dir.path()))
            .unwrap();
        assert_eq!(verified["status"], "verified");
        assert_eq!(verified["receipt"]["receipt_id"], "rcpt1");

        let absent = a
            .handle_tool_call("nerve_verify", &json!({"run_id":"bbbb"}), Some(dir.path()))
            .unwrap();
        assert_eq!(absent["status"], "verify_not_available");
        assert!(
            absent.get("verdict").is_none(),
            "never fabricates a verdict"
        );

        // No served root: still honest, never a crash.
        let none = a
            .handle_tool_call("nerve_verify", &json!({"run_id":"x"}), None)
            .unwrap();
        assert_eq!(none["status"], "verify_not_available");
    }

    #[test]
    fn unknown_tool_is_an_error() {
        let a = SubstrateToolAdapter;
        assert!(
            a.handle_tool_call("nerve_unknown", &json!({}), None)
                .is_err()
        );
    }
}
