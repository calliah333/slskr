//! Minimal SignalR client (JSON hub protocol over WebSockets), port of slskd `lib/hubFactory.js`.
//!
//! Behaves like `@microsoft/signalr`'s `HubConnection` as configured by slskd: negotiate
//! (version 1) with the bearer token, WebSocket transport with `access_token` query
//! parameter, JSON protocol, keep-alive pings, server timeout, and automatic reconnect
//! with slskd's retry delays.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    fmt,
    rc::{Rc, Weak},
};

use gloo_net::http::{Method, RequestBuilder};
use gloo_timers::callback::Interval;
use serde::Deserialize;
use serde_json::{Value, json};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{CloseEvent, MessageEvent, RequestCredentials, WebSocket};

use super::{
    token::{get_token, is_passthrough_enabled},
    util::{encode_uri_component, sleep},
};
use crate::config::hub_base_url;

/// Record separator terminating every JSON hub protocol message.
const RECORD_SEPARATOR: char = '\u{1e}';
/// slskd's `withAutomaticReconnect` delays (ms).
const RECONNECT_DELAYS: [u32; 12] = [
    0, 100, 250, 500, 1_000, 2_000, 3_000, 5_000, 5_000, 5_000, 5_000, 5_000,
];
/// Send a ping when nothing was sent for this long (signalr default).
const KEEP_ALIVE_INTERVAL_MS: f64 = 15_000.0;
/// Close the connection when nothing was received for this long (signalr default).
const SERVER_TIMEOUT_MS: f64 = 30_000.0;
/// Resolution of the keep-alive / server-timeout timer.
const TIMER_TICK_MS: u32 = 1_000;
/// Maximum number of negotiate redirects (signalr's `MAX_REDIRECTS`).
const MAX_REDIRECTS: usize = 100;

const MESSAGE_INVOCATION: u64 = 1;
const MESSAGE_STREAM_ITEM: u64 = 2;
const MESSAGE_COMPLETION: u64 = 3;
const MESSAGE_PING: u64 = 6;
const MESSAGE_CLOSE: u64 = 7;

/// A hub connection failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubError(pub String);

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HubError {}

impl From<JsValue> for HubError {
    fn from(value: JsValue) -> Self {
        let message = value
            .as_string()
            .or_else(|| {
                value
                    .dyn_ref::<js_sys::Error>()
                    .map(|error| String::from(error.message()))
            })
            .unwrap_or_else(|| format!("{value:?}"));
        Self(message)
    }
}

/// Connection lifecycle state (mirrors signalr's `HubConnectionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HubConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Reconnecting,
}

type Handler = Rc<dyn Fn(Vec<Value>)>;
type Callback = Rc<dyn Fn(Option<String>)>;

/// Resolve/reject pair of a JS promise, used as a single-threaded oneshot.
struct Deferred {
    resolve: js_sys::Function,
    reject: js_sys::Function,
}

impl Deferred {
    fn new() -> (Self, js_sys::Promise) {
        let mut slot = None;
        let promise =
            js_sys::Promise::new(&mut |resolve, reject| slot = Some(Self { resolve, reject }));
        (slot.expect("promise executor runs synchronously"), promise)
    }

    fn resolve(self, value: &JsValue) {
        let _ = self.resolve.call1(&JsValue::NULL, value);
    }

    fn reject(self, message: &str) {
        let _ = self
            .reject
            .call1(&JsValue::NULL, &JsValue::from_str(message));
    }
}

/// Why the current socket is being closed by the client.
struct CloseIntent {
    error: Option<String>,
    reconnect: bool,
}

