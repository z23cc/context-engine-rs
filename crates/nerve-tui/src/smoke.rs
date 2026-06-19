//! `nerve-tui smoke` — a no-LLM round-trip against `nerve daemon --stdio`.
//!
//! Mirrors `bun run tui:smoke` (`packages/tui/src/cli/smoke.ts`): connect +
//! handshake, start a `ping` job, await its terminal event, then assert the job
//! completed with `{ "status": "ok" }`. Used both as a CLI subcommand and by the
//! integration test.

use anyhow::{Result, anyhow};
use nerve_runtime::{RuntimeCommand, RuntimeJobStatus};
use serde_json::Value;
use std::path::PathBuf;

use crate::protocol::{DaemonSpec, NerveClient};

/// Outcome of a smoke round-trip, returned so the CLI/test can report it.
#[derive(Debug)]
pub struct SmokeReport {
    pub server_name: String,
    pub server_version: String,
    pub tools: usize,
    pub job_status: RuntimeJobStatus,
}

impl SmokeReport {
    /// The single-line pass marker the harness greps for.
    #[must_use]
    pub fn pass_line(&self) -> String {
        format!(
            "nerve-tui smoke: OK — {} v{} · {} tools · ping {:?}",
            self.server_name, self.server_version, self.tools, self.job_status
        )
    }
}

/// Run the smoke round-trip against a daemon spawned from `spec`.
pub async fn run_smoke(spec: DaemonSpec) -> Result<SmokeReport> {
    let (client, mut _events) = NerveClient::connect(spec).await?;
    let result = smoke_round_trip(&client).await;
    client.shutdown().await;
    result
}

async fn smoke_round_trip(client: &NerveClient) -> Result<SmokeReport> {
    let info = client.info().await?;
    let tools = client.list_tools().await?.len();
    let job_id = "smoke-ping".to_string();
    let result = client
        .run_job(RuntimeCommand::Ping, Some(job_id.clone()))
        .await?;
    assert_ping_ok(&result)?;
    let job = client.get_job(&job_id, true).await?;
    if job.status != RuntimeJobStatus::Completed {
        return Err(anyhow!("smoke ping job did not complete: {:?}", job.status));
    }
    Ok(SmokeReport {
        server_name: info.server_info.name,
        server_version: info.server_info.version,
        tools,
        job_status: job.status,
    })
}

fn assert_ping_ok(result: &Value) -> Result<()> {
    let status = result.get("status").and_then(Value::as_str);
    if status == Some("ok") {
        Ok(())
    } else {
        Err(anyhow!("ping result was not {{status: ok}}: {result}"))
    }
}

/// Build a [`DaemonSpec`] for the smoke command from CLI-ish inputs.
#[must_use]
pub fn smoke_spec(root: PathBuf, binary: Option<PathBuf>) -> DaemonSpec {
    let mut spec = DaemonSpec::new(root);
    if let Some(binary) = binary {
        spec = spec.with_binary(binary);
    }
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn assert_ping_ok_accepts_ok_status() {
        assert_ping_ok(&json!({ "status": "ok" })).expect("ok");
    }

    #[test]
    fn assert_ping_ok_rejects_other_status() {
        assert!(assert_ping_ok(&json!({ "status": "nope" })).is_err());
        assert!(assert_ping_ok(&json!({})).is_err());
    }

    #[test]
    fn pass_line_mentions_status_and_tools() {
        let report = SmokeReport {
            server_name: "nerve".to_string(),
            server_version: "0.0.0".to_string(),
            tools: 12,
            job_status: RuntimeJobStatus::Completed,
        };
        let line = report.pass_line();
        assert!(line.contains("OK"));
        assert!(line.contains("12 tools"));
        assert!(line.contains("Completed"));
    }
}
