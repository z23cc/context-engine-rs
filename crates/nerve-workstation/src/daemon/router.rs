use crate::jobs::{JobError, JobManager};
use crate::policy::Policy;
use crate::providers::ProviderRegistry;
use crate::rpc::{RpcMessage, jsonrpc_error, jsonrpc_result};
use crate::session::SessionStore;
use crate::tools;
use anyhow::Result;
use nerve_runtime::{
    RuntimeEvent, RuntimeJobCancelRequest, RuntimeJobGetRequest, RuntimeJobListRequest,
    RuntimeJobStartRequest,
    protocol::{
        RUNTIME_DAEMON_NAME, RUNTIME_EVENT_METHOD, RUNTIME_INFO_METHOD, RUNTIME_JOB_CANCEL_METHOD,
        RUNTIME_JOB_GET_METHOD, RUNTIME_JOB_LIST_METHOD, RUNTIME_JOB_START_METHOD,
        RUNTIME_TOOLS_LIST_METHOD, RuntimeEventNotification, RuntimeInfo,
    },
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

pub(super) struct RuntimeDaemonRouter {
    jobs: Arc<JobManager>,
}

impl RuntimeDaemonRouter {
    pub(super) fn new(
        runtime: Arc<tools::NerveRuntime>,
        registry: ProviderRegistry,
        policy: Policy,
        session_store: Option<SessionStore>,
        allow_delegate: bool,
        delegate_launcher: Arc<dyn crate::sandbox::SandboxLauncher>,
        emit_notification: impl Fn(Value) + Send + Sync + 'static,
    ) -> Self {
        let jobs = Arc::new(JobManager::with_delegate_launcher(
            runtime,
            registry,
            policy,
            session_store,
            allow_delegate,
            delegate_launcher,
            sequenced_emitter(emit_notification),
        ));
        Self { jobs }
    }

    #[cfg(test)]
    pub(super) fn with_jobs(jobs: Arc<JobManager>) -> Self {
        Self { jobs }
    }

    pub(super) fn handle_message(
        &self,
        message: RpcMessage,
        mut emit: impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let response_id = message.id.clone();
        match message.method.as_str() {
            "initialize" | RUNTIME_INFO_METHOD => {
                emit_response(response_id, runtime_info, &mut emit)
            }
            RUNTIME_TOOLS_LIST_METHOD => emit_response(
                response_id,
                || json!({ "tools": self.jobs.runtime().tool_specs() }),
                &mut emit,
            ),
            RUNTIME_JOB_START_METHOD => {
                self.handle_job_start(response_id, message.params, &mut emit)
            }
            RUNTIME_JOB_GET_METHOD => self.handle_job_get(response_id, message.params, &mut emit),
            RUNTIME_JOB_LIST_METHOD => self.handle_job_list(response_id, message.params, &mut emit),
            RUNTIME_JOB_CANCEL_METHOD => {
                self.handle_job_cancel(response_id, message.params, &mut emit)
            }
            _ => emit_error(response_id, -32601, "method not found", &mut emit),
        }
    }

    fn handle_job_start(
        &self,
        response_id: Option<Value>,
        params: Value,
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let request = match serde_json::from_value::<RuntimeJobStartRequest>(params) {
            Ok(request) => request,
            Err(err) => return emit_error(response_id, -32602, err.to_string(), emit),
        };
        match self.jobs.start(request) {
            Ok(job) => emit_response_value(response_id, json!({ "job": job }), emit),
            Err(err) => emit_job_error(response_id, err, emit),
        }
    }

    fn handle_job_get(
        &self,
        response_id: Option<Value>,
        params: Value,
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let request = match serde_json::from_value::<RuntimeJobGetRequest>(params) {
            Ok(request) => request,
            Err(err) => return emit_error(response_id, -32602, err.to_string(), emit),
        };
        match self.jobs.get(request) {
            Ok(job) => emit_response_value(response_id, json!({ "job": job }), emit),
            Err(err) => emit_job_error(response_id, err, emit),
        }
    }

    fn handle_job_list(
        &self,
        response_id: Option<Value>,
        params: Value,
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let request = match serde_json::from_value::<RuntimeJobListRequest>(params) {
            Ok(request) => request,
            Err(err) => return emit_error(response_id, -32602, err.to_string(), emit),
        };
        emit_response_value(
            response_id,
            json!({ "jobs": self.jobs.list(request) }),
            emit,
        )
    }

    fn handle_job_cancel(
        &self,
        response_id: Option<Value>,
        params: Value,
        emit: &mut impl FnMut(Value) -> Result<()>,
    ) -> Result<()> {
        let request = match serde_json::from_value::<RuntimeJobCancelRequest>(params) {
            Ok(request) => request,
            Err(err) => return emit_error(response_id, -32602, err.to_string(), emit),
        };
        match self.jobs.cancel(&request.job_id) {
            Ok((cancellation_requested, job)) => emit_response_value(
                response_id,
                json!({ "cancellation_requested": cancellation_requested, "job": job }),
                emit,
            ),
            Err(err) => emit_job_error(response_id, err, emit),
        }
    }
}

fn emit_job_error(
    response_id: Option<Value>,
    error: JobError,
    emit: &mut impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    emit_error(response_id, error.code(), error.to_string(), emit)
}

fn emit_response(
    id: Option<Value>,
    result: impl FnOnce() -> Value,
    emit: &mut impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    if let Some(id) = id {
        emit(jsonrpc_result(id, result()))?;
    }
    Ok(())
}

fn emit_response_value(
    id: Option<Value>,
    result: Value,
    emit: &mut impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    emit_response(id, || result, emit)
}

fn emit_error(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
    emit: &mut impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    if let Some(id) = id {
        emit(jsonrpc_error(id, code, message))?;
    }
    Ok(())
}

/// Wrap a raw notification sink in a sequence-stamping emitter shared by both
/// daemon transports (HTTP/SSE and stdio).
///
/// The monotonic per-stream sequence is minted **and** the notification emitted
/// under one critical section (a `Mutex<u64>`, not an atomic), so seq order ==
/// persist/deliver order. Concurrent emitters — each job runs on its own thread,
/// `start`/`cancel` emit from the RPC thread, auth from its own thread — would
/// otherwise mint a seq and emit in separate sections, letting frames be
/// persisted/delivered out of seq order. That manifests as replay-ring eviction
/// inversion, duplicate replay, and a late low-seq frame permanently lost on
/// reconnect. Holding the gate across `emit` is safe: downstream sinks only do a
/// non-blocking mpsc send (SSE hub) or a buffered write (stdout). Seq starts at
/// 1; `0` is reserved as "before any event" for `last_seq=0` full replay.
fn sequenced_emitter(
    emit: impl Fn(Value) + Send + Sync + 'static,
) -> impl Fn(RuntimeEvent) + Send + Sync + 'static {
    let event_seq = Mutex::new(1u64);
    move |event| {
        let mut seq = crate::sync::lock_recover(&event_seq);
        let current = *seq;
        *seq += 1;
        emit(runtime_event_notification(current, event));
    }
}

