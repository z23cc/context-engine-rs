//! L3 **policy plane** — the host-side runtime that loads, seals, and serves the
//! org's policy-as-code (`docs/designs/trust-substrate.md` §3 L3, §5). The pure,
//! golden-tested content-addressing lives in [`nerve_core::policy`] (sealing a
//! [`PolicyDoc`] under a self-certifying `policy_version`, hashing decisions); this
//! module is the impure seam above the determinism boundary that touches the world:
//! it reads `<root>/.nerve/policy-plane.json` from disk and routes every recorded
//! [`PolicyDecisionRecord`] to an [`EvidenceSink`].
//!
//! **Court reporter, not judge (INV-R1):** the plane *records* the decision the
//! org's own policy implied; it never asserts a change is "correct".
//!
//! The decision-evidence sink is the live L3↔L1 link: the composition root wires the
//! [`LedgerEvidenceSink`] (commits each decision to the L1 evidence ledger as a
//! [`LedgerKind::PolicyDecision`], returning its `seq`) whenever a served scope resolves
//! a `LedgerStore`, falling back to [`NullEvidenceSink`] (no-op, `seq=None`) otherwise.
//! The plane only ever sees `&dyn EvidenceSink`, so the backing store is swappable with
//! no L3 type change. `policy.decisions` reads the committed records straight from L1.
//!
//! Best-effort throughout (mirrors [`RunStore`](crate::run_store)): a missing served
//! root, an absent or malformed `policy-plane.json`, or a sink failure degrades to
//! the empty (deny-by-default) sealed policy / a `None` sequence — it never panics
//! and never fails the delegated turn.

use crate::ledger_store::LedgerStore;
use nerve_core::ledger::{LedgerKind, PolicyDecisionOutcome};
use nerve_core::policy::{
    Capability, POLICY_SCHEMA_VERSION, PolicyDecisionRecord, PolicyDoc, seal_policy,
};
use nerve_runtime::{DelegateAutonomy, DelegateRole};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The on-disk policy-plane document filename, read from `<root>/.nerve/`.
const POLICY_PLANE_FILE: &str = "policy-plane.json";

/// Where a captured policy gate decision is committed for the audit trail. The
/// shipped default ([`NullEvidenceSink`]) is a no-op; the real impl (post-L1) wraps
/// a `LedgerStore` and returns the appended ledger sequence so a host can announce
/// it via the `policy_decision_recorded` event. Best-effort: an append failure must
/// return `None`, never propagate.
pub(crate) trait EvidenceSink: Send + Sync {
    /// Commit one decision record to the evidence trail; returns its ledger
    /// sequence on success, or `None` when there is no backing ledger (the shipped
    /// default) or the append failed.
    fn append_decision(&self, record: &PolicyDecisionRecord) -> Option<u64>;
}

/// The shipped default [`EvidenceSink`]: records nothing and always returns `None`.
/// L3 is advisory-only by construction until the L1 ledger is wired in — the
/// decision is still surfaced as a `policy_decision_recorded` event with no
/// `ledger_seq`. Swapped for a `LedgerStore`-backed sink at the composition root
/// once L1 has merged (master-spec §5).
pub(crate) struct NullEvidenceSink;

impl EvidenceSink for NullEvidenceSink {
    fn append_decision(&self, _record: &PolicyDecisionRecord) -> Option<u64> {
        None
    }
}

/// The L3↔L1 [`EvidenceSink`]: commits each policy decision to the L1 evidence ledger
/// as a [`LedgerKind::PolicyDecision`] record, returning the appended ledger `seq` so a
/// host can announce it on the `policy_decision_recorded` event. Best-effort — an
/// append failure yields `None` and never propagates (the audit trail is *evidence*,
/// not the live admission gate). This is the sink swapped in at the composition root
/// now that L1 has merged (master-spec §5).
pub(crate) struct LedgerEvidenceSink {
    store: LedgerStore,
}

impl LedgerEvidenceSink {
    pub(crate) fn new(store: LedgerStore) -> Self {
        Self { store }
    }
}

