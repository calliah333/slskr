//! The IRC-style buffer shared by Chat and Rooms: a monospace line log (time, nick
//! column, text with a hanging indent, muted event lines, day changes), the input line
//! with slash commands, and small helpers. Styled by `assets/static/css/pages/chat.css`.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Local, NaiveDate, Utc};
use leptos::{ev::KeyboardEvent, html, prelude::*};

use crate::{
    components::app::AppContext,
    services::{api, util},
    ui::{Alert, Icon},
};

/// Sets `signal` only when the value differs, so unchanged polls do not re-render.
pub(crate) fn set_if_changed<T: PartialEq + Send + Sync + 'static>(signal: RwSignal<T>, value: T) {
    if signal.try_with_untracked(|current| *current != value) == Some(true) {
        let _ = signal.try_set(value);
    }
}

/// Whether the Soulseek server session is logged in (sending and acknowledging need it).
pub(crate) fn use_logged_in() -> Signal<bool> {
    use_context::<AppContext>()
        .map(|context| context.state)
        .map(|state| Signal::derive(move || state.with(|s| s.server.state.is_logged_in())))
        .unwrap_or_default()
}

/// Our Soulseek username, once known.
pub(crate) fn use_own_nick() -> Signal<String> {
    use_context::<AppContext>()
        .map(|context| context.state)
        .map(|state| {
            Signal::derive(move || {
                state.with(|s| s.user.username.clone().unwrap_or_else(|| "me".to_owned()))
            })
        })
        .unwrap_or_else(|| Signal::derive(|| "me".to_owned()))
}

pub(crate) const OFFLINE_REASON: &str = "Not connected to the Soulseek server";

/// `14:05` in local time.
pub(crate) fn clock(at: &DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

/// The full local timestamp, for `title` attributes.
pub(crate) fn full_time(at: &DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%A, %-d %B %Y %H:%M:%S")
        .to_string()
}

/// A stable colour slot (`nick-0` … `nick-11`) for a nickname.
pub(crate) fn nick_color(nick: &str) -> String {
    // FNV-1a, as the owner's IRC client does: deterministic across sessions.
    let hash = nick.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    format!("nick-{}", hash % 12)
}

/// How an event line looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// `*` lines: joins, parts, ticker changes, command output.
    Info,
    /// `--` lines: connection changes.
    Status,
    /// `!` lines in the error colour.
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Body {
    Message { nick: String, text: String },
    Event { tone: Tone, text: String },
}

/// One line of a buffer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    /// Unique and stable (the list's `For` key).
    pub key: String,
    pub at: DateTime<Utc>,
    /// Sent by us (messages) or caused by us (local event lines): keeps the view at the
    /// bottom.
    pub own: bool,
    pub body: Body,
}

impl Entry {
    pub fn message(key: String, at: DateTime<Utc>, nick: String, own: bool, text: String) -> Self {
        Self {
            key,
            at,
            own,
            body: Body::Message { nick, text },
        }
    }
}

/// Local event lines of a page: `(buffer, entry)`; a `None` buffer shows in every buffer.
pub(crate) type Events = RwSignal<Vec<(Option<String>, Entry)>>;

/// Adds an event line now.
pub(crate) fn push_event(
    events: Events,
    buffer: Option<String>,
    tone: Tone,
    own: bool,
    text: impl Into<String>,
) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let entry = Entry {
        key: format!("event:{}", NEXT.fetch_add(1, Ordering::Relaxed)),
        at: Utc::now(),
        own,
        body: Body::Event {
            tone,
            text: text.into(),
        },
    };
    events.try_update(|events| events.push((buffer, entry)));
}

/// The messages of a buffer with its event lines, in time order.
pub(crate) fn merge(mut messages: Vec<Entry>, events: Events, buffer: &str) -> Vec<Entry> {
    events.with(|events| {
        messages.extend(
            events
                .iter()
                .filter(|(scope, _)| scope.as_deref().is_none_or(|s| s == buffer))
                .map(|(_, entry)| entry.clone()),
        );
    });
    messages.sort_by_key(|entry| entry.at);
    messages
}

