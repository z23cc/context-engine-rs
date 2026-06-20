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
//!
//! Transport hardening ([`HttpSecurity`]) — this is a *transport guard*, it adds
//! no protocol vocabulary:
//!   * **Bearer token.** A per-run [`random_urlsafe`] token gates `POST /rpc` and
//!     `GET /events` (else `401`). On a loopback bind it is embedded into the
//!     served GUI so the local UI stays zero-config; on a remote bind it is
//!     supplied out-of-band via the URL fragment (never sent to the server).
//!   * **Origin allowlist.** CORS headers are echoed only for loopback origins —
//!     never the old `*` — so a foreign web page cannot read daemon responses.
//!   * **Host guard.** A loopback bind also requires a loopback `Host` header,
//!     defeating DNS-rebinding. A non-loopback bind is refused unless the
//!     operator passes `--http-allow-remote`, which keeps the token mandatory.

use super::router::RuntimeDaemonRouter;
use crate::rpc::{RpcMessage, jsonrpc_error};
use crate::workspace;
use anyhow::{Result, anyhow};
use nerve_agent::auth::oauth::random_urlsafe;
use serde_json::Value;
use std::collections::VecDeque;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Idle interval after which an open SSE connection emits a keep-alive comment.
/// Also bounds how quickly a vanished client is noticed (its next write fails).
const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

/// SSE keep-alive comment frame: ignored by `EventSource`, keeps the socket hot.
const SSE_KEEPALIVE: &str = ": keep-alive\n\n";

/// How many recent events the hub keeps for `Last-Event-ID` replay. Bounded: on
/// overflow the oldest frame is dropped, so a client that reconnects after a long
/// gap may have missed events older than this window (it then resyncs by polling
/// job/session state). Sized to cover a typical reconnect blip, not full history.
const SSE_REPLAY_CAPACITY: usize = 1024;

/// Per-subscriber outbound channel depth. The channel is bounded so a stalled
/// `/events` reader cannot make `broadcast` grow memory without limit; a
/// subscriber whose buffer fills is treated as a dead/slow client and pruned
/// (it can reconnect with `Last-Event-ID` to replay what it missed).
const SSE_SUBSCRIBER_CAPACITY: usize = 512;

/// The self-contained runtime GUI, embedded into the binary so the daemon serves
/// it with no build step or external assets. It is a *client* of Protocol v3:
/// it only POSTs `/rpc` and subscribes to `/events`, adding no new transport.
const GUI_HTML: &str = include_str!("gui.html");

/// Placeholder in [`GUI_HTML`] replaced with the per-run bearer token on a
/// loopback bind. Kept distinct from its bare substring so [`HttpSecurity::render_gui`]
/// and the GUI's own bootstrap can both tell "injected" from "not injected".
const GUI_TOKEN_PLACEHOLDER: &str = "__NERVE_DAEMON_TOKEN__";

