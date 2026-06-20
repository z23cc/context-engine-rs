//! Mapping runtime events → shell state, and the key actions the loop performs.
//!
//! Kept separate from the IO loop so the event→state reduction is unit-testable
//! without a terminal or a live daemon. Mirrors the relevant arms of the TS
//! `#onEvent` / `#onAgentEvent`.

use nerve_runtime::{
    AgentEventKind, FlowDecisionKind, FlowNodeUsage, FlowRunOutcome, RuntimeEvent, Strategy,
};

use super::state::{ApprovalState, Mode, State, Tone};

/// Apply one runtime event to the shell state. Returns `true` if the frame
/// should be re-rendered. Only the subset the minimal shell understands is
/// handled; everything else is ignored (additive-safe).
pub fn apply_event(state: &mut State, event: &RuntimeEvent) -> bool {
    // The flow-event family is handled in its own reducer to keep this switch
    // under the line cap; `Some` means it was a flow event (additive-safe).
    if let Some(redraw) = apply_flow_event(state, event) {
        return redraw;
    }
    match event {
        RuntimeEvent::SessionStarted { session_id } => {
            state.session_id = Some(session_id.clone());
            true
        }
        RuntimeEvent::TurnStarted { .. } => {
            state.running = true;
            state.turn_started_at = Some(std::time::Instant::now());
            state.elapsed_ms = 0;
            true
        }
        RuntimeEvent::SessionIdle { .. } => {
            state.running = false;
            state.end_stream();
            true
        }
        RuntimeEvent::ApprovalRequested {
            session_id,
            request_id,
            tool,
            arguments,
            tier,
            preview,
        } => {
            // Stage the modal state; render/handling live in `render`/`input`.
            // Mirrors the TS `approval_requested` arm (compact-JSON args).
            state.mode = Mode::Approval;
            state.approval = Some(ApprovalState {
                tool: tool.clone(),
                args: args_to_string(arguments),
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                tier: *tier,
                preview: preview.clone(),
            });
            true
        }
        RuntimeEvent::DelegateProgress { agent, text, .. } => {
            // Stream the delegated agent's stdout/stderr into a coalescing
            // "delegate" block (one growing entry per agent), distinct from the
            // main assistant text. Empty chunks no-op and skip a redraw.
            if text.is_empty() {
                return false;
            }
            state.append_delegate(agent, text);
            true
        }
        RuntimeEvent::SessionAgent { event, .. } => apply_agent_event(state, event),
        RuntimeEvent::JobFailed { .. }
        | RuntimeEvent::JobCompleted { .. }
        | RuntimeEvent::JobCancelled { .. } => apply_terminal_job(state, event),
        _ => false,
    }
}

/// Reduce a terminal job event (`job_failed`/`job_completed`/`job_cancelled`).
/// A terminal event for the active flow's/delegate's id ends that session (their
/// `flow_id`/`session_id` IS the originating job id); a flow-start failing outright
/// (a CLI worker without `--allow-delegate`) reaches here before any `flow_started`,
/// so this is the one place the daemon's clear error is shown for a flow.
fn apply_terminal_job(state: &mut State, event: &RuntimeEvent) -> bool {
    match event {
        RuntimeEvent::JobFailed { job_id, error } => {
            if clear_flow_on_terminal(state, job_id) {
                state.push_notice(Tone::Error, error.message.clone());
                return true;
            }
            if clear_delegate_on_terminal(state, job_id) {
                state.running = false;
                state.push_notice(Tone::Error, error.message.clone());
                return true;
            }
            // A second message racing an in-flight turn: the genuine turn is still
            // live, so hint rather than clearing `running` / dumping a red line.
            if error.message.contains("is already running") {
                state.hint = "still working — Ctrl-C to interrupt".to_string();
            } else {
                state.running = false;
                state.push_notice(Tone::Error, error.message.clone());
            }
            true
        }
        RuntimeEvent::JobCompleted { job_id } | RuntimeEvent::JobCancelled { job_id } => {
            if clear_flow_on_terminal(state, job_id) {
                state.note("flow ended");
                return true;
            }
            if clear_delegate_on_terminal(state, job_id) {
                state.running = false;
                state.note("delegate session ended");
                return true;
            }
            false
        }
        _ => false,
    }
}

