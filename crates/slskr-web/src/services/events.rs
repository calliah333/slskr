//! Live updates over one WebSocket at `{url_base}/api/events` (see `slskr_api::realtime`).
//!
//! [`Events::start`] opens the connection once the session is known to be valid and
//! provides it as context. Components register handlers per [`Topic`] with
//! [`Events::on`]; the topic is subscribed while at least one handler exists, and each
//! handler is removed when its owner is cleaned up. A subscription first delivers the
//! topic's snapshot, so handlers replace their data on snapshot events and apply the rest.
//!
//! A lost connection is retried after 0 s, 2 s, 10 s, then every 30 s; every topic is
//! subscribed again, which delivers fresh snapshots. A close with
//! [`CLOSE_UNAUTHORIZED`], or a connection refused while `GET /session` answers 401, ends
//! in [`Status::Unauthorized`].

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use gloo_timers::callback::Timeout;
use leptos::prelude::*;
use slskr_api::realtime::{CLOSE_UNAUTHORIZED, Command};
pub use slskr_api::realtime::{Event, Topic};
use wasm_bindgen::{JsCast, closure::Closure};
use wasm_bindgen_futures::spawn_local;
use web_sys::{CloseEvent, MessageEvent, WebSocket};

use super::api;
use crate::config;

/// Delays before reconnect attempts (ms); the last one repeats.
const RECONNECT_DELAYS: [u32; 4] = [0, 2_000, 10_000, 30_000];
/// A connection that stayed open this long restarts the delays from the beginning.
const STABLE_MS: f64 = 10_000.0;

/// Connection status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// The first connection attempt is in progress.
    Connecting,
    Open,
    /// The connection was lost; retrying.
    Reconnecting,
    /// The session ended; sign in again.
    Unauthorized,
}

type Handler = Rc<dyn Fn(&Event)>;

struct Socket {
    ws: WebSocket,
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl Socket {
    fn close(self) {
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
        // May run inside one of the socket's own callbacks: drop the closures afterwards.
        spawn_local(async move { drop(self) });
    }
}

struct Inner {
    status: RwSignal<Status>,
    socket: RefCell<Option<Socket>>,
    /// Incremented per socket; events of replaced sockets are ignored.
    generation: Cell<u64>,
    opened_at: Cell<Option<f64>>,
    attempt: Cell<usize>,
    retry: RefCell<Option<Timeout>>,
    stopped: Cell<bool>,
    handlers: RefCell<Vec<(u64, Topic, Handler)>>,
    next_handler: Cell<u64>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(socket) = self.socket.get_mut().take() {
            socket.close();
        }
    }
}

/// `ws:` or `wss:` URL of the endpoint, following the page's scheme and `url_base`.
fn url() -> String {
    let location = window().location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss:",
        _ => "ws:",
    };
    let host = location.host().unwrap_or_default();
    format!("{scheme}//{host}{}", config::events_path())
}

impl Inner {
    fn is_current(&self, generation: u64) -> bool {
        self.generation.get() == generation && !self.stopped.get()
    }

    fn topics(&self) -> Vec<Topic> {
        let mut topics: Vec<_> = self.handlers.borrow().iter().map(|h| h.1).collect();
        topics.sort_unstable();
        topics.dedup();
        topics
    }

    fn send(&self, command: &Command) {
        let socket = self.socket.borrow();
        let Some(socket) = socket.as_ref() else {
            return;
        };
        if socket.ws.ready_state() != WebSocket::OPEN {
            return;
        }
        if let Ok(text) = serde_json::to_string(command) {
            let _ = socket.ws.send_with_str(&text);
        }
    }

    fn dispatch(&self, event: &Event) {
        let topic = match event {
            Event::Error {
                topic: None,
                message,
            } => {
                leptos::logging::warn!("Live updates: {message}");
                return;
            }
            Event::Error {
                topic: Some(topic), ..
            } => Some(*topic),
            event => event.topic(),
        };
        let handlers: Vec<_> = self
            .handlers
            .borrow()
            .iter()
            .filter(|(_, t, _)| Some(*t) == topic)
            .map(|(_, _, handler)| handler.clone())
            .collect();
        for handler in handlers {
            handler(event);
        }
    }

