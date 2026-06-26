# Trust Substrate — the deterministic flight-recorder & execution-grounded re-verifier

Status: **governing (positioning)** — this is the decision record for *what Nerve is winning at*.
It supersedes the positioning in `architecture-north-star.md` §1 (the "cockpit over external CLIs"
framing of 2026-06-23). The engineering invariants in north-star §3–§7/§10 still hold; this doc
sharpens the **product thesis** and adds the named contracts (Run / Ledger / Verdict / Receipt /
Policy) the thesis turns on.
Date: 2026-06-24. Validated by an unconstrained adversarial tournament (8 competing winning-layer
theses → independent red-team → 3 priority-diverse judges; all three converged on this optimum).

---

## 0. The decision (one paragraph)

> **Nerve is the deterministic flight-recorder + execution-grounded re-verifier for fleets of
> external coding agents.** It orchestrates the best stochastic agents (Claude Code, Codex, Gemini,
> …) as userland through the `delegate.*` cockpit, and its *moat* is that **every agent run is
> captured as a content-addressed, bit-for-bit replayable Run, and gated by a portable, signed
> Verification Receipt** whose verdict is borrowed from the org's own tests — not invented by us.
> We do not judge whether the code is *correct*; we prove, to a party who trusts no one, **what an
> agent did, that it is replayable, and that it cleared the org's own bar.** Court reporter, not
> judge.

The cockpit is the **body** (distribution); the replay kernel + receipt is the **moat**. Generation
is the commodity we orchestrate; **adjudicated, replayable provenance is the product.**

---

## 1. Why this is the position (validated reasoning)

1. **Neutrality is structurally true here and false everywhere else.** A layer that *generates
   nothing* can verify everyone's output without conflict. A verifier owned by a model vendor is a
   self-grader, which the field already distrusts (SWE-bench Verified deprecated over contamination;
   ~59% of audited "failures" were test artifacts; self-grading rejected). Cursor / Anthropic /
   OpenAI / Factory are **structurally barred** from this lane. No model release dissolves it.
2. **Ride distribution, don't fight it.** The cockpit makes the labs' free, co-trained, best-in-class
   agents into userland; the receipt lands as a GitHub/GitLab merge-gate on incumbent rails. We never
   out-UX Claude Code on its home turf — the failure mode that sinks "be a better cockpit / harness /
   agent-OS."
3. **It avoids the category error** (see §2) that detonates every "own the correctness verdict"
   thesis.
4. **The moat compounds without an install base.** The replay kernel is systems engineering (not a
   model, not buyable); the cross-agent outcome corpus (§3, L6) is data only a multi-agent cockpit at
   the merge boundary can see, and it starts compounding at **agent #2 on repo #1**, not org #1000.
5. **Durability inverts with model progress.** More capable, more autonomous agents → more
   machine-speed code humans cannot review → *more* demand for replayable provenance. Our advantage
   **grows** as the frontier advances and as the agent market consolidates into 2–3 labs — the
   opposite of every capability/harness/index thesis, which decay as models improve.

---

## 2. The category error we reject (binding)

**Claim we will never make: "this code is correct."** Deciding whether a diff implements fuzzy intent
is undecidable-in-general and irreducibly **model-bound** — to out-judge a generator you need a
smarter generator, which we would rent from the very vendors we claim independence from. Any
"verifier court of LLM judges" therefore degrades to a **laundered self-grade**, and goes radioactive
the first time a "proven-correct" change causes a regulated incident.

**The rule that follows (INV-R1, INV-R3):** determinism buys us *reproducibility of the run and the
record*, never *correctness of the answer*. The authoritative verdict is **execution-grounded** — the
org's own tests/typecheck/build re-run in a hermetic closure — so its authority is *borrowed from the
customer's CI*, not manufactured by us. LLM-judge panels may exist only as a clearly-labeled
**advisory** signal, quarantined, never load-bearing.

> The inversion that drives the strategy: our most valuable artifact is a perfectly replayable record
> of an agent's **failure** (devs pay for triage; auditors/insurers/regulators legally require chain
> of custody) — exactly the artifact a generator vendor is disincentivized to ever produce about
> itself.

---

## 3. Architecture (layered)