struct Socket {
    ws: WebSocket,
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl Socket {
    fn detach(&self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
    }
}

struct Negotiated {
    url: String,
    connection_id: Option<String>,
    connection_token: Option<String>,
    access_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NegotiateResponse {
    connection_id: Option<String>,
    connection_token: Option<String>,
    negotiate_version: Option<u32>,
    available_transports: Option<Vec<AvailableTransport>>,
    url: Option<String>,
    access_token: Option<String>,
    error: Option<String>,
    #[serde(rename = "ProtocolVersion")]
    protocol_version: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvailableTransport {
    transport: String,
    #[serde(default)]
    transfer_formats: Vec<String>,
}

struct Inner {
    url: String,
    use_access_token: bool,
    state: Cell<HubConnectionState>,
    socket: RefCell<Option<Socket>>,
    /// Incremented per socket; events from stale sockets are ignored.
    generation: Cell<u64>,
    /// Incremented per reconnect loop and on stop; stale loops exit.
    reconnect_epoch: Cell<u64>,
    connection_id: RefCell<Option<String>>,
    handshake: RefCell<Option<Deferred>>,
    timer: RefCell<Option<Interval>>,
    last_received: Cell<f64>,
    last_sent: Cell<f64>,
    handlers: RefCell<HashMap<String, Vec<Handler>>>,
    on_reconnecting: RefCell<Vec<Callback>>,
    on_reconnected: RefCell<Vec<Callback>>,
    on_close: RefCell<Vec<Callback>>,
    invocations: RefCell<HashMap<String, Deferred>>,
    next_invocation_id: Cell<u64>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(socket) = self.socket.get_mut().take() {
            socket.detach();
            let _ = socket.ws.close();
        }
    }
}

impl Inner {
    fn is_current(&self, generation: u64) -> bool {
        self.generation.get() == generation
    }

    fn fire(callbacks: &RefCell<Vec<Callback>>, argument: Option<String>) {
        let callbacks = callbacks.borrow().clone();
        for callback in callbacks {
            callback(argument.clone());
        }
    }

    fn send_raw(&self, payload: &str) -> Result<(), HubError> {
        let socket = self.socket.borrow();
        let socket = socket
            .as_ref()
            .ok_or_else(|| HubError("WebSocket is not in the OPEN state".into()))?;
        socket.ws.send_with_str(payload)?;
        self.last_sent.set(js_sys::Date::now());
        Ok(())
    }

    fn send_message(&self, message: &Value) -> Result<(), HubError> {
        self.send_raw(&format!("{message}{RECORD_SEPARATOR}"))
    }

    /// Detaches and drops the current socket (closing it if still open) and fails a pending handshake.
    fn teardown_socket(&self) {
        self.generation.set(self.generation.get() + 1);
        // May run inside the timer's own callback: drop it afterwards.
        if let Some(timer) = self.timer.borrow_mut().take() {
            spawn_local(async move { drop(timer) });
        }
        if let Some(socket) = self.socket.borrow_mut().take() {
            socket.detach();
            let _ = socket.ws.close();
            // May run inside one of the socket's own callbacks: drop the closures afterwards.
            spawn_local(async move { drop(socket) });
        }
        if let Some(handshake) = self.handshake.borrow_mut().take() {
            handshake.reject("The connection was stopped during the handshake.");
        }
    }

    /// Closes the socket and finishes the close according to `intent` immediately.
    ///
    /// Like `@microsoft/signalr`'s `WebSocketTransport.stop()`, this does not wait for the
    /// socket's `close` event, which can take a long time (or never arrive) on a dead
    /// network. `teardown_socket` bumps the generation, so the late event is ignored.
    fn close_socket(self: &Rc<Self>, intent: CloseIntent) {
        if self.socket.borrow().is_some() {
            self.connection_closed(intent);
        } else if self.state.get() == HubConnectionState::Disconnecting {
            self.complete_close(intent.error);
        }
    }

    fn complete_close(&self, error: Option<String>) {
        self.state.set(HubConnectionState::Disconnected);
        self.connection_id.borrow_mut().take();
        Self::fire(&self.on_close, error);
    }

    fn cancel_invocations(&self, error: Option<&str>) {
        let pending: Vec<Deferred> = self
            .invocations
            .borrow_mut()
            .drain()
            .map(|(_, deferred)| deferred)
            .collect();
        let message =
            error.unwrap_or("Invocation canceled due to the underlying connection being closed.");
        for deferred in pending {
            deferred.reject(message);
        }
    }

