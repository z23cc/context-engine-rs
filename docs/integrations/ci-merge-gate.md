# CI merge-gate: gate a PR on a Nerve Verification Receipt

Drop this in front of **any** coding agent's output — Claude Code, Codex, Cursor,
Devin — to gate a pull request on a portable, signed **Nerve Verification Receipt**.
Nerve re-runs **your org's own checks** against a captured agent Run, seals a
Receipt, and the CI job's exit code becomes the merge gate.

This is the **L5 integration seam** of the trust substrate (the "distribution
wedge", trust-substrate.md §8.3): a free GitHub/GitLab check any team can adopt
without switching IDEs or generators.

## TL;DR — add ~6 lines to a consumer PR workflow

```yaml
# .github/workflows/pr-gate.yml in YOUR repo
jobs:
  nerve-gate:
    uses: z23cc/nerve-workstation/.github/workflows/nerve-gate.reusable.yml@main
    with:
      run-id: ${{ needs.delegate.outputs.run-id }}   # from your delegated nerve run
    secrets: inherit
```

That's it. The job re-runs your checks, seals a Receipt, posts a GitHub check-run,
and fails the PR unless the verdict is **Passed**.

Prefer the composite action directly if you already check out and install Rust:

```yaml
- uses: z23cc/nerve-workstation/.github/actions/nerve-gate@main
  with:
    run-id: ${{ steps.delegate.outputs.run-id }}
    # root: .            # repo root holding `.nerve/` (default ".")
    # emit: gh           # post a GitHub check-run (default), or "none"
    # nerve-version: latest
    # reruns: "3"        # optional flaky-test re-run count
  env:
    GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

GitLab adopters `include` the equivalent template:

```yaml
include:
  - project: 'your-group/nerve-workstation'
    file: '/templates/gitlab/nerve-gate.gitlab-ci.yml'

nerve-gate:
  extends: .nerve-gate
  variables:
    NERVE_RUN_ID: "$NERVE_RUN_ID"
```

## What a Receipt is

A **Verification Receipt** is a portable, signed, third-party re-verifiable manifest
(trust-substrate.md **§L4**) that records: which captured **Run** was checked, which
checks ran, their outcomes, the sealed **Verdict**, and the ledger entry it was
appended to. It is open-schema and closed-kernel: anyone can re-verify it, but only
the org's own checks decide the verdict. The merge-gate consumes it at **§L5**.

## How the `run-id` is produced

The `run-id` is the **content address of a captured Run** — produced when you run an
agent through Nerve's delegate seam (`delegate.start`, e.g. a Claude Code / Codex /
Gemini CLI session). Each delegated run is recorded as a replayable, content-addressed
Run (trust-substrate.md L0). Capture the resulting id and pass it to this gate; it is
the thing the Receipt attests.

## Court reporter, not judge (INV-R1)

Nerve never claims the code is "correct." It proves three things and only three:

1. **what the agent did** (the captured, content-addressed Run),
2. **that it is replayable** (bit-for-bit re-runnable), and
3. **that it cleared your org's OWN bar** — the verdict is **borrowed** from the
   checks in `<root>/.nerve/checks.json`, never invented by Nerve.

If `<root>/.nerve/checks.json` is **absent**, the verdict is **Inconclusive** and the
gate exits **2** — Nerve will **never** fabricate a pass. Define your checks to make
the gate meaningful.

## Exit-code semantics (authoritative)

The gate's process exit code is the source of truth, even with no GitHub/GitLab App
deployed:

| Exit | Verdict                | Merge gate     |
|------|------------------------|----------------|
| `0`  | **Passed**             | allow          |
| `1`  | **Failed**             | block          |
| `2`  | **Inconclusive/Error** | block (no pass) |

On GitHub, `emit: gh` also posts a `nerve/verification-receipt` check-run via the
`gh` CLI (authed with `GITHUB_TOKEN`); the exit code remains authoritative.

On GitLab, `--emit gitlab` posts a `nerve-gate` **commit status** via the Commit
Status API (shelling `curl`). GitLab CI provides everything it needs automatically —
`CI_API_V4_URL` (defaults to `https://gitlab.com/api/v4` off-pipeline), `CI_PROJECT_ID`,
`CI_COMMIT_SHA`, and `CI_JOB_TOKEN` (the auth) — or set a masked `GITLAB_TOKEN` CI
variable to use a `PRIVATE-TOKEN` instead. The status only **mirrors** the exit code:
it is `success` only on a **Passed** verdict (exit 0); an **un-cleared verdict**
(Failed / Inconclusive / Error) posts `failed`, **never** `success` (INV-R1). The exit
code remains authoritative — a posting failure is reported but never overrides it.

## Signature re-verification at the gate (INV-R5)

Before trusting a **pre-sealed** Receipt's verdict, `nerve gate` **re-verifies the
Receipt offline** (trust-substrate.md §L4/§L5, INV-R5 — *"re-verifiable by a party who
trusts none of the participants"*). It re-derives the statement's content address
(tamper-evidence) and checks the detached ed25519 signature over the DSSE PAE with the
public key **embedded in the receipt** — no network, no key distribution needed.

A Receipt that does **not** verify is **REFUSED**: the gate exits non-zero (`2`), never
trusts the claimed verdict, and posts **no** check-run / commit-status. A tampered or
forged receipt file therefore can never gate a fabricated pass through CI (INV-R1).

```bash
nerve gate --receipt receipt.json          # verifies, then gates the verdict
# tampered/forged receipt:
#   REFUSED (exit 2): receipt integrity check FAILED — refusing to gate …
```

The `--json` output (and `nerve verify`'s) carries a `verification` block —
`{statement_intact, signature_valid, issuer_pinned, signed_by{keyid}, refused}` — so a
consumer sees that the gate checked the signature before trusting the verdict.

### Honest trust model — what the signature does and does NOT prove

Self-signature verification proves the receipt is **tamper-evident**: it was not
modified after signing. It does **NOT** prove **issuer identity** — a forger can simply
re-sign a fabricated receipt with their **own** key, and that self-consistent receipt
still validates. So without a pinned key, treat a receipt found in an untrusted location
as *unproven provenance*. Unpinned, the gate still gates, but prints a one-line advisory
that issuer identity is not pinned.

To pin issuer identity, supply the org's known signing key:

```bash
nerve gate --receipt receipt.json --trusted-key "<base64-ed25519-public-key>"
# or via env:
NERVE_TRUSTED_RECEIPT_KEY="<base64-…>" nerve gate --receipt receipt.json
```

With a pin set, the gate **additionally** requires the receipt's **verified public key**
— the embedded key the ed25519 signature actually checks out against, *not* the
self-declared `keyid` — to equal the trusted key; a mismatch is **REFUSED** (exit 2)
even when the self-signature is valid. (Pinning `keyid` would be forgeable: `keyid` is a
free-form label, so a forger could spoof it to your key while signing with their own.)

> We deliberately do **not** over-claim "trusts no one" without a pinned key.
> **Sigstore-keyless** issuer identity (Fulcio short-lived cert + Rekor transparency
> log) remains the deferred upgrade behind the same signing seam — it would establish
> issuer identity without a manually distributed key.

## Feeding the outcome corpus (L6)

The gate above runs **before** merge. The trust substrate also wants to know what
**actually happened after** merge — did the change ship and stick, or get reverted, or
cause an incident? That post-merge signal is the **L6 cross-agent outcome corpus**
(trust-substrate.md §3 L6): the data a future calibration model reads to learn "which
evidence predicts shipped-and-didn't-regress". It is the late dividend, not the day-one
moat — and it is **advisory and non-load-bearing**: a recorded outcome **never** feeds a
verdict, a gate, or a receipt (INV-R1/R3/R4).

`nerve outcome` is the **ingestion rail** — the offline, daemon-free twin of the daemon's
`outcome.label`, symmetric with `nerve verify` / `nerve gate` / `nerve ledger`:

```bash
nerve outcome <merged|reverted|incident|shipped-no-regress> --run <run_id> \
  [--receipt <id>] [--session <id>] [--source human|ci|observation] [--note <text>] \
  --root <path> [--json]
```

It appends the REAL outcome to the run's L6 corpus (`<root>/.nerve/outcomes/<run>.json`)
**and** mirrors it onto the L1 evidence ledger as an `OutcomeRecorded` fact, so the
observation joins the same tamper-evident chain `nerve ledger verify` re-derives.

> **Honest ingestion, not invention (INV-R1).** `nerve outcome` records what the **caller
> asserts happened** — a post-merge hook asserts `merged` because the platform merged the
> PR; a revert pipeline asserts `reverted`. Nerve does **not** invent the outcome, and it
> **never** derives one from a verify verdict (an `Outcome` is a *real-world* disposition —
> `merged` / `reverted` / `incident` / `shipped-no-regress` — not a pass/fail). The corpus
> stays advisory: it is **never** an input to a verdict, gate, or receipt.

### Auto-loop: record the outcome when a PR actually merges

This is an **opt-in** post-merge step a team adds. On GitHub, trigger it on a closed PR
that was actually merged and call `nerve outcome merged`:

```yaml
# .github/workflows/nerve-outcome.yml in YOUR repo (opt-in, post-merge)
on:
  pull_request:
    types: [closed]

jobs:
  record-outcome:
    # Only when the PR was actually MERGED (not just closed).
    if: github.event.pull_request.merged == true
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      # install `nerve` however the gate job does (pinned release or `cargo install`)…
      - name: Record the real merge outcome (advisory, never a gate)
        env:
          NERVE_RUN_ID: ${{ needs.delegate.outputs.run-id }}   # the captured Run id
          RECEIPT_ID: ${{ needs.gate.outputs.receipt-id }}      # optional, for query
        run: |
          nerve outcome merged --run "$NERVE_RUN_ID" \
            ${RECEIPT_ID:+--receipt "$RECEIPT_ID"} --source ci --root .
```

A push-to-`main` job works equally well (`on: push: branches: [main]`) when you key the
outcome off the merge commit rather than the PR-close event.

GitLab adopters `extend` the post-merge template (it runs on the default-branch push that
a merge produces):

```yaml
nerve-outcome:
  extends: .nerve-outcome
  variables:
    NERVE_RUN_ID: "$NERVE_RUN_ID"
    # NERVE_OUTCOME defaults to "merged"; set "reverted" in a revert pipeline.
```

Later real-world dispositions append to the same corpus and chain — e.g. a revert
pipeline records `nerve outcome reverted --run "$NERVE_RUN_ID" --source ci`, and an
incident monitor records `nerve outcome incident --run "$NERVE_RUN_ID" --source observation`.

## Honest limitations / follow-ups

- **Hermetic isolation is partial.** Replay/re-run currently relies on the runner's
  environment; the strong Landlock/seccomp sandbox closure (agent-exec-sandbox.md)
  that makes replay bit-for-bit trustworthy is still being finished — it is
  load-bearing, not optional.
- **Issuer identity without a manual key pin** (sigstore-keyless) is the deferred
  upgrade — today, issuer identity requires `--trusted-key` / `NERVE_TRUSTED_RECEIPT_KEY`.

See `docs/designs/trust-substrate.md` §L4 / §L5 / §8 for the full design.