pub(super) fn runtime_event_notification(event_seq: u64, event: RuntimeEvent) -> Value {
    // `event_seq` is the monotonic per-stream sequence assigned at emit time (see
    // `RuntimeDaemonRouter::new`). The carrier flattens the event, so `params`
    // stays backward compatible with clients reading the bare event; SSE clients
    // additionally use `event_seq` for ordering and `Last-Event-ID` replay.
    let params = RuntimeEventNotification::new(event_seq, event);
    json!({ "jsonrpc": "2.0", "method": RUNTIME_EVENT_METHOD, "params": params })
}

fn runtime_info() -> Value {
    json!(RuntimeInfo::current(
        RUNTIME_DAEMON_NAME,
        env!("CARGO_PKG_VERSION")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::thread;

    fn emitted_seq(value: &Value) -> u64 {
        value["params"]["eventSeq"].as_u64().expect("eventSeq")
    }

    #[test]
    fn sequenced_emitter_starts_at_one_and_is_monotonic() {
        let recorded: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        let emit = sequenced_emitter(move |value| {
            sink.lock().expect("sink").push(emitted_seq(&value));
        });
        for _ in 0..3 {
            emit(RuntimeEvent::JobCancelRequested {
                job_id: "j".to_string(),
            });
        }
        assert_eq!(*recorded.lock().expect("recorded"), vec![1, 2, 3]);
    }

    // The seq is minted AND the notification delivered under one critical
    // section, so the order frames land in the sink (delivery order) is exactly
    // the seq order. Were the seq minted separately from emit, concurrent
    // emitters could deliver a higher-seq frame before a lower-seq one — the
    // inversion this asserts against.
    #[test]
    fn concurrent_emit_keeps_delivery_order_equal_to_seq_order() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 200;
        let recorded: Arc<StdMutex<Vec<u64>>> = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&recorded);
        // The sink itself takes a lock, so the seq it sees is the delivery order;
        // the emitter's own lock spans mint+emit, so that order must match seq.
        let emit = Arc::new(sequenced_emitter(move |value| {
            sink.lock().expect("sink").push(emitted_seq(&value));
        }));

        thread::scope(|scope| {
            for _ in 0..THREADS {
                let emit = Arc::clone(&emit);
                scope.spawn(move || {
                    for _ in 0..PER_THREAD {
                        emit(RuntimeEvent::JobCancelRequested {
                            job_id: "j".to_string(),
                        });
                    }
                });
            }
        });

        let delivered = recorded.lock().expect("recorded").clone();
        assert_eq!(delivered.len(), THREADS * PER_THREAD);
        // Delivery order must be strictly increasing in seq (no inversion, no
        // duplicate), and cover exactly 1..=N with none lost.
        for window in delivered.windows(2) {
            assert!(
                window[0] < window[1],
                "seq inversion in delivery order: {} then {}",
                window[0],
                window[1]
            );
        }
        assert_eq!(*delivered.first().expect("first"), 1);
        assert_eq!(
            *delivered.last().expect("last"),
            (THREADS * PER_THREAD) as u64
        );
    }
}