/// Apply `flow_started`: open the flow header (with the at-minimum node count from
/// the strategy shape) and record the active flow session. The flow's name isn't
/// carried on the event (only the strategy is), so the header uses the strategy
/// label as the name; `start_flow` records both on the active [`FlowSession`].
fn apply_flow_started(state: &mut State, flow_id: &str, strategy: &Strategy) {
    let label = strategy_label(strategy);
    let nodes = strategy_min_nodes(strategy);
    state.start_flow(flow_id, &label, &label, nodes);
}

/// Apply a `flow_node_agent` step — reuses the session/delegate agent-event
/// rendering, but streams ONLY the node's message/reasoning text into its pane
/// (keyed by `node_id`), so concurrent nodes don't interleave (C-TUI §2). Tool
/// calls inside a node surface as text lines in the pane (kept compact).
fn apply_flow_node_agent(state: &mut State, node_id: &str, event: &AgentEventKind) -> bool {
    match event {
        AgentEventKind::Message { text } | AgentEventKind::Reasoning { text } => {
            if text.is_empty() {
                return false;
            }
            state.append_flow_node(node_id, text);
            true
        }
        AgentEventKind::ToolStarted { tool, .. } => {
            state.append_flow_node(node_id, &format!("\n⚙ {tool}\n"));
            true
        }
        AgentEventKind::Usage { .. }
        | AgentEventKind::ToolFinished { .. }
        | AgentEventKind::TurnStarted { .. }
        | AgentEventKind::Interrupted { .. } => false,
    }
}

/// Apply `flow_completed`: a final outcome audit row carrying the summary + the
/// flow's final text.
fn apply_flow_completed(state: &mut State, outcome: &FlowRunOutcome) {
    let (tone, marker) = if outcome.ok {
        (Tone::Info, "✓")
    } else {
        (Tone::Error, "✗")
    };
    let mut line = format!("{marker} flow done · {}", outcome.summary);
    if !outcome.final_text.is_empty() {
        line.push_str(&format!("\n{}", outcome.final_text));
    }
    state.push_flow_audit(tone, line);
}

/// If `job_id` is the active flow's id, clear the flow and report `true`. The flow
/// keeps the `flow.start` job id (the `flow_id` IS the job id) for its lifetime, so
/// a terminal event for that id ends it.
fn clear_flow_on_terminal(state: &mut State, job_id: &str) -> bool {
    if state.flow_session.as_ref().map(|f| f.flow_id.as_str()) == Some(job_id) {
        state.end_flow();
        true
    } else {
        false
    }
}

/// A short human label for a strategy (the audit/header vocabulary).
fn strategy_label(strategy: &Strategy) -> String {
    match strategy {
        Strategy::Single { .. } => "single",
        Strategy::Parallel { .. } => "parallel",
        Strategy::Pipeline { .. } => "pipeline",
        Strategy::MapReduce { .. } => "map-reduce",
        Strategy::VoteJudge { .. } => "vote",
        Strategy::Debate { .. } => "debate",
        Strategy::Hierarchical { .. } => "hierarchical",
        // `Strategy` is non-exhaustive; a future variant labels generically.
        _ => "flow",
    }
    .to_string()
}

/// The at-minimum node count from a strategy's shape, for the initial header
/// (the count grows as nodes actually start).
fn strategy_min_nodes(strategy: &Strategy) -> usize {
    match strategy {
        Strategy::Single { .. } => 1,
        Strategy::Parallel { branches, .. } => branches.len(),
        Strategy::Pipeline { stages } => stages.len(),
        Strategy::MapReduce { .. } => 2,
        Strategy::VoteJudge { candidates, .. } => candidates.len() + 1,
        Strategy::Debate { sides, .. } => sides.len() + 1,
        Strategy::Hierarchical { .. } => 1,
        // `Strategy` is non-exhaustive; a future variant starts the count at 1.
        _ => 1,
    }
}

