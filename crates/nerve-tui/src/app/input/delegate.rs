//! The `/delegate` + `/done` command handlers and the steer-vs-chat routing
//! (DA-5d). Splitting these out of [`super`] keeps the input module under the
//! file-size cap; the pure command-builders and [`submit_route`] live here next
//! to the handlers that use them, mirroring `super`'s "pure decisions are
//! unit-testable, IO is on [`Shell`]" split.

use nerve_runtime::{DelegateAutonomy, RuntimeCommand};

use super::super::Shell;
use super::super::state::{DelegateSession, State, Tone};
use crate::ui::commands::parse_delegate;

impl Shell {
    /// `/delegate <agent> [task]`: start a persistent, steerable delegate session.
    /// The started job's `job_id` becomes the active delegate session; while it is
    /// active, plain input steers it (see [`submit_route`]) and `/done` ends it.
    /// One session at a time — reject a second `/delegate`.
    pub(super) async fn cmd_delegate(&mut self, rest: &str) {
        if let Some(session) = &self.state.delegate_session {
            self.state.hint = format!("already steering {} — /done to end it first", session.agent);
            return;
        }
        let args = match parse_delegate(rest) {
            Ok(args) => args,
            Err(hint) => {
                self.state.hint = hint;
                return;
            }
        };
        if args.task.is_empty() {
            self.state.hint = format!(
                "usage: /delegate {} <task> — describe what to do",
                args.agent
            );
            return;
        }
        self.start_delegate(args.agent, args.task).await;
    }

    /// Start the `delegate.start` job and, on success, record its `job_id` as the
    /// active delegate session. A daemon failure (e.g. delegation disabled when
    /// `--allow-delegate` was not passed) surfaces as a red notice rather than a
    /// crash.
    async fn start_delegate(&mut self, agent: String, task: String) {
        let command = delegate_start_command(agent.clone(), task);
        match self.client.start_job(command, None).await {
            Ok(job) => {
                let session_id = job.job_id;
                self.state.note(format!(
                    "started delegate session {agent} ({session_id}) — steer with messages, /done to end"
                ));
                self.state.delegate_session = Some(DelegateSession { session_id, agent });
            }
            Err(err) => self.state.push_notice(Tone::Error, err.to_string()),
        }
    }

    /// `/done` (alias `/close`): end the active delegate session and return to
    /// normal chat. Sends `delegate.close` and clears the steer state; the header
    /// reverts. A no-op (with a hint) when no session is active.
    pub(super) async fn cmd_done(&mut self) {
        let Some(session) = self.state.delegate_session.take() else {
            self.state.hint = "no delegate session — /delegate <agent> <task> to start".to_string();
            return;
        };
        self.state.running = false;
        self.state
            .note(format!("ended delegate session {}", session.agent));
        self.send(close_command(session.session_id)).await;
    }
}

/// Build the `delegate.start` command for a `/delegate <agent> <task>` (DA-5d).
/// Pure so the command shape is testable without a live client; autonomy defaults
/// to the most-restricted [`DelegateAutonomy::ReadOnly`].
#[must_use]
fn delegate_start_command(agent: String, task: String) -> RuntimeCommand {
    RuntimeCommand::DelegateStart {
        agent,
        task,
        cwd: None,
        autonomy: DelegateAutonomy::ReadOnly,
        model: None,
    }
}

/// Build the `delegate.steer` command routing a follow-up message to a live
/// delegate session. Pure so the session routing is testable without a client.
#[must_use]
fn steer_command(session_id: String, message: String) -> RuntimeCommand {
    RuntimeCommand::DelegateSteer {
        session_id,
        message,
    }
}

/// Build the `delegate.close` command ending a live delegate session. Pure for
/// testability (matches `super::respond_command` / the steer/start builders).
#[must_use]
fn close_command(session_id: String) -> RuntimeCommand {
    RuntimeCommand::DelegateClose { session_id }
}

/// The outcome of submitting a plain (non-slash) message: either a one-shot hint
/// (no command sent) or a command to send plus the line to echo as the user block.
#[derive(Debug, PartialEq)]
pub(super) enum SubmitRoute {
    /// Set a status hint and send nothing (no session / still running).
    Hint(String),
    /// Send a command and echo `String` as the user's transcript line. The
    /// command is boxed — `RuntimeCommand` is large relative to the hint variant.
    Send(Box<RuntimeCommand>, String),
}

