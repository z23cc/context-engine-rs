//! Hidden, experimental `nerve flow run` — the off-protocol engine driver (C1).
//!
//! This subcommand exists to **harden the deterministic orchestration engine
//! (`crate::flow`) with ZERO protocol commitment** (design §10, Wave C1). It
//! reads a [`WorkflowDef`] from a JSON file, builds a C0
//! [`WorkerFactory`](crate::worker::WorkerFactory), runs the engine, streams each
//! node's progress to stdout, and prints the final aggregated outcome. It mints
//! **no** `RuntimeCommand`/`RuntimeEvent` vocabulary — C2 adds the `flow.*`
//! protocol on top of the engine this command exercises.
//!
//! It is hidden from the top-level help (`hide = true`) and clearly marked
//! experimental: the shape of `WorkflowDef` and the engine are not yet a stable
//! contract.

use crate::flow::{Driver, FactoryResolver, FlowObserver, FlowProgress};
use crate::providers::ProviderRegistry;
use crate::subagent::DEFAULT_MAX_DEPTH;
use crate::tools;
use crate::worker::{
    BudgetDecision, BudgetLedger, BudgetSnapshot, FleetBudget, SpawnRefusal, TurnResult,
    WorkerFactory,
};
use crate::workspace::{self, ServeArgs};
use anyhow::{Context, Result, anyhow};
use clap::Args;
use nerve_core::CancelToken;
use nerve_runtime::{RiskTier, SessionApprovalDecision, Strategy, WorkflowDef};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Args)]
pub(crate) struct FlowArgs {
    #[command(flatten)]
    serve: ServeArgs,
    /// Path to a JSON file describing the `WorkflowDef` to run.
    #[arg(long = "file")]
    file: std::path::PathBuf,
    /// Maximum branches running concurrently in a `Parallel` wave (default 4).
    #[arg(long = "concurrency")]
    concurrency: Option<usize>,
    /// Approve every worker permission prompt without asking. Off by default, so
    /// an experimental batch run is fail-closed (a CLI worker's `can_use_tool`
    /// ask is denied) unless this is set.
    #[arg(long = "allow-all", visible_alias = "yes", short = 'y')]
    allow_all: bool,
    /// Allow CLI workers (codex/claude/gemini) to be spawned as subprocesses.
    /// Off by default — a flow then runs only in-process provider workers, so the
    /// experimental command never spawns an external agent unless asked.
    #[arg(long = "allow-delegate")]
    allow_delegate: bool,
}

/// Entry point for `nerve flow run` (wired from `cli.rs`). Experimental.
pub(crate) fn run(args: FlowArgs) -> Result<()> {
    eprintln!(
        "\u{26a0}  nerve flow run is EXPERIMENTAL: the WorkflowDef shape and engine are not a stable contract (C1)."
    );
    let def = load_workflow(&args.file)?;
    // Static safety checks (design §8): reject a zero-depth hierarchy, an unresolvable
    // named worker, or a planner fork-loop before any worker spawns.
    crate::flow::validate_workflow(&def)
        .map_err(|err| anyhow::anyhow!("invalid workflow: {err}"))?;
    let cancel = CancelToken::new();
    crate::agent::install_interrupt_handler(&cancel);

    let factory = build_factory(&args)?;
    let resolver = FactoryResolver::new(factory);
    let ledger = Arc::new(crate::worker::WorkerLedger::new());
    let approver: Arc<dyn crate::delegate_proxy::DelegateApprover> =
        Arc::new(FlowApprover::new(args.allow_all));
    let root = args.serve.roots.first().cloned();

    // Per-flow budget governance (C3b, design §6/§8): the WorkflowDef's budget +
    // max_depth carve the BudgetLedger + root FleetBudget, so the experimental run
    // also self-terminates on a USD/token/worker overrun and refuses past the depth
    // ceiling. A default (all-None) budget caps nothing — the C1 behaviour.
    let budget = Arc::new(BudgetLedger::new(def.budget));
    let fleet = FleetBudget::root(
        def.max_depth,
        def.budget.max_workers,
        budget.remaining_usd(),
        budget.remaining_tokens(),
    );
    let on_progress = |progress: FlowProgress| print_progress(&progress);
    let observer = StdoutBudgetObserver;
    let driver = {
        let mut driver = Driver::new(&resolver, Arc::clone(&ledger), approver, root)
            .with_progress(&on_progress)
            .with_observer(&observer)
            .with_budget(Arc::clone(&budget), fleet);
        if let Some(concurrency) = args.concurrency {
            driver = driver.with_concurrency(concurrency);
        }
        driver
    };

    println!(
        "\u{25b6} flow `{}` ({}) starting",
        def.name,
        strategy_label(&def.strategy)
    );
    let outcome = driver.run(&def, &cancel);
    print_outcome(&def, &outcome);
    if outcome.ok {
        Ok(())
    } else if cancel.is_cancelled() {
        Err(anyhow!("flow interrupted"))
    } else {
        Err(anyhow!("flow did not succeed: {}", outcome.summary))
    }
}