    fn on_open(&self, generation: u64) {
        if !self.is_current(generation) {
            return;
        }
        let handshake = format!(r#"{{"protocol":"json","version":1}}{RECORD_SEPARATOR}"#);
        if let Err(error) = self.send_raw(&handshake)
            && let Some(deferred) = self.handshake.borrow_mut().take()
        {
            deferred.reject(&format!("Failed to send the handshake: {error}"));
        }
    }

    fn on_message(self: &Rc<Self>, generation: u64, event: MessageEvent) {
        if !self.is_current(generation) {
            return;
        }
        let Some(data) = event.data().as_string() else {
            leptos::logging::warn!(
                "SignalR: ignoring non-text message on JSON hub protocol connection"
            );
            return;
        };
        self.last_received.set(js_sys::Date::now());

        let mut data = data.as_str();
        let pending_handshake = self.handshake.borrow_mut().take();
        if let Some(handshake) = pending_handshake {
            let Some(index) = data.find(RECORD_SEPARATOR) else {
                handshake.reject("Message is incomplete.");
                return;
            };
            let response = &data[..index];
            data = &data[index + RECORD_SEPARATOR.len_utf8()..];
            match serde_json::from_str::<Value>(response) {
                Ok(value) => match value.get("error").and_then(Value::as_str) {
                    Some(error) => {
                        handshake.reject(&format!("Server returned handshake error: {error}"));
                        return;
                    }
                    None => {
                        handshake.resolve(&JsValue::UNDEFINED);
                        self.start_timer(generation);
                    }
                },
                Err(error) => {
                    handshake.reject(&format!("Error parsing handshake response: {error}"));
                    return;
                }
            }
        }

        if data.is_empty() {
            return;
        }
        if !data.ends_with(RECORD_SEPARATOR) {
            leptos::logging::warn!("SignalR: Message is incomplete.");
            return;
        }
        for record in data
            .split(RECORD_SEPARATOR)
            .filter(|record| !record.is_empty())
        {
            if !self.is_current(generation) {
                return;
            }
            match serde_json::from_str::<Value>(record) {
                Ok(message) => self.process_message(message),
                Err(error) => leptos::logging::warn!("SignalR: invalid message: {}", error),
            }
        }
    }

    fn process_message(self: &Rc<Self>, message: Value) {
        match message.get("type").and_then(Value::as_u64) {
            Some(MESSAGE_INVOCATION) => self.invoke_client_method(&message),
            Some(MESSAGE_COMPLETION) => {
                let Some(id) = message.get("invocationId").and_then(Value::as_str) else {
                    return;
                };
                let Some(deferred) = self.invocations.borrow_mut().remove(id) else {
                    return;
                };
                match message.get("error").and_then(Value::as_str) {
                    Some(error) => deferred.reject(error),
                    None => {
                        let result = message.get("result").cloned().unwrap_or(Value::Null);
                        deferred.resolve(&JsValue::from_str(&result.to_string()));
                    }
                }
            }
            Some(MESSAGE_STREAM_ITEM | MESSAGE_PING) => {}
            Some(MESSAGE_CLOSE) => {
                let error = message
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|error| format!("Server returned an error on close: {error}"));
                let reconnect =
                    message.get("allowReconnect").and_then(Value::as_bool) == Some(true);
                if !reconnect {
                    self.state.set(HubConnectionState::Disconnecting);
                }
                self.close_socket(CloseIntent { error, reconnect });
            }
            other => leptos::logging::warn!("SignalR: invalid message type {:?}", other),
        }
    }

    fn invoke_client_method(&self, message: &Value) {
        let target = message
            .get("target")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let arguments = match message.get("arguments") {
            Some(Value::Array(arguments)) => arguments.clone(),
            _ => Vec::new(),
        };
        let handlers = self
            .handlers
            .borrow()
            .get(&target.to_lowercase())
            .cloned()
            .unwrap_or_default();
        if handlers.is_empty() {
            leptos::logging::warn!(
                "No client method with the name '{}' found.",
                target.to_lowercase()
            );
        }
        for handler in &handlers {
            handler(arguments.clone());
        }
        // Client results are not supported; answer so the server does not wait forever.
        if let Some(id) = message.get("invocationId").and_then(Value::as_str) {
            let completion = json!({ "type": MESSAGE_COMPLETION, "invocationId": id, "error": "Client didn't provide a result." });
            let _ = self.send_message(&completion);
        }
    }