/// Rows the buffer renders: day changes and lines.
#[derive(Clone, Debug, PartialEq)]
enum Row {
    Day(NaiveDate),
    Line { entry: Entry, repeat: bool },
}

impl Row {
    fn key(&self) -> String {
        match self {
            Row::Day(day) => format!("day:{day}"),
            Row::Line { entry, repeat } => format!("{}:{repeat}", entry.key),
        }
    }
}

fn local_day(at: &DateTime<Utc>) -> NaiveDate {
    at.with_timezone(&Local).date_naive()
}

fn rows(entries: &[Entry]) -> Vec<Row> {
    let mut out = Vec::with_capacity(entries.len() + 4);
    let mut previous: Option<&Entry> = None;
    for entry in entries {
        let day = local_day(&entry.at);
        let new_day = previous.is_none_or(|p| local_day(&p.at) != day);
        // The first line is preceded by its day so the date of old history is clear.
        if new_day {
            out.push(Row::Day(day));
        }
        let repeat = !new_day
            && match (&entry.body, previous.map(|p| &p.body)) {
                (Body::Message { nick, .. }, Some(Body::Message { nick: last, .. })) => {
                    nick == last
                }
                _ => false,
            };
        out.push(Row::Line {
            entry: entry.clone(),
            repeat,
        });
        previous = Some(entry);
    }
    out
}

/// Within this distance of the end the log counts as scrolled to the bottom.
const BOTTOM_SLACK_PX: i32 = 48;

