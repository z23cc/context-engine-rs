//! HTTP transport for the runtime daemon — a *new transport for the existing
//! protocol*, never a new protocol (architecture north star §3/§6/§8: "a new
//! client surface reuses the versioned runtime protocol, never a bespoke RPC";
//! "the GUI transport reuses the transport-neutral router"). It adds **no**
//! `RuntimeCommand` / `RuntimeEvent` / method — it drives the same
//! [`RuntimeDaemonRouter`] the stdio transport does, so a browser GUI speaks
//! the identical Protocol v3.
//!
//! Endpoints (bound to loopback by default):
//!   * `POST /rpc`    — one JSON-RPC request in, its JSON-RPC response out.
//!   * `GET  /events` — Server-Sent Events stream of `runtime/event` notifications.
//!   * `GET  /`       — the embedded single-page GUI ([`GUI_HTML`], `gui.html`),
//!     a client of this same Protocol v3 (it only ever talks to `/rpc` + `/events`).
//!
//! Event fan-out: the router is handed a single notification sink that feeds an
//! [`SseHub`]; every open `/events` connection registers an mpsc channel and
//! writes each notification as a `data: <json>\n\n` SSE frame.

use super::router::RuntimeDaemonRouter;
use crate::rpc::{RpcMessage, jsonrpc_error};
use crate::workspace;
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Idle interval after which an open SSE connection emits a keep-alive comment.
/// Also bounds how quickly a vanished client is noticed (its next write fails).
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

/// SSE keep-alive comment frame: ignored by `EventSource`, keeps the socket hot.
const SSE_KEEPALIVE: &str = ": keep-alive\n\n";

/// The self-contained runtime GUI, embedded into the binary so the daemon serves
/// it with no build step or external assets. It is a *client* of Protocol v3:
/// it only POSTs `/rpc` and subscribes to `/events`, adding no new transport.
const GUI_HTML: &str = include_str!("gui.html");

/// Run the daemon HTTP transport, blocking until the process exits.
pub(super) fn run_http(serve_args: workspace::ServeArgs, addr: SocketAddr) -> Result<()> {
    let hub = Arc::new(SseHub::new());
    let broadcast = Arc::clone(&hub);
    let router = super::setup::build_router(&serve_args, move |value| broadcast.broadcast(value))?;
    let ctx = Arc::new(HttpContext { router, hub });
    let server = Server::http(addr)
        .map_err(|err| anyhow!("failed to bind daemon HTTP transport on {addr}: {err}"))?;
    eprintln!("nerve daemon: HTTP transport on http://{addr} (POST /rpc, GET /events)");
    for request in server.incoming_requests() {
        let ctx = Arc::clone(&ctx);
        // Thread-per-request so long-lived `/events` streams never block the
        // accept loop or concurrent `/rpc` calls.
        std::thread::spawn(move || {
            if let Err(err) = handle_request(&ctx, request) {
                eprintln!("nerve daemon: HTTP request error: {err}");
            }
        });
    }
    Ok(())
}

/// Shared, thread-safe state handed to every request thread.
struct HttpContext {
    router: RuntimeDaemonRouter,
    hub: Arc<SseHub>,
}

fn handle_request(ctx: &HttpContext, request: Request) -> Result<()> {
    let method = request.method().clone();
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    match (&method, path.as_str()) {
        (Method::Post, "/rpc") => handle_rpc(&ctx.router, request),
        (Method::Get, "/events") => handle_events(&ctx.hub, request),
        (Method::Options, _) => respond_preflight(request),
        (Method::Get, "/") => respond_html(request, GUI_HTML),
        _ => respond_text(request, 404, "not found"),
    }
}

// ---- POST /rpc ------------------------------------------------------------

fn handle_rpc(router: &RuntimeDaemonRouter, mut request: Request) -> Result<()> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|err| anyhow!("failed to read /rpc body: {err}"))?;
    match route_rpc_body(router, &body) {
        Some(response) => respond_json(request, 200, &response),
        None => respond_text(request, 204, ""),
    }
}

/// Route one JSON-RPC request body through the shared router and return the
/// response to send (or `None` for a notification that produces no response).
/// Pure over `(router, body)`, hence unit-testable without a socket.
fn route_rpc_body(router: &RuntimeDaemonRouter, body: &str) -> Option<Value> {
    let message: RpcMessage = match serde_json::from_str(body) {
        Ok(message) => message,
        Err(err) => return Some(jsonrpc_error(Value::Null, -32700, err.to_string())),
    };
    let mut responses = Vec::new();
    if let Err(err) = router.handle_message(message, |value| {
        responses.push(value);
        Ok(())
    }) {
        return Some(jsonrpc_error(Value::Null, -32603, err.to_string()));
    }
    responses.into_iter().next()
}

// ---- GET /events (SSE) ----------------------------------------------------

fn handle_events(hub: &Arc<SseHub>, request: Request) -> Result<()> {
    let (id, receiver) = hub.subscribe();
    let mut writer = request.into_writer();
    let result = stream_events(&mut writer, &receiver);
    hub.unsubscribe(id);
    result
}