/// Decide what a submitted plain message becomes, purely from state (DA-5d §2).
/// A live delegate session steers (`delegate.steer`); otherwise the chat session
/// is messaged (`session.message`). The "still running" / "no session" guards
/// short-circuit to a hint. Pure so the routing is unit-testable without a client.
#[must_use]
pub(super) fn submit_route(state: &State, text: String) -> SubmitRoute {
    if let Some(session) = &state.delegate_session {
        if state.running {
            return SubmitRoute::Hint("delegate still working — Ctrl-C to interrupt".to_string());
        }
        return SubmitRoute::Send(
            Box::new(steer_command(session.session_id.clone(), text.clone())),
            text,
        );
    }
    let Some(session_id) = state.session_id.clone() else {
        return SubmitRoute::Hint("session not ready yet".to_string());
    };
    if state.running {
        return SubmitRoute::Hint("still working — Ctrl-C to interrupt".to_string());
    }
    SubmitRoute::Send(
        Box::new(RuntimeCommand::SessionMessage {
            session_id,
            text: text.clone(),
        }),
        text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::commands::parse_command;

    #[test]
    fn delegate_start_command_defaults_to_read_only() {
        let command = delegate_start_command("claude".into(), "fix the bug".into());
        match command {
            RuntimeCommand::DelegateStart {
                agent,
                task,
                cwd,
                autonomy,
                model,
            } => {
                assert_eq!(agent, "claude");
                assert_eq!(task, "fix the bug");
                assert_eq!(cwd, None);
                assert_eq!(autonomy, DelegateAutonomy::ReadOnly);
                assert_eq!(model, None);
            }
            other => panic!("expected DelegateStart, got {other:?}"),
        }
    }

    #[test]
    fn steer_command_routes_message_to_session() {
        match steer_command("delegate-job-7".into(), "now run the tests".into()) {
            RuntimeCommand::DelegateSteer {
                session_id,
                message,
            } => {
                assert_eq!(session_id, "delegate-job-7");
                assert_eq!(message, "now run the tests");
            }
            other => panic!("expected DelegateSteer, got {other:?}"),
        }
    }

    #[test]
    fn close_command_routes_to_session() {
        match close_command("delegate-job-7".into()) {
            RuntimeCommand::DelegateClose { session_id } => {
                assert_eq!(session_id, "delegate-job-7");
            }
            other => panic!("expected DelegateClose, got {other:?}"),
        }
    }

    #[test]
    fn submit_routes_to_steer_when_delegate_active() {
        let mut state = State::new("p", "m");
        state.session_id = Some("chat-1".into());
        state.delegate_session = Some(DelegateSession {
            session_id: "del-9".into(),
            agent: "claude".into(),
        });
        match submit_route(&state, "now run the tests".into()) {
            SubmitRoute::Send(command, echo) => {
                assert_eq!(echo, "now run the tests");
                match *command {
                    RuntimeCommand::DelegateSteer {
                        session_id,
                        message,
                    } => {
                        assert_eq!(session_id, "del-9");
                        assert_eq!(message, "now run the tests");
                    }
                    other => panic!("expected DelegateSteer, got {other:?}"),
                }
            }
            other => panic!("expected a Send route, got {other:?}"),
        }
    }

    #[test]
    fn submit_routes_to_chat_message_with_no_delegate() {
        let mut state = State::new("p", "m");
        state.session_id = Some("chat-1".into());
        match submit_route(&state, "hello".into()) {
            SubmitRoute::Send(command, _) => match *command {
                RuntimeCommand::SessionMessage { session_id, text } => {
                    assert_eq!(session_id, "chat-1");
                    assert_eq!(text, "hello");
                }
                other => panic!("expected SessionMessage, got {other:?}"),
            },
            other => panic!("expected a Send route, got {other:?}"),
        }
    }

    #[test]
    fn submit_hints_while_running_and_without_session() {
        let mut running = State::new("p", "m");
        running.session_id = Some("chat-1".into());
        running.running = true;
        assert!(matches!(
            submit_route(&running, "x".into()),
            SubmitRoute::Hint(h) if h.contains("still working")
        ));
        let no_session = State::new("p", "m");
        assert!(matches!(
            submit_route(&no_session, "x".into()),
            SubmitRoute::Hint(h) if h.contains("not ready")
        ));
    }

    #[test]
    fn submit_hints_when_delegate_turn_is_running() {
        let mut state = State::new("p", "m");
        state.delegate_session = Some(DelegateSession {
            session_id: "del-9".into(),
            agent: "codex".into(),
        });
        state.running = true;
        assert!(matches!(
            submit_route(&state, "x".into()),
            SubmitRoute::Hint(h) if h.contains("delegate still working")
        ));
    }

    #[test]
    fn delegate_command_parses_agent_and_task() {
        // `/delegate claude fix the bug` → DelegateStart{agent=claude, task=...}.
        let parsed = parse_command("/delegate claude fix the bug").expect("slash command");
        assert_eq!(parsed.cmd, "delegate");
        let args = parse_delegate(&parsed.rest).expect("delegate args");
        let command = delegate_start_command(args.agent, args.task);
        match command {
            RuntimeCommand::DelegateStart { agent, task, .. } => {
                assert_eq!(agent, "claude");
                assert_eq!(task, "fix the bug");
            }
            other => panic!("expected DelegateStart, got {other:?}"),
        }
    }
}