/// The scrolling line log of one buffer.
///
/// Keeps the newest line in view while the reader is at the bottom; when they have
/// scrolled up, new lines show a "New messages" pill instead of moving the log.
/// Changing `scope` (another buffer) jumps to the newest line.
#[component]
pub(crate) fn BufferView(
    #[prop(into)] entries: Signal<Vec<Entry>>,
    #[prop(into)] scope: Signal<String>,
    /// Our nickname (emphasised).
    #[prop(into)]
    own_nick: Signal<String>,
    /// Accessible name of the log.
    #[prop(into)]
    label: Signal<String>,
    /// Shown when the buffer has no lines.
    empty: ViewFn,
) -> impl IntoView {
    let rows = Memo::new(move |_| entries.with(|entries| rows(entries)));
    let is_empty = Memo::new(move |_| entries.with(Vec::is_empty));
    let scroller = NodeRef::<html::Div>::new();
    let end = NodeRef::<html::Div>::new();
    let pinned = StoredValue::new(true);
    let unseen = RwSignal::new(false);
    let seen = StoredValue::new((None::<String>, None::<String>));

    let jump_to_bottom = move || {
        request_animation_frame(move || {
            if let Some(el) = scroller.try_get_untracked().flatten() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    };
    Effect::new(move |_| {
        let scope = scope.get();
        let last = entries.with(|entries| entries.last().map(|e| (e.key.clone(), e.own)));
        let (previous_scope, previous_last) = seen.get_value();
        seen.set_value((Some(scope.clone()), last.as_ref().map(|l| l.0.clone())));
        let Some((last_key, last_own)) = last else {
            pinned.set_value(true);
            unseen.set(false);
            return;
        };
        if previous_scope.as_deref() != Some(scope.as_str()) || previous_last.is_none() {
            pinned.set_value(true);
            unseen.set(false);
            jump_to_bottom();
        } else if previous_last.as_deref() != Some(last_key.as_str()) {
            if pinned.get_value() || last_own {
                jump_to_bottom();
            } else {
                unseen.set(true);
            }
        }
    });
    let on_scroll = move |_| {
        if let Some(el) = scroller.get_untracked() {
            let distance = el.scroll_height() - el.scroll_top() - el.client_height();
            let at_bottom = distance < BOTTOM_SLACK_PX;
            pinned.set_value(at_bottom);
            if at_bottom && unseen.get_untracked() {
                unseen.set(false);
            }
        }
    };
    let show_newest = move |_| {
        unseen.set(false);
        if let Some(end) = end.get_untracked() {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_behavior(web_sys::ScrollBehavior::Smooth);
            end.scroll_into_view_with_scroll_into_view_options(&options);
        }
    };

    view! {
        <div class="buffer-wrap">
            <div class="buffer" node_ref=scroller role="log" aria-label=move || label.get()
                tabindex="0" on:scroll=on_scroll>
                <Show when=move || !is_empty.get() fallback=move || empty.run()>
                    <For each=move || rows.get() key=Row::key children=move |row| match row {
                        Row::Day(day) => view! {
                            <div class="line line--day" role="separator">
                                {format!("\u{2014} {} \u{2014}", day.format("%A, %-d %B %Y"))}
                            </div>
                        }.into_any(),
                        Row::Line { entry, repeat } => line_view(entry, repeat, own_nick).into_any(),
                    }/>
                </Show>
                <div class="buffer__end" node_ref=end aria-hidden="true"></div>
            </div>
            <Show when=move || unseen.get()>
                <button type="button" class="buffer__pill" on:click=show_newest>
                    "New messages"<Icon name="arrow-down" size="sm"/>
                </button>
            </Show>
        </div>
    }
}

fn line_view(entry: Entry, repeat: bool, own_nick: Signal<String>) -> impl IntoView {
    let title = full_time(&entry.at);
    let datetime = entry.at.to_rfc3339();
    let time = view! {
        <time class="line__time" datetime=datetime title=title>{clock(&entry.at)}</time>
    };
    match entry.body {
        Body::Message { nick, text } => {
            let color = nick_color(&nick);
            let own_nick_name = nick.clone();
            let title_nick = nick.clone();
            let own = entry.own;
            let is_own = move || own || own_nick.with(|o| *o == own_nick_name);
            view! {
                <div class="line line--msg" class:line--own=is_own class:line--repeat=repeat>
                    {time}
                    <span class="line__from"><span class=format!("line__nick {color}") title=title_nick>{nick}</span></span>
                    <span class="line__text">{text}</span>
                </div>
            }
            .into_any()
        }
        Body::Event { tone, text } => {
            let (class, marker) = match tone {
                Tone::Info => ("line line--event", "*"),
                Tone::Status => ("line line--event line--status", "--"),
                Tone::Error => ("line line--event line--error", "!"),
            };
            view! {
                <div class=class role=(tone == Tone::Error).then_some("alert")>
                    {time}
                    <span class="line__from" aria-hidden="true">{marker}</span>
                    <span class="line__text">{text}</span>
                </div>
            }
            .into_any()
        }
    }
}

/// The input line under a buffer: our nick, a textarea that grows to about five lines
/// (Enter sends, Shift+Enter adds a line, Up on an empty line recalls the last one).
#[component]
pub(crate) fn InputLine(
    draft: RwSignal<String>,
    /// Gets the text (the draft is cleared first).
    #[prop(into)]
    on_submit: Callback<String>,
    /// Shown as a hint (e.g. not logged in); the line stays usable for commands.
    #[prop(into)]
    offline_reason: Signal<Option<String>>,
    #[prop(into)] placeholder: Signal<String>,
    #[prop(into)] nick: Signal<String>,
    /// The textarea's id (a hook for tests and for focusing it).
    #[prop(into)]
    input_id: String,
) -> impl IntoView {
    let textarea = NodeRef::<html::Textarea>::new();
    let history = StoredValue::new(Vec::<String>::new());
    let can_send = move || draft.with(|d| !d.trim().is_empty());
    let resize = move || {
        if let Some(el) = textarea.get_untracked() {
            let _ = el.set_attribute("style", "height:auto");
            let height = el.scroll_height();
            let _ = el.set_attribute("style", &format!("height:{height}px"));
        }
    };
    // Also resizes after the draft is cleared by sending.
    Effect::new(move |_| {
        draft.track();
        resize();
    });
    let send = move || {
        if !can_send() {
            return;
        }
        let text = draft.get_untracked();
        history.update_value(|h| {
            h.push(text.clone());
            if h.len() > 50 {
                h.remove(0);
            }
        });
        draft.set(String::new());
        on_submit.run(text);
    };
    let on_keydown = move |ev: KeyboardEvent| {
        if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() {
            ev.prevent_default();
            send();
        } else if ev.key() == "ArrowUp"
            && draft.with_untracked(String::is_empty)
            && let Some(last) = history.with_value(|h| h.last().cloned())
        {
            ev.prevent_default();
            draft.set(last);
        }
    };
    let hint_id = format!("{input_id}-hint");
    view! {
        <form class="inputline" on:submit=move |ev| { ev.prevent_default(); send(); }>
            <div class="inputline__box">
                <span class=move || format!("inputline__nick {}", nick_color(&nick.get())) title="Your nickname">
                    {move || nick.get()}
                </span>
                <textarea
                    id=input_id
                    node_ref=textarea
                    class="inputline__input"
                    rows="1"
                    spellcheck="true"
                    aria-label=move || placeholder.get()
                    aria-describedby=hint_id.clone()
                    placeholder=move || placeholder.get()
                    prop:value=move || draft.get()
                    on:input=move |ev| draft.set(event_target_value(&ev))
                    on:keydown=on_keydown
                ></textarea>
                <button type="submit" class="btn btn--ghost btn--icon btn--sm inputline__send"
                    aria-label="Send" title="Send" disabled=move || !can_send()>
                    <Icon name="send"/>
                </button>
            </div>
            <p class="inputline__hint" id=hint_id>
                {move || match offline_reason.get() {
                    Some(reason) => view! {
                        <span class="inputline__offline"><Icon name="wifi-off" size="xs"/>{reason}</span>
                    }.into_any(),
                    None => view! {
                        <span class="hide-sm"><kbd class="kbd">"Enter"</kbd>" send \u{b7} "
                        <kbd class="kbd">"Shift+Enter"</kbd>" new line \u{b7} "
                        <kbd class="kbd">"\u{2191}"</kbd>" last message \u{b7} "
                        <code>"/help"</code>" commands"</span>
                    }.into_any(),
                }}
            </p>
        </form>
    }
}

/// A slash command typed in the input line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    /// Plain text to send (`//text` sends `/text`).
    Say(String),
    Join(String),
    Part,
    Query(String),
    Msg(String, String),
    Help,
    /// A problem to show as an error line.
    Invalid(String),
}

pub(crate) const HELP: [&str; 6] = [
    "Commands:",
    "/join <room>  join a room (a leading # is optional)",
    "/part  leave this room, or close this conversation",
    "/query <nick>  open a private conversation",
    "/msg <nick> <text>  send a private message",
    "//text  send text that starts with /",
];

pub(crate) fn parse_command(text: &str) -> Command {
    let Some(rest) = text.strip_prefix('/') else {
        return Command::Say(text.to_owned());
    };
    if rest.starts_with('/') {
        return Command::Say(rest.to_owned());
    }
    let (name, args) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(name, args)| (name, args.trim()));
    match name.to_lowercase().as_str() {
        "join" | "j" => {
            let room = args.strip_prefix('#').unwrap_or(args).trim();
            if room.is_empty() {
                Command::Invalid("Usage: /join <room>".to_owned())
            } else {
                Command::Join(room.to_owned())
            }
        }
        "part" | "leave" | "close" => Command::Part,
        "query" | "q" => {
            if args.is_empty() {
                Command::Invalid("Usage: /query <nick>".to_owned())
            } else {
                Command::Query(args.to_owned())
            }
        }
        "msg" | "m" => match args.split_once(char::is_whitespace) {
            Some((nick, text)) if !text.trim().is_empty() => {
                Command::Msg(nick.to_owned(), text.trim().to_owned())
            }
            _ => Command::Invalid("Usage: /msg <nick> <text>".to_owned()),
        },
        "help" | "?" => Command::Help,
        "me" => {
            Command::Invalid("/me is not supported: Soulseek has no action messages".to_owned())
        }
        other => Command::Invalid(format!("Unknown command /{other}; try /help")),
    }
}

