//! The `/flow` + `/flow close` command handlers (C-TUI §1/§3).
//!
//! Mirrors `input/delegate.rs`'s split: the pure spec parsing lives in
//! [`crate::ui::flow`]; this module is the [`Shell`] glue that turns a parsed
//! [`FlowSpec`] into a `flow.start` job (and `/flow close` into `flow.close`). One
//! flow at a time — a second `/flow` is rejected with a hint.

use std::fs;

use nerve_runtime::{RuntimeCommand, WorkflowDef};

use super::super::Shell;
use super::super::state::{FlowSession, Tone};
use crate::ui::flow::{FlowSpec, parse_flow, usage};

impl Shell {
    /// `/flow <strategy> …` (or `/flow close`): start an orchestration flow.
    /// Rejects a second concurrent flow; a bad spec prints a usage notice rather
    /// than crashing; a CLI-worker flow without `--allow-delegate` surfaces the
    /// daemon's clear error (via the `job_failed` arm) as a notice.
    pub(super) async fn cmd_flow(&mut self, rest: &str) {
        let rest = rest.trim();
        if rest == "close" || rest == "cancel" {
            self.cmd_flow_close().await;
            return;
        }
        if let Some(session) = &self.state.flow_session {
            self.state.hint = format!(
                "a flow is already running ({}) — /flow close to cancel it first",
                session.name
            );
            return;
        }
        let default_worker = (self.state.provider.as_str(), self.state.model.as_str());
        let spec = match parse_flow(rest, default_worker) {
            Ok(spec) => spec,
            Err(hint) => {
                self.state.push_notice(Tone::Warn, hint);
                return;
            }
        };
        self.start_flow(spec).await;
    }

    /// Build the `flow.start` command (loading a `--file` spec from disk) and send
    /// it; on success record the active flow session keyed by the started job id.
    /// A CLI-worker flow still needs `--allow-delegate`; the daemon enforces the
    /// lift and a clear `job_failed` surfaces if it was not passed.
    async fn start_flow(&mut self, spec: FlowSpec) {
        let command = match spec.into_command(load_workflow_file) {
            Ok(command) => command,
            Err(hint) => {
                self.state.push_notice(Tone::Error, hint);
                return;
            }
        };
        match self.client.start_job(command, None).await {
            Ok(job) => {
                let flow_id = job.job_id;
                self.state.note(format!(
                    "started flow ({flow_id}) — watch the nodes below · /flow close to cancel"
                ));
                // The header/blocks open on `flow_started`; record the session now
                // so a second `/flow` is rejected and the approval path routes
                // `flow.respond`. The name/strategy refine on `flow_started`.
                self.state.flow_session = Some(FlowSession {
                    flow_id,
                    name: "flow".to_string(),
                    strategy: "flow".to_string(),
                });
            }
            Err(err) => self.state.push_notice(Tone::Error, err.to_string()),
        }
    }

    /// `/flow close`: cancel the active flow. Sends `flow.close`; the flow session
    /// is cleared when the job's terminal event arrives. A no-op (with a hint) when
    /// no flow is active.
    async fn cmd_flow_close(&mut self) {
        let Some(session) = self.state.flow_session.clone() else {
            self.state.hint = format!("no flow running — {}", usage());
            return;
        };
        self.state.note(format!("closing flow {} …", session.name));
        self.send_flow(RuntimeCommand::FlowClose {
            flow_id: session.flow_id,
        })
        .await;
    }

    /// Send a flow command, surfacing a transport error as a red notice (mirrors
    /// `Shell::send`, kept here so the flow handlers don't reach into `super`).
    async fn send_flow(&mut self, command: RuntimeCommand) {
        if let Err(err) = self.client.start_job(command, None).await {
            self.state.push_notice(Tone::Error, err.to_string());
        }
    }
}

/// Load + parse a `WorkflowDef` from a JSON file (the `--file` escape hatch). A
/// read/parse failure is mapped to a user-facing hint.
fn load_workflow_file(path: &str) -> Result<WorkflowDef, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("can't read {path}: {err}"))?;
    serde_json::from_str(&text).map_err(|err| format!("invalid workflow JSON in {path}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::{FlowSource, Strategy};

    #[test]
    fn load_workflow_file_reads_and_parses_json() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("nerve-tui-flow-{}.json", std::process::id()));
        let json = serde_json::json!({
            "schema_version": 1,
            "name": "from-file",
            "strategy": {
                "type": "single",
                "step": { "worker": { "kind": "cli", "name": "claude" }, "task": "do it" }
            }
        });
        fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let def = load_workflow_file(path.to_str().unwrap()).expect("loaded");
        assert_eq!(def.name, "from-file");
        assert!(matches!(def.strategy, Strategy::Single { .. }));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn load_workflow_file_surfaces_missing_and_bad_json() {
        let err = load_workflow_file("/nonexistent/path/wf.json").expect_err("missing");
        assert!(err.contains("can't read"), "{err}");

        let dir = std::env::temp_dir();
        let path = dir.join(format!("nerve-tui-flow-bad-{}.json", std::process::id()));
        fs::write(&path, "{ not json").unwrap();
        let err = load_workflow_file(path.to_str().unwrap()).expect_err("bad json");
        assert!(err.contains("invalid workflow JSON"), "{err}");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn file_spec_into_command_uses_loader() {
        // The full path the `/flow --file` handler takes, exercised purely.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("nerve-tui-flow-cmd-{}.json", std::process::id()));
        let json = serde_json::json!({
            "schema_version": 1,
            "name": "x",
            "strategy": {
                "type": "parallel",
                "branches": [
                    { "worker": { "kind": "cli", "name": "claude" }, "task": "a" }
                ],
                "join": { "kind": "all" }
            }
        });
        fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let spec = FlowSpec::File(path.to_str().unwrap().to_string());
        let command = spec.into_command(load_workflow_file).expect("command");
        match command {
            RuntimeCommand::FlowStart { workflow, .. } => match workflow {
                FlowSource::Inline { workflow } => {
                    assert!(matches!(workflow.strategy, Strategy::Parallel { .. }));
                }
                FlowSource::Named { .. } => panic!("expected inline workflow"),
            },
            other => panic!("expected FlowStart, got {}", other.name()),
        }
        let _ = fs::remove_file(&path);
    }
}
