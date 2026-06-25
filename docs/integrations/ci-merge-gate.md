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

## Honest limitations / follow-ups

- **Hermetic isolation is partial.** Replay/re-run currently relies on the runner's
  environment; the strong Landlock/seccomp sandbox closure (agent-exec-sandbox.md)
  that makes replay bit-for-bit trustworthy is still being finished — it is
  load-bearing, not optional.
- **GitLab posts no commit status yet.** The GitLab template gates on the **exit
  code** today (a complete gate on its own); a GitLab Commit Status / MR-badge
  emitter (the equivalent of the GitHub `--emit gh` check-run) is a follow-up.
- **Receipt signature verification at the gate** (re-checking the signature before
  trusting a pre-sealed Receipt) is a planned hardening step.

See `docs/designs/trust-substrate.md` §L4 / §L5 / §8 for the full design.