/// Parse + validate the `WorkflowDef` JSON file (a clear error if it is missing
/// or malformed).
fn load_workflow(path: &std::path::Path) -> Result<WorkflowDef> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading workflow file {}", path.display()))?;
    let def: WorkflowDef = serde_json::from_str(&text)
        .with_context(|| format!("parsing workflow file {} as a WorkflowDef", path.display()))?;
    Ok(def)
}

/// Build the C0 [`WorkerFactory`] over the shared deps: a delegate launcher (real
/// only when `--allow-delegate`), the runtime (the shared snapshot), the provider
/// registry, the permission gate, and the recursion depth ceiling.
fn build_factory(args: &FlowArgs) -> Result<WorkerFactory> {
    let registry = ProviderRegistry::from_args(&args.serve)?;
    let runtime = Arc::new(crate::mcp::attach(
        tools::runtime(workspace::registry(&args.serve)?),
        &args.serve,
    )?);
    let gate = crate::policy::ToolGate::cli(
        args.serve.roots.first().map(|root| root.as_path()),
        args.allow_all,
    )?;
    let delegate_launcher = if args.allow_delegate {
        crate::sandbox::process_launcher()
    } else {
        crate::sandbox::refuse_launcher()
    };
    Ok(WorkerFactory::new(
        delegate_launcher,
        runtime,
        registry,
        gate,
        DEFAULT_MAX_DEPTH,
    ))
}

/// Stream one node's progress line to stdout. Only the structured `Message` /
/// `Progress` text is echoed (the rest is recorded in the ledger).
fn print_progress(progress: &FlowProgress) {
    use crate::worker::WorkerEvent;
    match &progress.event {
        WorkerEvent::Step(nerve_runtime::AgentEventKind::Message { text }) => {
            println!("[{}] {}", progress.node, truncate(text, 200));
        }
        WorkerEvent::Progress { text } => {
            let line = text.trim();
            if !line.is_empty() {
                println!("[{}] {}", progress.node, truncate(line, 200));
            }
        }
        _ => {}
    }
}

/// Print the aggregated outcome (the fold of the recorded results, in declared
/// order).
fn print_outcome(def: &WorkflowDef, outcome: &crate::flow::FlowOutcome) {
    println!(
        "\n\u{2014} flow `{}` done: ok={} \u{2014} {}",
        def.name, outcome.ok, outcome.summary
    );
    let text = outcome.final_text();
    if !text.is_empty() {
        println!("\n{text}");
    }
}

