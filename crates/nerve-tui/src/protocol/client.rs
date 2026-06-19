//! The runtime-protocol client: spawn `nerve daemon --stdio`, speak JSON-RPC
//! 2.0 over NDJSON, and surface `runtime/event` notifications on a broadcast
//! channel. The async sibling of `packages/tui/src/backend/nerveClient.ts`.
//!
//! Concurrency model (all `tokio`):
//! - one **reader task** owns the child's stdout, parses each line, and either
//!   resolves a pending request (id → `oneshot`) or broadcasts an event;
//! - requests write one NDJSON line to the child's stdin under a `Mutex` and
//!   register a `oneshot` in the shared pending map keyed by request id;
//! - a **stderr drain** keeps captured stderr for richer error messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow};
use nerve_runtime::protocol::{
    RUNTIME_EVENT_METHOD, RUNTIME_INFO_METHOD, RUNTIME_JOB_CANCEL_METHOD, RUNTIME_JOB_GET_METHOD,
    RUNTIME_JOB_LIST_METHOD, RUNTIME_JOB_START_METHOD, RUNTIME_TOOLS_LIST_METHOD,
    RuntimeEventNotification, RuntimeInfo,
};
use nerve_runtime::{
    RuntimeCommand, RuntimeEvent, RuntimeJobSnapshot, RuntimeJobStatus, RuntimeToolSpec,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, broadcast, oneshot};

use super::envelope::{Inbound, RpcRequest, RpcResult, id_key, parse_inbound};
use super::handshake::validate_runtime_info;
use super::spawn::DaemonSpec;

/// Capacity of the broadcast channel that fans events out to subscribers. Large
/// enough that a slow consumer lags (gets `RecvError::Lagged`) rather than the
/// reader blocking; the shell only needs the latest, so lag is recoverable.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<RpcResult>>>>;

/// A connected runtime-protocol client. Cheap to clone (shared internals).
#[derive(Clone)]
pub struct NerveClient {
    inner: Arc<Inner>,
}

struct Inner {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: PendingMap,
    events: broadcast::Sender<RuntimeEvent>,
    next_id: AtomicU64,
    next_job_id: AtomicU64,
}