/// A token-usage summary for a finished node header (`↑in ↓out`), empty when none.
fn usage_summary(usage: &FlowNodeUsage) -> String {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return String::new();
    }
    format!("↑{} ↓{}", usage.input_tokens, usage.output_tokens)
}

/// A human audit line for a [`FlowDecisionKind`] (C-TUI §2): the typed,
/// replayable decisions the engine recorded.
fn decision_line(kind: &FlowDecisionKind) -> String {
    match kind {
        FlowDecisionKind::BudgetExhausted => "⚖ budget exhausted · branches cancelled".to_string(),
        FlowDecisionKind::DepthCeiling { depth, max_depth } => {
            format!("⚖ depth ceiling · {depth}/{max_depth} — spawn refused")
        }
        FlowDecisionKind::WorkerCeiling {
            live_workers,
            max_workers,
        } => format!("⚖ worker ceiling · {live_workers}/{max_workers} — spawn refused"),
        FlowDecisionKind::VoteTally {
            ok,
            total,
            k,
            reached,
        } => {
            let status = if *reached { "quorum" } else { "short" };
            format!("⚖ vote {ok}/{total} ok (k={k}, {status}) → judge")
        }
        FlowDecisionKind::JudgePick { node_id, ok } => {
            let verdict = if *ok { "picked" } else { "failed" };
            format!("⚖ judge {verdict} → {node_id}")
        }
        FlowDecisionKind::DebateRound { round, sides_ok } => {
            format!("⚖ debate round {round} · {sides_ok} side(s) ok")
        }
    }
}

/// Reduce the `flow_*` / budget event family into flow state (C-TUI §2). Returns
/// `Some(redraw)` when `event` is a flow event, `None` otherwise (so the caller's
/// switch handles the non-flow events). Split out of [`apply_event`] for the line
/// cap; the terminal-job clearing of a flow stays in [`apply_event`] because a
/// `job_failed` for a flow can predate any `flow_started`.
fn apply_flow_event(state: &mut State, event: &RuntimeEvent) -> Option<bool> {
    match event {
        RuntimeEvent::FlowStarted { flow_id, strategy } => {
            apply_flow_started(state, flow_id, strategy);
            Some(true)
        }
        RuntimeEvent::FlowNodeStarted {
            node_id, worker, ..
        } => {
            state.open_flow_node(node_id, worker);
            Some(true)
        }
        RuntimeEvent::FlowNodeAgent { node_id, event, .. } => {
            Some(apply_flow_node_agent(state, node_id, event))
        }
        RuntimeEvent::FlowNodeFinished {
            node_id, ok, usage, ..
        } => {
            state.finish_flow_node(node_id, *ok, usage_summary(usage));
            Some(true)
        }
        RuntimeEvent::FlowEdge { from, to, .. } => {
            // A compact connector row keeps the DAG shape readable in a linear
            // transcript. Skip the synthetic root→node-0 edge — the header implies it.
            if from == "flow" {
                return Some(false);
            }
            state.push_flow_audit(Tone::Info, format!("↪ {from} → {to}"));
            Some(true)
        }
        RuntimeEvent::FlowDecision { kind, .. } => {
            state.push_flow_audit(Tone::Info, decision_line(kind));
            Some(true)
        }
        RuntimeEvent::BudgetUpdate {
            spent_usd, tokens, ..
        } => {
            state.record_flow_budget(*spent_usd, *tokens);
            Some(true)
        }
        RuntimeEvent::BudgetWarning {
            spent_usd,
            limit_usd,
            ..
        } => {
            state.record_flow_budget_warning(*spent_usd, *limit_usd);
            state.push_flow_audit(
                Tone::Warn,
                format!("◧ budget warning · ${spent_usd:.2} / ${limit_usd:.2}"),
            );
            Some(true)
        }
        RuntimeEvent::FlowCompleted { outcome, .. } => {
            apply_flow_completed(state, outcome);
            Some(true)
        }
        RuntimeEvent::FlowFailed { node_id, error, .. } => {
            let where_ = node_id
                .as_deref()
                .map_or_else(String::new, |n| format!(" [{n}]"));
            state.push_flow_audit(Tone::Error, format!("✗ flow failed{where_}: {error}"));
            Some(true)
        }
        _ => None,
    }
}