/// Joins a room on the server.
pub(crate) async fn join_room(name: &str) -> Result<(), String> {
    api::post_text(
        "/rooms/joined",
        &slskr_types::rooms::JoinRoom {
            name: name.to_owned(),
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Sends a private message.
pub(crate) async fn send_private(nick: &str, text: &str) -> Result<(), String> {
    let path = format!(
        "/conversations/{}/messages",
        util::encode_uri_component(nick)
    );
    api::post_text(
        &path,
        &slskr_types::conversations::SendMessage {
            message: text.to_owned(),
        },
    )
    .await
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Adds "Connected"/"Disconnected" lines to every buffer when the server session changes.
pub(crate) fn announce_connection(events: Events, logged_in: Signal<bool>) {
    let previous = StoredValue::new(None::<bool>);
    Effect::new(move |_| {
        let now = logged_in.get();
        if let Some(before) = previous.get_value()
            && before != now
        {
            let text = if now {
                "Connected to the Soulseek server"
            } else {
                "Disconnected from the Soulseek server"
            };
            push_event(events, None, Tone::Status, false, text);
        }
        previous.set_value(Some(now));
    });
}

/// The latest polling problem, while it lasts (action errors go into the buffer).
#[component]
pub(crate) fn PollProblem(poll_error: RwSignal<Option<String>>) -> impl IntoView {
    view! {
        {move || poll_error.get().map(|e| view! {
            <div class="messenger__alerts">
                <Alert variant="warning" title="Could not refresh"><p>{e}</p></Alert>
            </div>
        })}
    }
}

/// Focuses an element by id once it is on screen.
pub(crate) fn focus_later(id: &'static str) {
    set_timeout(
        move || {
            if let Some(el) = document().get_element_by_id(id)
                && let Ok(el) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(el)
            {
                let _ = el.focus();
            }
        },
        std::time::Duration::from_millis(50),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn message(key: &str, nick: &str, minute: u32) -> Entry {
        Entry::message(
            key.into(),
            Utc.with_ymd_and_hms(2026, 11, 22, 12, minute, 0).unwrap(),
            nick.into(),
            false,
            "x".into(),
        )
    }

    #[test]
    fn marks_repeated_nicks() {
        let entries = [
            message("a", "dave", 0),
            message("b", "dave", 1),
            message("c", "eve", 2),
        ];
        let repeats: Vec<bool> = rows(&entries)
            .into_iter()
            .filter_map(|row| match row {
                Row::Line { repeat, .. } => Some(repeat),
                Row::Day(_) => None,
            })
            .collect();
        assert_eq!(repeats, [false, true, false]);
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("hello"), Command::Say("hello".into()));
        assert_eq!(parse_command("//etc"), Command::Say("/etc".into()));
        assert_eq!(
            parse_command("/join #indie rock"),
            Command::Join("indie rock".into())
        );
        assert_eq!(
            parse_command("/msg bob hi there"),
            Command::Msg("bob".into(), "hi there".into())
        );
        assert_eq!(parse_command("/query bob"), Command::Query("bob".into()));
        assert_eq!(parse_command("/part"), Command::Part);
        assert!(matches!(parse_command("/me waves"), Command::Invalid(_)));
        assert!(matches!(parse_command("/nope"), Command::Invalid(_)));
        assert!(matches!(parse_command("/msg bob"), Command::Invalid(_)));
    }

    #[test]
    fn nick_colors_are_stable() {
        assert_eq!(nick_color("dave"), nick_color("dave"));
    }
}