impl EvidenceSink for LedgerEvidenceSink {
    fn append_decision(&self, record: &PolicyDecisionRecord) -> Option<u64> {
        let kind = LedgerKind::PolicyDecision {
            run_id: record.session_id.clone(),
            policy_version: record.policy_version.clone(),
            capability: capability_label(record.capability).to_string(),
            decision: decision_outcome(&record.decision),
            // The redacted rationale/args stay out-of-band; only their hash is committed.
            detail_hash: (!record.args_hash.is_empty()).then(|| record.args_hash.clone()),
        };
        self.store.append(kind).ok().map(|appended| appended.seq)
    }
}

/// Stable lowercase label for a capability class (the ledger record's `capability`).
fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::Read => "read",
        Capability::Write => "write",
        Capability::Egress => "egress",
        Capability::Exec => "exec",
    }
}

/// Map the host-interpreted decision string to the ledger's binary outcome. Anything
/// other than an explicit `allow` records as `deny` — fail-closed (INV-R1).
fn decision_outcome(decision: &str) -> PolicyDecisionOutcome {
    match decision {
        "allow" => PolicyDecisionOutcome::Allow,
        _ => PolicyDecisionOutcome::Deny,
    }
}

/// The resolved policy plane for a served scope: a sealed [`PolicyDoc`] plus the
/// [`EvidenceSink`] every recorded decision is routed to. Construct with
/// [`PolicyPlane::resolve`] (reads + seals from disk) or [`PolicyPlane::with_sink`]
/// (tests / explicit composition).
#[derive(Clone)]
pub(crate) struct PolicyPlane {
    /// The sealed policy doc — `seal_policy` has stamped its content-addressed
    /// `policy_version`, so `self.doc.policy_version` is the in-force pin.
    doc: PolicyDoc,
    sink: Arc<dyn EvidenceSink>,
}

impl PolicyPlane {
    /// Resolve the policy plane for a served scope. Loads + seals the [`PolicyDoc`]
    /// from `<root>/.nerve/policy-plane.json` (an absent or malformed file yields the
    /// empty deny-by-default doc — best-effort), wiring the shipped
    /// [`NullEvidenceSink`]. `None` root resolves the empty sealed policy.
    pub(crate) fn resolve(root: Option<&Path>) -> Self {
        Self::with_sink(root, Arc::new(NullEvidenceSink))
    }

    /// Resolve as [`resolve`](Self::resolve) but with an explicit [`EvidenceSink`] —
    /// the composition-root hook for swapping in the L1-backed sink post-merge, and
    /// the test seam for asserting decisions are routed.
    pub(crate) fn with_sink(root: Option<&Path>, sink: Arc<dyn EvidenceSink>) -> Self {
        let doc = seal_policy(load_policy_doc(root).unwrap_or_default());
        Self { doc, sink }
    }

    /// Resolve the plane wired to the L1-backed [`LedgerEvidenceSink`] over `store`, so
    /// every recorded decision lands in the evidence ledger (the live L3↔L1 link). The
    /// composition root passes the served scope's `LedgerStore`.
    pub(crate) fn with_ledger(root: Option<&Path>, store: LedgerStore) -> Self {
        Self::with_sink(root, Arc::new(LedgerEvidenceSink::new(store)))
    }

    /// The content-addressed `policy_version` of the in-force sealed policy — the
    /// value a [`PolicyDecisionRecord`] and an L4 receipt pin to.
    pub(crate) fn policy_version(&self) -> String {
        self.doc.policy_version.clone()
    }

    /// The sealed policy document currently in force.
    #[allow(
        dead_code,
        reason = "test-only accessor; runtime reads self.doc directly via run_policy_get"
    )]
    pub(crate) fn doc(&self) -> &PolicyDoc {
        &self.doc
    }

    /// Record one gate decision to the evidence sink, returning the assigned ledger
    /// sequence (`None` with the shipped [`NullEvidenceSink`] or on append failure).
    pub(crate) fn record_decision(&self, record: &PolicyDecisionRecord) -> Option<u64> {
        self.sink.append_decision(record)
    }
}

/// Resolve a `policy.get`: the in-force sealed [`PolicyDoc`]. `None` plane (no served
/// root) returns the empty sealed policy so a client always sees a well-formed doc.
pub(crate) fn run_policy_get(plane: Option<&PolicyPlane>) -> Value {
    let doc = plane
        .map(|plane| plane.doc.clone())
        .unwrap_or_else(|| seal_policy(PolicyDoc::default()));
    let doc = serde_json::to_value(&doc).unwrap_or(Value::Null);
    json!({ "policy": doc })
}