    fn on_close_event(self: &Rc<Self>, generation: u64, event: CloseEvent) {
        if !self.is_current(generation) {
            return;
        }
        let close_error = (!event.was_clean() || event.code() != 1000).then(|| {
            let reason = event.reason();
            let reason = if reason.is_empty() {
                "no reason given".to_string()
            } else {
                reason
            };
            format!(
                "WebSocket closed with status code: {} ({reason}).",
                event.code()
            )
        });

        let pending_handshake = self.handshake.borrow_mut().take();
        if let Some(handshake) = pending_handshake {
            let message = close_error
                .unwrap_or_else(|| "The connection was closed during the handshake.".into());
            handshake.reject(&message);
            return;
        }

        // Client-initiated closes finish synchronously in `close_socket`, so a close event
        // for the current socket always comes from the server or the network.
        self.connection_closed(CloseIntent {
            error: close_error,
            reconnect: true,
        });
    }

    fn connection_closed(self: &Rc<Self>, intent: CloseIntent) {
        self.teardown_socket();
        self.cancel_invocations(intent.error.as_deref());
        match self.state.get() {
            HubConnectionState::Disconnecting => self.complete_close(intent.error),
            HubConnectionState::Connected if intent.reconnect => {
                spawn_local(Self::reconnect(Rc::downgrade(self), intent.error));
            }
            HubConnectionState::Connected => self.complete_close(intent.error),
            // Closed between handshake completion and `start_internal` resuming: it reports the failure.
            _ => {}
        }
    }

    fn start_timer(self: &Rc<Self>, generation: u64) {
        let now = js_sys::Date::now();
        self.last_received.set(now);
        self.last_sent.set(now);
        let weak = Rc::downgrade(self);
        let interval = Interval::new(TIMER_TICK_MS, move || {
            let Some(inner) = weak.upgrade() else { return };
            if !inner.is_current(generation)
                || inner.state.get() == HubConnectionState::Disconnecting
            {
                return;
            }
            let now = js_sys::Date::now();
            if now - inner.last_received.get() >= SERVER_TIMEOUT_MS {
                inner.close_socket(CloseIntent {
                    error: Some(
                        "Server timeout elapsed without receiving a message from the server."
                            .into(),
                    ),
                    reconnect: true,
                });
            } else if now - inner.last_sent.get() >= KEEP_ALIVE_INTERVAL_MS {
                let _ = inner.send_message(&json!({ "type": MESSAGE_PING }));
            }
        });
        *self.timer.borrow_mut() = Some(interval);
    }

    /// Negotiates and opens a socket, resolving once the handshake completed.
    async fn start_internal(self: &Rc<Self>) -> Result<(), HubError> {
        let token = if self.use_access_token {
            get_token()
        } else {
            None
        };
        let negotiated = negotiate(&resolve_url(&self.url)?, token).await?;
        if !matches!(
            self.state.get(),
            HubConnectionState::Connecting | HubConnectionState::Reconnecting
        ) {
            return Err(HubError(
                "The connection was stopped during negotiation.".into(),
            ));
        }

        let mut ws_url = negotiated.url.clone();
        if let Some(connection_token) = &negotiated.connection_token {
            ws_url.push(if ws_url.contains('?') { '&' } else { '?' });
            ws_url.push_str("id=");
            ws_url.push_str(connection_token);
        }
        if let Some(token) = &negotiated.access_token {
            ws_url.push(if ws_url.contains('?') { '&' } else { '?' });
            ws_url.push_str("access_token=");
            ws_url.push_str(&encode_uri_component(token));
        }
        if let Some(rest) = ws_url.strip_prefix("http") {
            ws_url = format!("ws{rest}");
        }

        self.teardown_socket();
        let generation = self.generation.get();
        let ws = WebSocket::new(&ws_url)?;

        let weak = Rc::downgrade(self);
        let on_open = Closure::<dyn FnMut()>::new(move || {
            if let Some(inner) = weak.upgrade() {
                inner.on_open(generation);
            }
        });
        let weak = Rc::downgrade(self);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if let Some(inner) = weak.upgrade() {
                inner.on_message(generation, event);
            }
        });
        let weak = Rc::downgrade(self);
        let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
            if let Some(inner) = weak.upgrade() {
                inner.on_close_event(generation, event);
            }
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let (handshake, promise) = Deferred::new();
        *self.handshake.borrow_mut() = Some(handshake);
        *self.socket.borrow_mut() = Some(Socket {
            ws,
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
        });