/// Write the SSE response head, then forward each broadcast notification as a
/// `data:` frame until the client disconnects (a failed write) or the hub stops.
fn stream_events(writer: &mut dyn Write, receiver: &Receiver<Arc<str>>) -> Result<()> {
    if write_sse_head(writer).is_err() {
        return Ok(());
    }
    loop {
        let frame = match receiver.recv_timeout(SSE_HEARTBEAT) {
            Ok(json) => sse_data_frame(&json),
            Err(RecvTimeoutError::Timeout) => SSE_KEEPALIVE.to_string(),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if write_sse_chunk(writer, frame.as_bytes()).is_err() {
            break; // client went away
        }
    }
    Ok(())
}

/// Format a runtime notification as an SSE `data:` event. The JSON is a single
/// line (serde never emits raw newlines), so one `data:` line is always valid.
fn sse_data_frame(json: &str) -> String {
    format!("data: {json}\n\n")
}

fn write_sse_head(writer: &mut dyn Write) -> std::io::Result<()> {
    writer.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Connection: keep-alive\r\n\
          Access-Control-Allow-Origin: *\r\n\
          Transfer-Encoding: chunked\r\n\r\n",
    )?;
    writer.flush()
}

/// Write one HTTP/1.1 chunked-transfer chunk and flush, so the browser's
/// `EventSource` receives the frame immediately rather than buffered.
fn write_sse_chunk(writer: &mut dyn Write, payload: &[u8]) -> std::io::Result<()> {
    write!(writer, "{:X}\r\n", payload.len())?;
    writer.write_all(payload)?;
    writer.write_all(b"\r\n")?;
    writer.flush()
}

// ---- SSE broadcast hub ----------------------------------------------------

/// A minimal broadcast hub: the router's single notification sink fans out to
/// every open `/events` subscriber. It lives in the transport (not in
/// `nerve-runtime`) so the protocol vocabulary stays untouched.
struct SseHub {
    subscribers: Mutex<Vec<Subscriber>>,
    next_id: AtomicU64,
}

struct Subscriber {
    id: u64,
    sender: mpsc::Sender<Arc<str>>,
}

impl SseHub {
    fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a subscriber, returning its id and the receiver its `/events`
    /// connection drains.
    fn subscribe(&self) -> (u64, Receiver<Arc<str>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel();
        self.subscribers
            .lock()
            .expect("sse hub lock")
            .push(Subscriber { id, sender });
        (id, receiver)
    }

    fn unsubscribe(&self, id: u64) {
        self.subscribers
            .lock()
            .expect("sse hub lock")
            .retain(|subscriber| subscriber.id != id);
    }

    /// Fan a runtime notification out to all live subscribers, dropping any
    /// whose receiver has hung up.
    fn broadcast(&self, value: Value) {
        let frame: Arc<str> = Arc::from(value.to_string());
        self.subscribers
            .lock()
            .expect("sse hub lock")
            .retain(|subscriber| subscriber.sender.send(Arc::clone(&frame)).is_ok());
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.subscribers.lock().expect("sse hub lock").len()
    }
}

// ---- HTTP responses -------------------------------------------------------

fn respond_json(request: Request, status: u16, value: &Value) -> Result<()> {
    let body =
        serde_json::to_string(value).map_err(|err| anyhow!("encode /rpc response: {err}"))?;
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(static_header("Content-Type", "application/json"))
        .with_header(static_header("Access-Control-Allow-Origin", "*"));
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write /rpc response: {err}"))
}

fn respond_html(request: Request, html: &str) -> Result<()> {
    let response = Response::from_string(html.to_string())
        .with_status_code(StatusCode(200))
        .with_header(static_header("Content-Type", "text/html; charset=utf-8"))
        .with_header(static_header("Access-Control-Allow-Origin", "*"));
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write GUI response: {err}"))
}

fn respond_text(request: Request, status: u16, message: &str) -> Result<()> {
    let response = Response::from_string(message.to_string())
        .with_status_code(StatusCode(status))
        .with_header(static_header("Content-Type", "text/plain; charset=utf-8"))
        .with_header(static_header("Access-Control-Allow-Origin", "*"));
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write response: {err}"))
}

fn respond_preflight(request: Request) -> Result<()> {
    let response = Response::empty(StatusCode(204))
        .with_header(static_header("Access-Control-Allow-Origin", "*"))
        .with_header(static_header(
            "Access-Control-Allow-Methods",
            "GET, POST, OPTIONS",
        ))
        .with_header(static_header(
            "Access-Control-Allow-Headers",
            "Content-Type",
        ));
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write preflight response: {err}"))
}