/// Resolve a `policy.decisions`: the captured decision evidence, optionally scoped to
/// a `session_id`.
///
/// The decision *corpus* is owned by the L1 evidence ledger (the plane only *routes*
/// decisions to its [`EvidenceSink`]); this surfaces the `policy_decision` records the
/// plane committed there — scoped to the session/run handle when given — alongside the
/// in-force `policy_version`. A `None` ledger (no served root) yields an empty list,
/// mirroring `run.list`; never a transport error.
pub(crate) fn run_policy_decisions(
    session_id: Option<&str>,
    plane: Option<&PolicyPlane>,
    ledger: Option<&LedgerStore>,
) -> Value {
    let policy_version = plane.map(PolicyPlane::policy_version).unwrap_or_default();
    // Reuse the L1 query path: policy_decision records key on the session/run handle.
    let queried = crate::ledger_store::run_ledger_query(
        ledger,
        session_id,
        None,
        None,
        None,
        Some("policy_decision"),
        200,
    );
    let decisions = queried
        .get("records")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    json!({
        "policy_version": policy_version,
        "session_id": session_id,
        "decisions": decisions,
    })
}

/// Record a delegated run's authorization posture to the evidence ledger via `plane`,
/// returning the `(record, ledger_seq)` pairs so a host with an event sink can announce
/// each (a host without one — the in-chat ToolBox — relies on the persisted ledger).
///
/// Two axes are recorded so the audit trail does not UNDER-state the posture:
/// 1. the filesystem/exec ceiling the (role-expanded) `autonomy` authorizes — reflecting
///    the autonomy *contract* (`ReadOnly`→Read, `Edit`→Write, `Full`→Exec); an agent
///    sandbox may differ (codex `workspace-write` permits confined exec), captured by the
///    run's pinned inputs;
/// 2. the **always-granted outbound network** — every delegate spawns with
///    `NetPolicy::Allow` to reach its LLM API regardless of autonomy, so even a read-only
///    scout holds egress (the exfiltration axis); recording it keeps that visible.
///
/// Best-effort (a `None` ledger seq just means the no-op sink). INV-R1: records the
/// authorization posture, never asserts the run's output is correct.
pub(crate) fn record_delegate_authorization(
    plane: &PolicyPlane,
    session_id: &str,
    agent: &str,
    role: DelegateRole,
    autonomy: DelegateAutonomy,
) -> Vec<(PolicyDecisionRecord, Option<u64>)> {
    let policy_version = plane.policy_version();
    let fs_capability = match autonomy {
        DelegateAutonomy::ReadOnly => Capability::Read,
        DelegateAutonomy::Edit => Capability::Write,
        DelegateAutonomy::Full => Capability::Exec,
    };
    let axes = [
        (
            fs_capability,
            format!("delegated agent authorized at {autonomy:?} autonomy (role {role:?})"),
        ),
        (
            Capability::Egress,
            "delegated agent granted outbound network (LLM API egress, all autonomy levels)"
                .to_string(),
        ),
    ];
    axes.into_iter()
        .map(|(capability, reason)| {
            let record = PolicyDecisionRecord {
                schema_version: POLICY_SCHEMA_VERSION,
                policy_version: policy_version.clone(),
                session_id: session_id.to_string(),
                agent: agent.to_string(),
                tool: "delegate.start".to_string(),
                capability,
                decision: "allow".to_string(),
                reason,
                args_hash: String::new(),
            };
            let seq = plane.record_decision(&record);
            (record, seq)
        })
        .collect()
}