        if let Err(error) = JsFuture::from(promise).await {
            if self.is_current(generation) {
                self.teardown_socket();
            }
            return Err(error.into());
        }
        if !self.is_current(generation) || self.socket.borrow().is_none() {
            return Err(HubError("The connection was closed during startup.".into()));
        }
        *self.connection_id.borrow_mut() = negotiated.connection_id;
        Ok(())
    }

    async fn reconnect(weak: Weak<Self>, error: Option<String>) {
        let Some(inner) = weak.upgrade() else { return };
        let epoch = inner.reconnect_epoch.get() + 1;
        inner.reconnect_epoch.set(epoch);
        inner.state.set(HubConnectionState::Reconnecting);
        Self::fire(&inner.on_reconnecting, error);
        drop(inner);

        let still_reconnecting = |inner: &Rc<Self>| {
            inner.reconnect_epoch.get() == epoch
                && inner.state.get() == HubConnectionState::Reconnecting
        };

        for delay in RECONNECT_DELAYS {
            sleep(delay).await;
            let Some(inner) = weak.upgrade() else { return };
            if !still_reconnecting(&inner) {
                return;
            }
            match inner.start_internal().await {
                Ok(()) if still_reconnecting(&inner) => {
                    inner.state.set(HubConnectionState::Connected);
                    let connection_id = inner.connection_id.borrow().clone();
                    Self::fire(&inner.on_reconnected, connection_id);
                    return;
                }
                Ok(()) => {
                    inner.teardown_socket();
                    return;
                }
                Err(error) => {
                    if !still_reconnecting(&inner) {
                        return;
                    }
                    leptos::logging::warn!("SignalR: reconnect attempt failed: {}", error);
                }
            }
        }

        if let Some(inner) = weak.upgrade()
            && still_reconnecting(&inner)
        {
            inner.complete_close(None);
        }
    }
}

/// Resolves `url` against the page location (signalr's `_resolveUrl`).
fn resolve_url(url: &str) -> Result<String, HubError> {
    let window = web_sys::window().ok_or_else(|| HubError("no window".into()))?;
    let base = window.location().href()?;
    Ok(web_sys::Url::new_with_base(url, &base)?.href())
}

/// `{url}/negotiate?negotiateVersion=1`, preserving an existing query.
fn negotiate_url(url: &str) -> String {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut out = String::with_capacity(url.len() + 32);
    out.push_str(path);
    out.push_str(if path.ends_with('/') {
        "negotiate"
    } else {
        "/negotiate"
    });
    out.push('?');
    out.push_str(query);
    if !query
        .split('&')
        .any(|pair| pair.starts_with("negotiateVersion="))
    {
        if !query.is_empty() {
            out.push('&');
        }
        out.push_str("negotiateVersion=1");
    }
    out
}

async fn negotiate(url: &str, mut access_token: Option<String>) -> Result<Negotiated, HubError> {
    let mut url = url.to_string();
    for _ in 0..MAX_REDIRECTS {
        let mut builder = RequestBuilder::new(&negotiate_url(&url))
            .method(Method::POST)
            .credentials(RequestCredentials::Include);
        if let Some(token) = &access_token {
            builder = builder.header("Authorization", &format!("Bearer {token}"));
        }
        let failed = |detail: String| {
            HubError(format!(
                "Failed to complete negotiation with the server: {detail}"
            ))
        };
        let response = builder
            .send()
            .await
            .map_err(|error| failed(error.to_string()))?;
        if !response.ok() {
            return Err(failed(format!("Status code '{}'", response.status())));
        }
        let response: NegotiateResponse = response
            .json()
            .await
            .map_err(|error| failed(error.to_string()))?;

        if response.protocol_version.is_some() {
            return Err(HubError(
                "Detected a connection attempt to an ASP.NET SignalR Server. This client only supports connecting to an ASP.NET Core SignalR Server. See https://aka.ms/signalr-core-differences for details.".into(),
            ));
        }
        if let Some(error) = response.error {
            return Err(HubError(error));
        }
        if let Some(redirect) = response.url {
            url = redirect;
            if response.access_token.is_some() {
                access_token = response.access_token;
            }
            continue;
        }
        if let Some(transports) = &response.available_transports
            && !transports.iter().any(|transport| {
                transport.transport == "WebSockets"
                    && transport
                        .transfer_formats
                        .iter()
                        .any(|format| format == "Text")
            })
        {
            return Err(HubError(
                "Unable to connect to the server with any of the available transports.".into(),
            ));
        }

        let connection_token = if response.negotiate_version.unwrap_or(0) >= 1 {
            response.connection_token
        } else {
            response.connection_id.clone()
        };
        return Ok(Negotiated {
            url,
            connection_id: response.connection_id,
            connection_token,
            access_token,
        });
    }
    Err(HubError("Negotiate redirection limit exceeded.".into()))
}