fn static_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static HTTP header is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use crate::providers::ProviderRegistry;
    use crate::tools;
    use crate::workspace::{args_with, registry};
    use serde_json::json;
    use std::fs;

    fn test_router() -> (tempfile::TempDir, RuntimeDaemonRouter) {
        let root = tempfile::tempdir().expect("tempdir");
        fs::write(root.path().join("notes.txt"), "alpha\n").expect("write notes");
        let runtime = tools::runtime(
            registry(&args_with(vec![root.path().to_path_buf()], Vec::new())).expect("registry"),
        );
        let router = RuntimeDaemonRouter::new(
            Arc::new(runtime),
            ProviderRegistry::default(),
            Policy::default(),
            None,
            |_| {},
        );
        (root, router)
    }

    #[test]
    fn route_rpc_body_returns_runtime_info() {
        let (_root, router) = test_router();
        let response = route_rpc_body(
            &router,
            r#"{"jsonrpc":"2.0","id":1,"method":"runtime/info"}"#,
        )
        .expect("runtime/info response");
        assert_eq!(response["result"]["protocol"], "nerve-runtime");
        assert_eq!(response["id"], json!(1));
    }

    #[test]
    fn route_rpc_body_lists_tools() {
        let (_root, router) = test_router();
        let response = route_rpc_body(
            &router,
            r#"{"jsonrpc":"2.0","id":2,"method":"runtime/tools/list"}"#,
        )
        .expect("tools/list response");
        assert!(
            response["result"]["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty())
        );
    }

    #[test]
    fn gui_html_is_embedded_and_targets_runtime_endpoints() {
        // The GUI is a client of Protocol v3: it must talk to `/rpc` + `/events`
        // only, and drive agent runs through the existing job methods.
        assert!(GUI_HTML.contains("<!doctype html"));
        assert!(GUI_HTML.contains("/rpc"));
        assert!(GUI_HTML.contains("/events"));
        assert!(GUI_HTML.contains("runtime/info"));
        assert!(GUI_HTML.contains("runtime/tools/list"));
        assert!(GUI_HTML.contains("runtime/jobs/start"));
        assert!(GUI_HTML.contains("runtime/jobs/get"));
        assert!(GUI_HTML.contains("runtime/jobs/cancel"));
        assert!(GUI_HTML.contains("agent.run"));
    }

    #[test]
    fn gui_html_drives_the_session_chat_surface() {
        // S3: the primary surface is a multi-turn chat over the `session.*`
        // command family, including the approval round-trip. All of this rides
        // the same Protocol v3 jobs/events — no new transport or method.
        assert!(GUI_HTML.contains("session.start"));
        assert!(GUI_HTML.contains("session.message"));
        assert!(GUI_HTML.contains("session.respond"));
        assert!(GUI_HTML.contains("session.interrupt"));
        assert!(GUI_HTML.contains("approval_requested"));
    }

    #[test]
    fn route_rpc_body_reports_parse_error_with_null_id() {
        let (_root, router) = test_router();
        let response = route_rpc_body(&router, "not json").expect("parse-error response");
        assert_eq!(response["error"]["code"], json!(-32700));
        assert_eq!(response["id"], Value::Null);
    }

    #[test]
    fn route_rpc_body_has_no_response_for_notification() {
        let (_root, router) = test_router();
        // No `id` => JSON-RPC notification => the router emits no response.
        let response = route_rpc_body(&router, r#"{"jsonrpc":"2.0","method":"runtime/info"}"#);
        assert!(response.is_none());
    }

    #[test]
    fn sse_data_frame_wraps_json_payload() {
        assert_eq!(sse_data_frame(r#"{"a":1}"#), "data: {\"a\":1}\n\n");
    }

    #[test]
    fn write_sse_chunk_emits_hex_length_framing() {
        let mut buf = Vec::new();
        write_sse_chunk(&mut buf, b"data: x\n\n").expect("write chunk");
        // 9-byte payload => hex length `9`, CRLF-delimited chunk.
        assert_eq!(
            String::from_utf8(buf).expect("utf8"),
            "9\r\ndata: x\n\n\r\n"
        );
    }

    #[test]
    fn write_sse_head_announces_event_stream() {
        let mut buf = Vec::new();
        write_sse_head(&mut buf).expect("write head");
        let head = String::from_utf8(buf).expect("utf8");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Type: text/event-stream\r\n"));
        assert!(head.contains("Transfer-Encoding: chunked\r\n"));
        assert!(head.ends_with("\r\n\r\n"));
    }

    #[test]
    fn hub_broadcasts_to_live_subscribers_and_prunes_dead_ones() {
        let hub = SseHub::new();
        let (_id_a, rx_a) = hub.subscribe();
        let (_id_b, rx_b) = hub.subscribe();
        assert_eq!(hub.subscriber_count(), 2);
        drop(rx_b); // a disconnected client
        hub.broadcast(json!({ "method": "runtime/event", "params": { "type": "ping" } }));
        let frame = rx_a.recv().expect("frame");
        assert!(frame.contains("runtime/event"));
        // The dead subscriber was pruned on its failed send.
        assert_eq!(hub.subscriber_count(), 1);
    }
}