```
BODY    delegate.* cockpit  ── orchestrate external CLI agents as userland (already ~80% built)
  │                            rent best stochastic models; NEVER own a generator (INV-R4)
  ▼
FLOOR
 L0  Run Capture & Deterministic Replay   the moat — record every run as a content-addressed Merkle-DAG;
                                          replay re-drives the harness against the recorded transcript
 L1  Content-Addressed Evidence Ledger    append-only, tamper-evident black box / system of record
 L2  Execution-Grounded Verdict           re-run the ORG'S OWN tests/typecheck/build hermetically
 L3  Policy & Capability Plane            declarative policy-as-code; every grant/denial is evidence
 L4  Portable Signed Verification Receipt  the chokepoint artifact; open schema, closed kernel
 L5  Integration Seam (Switzerland)       GitHub/GitLab merge-gate · MCP verify.*/replay.* · OTel ingestion
 L6  Cross-Agent Outcome Corpus           late dividend; the only load-bearing ML (calibration, not generation)
```

- **L0 — Run Capture & Deterministic Replay.** A `Run` = `(inputs hash → ordered Merkle-DAG of every
  delegated tool call / file read / edit / command / egress / model token-chunk / approval → output
  diff hash)`, executed in a pinned hermetic environment (OCI digest + lockfile + toolchain hash).
  We do **not** make the (external, vendor-owned) agent deterministic; we **record** its nondeterminism
  once — the model's token stream and all external inputs become content-addressed events — and
  *replay* re-drives the harness against that recorded transcript deterministically. Re-verification
  (§L2) is deterministic in the pinned closure. This is record/replay (rr / Antithesis-grade) applied
  to **agent loops** — the hardest unglamorous systems work in the stack, and the one moat that is not
  a model. The existing `nerve-core` snapshot determinism and `SandboxLauncher` (north-star §3.9 / P4)
  are the seed.
- **L1 — Content-Addressed Evidence Ledger.** Append-only, tamper-evident (Merkle / transparency-log,
  Sigstore/in-toto-aligned). Every Run, diff, policy decision, and verdict is hash-addressed; the DAG
  links `task → agent → tool-call → diff → test-result → receipt`. Switching cost = abandoning
  tamper-evident history = forensic/regulatory, not merely technical.
- **L2 — Execution-Grounded Verdict (NOT a court).** Run the org's own typecheck / build / test / lint
  inside the L0 closure; support best-of-N by snapshot-fork; add **flaky-test / test-artifact /
  contamination detection** (the honest answer to the "59% bogus failures" crisis — distinguish
  "agent wrong" from "env/test wrong"). The verdict's authority is the customer's existing acceptance
  bar, made reproducible and signed. This deprecates the `verify_completion` self-grade opt-in.