impl NerveClient {
    /// Spawn the daemon, start the read loop, and run the `runtime/info`
    /// handshake. On a handshake mismatch the child is killed before returning.
    pub async fn connect(spec: DaemonSpec) -> Result<(Self, broadcast::Receiver<RuntimeEvent>)> {
        let mut command = spec.command();
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn daemon `{}`", spec.binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("daemon stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("daemon stdout missing"))?;
        let stderr = child.stderr.take();

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (events, receiver) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        spawn_reader(stdout, Arc::clone(&pending), events.clone());
        if let Some(stderr) = stderr {
            spawn_stderr_drain(stderr);
        }

        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending,
            events,
            next_id: AtomicU64::new(1),
            next_job_id: AtomicU64::new(1),
        });
        let client = Self { inner };
        match client
            .info()
            .await
            .and_then(|info| validate_runtime_info(&info).map(|()| info))
        {
            Ok(_) => Ok((client, receiver)),
            Err(err) => {
                client.shutdown().await;
                Err(err)
            }
        }
    }

    /// Subscribe to runtime events (each subscriber sees every event from the
    /// moment of subscription). The receiver from [`Self::connect`] is the first.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.inner.events.subscribe()
    }

    /// `runtime/info` handshake payload.
    pub async fn info(&self) -> Result<RuntimeInfo> {
        let value = self.request(RUNTIME_INFO_METHOD, None).await?;
        serde_json::from_value(value).context("malformed runtime/info response")
    }

    /// `runtime/tools/list` — the runtime tool specs.
    pub async fn list_tools(&self) -> Result<Vec<RuntimeToolSpec>> {
        let value = self.request(RUNTIME_TOOLS_LIST_METHOD, None).await?;
        let tools = value.get("tools").cloned().unwrap_or(Value::Null);
        if tools.is_null() {
            return Ok(Vec::new());
        }
        serde_json::from_value(tools).context("malformed runtime/tools/list response")
    }

    /// `runtime/jobs/start` — enqueue a command as a job. A `job_id` is minted
    /// when not supplied so the caller can correlate terminal events.
    pub async fn start_job(
        &self,
        command: RuntimeCommand,
        job_id: Option<String>,
    ) -> Result<RuntimeJobSnapshot> {
        let job_id = job_id.unwrap_or_else(|| self.mint_job_id());
        let params = json!({ "job_id": job_id, "command": command });
        let value = self.request(RUNTIME_JOB_START_METHOD, Some(params)).await?;
        job_from_response(value, "jobs/start")
    }

    /// `runtime/jobs/get` — fetch one job snapshot (with its result by default).
    pub async fn get_job(&self, job_id: &str, include_result: bool) -> Result<RuntimeJobSnapshot> {
        let params = json!({ "job_id": job_id, "include_result": include_result });
        let value = self.request(RUNTIME_JOB_GET_METHOD, Some(params)).await?;
        job_from_response(value, "jobs/get")
    }

    /// `runtime/jobs/list` — list job snapshots.
    pub async fn list_jobs(
        &self,
        include_terminal: bool,
        include_results: bool,
        limit: usize,
    ) -> Result<Vec<RuntimeJobSnapshot>> {
        let params = json!({
            "include_terminal": include_terminal,
            "include_results": include_results,
            "limit": limit,
        });
        let value = self.request(RUNTIME_JOB_LIST_METHOD, Some(params)).await?;
        let jobs = value.get("jobs").cloned().unwrap_or(Value::Null);
        if jobs.is_null() {
            return Ok(Vec::new());
        }
        serde_json::from_value(jobs).context("malformed runtime/jobs/list response")
    }

    /// `runtime/jobs/cancel` — request cancellation of a job.
    pub async fn cancel_job(&self, job_id: &str) -> Result<RuntimeJobSnapshot> {
        let params = json!({ "job_id": job_id });
        let value = self
            .request(RUNTIME_JOB_CANCEL_METHOD, Some(params))
            .await?;
        job_from_response(value, "jobs/cancel")
    }

    /// Start a command and await its terminal event, then return the final
    /// snapshot's `result`. Mirrors the TS `runJob`: subscribe *before* starting
    /// so we never miss the terminal event, then poll `get` for the result.
    pub async fn run_job(&self, command: RuntimeCommand, job_id: Option<String>) -> Result<Value> {
        let job_id = job_id.unwrap_or_else(|| self.mint_job_id());
        let mut events = self.subscribe();
        self.start_job(command, Some(job_id.clone())).await?;
        wait_for_terminal_event(&mut events, &job_id).await?;
        let job = self.get_job(&job_id, true).await?;
        match job.status {
            RuntimeJobStatus::Completed => Ok(job.result.unwrap_or(Value::Null)),
            other => Err(anyhow!(
                "runtime job {job_id} ended {other:?}: {}",
                job.error
                    .map_or_else(|| "no error".to_string(), |e| e.message)
            )),
        }
    }

    /// Terminate the daemon. Drops pending waiters and kills the child.
    pub async fn shutdown(&self) {
        let _ = self.inner.stdin.lock().await.shutdown().await;
        let mut child = self.inner.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
        self.inner.pending.lock().await.clear();
    }

    fn mint_job_id(&self) -> String {
        let n = self.inner.next_job_id.fetch_add(1, Ordering::Relaxed);
        format!("tui-job-{n}")
    }

    /// Issue one request and await its response. Registers a `oneshot` keyed by a
    /// fresh id, writes the NDJSON line, then awaits the reader's resolution.
    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest::new(id, method, params);
        let line = serde_json::to_string(&request).context("failed to encode request")?;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id.to_string(), tx);
        {
            let mut stdin = self.inner.stdin.lock().await;
            if let Err(err) = write_line(&mut stdin, &line).await {
                self.inner.pending.lock().await.remove(&id.to_string());
                return Err(err);
            }
        }
        match rx.await {
            Ok(RpcResult::Ok(value)) => Ok(value),
            Ok(RpcResult::Err(err)) => Err(anyhow!("{}: {err}", method)),
            Err(_) => Err(anyhow!("daemon closed before responding to {method}")),
        }
    }
}

async fn write_line(stdin: &mut ChildStdin, line: &str) -> Result<()> {
    stdin
        .write_all(line.as_bytes())
        .await
        .context("write to daemon")?;
    stdin
        .write_all(b"\n")
        .await
        .context("write newline to daemon")?;
    stdin.flush().await.context("flush daemon stdin")
}

fn job_from_response(value: Value, context: &str) -> Result<RuntimeJobSnapshot> {
    let job = value
        .get("job")
        .cloned()
        .ok_or_else(|| anyhow!("missing `job` in {context} response: {value}"))?;
    serde_json::from_value(job).with_context(|| format!("malformed runtime job in {context}"))
}

/// Read the daemon's stdout line by line, dispatching responses and events.
fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    pending: PendingMap,
    events: broadcast::Sender<RuntimeEvent>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match parse_inbound(&line) {
                Ok(Some(inbound)) => dispatch(inbound, &pending, &events).await,
                Ok(None) => {}
                Err(_err) => {
                    // Malformed line: skip it. The TS client treats this as
                    // fatal, but a lenient reader keeps a stray log line from
                    // taking the session down.
                }
            }
        }
        // Stream closed: drop every pending waiter so callers see an error
        // rather than hanging forever.
        pending.lock().await.clear();
    });
}