fn strategy_label(strategy: &Strategy) -> &'static str {
    match strategy {
        Strategy::Single { .. } => "single",
        Strategy::Parallel { .. } => "parallel",
        Strategy::Pipeline { .. } => "pipeline",
        _ => "experimental-strategy",
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// A [`FlowObserver`] for the experimental CLI that surfaces budget telemetry to
/// stdout (the daemon path emits protocol events instead): a running spend line
/// per debit, a warning, an exhaustion notice, and any spawn refusal. The node
/// lifecycle callbacks are no-ops (the progress sink already prints node output).
struct StdoutBudgetObserver;

impl FlowObserver for StdoutBudgetObserver {
    fn node_started(&self, _node: &str, _worker: &nerve_runtime::WorkerRef) {}
    fn node_finished(&self, _node: &str, _result: &TurnResult) {}

    fn budget_debited(&self, snapshot: BudgetSnapshot, decision: BudgetDecision) {
        match decision {
            BudgetDecision::Within => {}
            BudgetDecision::Warn { limit_usd } => {
                println!(
                    "\u{26a0}  budget warning: ${:.4} spent of ${limit_usd:.4}",
                    snapshot.spent_usd
                );
            }
            BudgetDecision::Exhausted => {
                println!(
                    "\u{26d4} budget exhausted (${:.4}, {} tokens) \u{2014} cancelling the flow",
                    snapshot.spent_usd, snapshot.spent_tokens
                );
            }
        }
    }

    fn spawn_refused(&self, node: &str, refusal: SpawnRefusal) {
        println!(
            "\u{26d4} spawn refused for `{node}`: {}",
            refusal_text(refusal)
        );
    }

    fn decision(&self, node: &str, kind: &nerve_runtime::FlowDecisionKind) {
        // Surface the richer-strategy audit decisions (C5) on the experimental CLI.
        println!("\u{2696} decision @ `{node}`: {}", decision_text(kind));
    }
}

/// A one-line description of an interpreter audit decision for the experimental CLI.
fn decision_text(kind: &nerve_runtime::FlowDecisionKind) -> String {
    use nerve_runtime::FlowDecisionKind as K;
    match kind {
        K::VoteTally {
            ok,
            total,
            k,
            reached,
        } => format!(
            "vote tally {ok}/{total} ok, quorum {k} {}",
            if *reached { "reached" } else { "short" }
        ),
        K::JudgePick { node_id, ok } => {
            format!("judge `{node_id}` {}", if *ok { "ok" } else { "failed" })
        }
        K::DebateRound { round, sides_ok } => {
            format!("debate round {round}: {sides_ok} side(s) ok")
        }
        K::DepthCeiling { depth, max_depth } => format!("depth ceiling ({depth}/{max_depth})"),
        K::WorkerCeiling {
            live_workers,
            max_workers,
        } => format!("worker ceiling ({live_workers}/{max_workers})"),
        K::BudgetExhausted => "budget exhausted".to_string(),
    }
}

/// A one-line description of a spawn refusal for the experimental CLI.
fn refusal_text(refusal: SpawnRefusal) -> String {
    match refusal {
        SpawnRefusal::Depth { depth, max_depth } => {
            format!("depth ceiling ({depth}/{max_depth})")
        }
        SpawnRefusal::Workers {
            live_workers,
            max_workers,
        } => format!("worker ceiling ({live_workers}/{max_workers})"),
        SpawnRefusal::Budget => "fleet budget exhausted".to_string(),
    }
}

/// The experimental driver's approver: with `--allow-all`, approve every worker
/// permission prompt; otherwise deny (the safe default for a non-interactive
/// batch run — a CLI worker that asks is refused rather than hanging on a prompt).
struct FlowApprover {
    allow_all: bool,
}

impl FlowApprover {
    fn new(allow_all: bool) -> Self {
        Self { allow_all }
    }
}

impl crate::delegate_proxy::DelegateApprover for FlowApprover {
    fn request(
        &self,
        _session_id: &str,
        tool: &str,
        _args: &Value,
        _tier: RiskTier,
        _preview: String,
        _cancel: &CancelToken,
    ) -> SessionApprovalDecision {
        if self.allow_all {
            SessionApprovalDecision::Allow
        } else {
            eprintln!(
                "\u{26a0}  flow worker asked to use `{tool}`; denied (pass --allow-all to approve)"
            );
            SessionApprovalDecision::Deny
        }
    }
}