/// Read + parse `<root>/.nerve/policy-plane.json` when present. An absent file (or a
/// `None` root) yields `Ok(None)`; a malformed file yields `Ok(None)` too — the plane
/// is best-effort and falls back to the empty deny-by-default doc rather than failing
/// the served scope. (Contrast `policy.rs`, which fails closed on its *permission*
/// config — the policy plane is audit evidence, not the live admission gate.)
fn load_policy_doc(root: Option<&Path>) -> Option<PolicyDoc> {
    let path = policy_plane_path(root)?;
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// The policy-plane document path for a scope: `<root>/.nerve/policy-plane.json` for
/// a project root, else the global `config_home()/policy-plane.json`. `None` when the
/// global config home cannot be resolved.
fn policy_plane_path(root: Option<&Path>) -> Option<PathBuf> {
    match root {
        Some(root) => Some(root.join(".nerve").join(POLICY_PLANE_FILE)),
        None => nerve_agent::auth::config_home()
            .ok()
            .map(|home| home.join(POLICY_PLANE_FILE)),
    }
}

/// Persist a [`PolicyDoc`] to `<root>/.nerve/policy-plane.json` (creating the dir);
/// used by tests to round-trip the resolve path. Kept module-private + test-gated so
/// it never widens the runtime surface (the plane is read-only at runtime).
#[cfg(test)]
fn write_policy_doc(root: &Path, doc: &PolicyDoc) -> anyhow::Result<()> {
    let path = policy_plane_path(Some(root)).expect("project path resolves");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(doc)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_core::policy::{Capability, CapabilityRule, MergeBar, POLICY_SCHEMA_VERSION};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn sample_doc() -> PolicyDoc {
        PolicyDoc {
            schema_version: POLICY_SCHEMA_VERSION,
            policy_version: String::new(),
            capabilities: vec![CapabilityRule {
                tool: "edit".into(),
                action: "write".into(),
                capability: Capability::Write,
                agent: None,
            }],
            merge_bar: MergeBar {
                required_checks: vec!["test".into(), "build".into()],
            },
            required_evidence: Vec::new(),
        }
    }

    fn sample_record(decision: &str) -> PolicyDecisionRecord {
        PolicyDecisionRecord {
            schema_version: POLICY_SCHEMA_VERSION,
            policy_version: "pv".into(),
            session_id: "job-7".into(),
            agent: "codex".into(),
            tool: "edit".into(),
            capability: Capability::Write,
            decision: decision.into(),
            reason: "matched rule".into(),
            args_hash: String::new(),
        }
    }

    /// A counting sink standing in for the (post-L1) ledger-backed sink: it records
    /// every decision and returns a monotonic sequence so `record_decision` routing
    /// is observable.
    struct CountingSink {
        count: AtomicUsize,
    }

    impl EvidenceSink for CountingSink {
        fn append_decision(&self, _record: &PolicyDecisionRecord) -> Option<u64> {
            Some(self.count.fetch_add(1, Ordering::SeqCst) as u64)
        }
    }

    #[test]
    fn null_sink_records_nothing() {
        assert_eq!(
            NullEvidenceSink.append_decision(&sample_record("allow")),
            None
        );
    }

    #[test]
    fn resolve_seals_an_absent_doc_to_the_empty_policy() {
        // No file on disk -> empty deny-by-default doc, sealed (version stamped).
        let dir = tempdir().unwrap();
        let plane = PolicyPlane::resolve(Some(dir.path()));
        assert_eq!(plane.policy_version().len(), 64);
        assert!(plane.doc().capabilities.is_empty());
        // Same empty body always seals to the same version (pure content address).
        assert_eq!(
            plane.policy_version(),
            seal_policy(PolicyDoc::default()).policy_version
        );
    }

    #[test]
    fn resolve_loads_seals_and_round_trips_a_doc_from_disk() {
        let dir = tempdir().unwrap();
        write_policy_doc(dir.path(), &sample_doc()).unwrap();
        let plane = PolicyPlane::resolve(Some(dir.path()));

        // The on-disk body seals to the same version a pure reseal produces — disk
        // round-trip does not perturb the content address.
        assert_eq!(
            plane.policy_version(),
            seal_policy(sample_doc()).policy_version
        );
        assert_eq!(plane.doc().capabilities.len(), 1);
        assert_eq!(plane.doc().merge_bar.required_checks, vec!["test", "build"]);
        // A non-empty doc seals to a different version than the empty policy.
        assert_ne!(
            plane.policy_version(),
            seal_policy(PolicyDoc::default()).policy_version
        );
    }

    #[test]
    fn resolve_tolerates_a_malformed_doc() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".nerve")).unwrap();
        std::fs::write(
            dir.path().join(".nerve").join(POLICY_PLANE_FILE),
            "not json",
        )
        .unwrap();
        // Malformed file -> empty sealed policy, never a panic.
        let plane = PolicyPlane::resolve(Some(dir.path()));
        assert_eq!(
            plane.policy_version(),
            seal_policy(PolicyDoc::default()).policy_version
        );
    }

    #[test]
    fn record_decision_routes_to_the_sink() {
        let dir = tempdir().unwrap();
        let sink = Arc::new(CountingSink {
            count: AtomicUsize::new(0),
        });
        let plane = PolicyPlane::with_sink(Some(dir.path()), sink.clone());
        // Both an allow and a deny are routed (the gate records every outcome).
        assert_eq!(plane.record_decision(&sample_record("allow")), Some(0));
        assert_eq!(plane.record_decision(&sample_record("deny")), Some(1));
        assert_eq!(sink.count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn record_decision_with_null_sink_returns_none() {
        let dir = tempdir().unwrap();
        let plane = PolicyPlane::resolve(Some(dir.path()));
        assert_eq!(plane.record_decision(&sample_record("allow")), None);
    }

    #[test]
    fn run_policy_get_returns_the_sealed_doc() {
        let dir = tempdir().unwrap();
        write_policy_doc(dir.path(), &sample_doc()).unwrap();
        let plane = PolicyPlane::resolve(Some(dir.path()));

        let value = run_policy_get(Some(&plane));
        assert_eq!(value["policy"]["schema_version"], POLICY_SCHEMA_VERSION);
        assert_eq!(
            value["policy"]["policy_version"],
            json!(plane.policy_version())
        );
        assert_eq!(value["policy"]["capabilities"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn run_policy_get_with_no_plane_returns_the_empty_sealed_policy() {
        let value = run_policy_get(None);
        // A well-formed (empty, sealed) doc is always returned, never null. The
        // empty `PolicyDoc::default()` carries schema_version 0 (the zero-value); a
        // resolved on-disk doc carries the live POLICY_SCHEMA_VERSION.
        assert_eq!(value["policy"]["schema_version"], 0);
        assert_eq!(
            value["policy"]["policy_version"],
            json!(seal_policy(PolicyDoc::default()).policy_version)
        );
        assert!(
            value["policy"]["capabilities"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn run_policy_decisions_scopes_to_session_and_is_an_empty_corpus() {
        let dir = tempdir().unwrap();
        let plane = PolicyPlane::resolve(Some(dir.path()));

        // No ledger wired -> empty corpus (mirrors run.list), never a transport error.
        let value = run_policy_decisions(Some("job-7"), Some(&plane), None);
        assert_eq!(value["session_id"], "job-7");
        assert_eq!(value["policy_version"], json!(plane.policy_version()));
        assert!(value["decisions"].as_array().unwrap().is_empty());

        // No plane / no session -> empty version + null session + empty corpus.
        let none = run_policy_decisions(None, None, None);
        assert_eq!(none["policy_version"], "");
        assert_eq!(none["session_id"], Value::Null);
        assert!(none["decisions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn ledger_backed_sink_records_a_decision_and_policy_decisions_surfaces_it() {
        // The live L3↔L1 wiring: a plane built `with_ledger` routes every recorded
        // decision into the L1 evidence ledger, and `policy.decisions` reads it back.
        let dir = tempdir().unwrap();
        let ledger_dir = dir.path().join("ledger");
        let plane =
            PolicyPlane::with_ledger(Some(dir.path()), LedgerStore::new(ledger_dir.clone()));
        // Both an allow and a deny are committed, with monotonic ledger sequences.
        assert_eq!(plane.record_decision(&sample_record("allow")), Some(0));
        assert_eq!(plane.record_decision(&sample_record("deny")), Some(1));

        // `policy.decisions` (scoped to the recorded session/run handle "job-7") reads
        // the two PolicyDecision records straight out of L1.
        let query_store = LedgerStore::new(ledger_dir);
        let value = run_policy_decisions(Some("job-7"), Some(&plane), Some(&query_store));
        let decisions = value["decisions"].as_array().unwrap();
        assert_eq!(decisions.len(), 2);
        assert!(
            decisions
                .iter()
                .all(|d| d["kind"]["kind"] == "policy_decision")
        );
        // The decision string maps to the ledger's binary outcome (allow + deny present).
        let outcomes: Vec<&str> = decisions
            .iter()
            .filter_map(|d| d["kind"]["decision"].as_str())
            .collect();
        assert!(outcomes.contains(&"allow"));
        assert!(outcomes.contains(&"deny"));
    }
}