    fn connect(self: &Rc<Self>) {
        if self.stopped.get() {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let ws = match WebSocket::new(&url()) {
            Ok(ws) => ws,
            Err(error) => {
                leptos::logging::warn!("Could not open the live updates connection: {error:?}");
                self.reconnect();
                return;
            }
        };

        let weak = Rc::downgrade(self);
        let on_open = Closure::<dyn FnMut()>::new(move || {
            let Some(inner) = weak.upgrade().filter(|i| i.is_current(generation)) else {
                return;
            };
            inner.opened_at.set(Some(js_sys::Date::now()));
            inner.status.try_set(Status::Open);
            let topics = inner.topics();
            if !topics.is_empty() {
                inner.send(&Command::Subscribe { topics });
            }
        });
        let weak = Rc::downgrade(self);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
            let Some(inner) = weak.upgrade().filter(|i| i.is_current(generation)) else {
                return;
            };
            let Some(text) = message.data().as_string() else {
                return;
            };
            match serde_json::from_str::<Event>(&text) {
                Ok(event) => inner.dispatch(&event),
                Err(error) => leptos::logging::warn!("Invalid live update: {error}"),
            }
        });
        let weak: Weak<Self> = Rc::downgrade(self);
        let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |close: CloseEvent| {
            let Some(inner) = weak.upgrade().filter(|i| i.is_current(generation)) else {
                return;
            };
            inner.closed(close.code());
        });
        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        let previous = self.socket.replace(Some(Socket {
            ws,
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
        }));
        if let Some(previous) = previous {
            previous.close();
        }
    }

    fn closed(self: &Rc<Self>, code: u16) {
        if let Some(socket) = self.socket.take() {
            socket.close();
        }
        let opened_at = self.opened_at.take();
        if code == CLOSE_UNAUTHORIZED {
            self.unauthorized();
            return;
        }
        if opened_at.is_some_and(|at| js_sys::Date::now() - at >= STABLE_MS) {
            self.attempt.set(0);
        }
        if opened_at.is_some() {
            self.reconnect();
            return;
        }
        // A refused upgrade is indistinguishable from a network error in the browser; ask
        // the session endpoint whether the session is still valid.
        let weak = Rc::downgrade(self);
        let generation = self.generation.get();
        spawn_local(async move {
            let unauthorized =
                matches!(api::get_text("/session").await, Err(error) if error.status == 401);
            let Some(inner) = weak.upgrade().filter(|i| i.is_current(generation)) else {
                return;
            };
            if unauthorized {
                inner.unauthorized();
            } else {
                inner.reconnect();
            }
        });
    }

    fn unauthorized(&self) {
        self.stopped.set(true);
        self.status.try_set(Status::Unauthorized);
    }

    fn reconnect(self: &Rc<Self>) {
        if self.stopped.get() {
            return;
        }
        if self.status.get_untracked() != Status::Connecting {
            self.status.try_set(Status::Reconnecting);
        }
        let attempt = self.attempt.get();
        self.attempt.set(attempt + 1);
        let delay = RECONNECT_DELAYS[attempt.min(RECONNECT_DELAYS.len() - 1)];
        let weak = Rc::downgrade(self);
        let timer = Timeout::new(delay, move || {
            if let Some(inner) = weak.upgrade() {
                inner.retry.take();
                inner.connect();
            }
        });
        if let Some(previous) = self.retry.replace(Some(timer)) {
            previous.cancel();
        }
    }

    fn stop(&self) {
        self.stopped.set(true);
        self.generation.set(self.generation.get() + 1);
        if let Some(timer) = self.retry.take() {
            timer.cancel();
        }
        if let Some(socket) = self.socket.take() {
            socket.close();
        }
    }
}

/// Handle to the live updates connection. Cheap to copy.
#[derive(Clone, Copy)]
pub struct Events {
    inner: StoredValue<Rc<Inner>, LocalStorage>,
    status: RwSignal<Status>,
}

/// The connection provided by [`Events::start`].
pub fn use_events() -> Events {
    expect_context()
}

impl Events {
    /// Opens the connection, provides it as context and closes it when the current owner
    /// is cleaned up.
    pub fn start() -> Self {
        let status = RwSignal::new(Status::Connecting);
        let inner = Rc::new(Inner {
            status,
            socket: RefCell::new(None),
            generation: Cell::new(0),
            opened_at: Cell::new(None),
            attempt: Cell::new(0),
            retry: RefCell::new(None),
            stopped: Cell::new(false),
            handlers: RefCell::new(Vec::new()),
            next_handler: Cell::new(0),
        });
        inner.connect();
        let events = Self {
            inner: StoredValue::new_local(inner),
            status,
        };
        on_cleanup(move || {
            events.inner.try_with_value(|inner| inner.stop());
        });
        provide_context(events);
        events
    }

    pub fn status(&self) -> ReadSignal<Status> {
        self.status.read_only()
    }

    /// Calls `handler` with each event of `topic`, starting with a snapshot, until the
    /// current owner is cleaned up. [`Event::Error`]s naming the topic are passed too.
    pub fn on(&self, topic: Topic, handler: impl Fn(&Event) + 'static) {
        let Some(id) = self.inner.try_with_value(|inner| {
            let id = inner.next_handler.get();
            inner.next_handler.set(id + 1);
            inner
                .handlers
                .borrow_mut()
                .push((id, topic, Rc::new(handler)));
            // Subscribing again sends the snapshot this handler needs.
            inner.send(&Command::Subscribe {
                topics: vec![topic],
            });
            id
        }) else {
            return;
        };
        let inner = self.inner;
        on_cleanup(move || {
            inner.try_with_value(|inner| {
                let remaining = {
                    let mut handlers = inner.handlers.borrow_mut();
                    handlers.retain(|(other, _, _)| *other != id);
                    handlers.iter().any(|(_, other, _)| *other == topic)
                };
                if !remaining {
                    inner.send(&Command::Unsubscribe {
                        topics: vec![topic],
                    });
                }
            });
        });
    }
}