/// If `job_id` is the active delegate session's id, clear it and report `true`
/// (the caller surfaces the reason). A delegate session keeps the `delegate.start`
/// job id for its whole lifetime, so a terminal event for that id ends the session.
fn clear_delegate_on_terminal(state: &mut State, job_id: &str) -> bool {
    if state
        .delegate_session
        .as_ref()
        .map(|s| s.session_id.as_str())
        == Some(job_id)
    {
        state.delegate_session = None;
        true
    } else {
        false
    }
}

fn apply_agent_event(state: &mut State, event: &AgentEventKind) -> bool {
    match event {
        // Empty deltas are dropped (providers emit trailing empty chunks); the
        // append helpers no-op on "", but skipping here also avoids a redraw.
        AgentEventKind::Message { text } => {
            if text.is_empty() {
                return false;
            }
            state.append_assistant(text);
            true
        }
        AgentEventKind::Reasoning { text } => {
            if text.is_empty() {
                return false;
            }
            state.append_reasoning(text);
            true
        }
        AgentEventKind::ToolStarted { tool, arguments } => {
            state.start_tool(tool.clone(), args_to_string(arguments));
            true
        }
        AgentEventKind::ToolFinished { tool, ok, output } => {
            state.finish_tool(tool, *ok, output.clone());
            true
        }
        AgentEventKind::Interrupted { reason } => {
            state.push_notice(Tone::Warn, format!("interrupted: {reason}"));
            true
        }
        // Usage feeds the status bar (tokens / context % / cost).
        AgentEventKind::Usage {
            input_tokens,
            output_tokens,
            ..
        } => {
            state.record_usage(*input_tokens, *output_tokens);
            true
        }
        // TurnStarted is handled at the RuntimeEvent layer.
        AgentEventKind::TurnStarted { .. } => false,
    }
}