- **L3 — Policy & Capability Plane.** Promote the existing delegate autonomy tiers + env-scrub + net
  policy (north-star §3.9) to declarative policy-as-code: what each external agent may read / write /
  egress, what bar a diff must clear to merge, what evidence must exist. Every grant/denial is itself
  an evidenced event in L1 — "allowed" and "passed" both become auditable and replayable.
  **Merge-bar enforcement (SHIPPED, protocol v15).** `PolicyDoc.merge_bar.required_checks` +
  `required_evidence` are no longer inert: the in-force bar is **co-sealed into (and signed as part of)
  the receipt statement** at issue time (`ReceiptStatement.merge_bar` / `.required_evidence`, plus a
  pinned `provenance.policy_version`), and the merge gate enforces *the bar the receipt SIGNED* via the
  pure `nerve_core::receipt_gate::enforce_merge_bar` — **never** a policy re-read from the gate host's
  disk (INV-R5: pin what is signed; a malicious host cannot strip the bar, and the wave-7
  `verify_receipt` refusal already protects the embedded bar from gate-side tampering). The overlay is
  **downgrade-only** (INV-R1): a present-and-failed required check downgrades a pass to *failure* (exit
  1); a missing required check or absent/unknown required evidence downgrades to *neutral* (exit 2,
  never a fabricated pass); a non-success base verdict is **never** upgraded (the bar report is appended
  to its summary). Evidence kinds are a CLOSED, fail-closed set `{receipt, replay, ledger, policy}`
  (unknown = unsatisfied — no threshold/coverage predicates that would drift into a judge). An empty
  bar is pure pass-through and serializes away, so a receipt sealed without an org bar is byte-identical
  to a pre-L3 receipt (additive-invariance). **Checkspec-identity binding (SHIPPED, protocol v15).** The
  v14 "matched **by name**" trust gap is closed: the bar may pin `MergeBar.expected_checkspec_hash` (the
  content address of the checkspec it was authored against, co-sealed + signed), and the receipt carries
  `ReceiptStatement.checkspec_hash` (its copy of the sealed `Verdict.checkspec_hash`). `enforce_merge_bar`
  gates on identity **before** name-matching — when the bar pins an expected checkspec, the receipt's
  checkspec MUST equal it, else the required-check *names* cannot be trusted and the gate **downgrades** to
  neutral (a renamed/stubbed `command:'true'` check can no longer impersonate the org's real one). It stays
  downgrade-only (a non-success base is never upgraded); a bar that pins no expected hash keeps the v14
  by-name behavior. Both fields are additive `Option`/`skip_serializing_if`, so a receipt/bar without them
  is byte-identical to a v14 record (no receipt-id churn).
- **L4 — Portable Verification Receipt.** A signed, portable manifest (in-toto / DSSE / SLSA / OTel-
  GenAI-embedded): these inputs, this run hash, these reproducible checks + verdicts, this policy
  version, this provenance — **re-verifiable by a third party who trusts none of the participants.**
  It travels with the PR, gates CI, and satisfies SOC2 / EU-AI-Act / sector audit. **Open the schema**
  (drive a vendor-neutral "Open Evidence" / "SARIF for agent runs"); **keep the replay kernel +
  calibration proprietary.**
- **L5 — Integration Seam (generator-neutral).** Ship as (a) a GitHub/GitLab merge check + CI plugin
  that emits/gates on the Receipt (zero IDE-switch); (b) an MCP server exposing `verify.*` / `replay.*`
  tools; (c) an OTel-GenAI ingestion path so even agents we did not instrument are partially attested
  from their traces. The more agents exist, the more they clear the same neutral bar.
- **L6 — Cross-Agent Outcome Corpus (late dividend, NOT the day-one moat).** Every Run carries a human
  merge / revert / incident outcome label. This trains flaky-test classification, contamination
  detection, and a "which evidence signals predict shipped-and-didn't-regress" calibration — **the only
  place ML is load-bearing, and it is calibration, never generation.** It is the cross-agent corpus
  only the neutral cockpit-at-the-merge-boundary can collect. Treat as a compounding dividend; the
  ignition moat is the replay kernel + receipt, valuable at n=1.
  - **Automatic real-outcome ingestion rail (live).** The corpus is no longer fed only by a manual,
    daemon-gated `outcome.label`: the offline `nerve outcome <merged|reverted|incident|shipped-no-regress>
    --run <id>` CLI (the daemon-free twin of `outcome.label`, symmetric with `nerve ledger verify`) lets a
    **post-merge CI hook** record the REAL outcome the platform observed — `nerve outcome merged --run
    "$NERVE_RUN_ID" --source ci` on a `pull_request: closed`/`merged == true` (or push-to-`main`) event —
    so every real merge auto-feeds the corpus and the observation joins the L1 tamper-evident chain
    (`LedgerKind::OutcomeRecorded`). **Honest by construction (INV-R1):** the rail records what the CALLER
    asserts happened (the platform merged it) — it NEVER derives an outcome from a verify verdict, and the
    corpus stays **advisory / non-load-bearing** (it never feeds a verdict, gate, or receipt — INV-R1/R3/R4).
    See `docs/integrations/ci-merge-gate.md` → "Feeding the outcome corpus (L6)".

---

## 4. Invariants (named, binding)

These extend north-star §3; they do **not** weaken any existing invariant (the determinism boundary
in particular is preserved — see INV-R2).

- **INV-R1 — Reproducibility, not correctness.** The substrate attests that a run *happened*, is
  *bit-for-bit replayable* from recorded inputs, and *met the org's own acceptance bar reproducibly*.
  It must **never** assert that a change is "correct." Any feature claiming a correctness verdict is a
  bug against this invariant.
- **INV-R2 — Determinism boundary holds.** Event canonicalization, hashing, and the Run/Ledger DAG
  schema are **pure** (golden-tested; may live in `nerve-core` / `nerve-proto`). Capture, replay
  execution, ledger I/O, signing, and verification all touch the non-deterministic world and live in
  `nerve-runtime` / `nerve-workstation`, **never** in `nerve-core`. (Consistent with north-star §3.1.)
- **INV-R3 — Verdict is execution-grounded only.** The authoritative verdict bottoms out in the org's
  own tests/typecheck/build/lint (+ property/mutation/contamination checks) re-run in the hermetic
  closure. LLM-judge panels are advisory, quarantined, and never load-bearing.
- **INV-R4 — Neutrality.** Nerve ships no first-party generation model as a product. The own-engine
  loop (`agent.run` / `session.*`) stays a demoted headless/test fixture (north-star §1, §8). A
  verifier that owns a generator is a self-grader; **neutrality is the moat — protect it.**
- **INV-R5 — Receipts & ledger are portable, signed, append-only, and additive protocol data.** The
  Receipt schema is open and third-party re-verifiable; the ledger is tamper-evident and append-only;
  the replay kernel and calibration models stay closed. Wire vocabulary is added per north-star §3.3
  (additive, versioned, `nerve-proto` authority, drift-checked).
- **INV-R6 — Ride distribution; own nothing upstream.** The substrate runs *on top of* external agents
  and lands on incumbent rails (merge-gate, MCP, OTel). Never try to *be* the execution cloud, *be* the
  merge platform, or out-distribute the agents from a standing start.

---

## 5. Core data shapes (illustrative)

Authoritative types live in `nerve-proto` and are added additively/versioned (north-star §3.3/§3.4 —
transport-neutral data). Shapes below are design intent, not the final wire.

```jsonc
// A recorded, replayable unit of trust.
Run {
  run_id,                 // content hash over { inputs, events-root, output }
  inputs: {
    task,                 // the delegated objective (hash + text)
    repo_snapshot_hash,   // CatalogSnapshot identity at start
    agent: { cli, version, model, prompt_hash },
    toolchain_digest,     // pinned env (OCI digest + lockfile hash)
    policy_version,       // L3 policy in force
  },
  events_root,            // Merkle root of the event DAG
  output: { diff_hash, exit_status }
}

Event {                  // content-addressed node in the Run DAG
  id, parent,            // hash + DAG edge
  kind,                  // tool_call | file_read | edit | command | egress | token_chunk | approval
  payload_hash,
  logical_clock          // deterministic ordering, not wall-clock
}

LedgerEntry {            // append-only, tamper-evident
  seq, run_id, kind, hash, prev_hash   // transparency-log chaining
}

Receipt {                // portable, signed, third-party re-verifiable
  run_id, inputs_hash, policy_version, provenance,
  checks: [ { name, kind, verdict, reproducible } ],  // kind: test|typecheck|build|lint|property|mutation|contamination
  replay_manifest,       // how to re-derive the run from inputs
  signature              // DSSE / Sigstore
}
```

---

## 6. Protocol surface (additive — proposed)

Per north-star §3.3 these are **additive, versioned** vocabulary in `nerve-proto`; MCP (§3.5) is a
separate external face and gets its own tools.

**Runtime commands (new):**
- `delegate.list` / `delegate.get` — enumerate/observe live + parked delegate sessions (also closes
  the cockpit-cannot-list-its-own-agents gap; mirrors `session.list`/`flow.list`).
- `run.list` / `run.get` — fetch recorded Runs (content-addressed).
- `replay.start` — re-execute a recorded Run deterministically; returns a replay job.
- `verify.start` — run the execution-grounded verdict (org's own checks) over a Run/diff in a hermetic
  closure; emits a Receipt.
- `receipt.get` — fetch a signed Receipt by id.
- `ledger.query` — query provenance (by run / agent / diff / outcome).

**Runtime events (new):** replace the single opaque `DelegateProgress` with a **structured** delegate
vocabulary — `TurnStarted`, `ToolStarted` / `ToolFinished`, `AwaitingApproval`, `UsageUpdated`,
`TurnFinished` — each carrying the content-addressed event id appended to the Run DAG; plus
`RunRecorded { run_id, events_root }` and `VerificationCompleted { run_id, receipt_id, verdict }`.

**MCP tools (new, separate face):** `verify`, `replay`, `attest`/`receipt` — so external agents and CI
call *into* the substrate. Keep session/agent vocabulary out of MCP (north-star §3.5).

**Durability:** the Run/Ledger/Receipt store is persistent and resumable (content-addressed flat files
first, per north-star's "SQLite only on a measured trigger"; promote when measured). Live delegate
sessions become enumerable and resume-by-id across daemon restart — a precondition for the substrate
(you cannot audit or replay state that evaporates).

---

## 7. What we abandon (convergent anti-goals)

- **Any "correctness verdict / verifier court / league-office-of-all-agents" framing** — INV-R1/R3. It
  is model-bound and goes radioactive on the first proven-wrong receipt.
- **The own-engine LLM loop as anything but a test/headless fixture** — INV-R4. Owning a generator
  poisons neutrality. Finish north-star's demotion.
- **"Own the harness/router as a moat."** The ~30-point scaffold swing is a *closing arbitrage* the
  labs RL into the weights each generation. Keep orchestration as the body/mechanism, never the moat.
- **The knowledge-graph learning flywheel / bitemporal-causal graph / nightly-fine-tuned owned
  models.** Violates the determinism boundary; PR0's ONNX removal is **validated**. Keep deterministic
  single-repo nav (`scout`/`build_context`/repomap/SCIP-nav) as a *feature handed to agents* and a
  *grounding source for evidence*, not a moat. Semantic recall, if wanted, is **consumed** non-
  deterministically via the MCP-client seam and tagged `deterministic:false` (see `code-graph.md`).
- **The spec-ladder / formal-methods pivot as the primary product.** Steal only the Open-Evidence
  *format* idea and execution-grounded property/mutation/contract checks; do not bet on reversing the
  revealed preference against upfront formalization.
- **"AWS-of-agents" billion-dollar microVM fleet.** Consume Firecracker/gVisor/Nix as commodity; do
  not try to *be* the hosting substrate and harvest tenants' execution data (mutually exclusive; the
  seat is taken).
- **`verify_completion` self-grade** — replaced wholesale by the L2 execution-grounded verdict.
- **A standards-body win as a prerequisite.** Publish the Open-Evidence schema for ubiquity, but make
  the product valuable at zero adopters (a single team replaying its own delegated runs is useful on
  day one); the standard is upside, not a dependency.

---

## 8. Build order (grounded in what already ships)

The optimum is a **re-aim, not a rebuild** — the deterministic kernel and the delegate cockpit are
already L0's seed and the BODY.

1. **Credibility floor (days; additive).** `delegate.list`/`delegate.get` + persist the job/live-
   session store with resume-by-id. Without enumeration + durability nothing above is sellable.
2. **L0 Run capture + content-addressed Run (the moat's first brick).** Record each delegated run as a
   replayable Merkle-DAG on the existing `delegate_runtime` event stream; pin the toolchain digest.
3. **L4 Receipt + L5 merge-gate (the distribution wedge).** Emit a signed Receipt and ship a free
   GitHub/GitLab check any team can drop in front of Cursor/Claude Code/Codex/Devin output without
   switching tools.
4. **L2 execution-grounded verdict.** Re-run the org's own checks in the L0 closure + flaky/artifact
   detection. Make the GUI Review tab an accept/reject gate, not a copy-packet.
5. **L1 ledger + L3 policy-as-code**, then **L6 outcome corpus** as the long-game dividend.

**Boldest bet:** that within ~18–24 months *replayable, attestable provenance of machine-generated
code* becomes a procurement / audit / insurance requirement before the labs ship a good-enough bundled
self-verifier. We win the way Sigstore won supply-chain attestation and clearinghouses won payments —
by being the neutral settlement layer the ecosystem is forced to clear through. Downside is a
best-in-class regulated-industry agent black box, i.e. our floor equals every rival thesis's ceiling.

---

## 9. Relationship to existing design docs

- **`architecture-north-star.md`** — this doc supersedes its §1 *positioning* (cockpit is now the
  body, not the headline; the replay/receipt substrate is the moat) and adds INV-R1..R6 to §3. All
  other invariants (§3.1–§3.9), crate layering (§4), the seam scorecard (§5), and governance (§10)
  remain binding and unchanged.
- **`code-graph.md`** — its "build deterministic edges in-core, consume non-deterministic edges via
  MCP" decision is *validated* and is exactly how semantic recall (if any) enters: as a consumed,
  tagged-non-deterministic feature, never a kernel-resident moat.
- **`agent-exec-sandbox.md`** — the `SandboxLauncher` + deferred Landlock/seccomp work is the hermetic
  closure L0/L2 depend on; finishing the strong-isolation backend is now load-bearing for trustworthy
  replay, not optional.
- **`agent-orchestration.md`** — its `flow.*` strategies (mapreduce/debate/vote) become heterogeneous
  *candidate generation* feeding the execution-grounded verdict; the reduce/judge node emits a
  Receipt-attested PR. The vote/debate tally is advisory input to L2, never the authority (INV-R3).
- **`session-layer.md` / persistence** — the durable, resumable store the credibility floor (§8.1)
  requires; extend the existing `SessionStore` pattern to the delegate seam + the Run/Ledger.

---

## 10. Risks & falsifiers

- **Make-or-break (technical):** record/replay of stochastic, network-touching external agents is the
  hardest systems work in the plan. Mitigation: record-once-replay-deterministically is a proven
  pattern (rr/Antithesis); we record the vendor model's outputs + external inputs rather than trying to
  make the model deterministic.
- **Timing (market):** the regulatory/procurement forcing function may lag. Mitigation: dev-facing
  triage value ("was it the agent or a flaky test? here is the exact replay") is real at n=1 with zero
  regulation, so the product stands alone; regulation is upside.
- **Incumbent bundling:** a lab could ship a self-verifier. Mitigation: INV-R4 neutrality — a
  self-grader is exactly what the field distrusts; our position is the one they structurally cannot
  occupy.

> Governance (north-star §10 applies): when this doc and the code disagree, it is a bug in one of them
> — fix the change or update this doc in the same PR.