/// A SignalR hub connection. Cloning shares the connection.
#[derive(Clone)]
pub struct HubConnection {
    inner: Rc<Inner>,
}

impl HubConnection {
    /// Creates a disconnected connection to the hub at `url` (absolute or relative to the page).
    ///
    /// Like slskd's `accessTokenFactory`, the stored token is sent unless passthrough auth is
    /// active at construction time.
    pub fn new(url: impl Into<String>) -> Self {
        let inner = Rc::new(Inner {
            url: url.into(),
            use_access_token: !is_passthrough_enabled(),
            state: Cell::new(HubConnectionState::Disconnected),
            socket: RefCell::new(None),
            generation: Cell::new(0),
            reconnect_epoch: Cell::new(0),
            connection_id: RefCell::new(None),
            handshake: RefCell::new(None),
            timer: RefCell::new(None),
            last_received: Cell::new(0.0),
            last_sent: Cell::new(0.0),
            handlers: RefCell::new(HashMap::new()),
            on_reconnecting: RefCell::new(Vec::new()),
            on_reconnected: RefCell::new(Vec::new()),
            on_close: RefCell::new(Vec::new()),
            invocations: RefCell::new(HashMap::new()),
            next_invocation_id: Cell::new(0),
        });
        Self { inner }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> HubConnectionState {
        self.inner.state.get()
    }

    /// Server-assigned connection id while connected.
    pub fn connection_id(&self) -> Option<String> {
        self.inner.connection_id.borrow().clone()
    }

    /// Registers a handler for server invocations of `target` (case-insensitive).
    pub fn on(&self, target: &str, handler: impl Fn(Vec<Value>) + 'static) {
        self.inner
            .handlers
            .borrow_mut()
            .entry(target.to_lowercase())
            .or_default()
            .push(Rc::new(handler));
    }

    /// Removes all handlers for `target`.
    pub fn off(&self, target: &str) {
        self.inner
            .handlers
            .borrow_mut()
            .remove(&target.to_lowercase());
    }

    /// Called with the triggering error when the connection is lost and reconnecting starts.
    pub fn on_reconnecting(&self, callback: impl Fn(Option<String>) + 'static) {
        self.inner
            .on_reconnecting
            .borrow_mut()
            .push(Rc::new(callback));
    }

    /// Called with the new connection id after a successful reconnect.
    pub fn on_reconnected(&self, callback: impl Fn(Option<String>) + 'static) {
        self.inner
            .on_reconnected
            .borrow_mut()
            .push(Rc::new(callback));
    }

    /// Called with the error (if any) when the connection closes for good.
    pub fn on_close(&self, callback: impl Fn(Option<String>) + 'static) {
        self.inner.on_close.borrow_mut().push(Rc::new(callback));
    }

    /// Connects (negotiate + WebSocket + handshake).
    pub async fn start(&self) -> Result<(), HubError> {
        let inner = &self.inner;
        if inner.state.get() != HubConnectionState::Disconnected {
            return Err(HubError(
                "Cannot start a HubConnection that is not in the 'Disconnected' state.".into(),
            ));
        }
        inner.state.set(HubConnectionState::Connecting);
        match inner.start_internal().await {
            Ok(()) if inner.state.get() == HubConnectionState::Connecting => {
                inner.state.set(HubConnectionState::Connected);
                Ok(())
            }
            Ok(()) => {
                inner.teardown_socket();
                inner.state.set(HubConnectionState::Disconnected);
                Err(HubError(
                    "The connection was stopped during startup.".into(),
                ))
            }
            Err(error) => {
                inner.state.set(HubConnectionState::Disconnected);
                Err(error)
            }
        }
    }

    /// Stops the connection (and any reconnect in progress); `on_close` fires once closed.
    pub fn stop(&self) {
        let inner = &self.inner;
        inner.reconnect_epoch.set(inner.reconnect_epoch.get() + 1);
        match inner.state.get() {
            HubConnectionState::Disconnected | HubConnectionState::Disconnecting => {}
            HubConnectionState::Connected => {
                inner.state.set(HubConnectionState::Disconnecting);
                inner.close_socket(CloseIntent {
                    error: None,
                    reconnect: false,
                });
            }
            HubConnectionState::Connecting => {
                // `start` observes the state change (or the failed handshake) and reports the error.
                inner.state.set(HubConnectionState::Disconnecting);
                inner.teardown_socket();
            }
            HubConnectionState::Reconnecting => {
                inner.teardown_socket();
                inner.cancel_invocations(None);
                inner.complete_close(None);
            }
        }
    }

    /// Invokes a hub method without waiting for a result.
    pub fn send(&self, target: &str, arguments: Vec<Value>) -> Result<(), HubError> {
        self.ensure_connected()?;
        self.inner.send_message(
            &json!({ "type": MESSAGE_INVOCATION, "target": target, "arguments": arguments }),
        )
    }

    /// Invokes a hub method and waits for its completion result.
    pub async fn invoke(&self, target: &str, arguments: Vec<Value>) -> Result<Value, HubError> {
        self.ensure_connected()?;
        let inner = &self.inner;
        let id = inner.next_invocation_id.get().to_string();
        inner
            .next_invocation_id
            .set(inner.next_invocation_id.get() + 1);

        let (deferred, promise) = Deferred::new();
        inner.invocations.borrow_mut().insert(id.clone(), deferred);
        let message = json!({ "type": MESSAGE_INVOCATION, "invocationId": id, "target": target, "arguments": arguments });
        if let Err(error) = inner.send_message(&message) {
            inner.invocations.borrow_mut().remove(&id);
            return Err(error);
        }

        let result = JsFuture::from(promise).await?;
        let text = result.as_string().unwrap_or_default();
        serde_json::from_str(&text)
            .map_err(|error| HubError(format!("Invalid completion result: {error}")))
    }

    fn ensure_connected(&self) -> Result<(), HubError> {
        if self.inner.state.get() == HubConnectionState::Connected {
            Ok(())
        } else {
            Err(HubError(
                "Cannot send data if the connection is not in the 'Connected' State.".into(),
            ))
        }
    }
}

/// Creates a connection to `{hub_base_url}/{name}`.
pub fn create_hub_connection(name: &str) -> HubConnection {
    HubConnection::new(format!("{}/{name}", hub_base_url()))
}

/// Hub pushing application state, options and version updates.
pub fn create_application_hub() -> HubConnection {
    create_hub_connection("application")
}

/// Hub streaming log entries.
pub fn create_logs_hub() -> HubConnection {
    create_hub_connection("logs")
}

/// Hub streaming search updates.
pub fn create_search_hub() -> HubConnection {
    create_hub_connection("search")
}

/// Hub streaming metrics.
pub fn create_metrics_hub() -> HubConnection {
    create_hub_connection("metrics")
}

/// Hub used by relay agents.
pub fn create_relay_hub() -> HubConnection {
    create_hub_connection("relay")
}

#[cfg(test)]
mod tests {
    use super::negotiate_url;

    #[test]
    fn builds_negotiate_url() {
        assert_eq!(
            negotiate_url("http://host/hub/application"),
            "http://host/hub/application/negotiate?negotiateVersion=1"
        );
        assert_eq!(
            negotiate_url("http://host/hub/"),
            "http://host/hub/negotiate?negotiateVersion=1"
        );
        assert_eq!(
            negotiate_url("http://host/hub?a=b"),
            "http://host/hub/negotiate?a=b&negotiateVersion=1"
        );
    }
}