/// Serialize tool arguments to a compact JSON string for the cell header. A JSON
/// string value is unquoted; everything else is its JSON encoding (mirrors the TS
/// `safeJson`).
fn args_to_string(arguments: &serde_json::Value) -> String {
    match arguments {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Block;
    use nerve_runtime::RuntimeJobError;

    #[test]
    fn session_started_records_id() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(&mut state, &RuntimeEvent::session_started("sess-1"));
        assert!(redraw);
        assert_eq!(state.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn turn_started_and_idle_toggle_running() {
        let mut state = State::new("p", "m");
        apply_event(&mut state, &RuntimeEvent::turn_started("s"));
        assert!(state.running);
        apply_event(&mut state, &RuntimeEvent::session_idle("s"));
        assert!(!state.running);
    }

    #[test]
    fn agent_message_streams_into_assistant_block() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Message { text: "ab".into() }),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Message { text: "cd".into() }),
        );
        assert_eq!(state.blocks, vec![Block::Assistant("abcd".to_string())]);
    }

    #[test]
    fn job_failed_clears_running_and_notes_error() {
        let mut state = State::new("p", "m");
        state.running = true;
        apply_event(
            &mut state,
            &RuntimeEvent::job_failed("j", RuntimeJobError::new("k", "boom")),
        );
        assert!(!state.running);
        assert!(matches!(
            state.blocks.last(),
            Some(Block::Notice { tone: Tone::Error, text }) if text.contains("boom")
        ));
    }

    #[test]
    fn agent_reasoning_streams_into_reasoning_block() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Reasoning { text: "th".into() }),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent("s", AgentEventKind::Reasoning { text: "ink".into() }),
        );
        assert_eq!(state.blocks, vec![Block::Reasoning("think".to_string())]);
    }

    #[test]
    fn tool_started_then_finished_builds_a_tool_block() {
        use crate::app::state::ToolStatus;
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent(
                "s",
                AgentEventKind::ToolStarted {
                    tool: "read_file".into(),
                    arguments: serde_json::json!({ "path": "a.rs" }),
                },
            ),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::session_agent(
                "s",
                AgentEventKind::ToolFinished {
                    tool: "read_file".into(),
                    ok: true,
                    output: "contents".into(),
                },
            ),
        );
        let Some(Block::Tool(cell)) = state.blocks.last() else {
            panic!("expected a tool block");
        };
        assert_eq!(cell.status, ToolStatus::Ok);
        assert_eq!(cell.tool, "read_file");
        assert_eq!(cell.output.as_deref(), Some("contents"));
    }

    #[test]
    fn empty_agent_delta_does_not_push_or_redraw() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::session_agent(
                "s",
                AgentEventKind::Message {
                    text: String::new(),
                },
            ),
        );
        assert!(!redraw);
        assert!(state.blocks.is_empty());
    }

    #[test]
    fn delegate_progress_appends_and_coalesces_into_delegate_block() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::delegate_progress("j", "codex", "look"),
        );
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::delegate_progress("j", "codex", "ing…"),
        );
        assert!(redraw);
        assert_eq!(
            state.blocks,
            vec![Block::Delegate {
                agent: "codex".to_string(),
                text: "looking…".to_string(),
            }]
        );
    }

    #[test]
    fn empty_delegate_progress_does_not_push_or_redraw() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::delegate_progress("j", "codex", ""),
        );
        assert!(!redraw);
        assert!(state.blocks.is_empty());
    }

    #[test]
    fn unknown_event_does_not_redraw() {
        let mut state = State::new("p", "m");
        let redraw = apply_event(&mut state, &RuntimeEvent::job_completed("j"));
        assert!(!redraw);
    }

    use crate::app::state::DelegateSession;

    fn with_delegate(session_id: &str, agent: &str) -> State {
        let mut state = State::new("p", "m");
        state.delegate_session = Some(DelegateSession {
            session_id: session_id.into(),
            agent: agent.into(),
        });
        state
    }

    #[test]
    fn delegate_start_job_completed_clears_active_session() {
        // The delegate session keeps the start-job id; a terminal event for it ends
        // the session and returns input to the chat (DA-5d §2).
        let mut state = with_delegate("del-1", "claude");
        state.running = true;
        let redraw = apply_event(&mut state, &RuntimeEvent::job_completed("del-1"));
        assert!(redraw);
        assert!(state.delegate_session.is_none());
        assert!(!state.running);
    }

    #[test]
    fn delegate_start_job_failed_clears_session_and_notes_error() {
        let mut state = with_delegate("del-2", "claude");
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::job_failed("del-2", RuntimeJobError::new("k", "delegation disabled")),
        );
        assert!(redraw);
        assert!(state.delegate_session.is_none());
        assert!(matches!(
            state.blocks.last(),
            Some(Block::Notice { tone: Tone::Error, text }) if text.contains("delegation disabled")
        ));
    }

    #[test]
    fn unrelated_terminal_event_keeps_delegate_session() {
        // A `delegate.steer`/other job is a *separate* job from the start job, so its
        // terminal event must not clear the session.
        let mut state = with_delegate("del-3", "codex");
        apply_event(&mut state, &RuntimeEvent::job_completed("tui-job-9"));
        assert!(state.delegate_session.is_some());
    }

    use crate::app::state::Block as B;
    use nerve_runtime::{
        FlowNodeUsage, FlowRunOutcome, FlowWorkerKind, Step, TaskTemplate, WorkerRef,
    };

    fn parallel_two() -> nerve_runtime::Strategy {
        let step = |name: &str| Step {
            worker: WorkerRef::Cli { name: name.into() },
            task: TaskTemplate::new("t"),
            autonomy: nerve_runtime::DelegateAutonomy::ReadOnly,
            on_fail: nerve_runtime::FailPolicy::Abort,
        };
        nerve_runtime::Strategy::Parallel {
            branches: vec![step("claude"), step("codex")],
            join: nerve_runtime::Join::All,
        }
    }

    /// Drive a full parallel flow lifecycle through the reducer.
    fn run_parallel_flow(state: &mut State) {
        apply_event(state, &RuntimeEvent::flow_started("flow-1", parallel_two()));
        apply_event(
            state,
            &RuntimeEvent::flow_node_started("flow-1", "node-0", "claude", FlowWorkerKind::Cli),
        );
        apply_event(
            state,
            &RuntimeEvent::flow_node_started("flow-1", "node-1", "codex", FlowWorkerKind::Cli),
        );
        // Interleaved node deltas must land in their own pane.
        apply_event(
            state,
            &RuntimeEvent::flow_node_agent(
                "flow-1",
                "node-0",
                AgentEventKind::Message {
                    text: "alpha".into(),
                },
            ),
        );
        apply_event(
            state,
            &RuntimeEvent::flow_node_agent(
                "flow-1",
                "node-1",
                AgentEventKind::Message {
                    text: "beta".into(),
                },
            ),
        );
        apply_event(
            state,
            &RuntimeEvent::flow_node_finished(
                "flow-1",
                "node-0",
                true,
                FlowNodeUsage {
                    input_tokens: 5,
                    output_tokens: 3,
                    ..FlowNodeUsage::default()
                },
            ),
        );
        apply_event(
            state,
            &RuntimeEvent::flow_node_finished("flow-1", "node-1", true, FlowNodeUsage::default()),
        );
        apply_event(
            state,
            &RuntimeEvent::flow_completed(
                "flow-1",
                FlowRunOutcome {
                    ok: true,
                    summary: "parallel: 2/2 ok".into(),
                    final_text: "alpha\nbeta".into(),
                },
            ),
        );
    }

    #[test]
    fn parallel_flow_opens_header_two_node_panes_and_outcome() {
        let mut state = State::new("xai", "grok-4-fast");
        run_parallel_flow(&mut state);
        // Header opened, active flow tracked.
        assert!(
            state
                .flow_session
                .as_ref()
                .is_some_and(|f| f.flow_id == "flow-1")
        );
        assert!(matches!(
            state.blocks.first(),
            Some(B::FlowHeader { nodes: 2, .. })
        ));
        // Two distinct node panes, each with its own (non-interleaved) text.
        let node0 = state.blocks.iter().find_map(|b| match b {
            B::FlowNode {
                node_id,
                text,
                done,
                ..
            } if node_id == "node-0" => Some((text.clone(), done.clone())),
            _ => None,
        });
        let node1 = state.blocks.iter().find_map(|b| match b {
            B::FlowNode { node_id, text, .. } if node_id == "node-1" => Some(text.clone()),
            _ => None,
        });
        let (n0_text, n0_done) = node0.expect("node-0 pane");
        assert_eq!(n0_text, "alpha");
        assert_eq!(n1_text(&node1), "beta");
        assert_eq!(n0_done, Some((true, "↑5 ↓3".to_string())));
        // A final outcome audit row carrying the summary + final text.
        assert!(state.blocks.iter().any(|b| matches!(
            b,
            B::FlowAudit { text, .. } if text.contains("flow done") && text.contains("parallel: 2/2 ok")
        )));
    }

    fn n1_text(node1: &Option<String>) -> &str {
        node1.as_deref().expect("node-1 pane")
    }

    #[test]
    fn flow_terminal_job_clears_active_flow() {
        let mut state = State::new("p", "m");
        run_parallel_flow(&mut state);
        assert!(state.flow_session.is_some());
        let redraw = apply_event(&mut state, &RuntimeEvent::job_completed("flow-1"));
        assert!(redraw);
        assert!(state.flow_session.is_none());
    }

    #[test]
    fn flow_decision_and_budget_render_audit_and_indicator() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::flow_started("flow-1", parallel_two()),
        );
        // A vote tally decision → a distinct ⚖ audit line.
        apply_event(
            &mut state,
            &RuntimeEvent::flow_decision(
                "flow-1",
                "flow",
                FlowDecisionKind::VoteTally {
                    ok: 2,
                    total: 3,
                    k: 2,
                    reached: true,
                },
            ),
        );
        assert!(state.blocks.iter().any(|b| matches!(
            b,
            B::FlowAudit { text, .. } if text.contains("⚖ vote 2/3 ok") && text.contains("judge")
        )));
        // A budget update → the header budget indicator (no audit row, just state).
        apply_event(
            &mut state,
            &RuntimeEvent::budget_update("flow-1", 0.42, 1234),
        );
        let budget = state.flow_budget.expect("budget");
        assert!((budget.spent_usd - 0.42).abs() < 1e-9);
        assert_eq!(budget.tokens, 1234);
        assert!(budget.warn_limit_usd.is_none());
        // A budget warning → the warning limit + a warn audit row.
        apply_event(
            &mut state,
            &RuntimeEvent::budget_warning("flow-1", 0.8, 1.0),
        );
        assert_eq!(state.flow_budget.unwrap().warn_limit_usd, Some(1.0));
        assert!(state.blocks.iter().any(|b| matches!(
            b,
            B::FlowAudit { tone: Tone::Warn, text } if text.contains("budget warning")
        )));
    }

    #[test]
    fn flow_approval_request_keyed_by_flow_id_opens_modal() {
        // A flow branch's approval carries the flow id as session_id; the existing
        // approval reducer stages the modal keyed by that id, so `on_approval_key`
        // can route `flow.respond` (C-TUI §3, verified in `input` unit tests).
        let mut state = State::new("p", "m");
        state.flow_session = Some(crate::app::state::FlowSession {
            flow_id: "flow-1".into(),
            name: "vote".into(),
            strategy: "vote".into(),
        });
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::approval_requested(
                "flow-1",
                "approval-3",
                "edit",
                serde_json::json!({ "path": "a.rs" }),
                nerve_runtime::RiskTier::Edit,
                "@@ -1 +1 @@",
            ),
        );
        assert!(redraw);
        assert_eq!(state.mode, Mode::Approval);
        let approval = state.approval.expect("staged approval");
        // The approval id is the flow id — the routing key the respond uses.
        assert_eq!(approval.session_id, "flow-1");
        assert_eq!(approval.request_id, "approval-3");
        assert_eq!(approval.tool, "edit");
    }

    #[test]
    fn flow_failed_event_renders_error_audit() {
        let mut state = State::new("p", "m");
        apply_event(
            &mut state,
            &RuntimeEvent::flow_started("flow-1", parallel_two()),
        );
        apply_event(
            &mut state,
            &RuntimeEvent::flow_failed("flow-1", Some("node-1".into()), "worker died"),
        );
        assert!(state.blocks.iter().any(|b| matches!(
            b,
            B::FlowAudit { tone: Tone::Error, text }
                if text.contains("flow failed") && text.contains("[node-1]") && text.contains("worker died")
        )));
    }

    #[test]
    fn flow_started_outright_failure_surfaces_daemon_error() {
        // A flow.start that fails before any flow_started (a CLI worker without
        // --allow-delegate) still ends the (pre-recorded) flow session and notes
        // the daemon's clear error.
        let mut state = State::new("p", "m");
        state.flow_session = Some(crate::app::state::FlowSession {
            flow_id: "flow-9".into(),
            name: "flow".into(),
            strategy: "flow".into(),
        });
        let redraw = apply_event(
            &mut state,
            &RuntimeEvent::job_failed(
                "flow-9",
                RuntimeJobError::new("k", "delegation disabled (--allow-delegate)"),
            ),
        );
        assert!(redraw);
        assert!(state.flow_session.is_none());
        assert!(matches!(
            state.blocks.last(),
            Some(B::Notice { tone: Tone::Error, text }) if text.contains("delegation disabled")
        ));
    }
}