async fn dispatch(
    inbound: Inbound,
    pending: &PendingMap,
    events: &broadcast::Sender<RuntimeEvent>,
) {
    match inbound {
        Inbound::Response { id, payload } => {
            if let Some(tx) = pending.lock().await.remove(&id_key(&id)) {
                let _ = tx.send(payload);
            }
        }
        Inbound::Notification { method, params } => {
            if method == RUNTIME_EVENT_METHOD
                && let Some(event) = parse_event(&params)
            {
                let _ = events.send(event);
            }
        }
    }
}

/// Parse a `runtime/event` `params` payload into a [`RuntimeEvent`]. Handles both
/// the `{eventSeq, ...event}` carrier and a bare event object (the carrier
/// flattens the event, and `event_seq` defaults to 0, so the notification type
/// deserializes a bare event too).
fn parse_event(params: &Value) -> Option<RuntimeEvent> {
    serde_json::from_value::<RuntimeEventNotification>(params.clone())
        .map(|notification| notification.event)
        .ok()
}

/// Drain the child's stderr so the pipe never fills (which would deadlock the
/// daemon). T1 discards it; a later wave can surface it on errors.
fn spawn_stderr_drain(stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(_line)) = lines.next_line().await {}
    });
}

/// Wait for a terminal job event (`job_completed`/`job_failed`/`job_cancelled`)
/// for `job_id`. Returns `Ok(())` on any terminal event; the caller inspects the
/// final status via `get_job`. `Lagged` is tolerated (we keep listening).
async fn wait_for_terminal_event(
    events: &mut broadcast::Receiver<RuntimeEvent>,
    job_id: &str,
) -> Result<()> {
    loop {
        match events.recv().await {
            Ok(event) => {
                if let Some(id) = terminal_job_id(&event)
                    && id == job_id
                {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => {
                return Err(anyhow!(
                    "daemon event stream closed before job {job_id} finished"
                ));
            }
        }
    }
}

/// The job id of a terminal job event, or `None` for non-terminal events.
fn terminal_job_id(event: &RuntimeEvent) -> Option<&str> {
    match event {
        RuntimeEvent::JobCompleted { job_id }
        | RuntimeEvent::JobFailed { job_id, .. }
        | RuntimeEvent::JobCancelled { job_id } => Some(job_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nerve_runtime::RuntimeJobError;

    #[test]
    fn parse_event_handles_carrier_shape() {
        let params = json!({
            "eventSeq": 4,
            "type": "job_completed",
            "job_id": "j1",
        });
        let event = parse_event(&params).expect("event");
        assert_eq!(terminal_job_id(&event), Some("j1"));
    }

    #[test]
    fn parse_event_handles_bare_event() {
        let params = json!({ "type": "job_completed", "job_id": "bare" });
        let event = parse_event(&params).expect("event");
        assert_eq!(terminal_job_id(&event), Some("bare"));
    }

    #[test]
    fn terminal_job_id_only_for_terminal_events() {
        assert_eq!(
            terminal_job_id(&RuntimeEvent::JobCompleted { job_id: "a".into() }),
            Some("a")
        );
        assert_eq!(
            terminal_job_id(&RuntimeEvent::JobFailed {
                job_id: "b".into(),
                error: RuntimeJobError::new("k", "m"),
            }),
            Some("b")
        );
        assert_eq!(
            terminal_job_id(&RuntimeEvent::JobStarted {
                job_id: "c".into(),
                command: "ping".into(),
                tool_name: None,
            }),
            None
        );
    }

    #[tokio::test]
    async fn wait_for_terminal_event_resolves_on_matching_job() {
        let (tx, _rx0) = broadcast::channel(16);
        let mut rx = tx.subscribe();
        tx.send(RuntimeEvent::JobStarted {
            job_id: "other".into(),
            command: "ping".into(),
            tool_name: None,
        })
        .unwrap();
        tx.send(RuntimeEvent::JobCompleted {
            job_id: "mine".into(),
        })
        .unwrap();
        wait_for_terminal_event(&mut rx, "mine")
            .await
            .expect("resolves");
    }

    #[tokio::test]
    async fn wait_for_terminal_event_errors_when_closed() {
        let (tx, _rx0) = broadcast::channel(16);
        let mut rx = tx.subscribe();
        drop(tx);
        let err = wait_for_terminal_event(&mut rx, "mine")
            .await
            .expect_err("closed");
        assert!(err.to_string().contains("closed"));
    }
}