/// Run the daemon HTTP transport, blocking until the process exits.
pub(super) fn run_http(
    serve_args: workspace::ServeArgs,
    addr: SocketAddr,
    allow_delegate: bool,
) -> Result<()> {
    let security = HttpSecurity::new(addr);
    let hub = Arc::new(SseHub::new());
    let broadcast = Arc::clone(&hub);
    let router = super::setup::build_router(&serve_args, allow_delegate, move |value| {
        broadcast.broadcast(value)
    })?;
    let ctx = Arc::new(HttpContext {
        router,
        hub,
        security,
    });
    let server = Server::http(addr)
        .map_err(|err| anyhow!("failed to bind daemon HTTP transport on {addr}: {err}"))?;
    eprintln!("nerve daemon: HTTP transport on http://{addr} (POST /rpc, GET /events)");
    announce_access(&ctx.security, addr);
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

/// Print the bearer token and how to reach the GUI. The token always goes to
/// stderr (the operator's own terminal) so non-browser clients can authenticate;
/// on a remote bind it is *not* embedded in the page, so the operator must open
/// the printed `#token=` URL.
fn announce_access(security: &HttpSecurity, addr: SocketAddr) {
    if security.enforce_loopback_host {
        eprintln!("nerve daemon: loopback-only; GUI token embedded at GET /");
    } else {
        eprintln!(
            "nerve daemon: REMOTE bind — token required; open http://{addr}/#token={}",
            security.token
        );
    }
    eprintln!("nerve daemon: auth token: {}", security.token);
}

/// Shared, thread-safe state handed to every request thread.
struct HttpContext {
    router: RuntimeDaemonRouter,
    hub: Arc<SseHub>,
    security: HttpSecurity,
}

/// Per-run transport hardening for the HTTP daemon. See the module docs.
pub(super) struct HttpSecurity {
    /// Bearer token required on `/rpc` and `/events`, fresh each run.
    token: String,
    /// True for a loopback bind: also require a loopback `Host` header (DNS
    /// rebinding defense) and embed the token into the served GUI.
    enforce_loopback_host: bool,
}

impl HttpSecurity {
    fn new(addr: SocketAddr) -> Self {
        Self {
            token: random_urlsafe(32),
            enforce_loopback_host: addr.ip().is_loopback(),
        }
    }

    /// The token to bake into a served page, or `None` on a remote bind (where
    /// the page must never carry it — the operator supplies it out-of-band).
    /// Used by the `/app` Leptos surface, mirroring [`Self::render_gui`].
    pub(super) fn embed_token(&self) -> Option<&str> {
        self.enforce_loopback_host.then_some(self.token.as_str())
    }

    /// Whether a request carries the correct token, via `Authorization: Bearer`
    /// (used by `/rpc` fetch) or a `?token=` query (used by `/events`, since
    /// `EventSource` cannot set headers). Pure over its two inputs for testing.
    fn token_matches(&self, auth_header: Option<&str>, query: Option<&str>) -> bool {
        let provided = auth_header
            .and_then(bearer_from_header)
            .or_else(|| query.and_then(|q| query_param(q, "token")));
        provided.is_some_and(|tok| ct_eq(tok.as_bytes(), self.token.as_bytes()))
    }

    fn authorize(&self, request: &Request) -> bool {
        let auth = header_value(request, "Authorization");
        let query = request.url().split_once('?').map(|(_, q)| q);
        self.token_matches(auth, query)
    }

    /// The GUI to serve: a loopback bind embeds the token (zero-config local UI);
    /// a remote bind serves the template unchanged so the page never carries the
    /// token — the operator supplies it via the URL fragment.
    fn render_gui(&self, template: &str) -> String {
        if self.enforce_loopback_host {
            template.replace(GUI_TOKEN_PLACEHOLDER, &self.token)
        } else {
            template.to_string()
        }
    }
}

fn handle_request(ctx: &HttpContext, request: Request) -> Result<()> {
    // DNS-rebinding defense: a loopback bind only ever serves loopback Hosts.
    if ctx.security.enforce_loopback_host && !host_is_loopback(&request) {
        return respond_text(request, 403, "forbidden: non-loopback Host", None);
    }
    let cors = allowed_origin(&request);
    let method = request.method().clone();
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_string();
    match (&method, path.as_str()) {
        (Method::Post, "/rpc") => guarded(ctx, request, cors.as_deref(), |req, cors| {
            handle_rpc(&ctx.router, req, cors)
        }),
        (Method::Get, "/events") => guarded(ctx, request, cors.as_deref(), |req, cors| {
            handle_events(&ctx.hub, req, cors)
        }),
        (Method::Options, _) => respond_preflight(request, cors.as_deref()),
        (Method::Get, "/") => {
            let html = ctx.security.render_gui(GUI_HTML);
            respond_html(request, &html, cors.as_deref())
        }
        // The Leptos WASM frontend (G1b): a second client of this same Protocol
        // v4 transport. The legacy GUI at `/` is untouched.
        (Method::Get, p) if super::app::is_app_path(p) => {
            super::app::serve_app(ctx.security.embed_token(), request, p, cors.as_deref())
        }
        _ => respond_text(request, 404, "not found", cors.as_deref()),
    }
}

/// Run `handler` only if the request carries the bearer token, else answer `401`.
/// Keeps the token check off the `GET /` (bootstrap) and `OPTIONS` (preflight)
/// paths, which must stay reachable for the GUI to load and CORS to negotiate.
fn guarded(
    ctx: &HttpContext,
    request: Request,
    cors: Option<&str>,
    handler: impl FnOnce(Request, Option<&str>) -> Result<()>,
) -> Result<()> {
    if ctx.security.authorize(&request) {
        handler(request, cors)
    } else {
        respond_text(request, 401, "unauthorized", cors)
    }
}

// ---- POST /rpc ------------------------------------------------------------

fn handle_rpc(
    router: &RuntimeDaemonRouter,
    mut request: Request,
    cors: Option<&str>,
) -> Result<()> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|err| anyhow!("failed to read /rpc body: {err}"))?;
    match route_rpc_body(router, &body) {
        Some(response) => respond_json(request, 200, &response, cors),
        None => respond_text(request, 204, "", cors),
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

fn handle_events(hub: &Arc<SseHub>, request: Request, cors: Option<&str>) -> Result<()> {
    let query = request.url().split_once('?').map(|(_, q)| q.to_string());
    // Scope: `?session=<id>` filters fan-out to that session (+ global events).
    let session_filter = query
        .as_deref()
        .and_then(|q| query_param(q, "session"))
        .map(str::to_string);
    // Replay cursor: SSE `Last-Event-ID` header (browser auto-reconnect) or an
    // explicit `?last_seq=` (manual clients). The header wins when both are set.
    let last_seq = header_value(&request, "Last-Event-ID")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            query
                .as_deref()
                .and_then(|q| query_param(q, "last_seq"))
                .and_then(|v| v.parse::<u64>().ok())
        });
    let (id, receiver, backlog) = hub.subscribe(session_filter, last_seq);
    // The Origin echo is fixed for the life of the stream; copy it out before the
    // request is consumed by `into_writer`.
    let cors = cors.map(str::to_string);
    let mut writer = request.into_writer();
    let result = stream_events(&mut writer, &receiver, &backlog, cors.as_deref());
    hub.unsubscribe(id);
    result
}

/// Write the SSE response head, replay any buffered backlog, then forward each
/// broadcast notification as an `id:`+`data:` frame until the client disconnects
/// (a failed write) or the hub stops.
fn stream_events(
    writer: &mut dyn Write,
    receiver: &Receiver<Arc<SseFrame>>,
    backlog: &[Arc<SseFrame>],
    cors: Option<&str>,
) -> Result<()> {
    if write_sse_head(writer, cors).is_err() {
        return Ok(());
    }
    for frame in backlog {
        if write_sse_chunk(writer, sse_event_frame(frame).as_bytes()).is_err() {
            return Ok(()); // client went away mid-replay
        }
    }
    loop {
        let chunk = match receiver.recv_timeout(SSE_HEARTBEAT) {
            Ok(frame) => sse_event_frame(&frame),
            Err(RecvTimeoutError::Timeout) => SSE_KEEPALIVE.to_string(),
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if write_sse_chunk(writer, chunk.as_bytes()).is_err() {
            break; // client went away
        }
    }
    Ok(())
}

/// Format a runtime notification as an SSE event, setting the `id:` field to the
/// frame's `event_seq` so the browser reports it back via `Last-Event-ID` on
/// reconnect. The JSON is a single line (serde never emits raw newlines), so one
/// `data:` line is always valid.
fn sse_event_frame(frame: &SseFrame) -> String {
    format!("id: {}\ndata: {}\n\n", frame.seq, frame.json)
}

fn write_sse_head(writer: &mut dyn Write, cors: Option<&str>) -> std::io::Result<()> {
    writer.write_all(
        b"HTTP/1.1 200 OK\r\n\
          Content-Type: text/event-stream\r\n\
          Cache-Control: no-cache\r\n\
          Connection: keep-alive\r\n",
    )?;
    if let Some(origin) = cors {
        write!(
            writer,
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        )?;
    }
    writer.write_all(b"Transfer-Encoding: chunked\r\n\r\n")?;
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

/// One fan-out unit: the rendered JSON plus the routing facts the hub extracts
/// from it once (so each subscriber doesn't re-parse). `session` scopes delivery;
/// `seq` becomes the SSE `id:` and drives `Last-Event-ID` replay.
struct SseFrame {
    seq: u64,
    /// `Some` for a session-scoped event (delivered only to subscribers watching
    /// that session, plus unscoped subscribers); `None` for global events.
    session: Option<String>,
    json: Arc<str>,
}

impl SseFrame {
    /// Build a frame from a `runtime/event` notification, lifting `event_seq` and
    /// the event's `session_id` out of `params` for routing. Both are advisory: a
    /// missing/garbled field degrades to seq `0` / unscoped rather than dropping.
    fn from_notification(value: &Value) -> Self {
        let params = value.get("params");
        // `event_seq` serializes as camelCase `eventSeq` (the notification
        // carrier's own `rename_all`); the flattened event keeps snake_case, so
        // its `session_id` field stays `session_id`.
        let seq = params
            .and_then(|p| p.get("eventSeq"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let session = params
            .and_then(|p| p.get("session_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Self {
            seq,
            session,
            json: Arc::from(value.to_string()),
        }
    }

    /// Whether this frame should reach a subscriber with the given session filter.
    /// Unfiltered subscribers (`None`) see everything; a session subscriber sees
    /// its own session-scoped frames plus all unscoped (global) frames.
    fn visible_to(&self, filter: Option<&str>) -> bool {
        match (filter, self.session.as_deref()) {
            (None, _) => true,
            (Some(_), None) => true,
            (Some(want), Some(have)) => want == have,
        }
    }
}

/// A broadcast hub with per-session fan-out and bounded replay: the router's
/// single notification sink feeds it, and every open `/events` subscriber
/// receives only the frames its filter selects. It lives in the transport (not in
/// `nerve-runtime`) so the protocol vocabulary stays untouched.
struct SseHub {
    inner: Mutex<HubState>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct HubState {
    subscribers: Vec<Subscriber>,
    /// Bounded ring of recent frames for `Last-Event-ID` replay, oldest first.
    replay: VecDeque<Arc<SseFrame>>,
}

struct Subscriber {
    id: u64,
    /// `None` = unfiltered (all frames); `Some(session_id)` = only that session's
    /// frames plus global ones.
    session_filter: Option<String>,
    sender: SyncSender<Arc<SseFrame>>,
}

impl SseHub {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HubState::default()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Register a subscriber with an optional session filter. Returns its id, the
    /// receiver its `/events` connection drains, and any buffered frames after
    /// `after_seq` that match the filter (bounded replay on reconnect).
    fn subscribe(
        &self,
        session_filter: Option<String>,
        after_seq: Option<u64>,
    ) -> (u64, Receiver<Arc<SseFrame>>, Vec<Arc<SseFrame>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(SSE_SUBSCRIBER_CAPACITY);
        let mut state = crate::sync::lock_recover(&self.inner);
        let backlog = match after_seq {
            Some(after) => state
                .replay
                .iter()
                .filter(|frame| frame.seq > after && frame.visible_to(session_filter.as_deref()))
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        state.subscribers.push(Subscriber {
            id,
            session_filter,
            sender,
        });
        (id, receiver, backlog)
    }

    fn unsubscribe(&self, id: u64) {
        crate::sync::lock_recover(&self.inner)
            .subscribers
            .retain(|subscriber| subscriber.id != id);
    }

    /// Record a runtime notification in the bounded replay ring and fan it out to
    /// every live subscriber whose filter selects it, dropping any whose receiver
    /// has hung up.
    fn broadcast(&self, value: Value) {
        let frame = Arc::new(SseFrame::from_notification(&value));
        let mut state = crate::sync::lock_recover(&self.inner);
        state.replay.push_back(Arc::clone(&frame));
        while state.replay.len() > SSE_REPLAY_CAPACITY {
            state.replay.pop_front();
        }
        state.subscribers.retain(|subscriber| {
            // A subscriber the frame isn't addressed to is kept untouched. For one
            // we *do* address, a non-blocking `try_send` keeps `broadcast` from
            // ever blocking on a slow reader: a full buffer (stalled `/events`
            // reader) or a dropped receiver both prune the subscriber — a slow
            // client is treated as disconnected (it can reconnect and replay via
            // `Last-Event-ID`). A long-idle subscriber is also pruned when its
            // connection closes (its `/events` thread calls `unsubscribe`).
            if frame.visible_to(subscriber.session_filter.as_deref()) {
                match subscriber.sender.try_send(Arc::clone(&frame)) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                }
            } else {
                true
            }
        });
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.inner.lock().expect("sse hub lock").subscribers.len()
    }

    #[cfg(test)]
    fn replay_len(&self) -> usize {
        self.inner.lock().expect("sse hub lock").replay.len()
    }
}

// ---- HTTP responses -------------------------------------------------------

fn respond_json(request: Request, status: u16, value: &Value, cors: Option<&str>) -> Result<()> {
    let body =
        serde_json::to_string(value).map_err(|err| anyhow!("encode /rpc response: {err}"))?;
    let response = with_cors(
        Response::from_string(body)
            .with_status_code(StatusCode(status))
            .with_header(static_header("Content-Type", "application/json")),
        cors,
    );
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write /rpc response: {err}"))
}

pub(super) fn respond_html(request: Request, html: &str, cors: Option<&str>) -> Result<()> {
    let response = with_cors(
        Response::from_string(html.to_string())
            .with_status_code(StatusCode(200))
            .with_header(static_header("Content-Type", "text/html; charset=utf-8")),
        cors,
    );
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write GUI response: {err}"))
}

/// Respond with a static binary/text asset and an explicit Content-Type. Used by
/// the `/app` Leptos surface to serve its embedded `.js` / `.wasm` / `.css` with
/// the right MIME (notably `application/wasm`, required for `instantiateStreaming`).
pub(super) fn respond_asset(
    request: Request,
    bytes: &'static [u8],
    content_type: &str,
    cors: Option<&str>,
) -> Result<()> {
    let response = with_cors(
        Response::from_data(bytes)
            .with_status_code(StatusCode(200))
            .with_header(static_header("Content-Type", content_type)),
        cors,
    );
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write asset response: {err}"))
}

pub(super) fn respond_text(
    request: Request,
    status: u16,
    message: &str,
    cors: Option<&str>,
) -> Result<()> {
    let response = with_cors(
        Response::from_string(message.to_string())
            .with_status_code(StatusCode(status))
            .with_header(static_header("Content-Type", "text/plain; charset=utf-8")),
        cors,
    );
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write response: {err}"))
}

fn respond_preflight(request: Request, cors: Option<&str>) -> Result<()> {
    let response = with_cors(
        Response::empty(StatusCode(204))
            .with_header(static_header(
                "Access-Control-Allow-Methods",
                "GET, POST, OPTIONS",
            ))
            .with_header(static_header(
                "Access-Control-Allow-Headers",
                "Content-Type, Authorization",
            )),
        cors,
    );
    request
        .respond(response)
        .map_err(|err| anyhow!("failed to write preflight response: {err}"))
}

/// Attach the CORS echo headers iff an origin was allowed (loopback). A request
/// with no allowed origin gets no `Access-Control-Allow-Origin`, so a foreign
/// web page cannot read the response — replacing the former wildcard `*`.
fn with_cors<R: std::io::Read>(response: Response<R>, cors: Option<&str>) -> Response<R> {
    match cors.and_then(|origin| dyn_header("Access-Control-Allow-Origin", origin)) {
        Some(header) => response
            .with_header(header)
            .with_header(static_header("Vary", "Origin")),
        None => response,
    }
}

fn static_header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static HTTP header is valid")
}

/// Build a header from a runtime-derived value, dropping it if the value is not
/// a valid header (never the case for an origin we already parsed, but keeps the
/// transport panic-free on hostile input).
fn dyn_header(name: &str, value: &str) -> Option<Header> {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

// ---- request inspection + transport-guard helpers -------------------------

/// Case-insensitive lookup of a request header value. `name` is `'static`
/// because `tiny_http`'s [`HeaderField::equiv`] requires it; every caller passes
/// a string literal.
fn header_value<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

/// The CORS origin to echo: the request `Origin` iff it is a loopback origin,
/// else `None`. Same-origin requests (which need no CORS) simply pass through.
fn allowed_origin(request: &Request) -> Option<String> {
    let origin = header_value(request, "Origin")?;
    origin_is_loopback(origin).then(|| origin.to_string())
}

fn host_is_loopback(request: &Request) -> bool {
    header_value(request, "Host").is_some_and(authority_host_is_loopback)
}

/// Whether an `Origin` (`scheme://host[:port]`) is an http(s) loopback origin.
fn origin_is_loopback(origin: &str) -> bool {
    match origin.split_once("://") {
        Some((scheme, authority)) if scheme == "http" || scheme == "https" => {
            authority_host_is_loopback(authority)
        }
        _ => false,
    }
}

/// Whether an authority (`host` or `host:port`, incl. `[ipv6]:port`) is loopback.
fn authority_host_is_loopback(authority: &str) -> bool {
    let host = strip_port(authority.trim());
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Strip a trailing `:port` from an authority, handling bracketed IPv6 literals
/// (`[::1]:4173` -> `::1`) and bare addresses (`::1`, `127.0.0.1`) unchanged.
fn strip_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    if authority.parse::<IpAddr>().is_ok() {
        return authority; // bare IPv4/IPv6 with no port
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Extract the token from an `Authorization: Bearer <token>` header value.
fn bearer_from_header(value: &str) -> Option<&str> {
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then_some(token)
}

/// Find `key`'s value in a raw `a=1&b=2` query string. The daemon token is
/// base64url-no-pad, so it is never percent-encoded and compares raw.
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

/// Constant-time byte comparison so token checking does not leak length/prefix
/// information through timing.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
            false,
            crate::sandbox::refuse_launcher(),
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

    /// Build a `runtime/event` notification frame for hub tests, with a seq and
    /// an optional `session_id` inside `params`, mirroring the live carrier shape.
    fn notification(seq: u64, session: Option<&str>) -> Value {
        let mut params = json!({ "eventSeq": seq, "type": "job_progress" });
        if let Some(session_id) = session {
            params["session_id"] = json!(session_id);
        }
        json!({ "jsonrpc": "2.0", "method": "runtime/event", "params": params })
    }

    #[test]
    fn sse_event_frame_sets_id_and_data() {
        let frame = SseFrame::from_notification(&notification(7, None));
        assert_eq!(
            sse_event_frame(&frame),
            "id: 7\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"runtime/event\",\"params\":{\"eventSeq\":7,\"type\":\"job_progress\"}}\n\n"
        );
    }

    #[test]
    fn sse_frame_extracts_seq_and_session_for_routing() {
        let scoped = SseFrame::from_notification(&notification(3, Some("sess-1")));
        assert_eq!(scoped.seq, 3);
        assert_eq!(scoped.session.as_deref(), Some("sess-1"));
        let global = SseFrame::from_notification(&notification(4, None));
        assert_eq!(global.seq, 4);
        assert_eq!(global.session, None);
        // A malformed frame degrades to seq 0 / unscoped rather than panicking.
        let degraded = SseFrame::from_notification(&json!({ "params": "oops" }));
        assert_eq!(degraded.seq, 0);
        assert_eq!(degraded.session, None);
    }

    #[test]
    fn frame_visibility_scopes_by_session() {
        let scoped = SseFrame::from_notification(&notification(1, Some("a")));
        let global = SseFrame::from_notification(&notification(2, None));
        // Unfiltered subscriber sees everything.
        assert!(scoped.visible_to(None));
        assert!(global.visible_to(None));
        // Session subscriber sees its own session and all global frames…
        assert!(scoped.visible_to(Some("a")));
        assert!(global.visible_to(Some("a")));
        // …but not another session's frames.
        assert!(!scoped.visible_to(Some("b")));
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
        write_sse_head(&mut buf, None).expect("write head");
        let head = String::from_utf8(buf).expect("utf8");
        assert!(head.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(head.contains("Content-Type: text/event-stream\r\n"));
        assert!(head.contains("Transfer-Encoding: chunked\r\n"));
        assert!(head.ends_with("\r\n\r\n"));
        // No allowed origin => no CORS echo on the stream.
        assert!(!head.contains("Access-Control-Allow-Origin"));
    }

    #[test]
    fn write_sse_head_echoes_allowed_origin() {
        let mut buf = Vec::new();
        write_sse_head(&mut buf, Some("http://127.0.0.1:4173")).expect("write head");
        let head = String::from_utf8(buf).expect("utf8");
        assert!(head.contains("Access-Control-Allow-Origin: http://127.0.0.1:4173\r\n"));
        assert!(head.contains("Vary: Origin\r\n"));
    }

    #[test]
    fn token_matches_accepts_header_or_query_and_rejects_otherwise() {
        let security = HttpSecurity {
            token: "s3cret".to_string(),
            enforce_loopback_host: true,
        };
        assert!(security.token_matches(Some("Bearer s3cret"), None));
        assert!(security.token_matches(Some("bearer s3cret"), None));
        assert!(security.token_matches(None, Some("x=1&token=s3cret&y=2")));
        // A query token wins when the header is absent/non-bearer.
        assert!(security.token_matches(Some("Basic zzz"), Some("token=s3cret")));
        assert!(!security.token_matches(Some("Bearer nope"), Some("token=nope")));
        assert!(!security.token_matches(None, None));
        assert!(!security.token_matches(Some("Bearer "), None));
        // A token that is a prefix of the real one must not pass (length check).
        assert!(!security.token_matches(Some("Bearer s3cre"), None));
    }

    #[test]
    fn render_gui_embeds_token_only_on_loopback() {
        let local = HttpSecurity {
            token: "TOKEN123".to_string(),
            enforce_loopback_host: true,
        };
        let rendered = local.render_gui("auth=__NERVE_DAEMON_TOKEN__;");
        assert_eq!(rendered, "auth=TOKEN123;");

        let remote = HttpSecurity {
            token: "TOKEN123".to_string(),
            enforce_loopback_host: false,
        };
        // A remote bind never bakes the token into the served page.
        assert_eq!(
            remote.render_gui("auth=__NERVE_DAEMON_TOKEN__;"),
            "auth=__NERVE_DAEMON_TOKEN__;"
        );
    }

    #[test]
    fn loopback_authorities_and_origins_are_recognized() {
        for ok in [
            "127.0.0.1",
            "127.0.0.1:4173",
            "localhost",
            "LocalHost:8080",
            "[::1]:4173",
            "::1",
            "127.5.6.7:80",
        ] {
            assert!(authority_host_is_loopback(ok), "{ok} should be loopback");
        }
        for bad in ["0.0.0.0", "192.168.1.5:4173", "example.com", "10.0.0.1"] {
            assert!(
                !authority_host_is_loopback(bad),
                "{bad} should not be loopback"
            );
        }
        assert!(origin_is_loopback("http://127.0.0.1:4173"));
        assert!(origin_is_loopback("https://localhost"));
        assert!(!origin_is_loopback("http://evil.example.com"));
        assert!(!origin_is_loopback("null"));
        assert!(!origin_is_loopback("file://127.0.0.1"));
    }

    #[test]
    fn bearer_and_query_token_parsing() {
        assert_eq!(bearer_from_header("Bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("Token abc"), None);
        assert_eq!(query_param("token=abc&x=1", "token"), Some("abc"));
        assert_eq!(query_param("x=1", "token"), None);
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn hub_broadcasts_to_live_subscribers_and_prunes_dead_ones() {
        let hub = SseHub::new();
        let (_id_a, rx_a, _) = hub.subscribe(None, None);
        let (_id_b, rx_b, _) = hub.subscribe(None, None);
        assert_eq!(hub.subscriber_count(), 2);
        drop(rx_b); // a disconnected client
        hub.broadcast(notification(1, None));
        let frame = rx_a.recv().expect("frame");
        assert!(frame.json.contains("runtime/event"));
        // The dead subscriber was pruned on its failed send.
        assert_eq!(hub.subscriber_count(), 1);
    }

    #[test]
    fn hub_prunes_subscriber_whose_buffer_fills() {
        // A stalled `/events` reader (receiver never drained) must not let
        // `broadcast` grow memory without bound: once its bounded buffer fills,
        // the subscriber is treated as a slow/dead client and pruned.
        let hub = SseHub::new();
        let (_id, rx, _) = hub.subscribe(None, None);
        // Never drain `rx`. Send one frame past the buffer capacity.
        for seq in 1..=(SSE_SUBSCRIBER_CAPACITY as u64 + 1) {
            hub.broadcast(notification(seq, None));
        }
        // The buffer filled, so the overflowing send pruned the subscriber.
        assert_eq!(hub.subscriber_count(), 0);
        // The frames buffered before the overflow are still readable (bounded,
        // not lost wholesale): exactly the capacity's worth.
        let mut received = 0;
        while rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, SSE_SUBSCRIBER_CAPACITY);
    }

    #[test]
    fn hub_scopes_session_events_to_matching_subscribers() {
        let hub = SseHub::new();
        let (_all_id, rx_all, _) = hub.subscribe(None, None);
        let (_a_id, rx_a, _) = hub.subscribe(Some("sess-a".to_string()), None);
        let (_b_id, rx_b, _) = hub.subscribe(Some("sess-b".to_string()), None);

        // A frame for session A reaches the unfiltered + the A subscriber only.
        hub.broadcast(notification(1, Some("sess-a")));
        assert!(rx_all.recv().expect("all sees a").json.contains("sess-a"));
        assert!(rx_a.recv().expect("a sees a").json.contains("sess-a"));
        assert!(rx_b.try_recv().is_err(), "b must not see session a's event");

        // A global (unscoped) frame reaches everyone.
        hub.broadcast(notification(2, None));
        assert_eq!(rx_all.recv().expect("all sees global").seq, 2);
        assert_eq!(rx_a.recv().expect("a sees global").seq, 2);
        assert_eq!(rx_b.recv().expect("b sees global").seq, 2);
    }

    #[test]
    fn hub_replays_buffered_events_after_last_seq() {
        let hub = SseHub::new();
        hub.broadcast(notification(1, None));
        hub.broadcast(notification(2, Some("sess-a")));
        hub.broadcast(notification(3, Some("sess-b")));
        hub.broadcast(notification(4, None));

        // Unfiltered reconnect after seq 1 replays 2,3,4 in order.
        let (_id, _rx, backlog) = hub.subscribe(None, Some(1));
        let seqs: Vec<u64> = backlog.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![2, 3, 4]);

        // Session-A reconnect after seq 1 replays only A's event + the global one.
        let (_id, _rx, backlog) = hub.subscribe(Some("sess-a".to_string()), Some(1));
        let seqs: Vec<u64> = backlog.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, vec![2, 4], "session A skips session B's event 3");

        // No cursor => no replay (a fresh subscriber gets only future frames).
        let (_id, _rx, backlog) = hub.subscribe(None, None);
        assert!(backlog.is_empty());
    }

    #[test]
    fn hub_replay_ring_is_bounded() {
        let hub = SseHub::new();
        for seq in 1..=(SSE_REPLAY_CAPACITY as u64 + 50) {
            hub.broadcast(notification(seq, None));
        }
        assert_eq!(hub.replay_len(), SSE_REPLAY_CAPACITY);
        // The oldest were dropped: replay after seq 0 starts past the dropped ones.
        let (_id, _rx, backlog) = hub.subscribe(None, Some(0));
        assert_eq!(backlog.len(), SSE_REPLAY_CAPACITY);
        let first = backlog.first().expect("non-empty backlog").seq;
        assert_eq!(first, 51, "oldest 50 frames were evicted");
    }
}
