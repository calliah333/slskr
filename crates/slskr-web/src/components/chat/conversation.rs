use leptos::{html, prelude::*};
use slskr_api::conversations::{MessageDirection, PrivateMessage};

#[component]
pub fn ConversationView(#[prop(into)] messages: Signal<Vec<PrivateMessage>>) -> impl IntoView {
    let anchor = NodeRef::<html::Div>::new();
    // Scroll to the newest message when the history grows or the conversation changes
    // (upstream Chat.jsx scrolls the list's last child into view after loading).
    let scrolled_to = StoredValue::new(None::<Option<(i64, i32)>>);
    Effect::new(move |_| {
        let last = messages.with(|m| super::last_message_key(m));
        if let Some(anchor) = anchor.get()
            && scrolled_to.get_value() != Some(last)
        {
            scrolled_to.set_value(Some(last));
            anchor.scroll_into_view();
        }
    });
    view! {
        <div class="chat-history">
            <For each=move || messages.get() key=|message| (message.timestamp, message.id) children=move |message| {
                let own = message.direction == MessageDirection::Out;
                let speaker = if own { "You".to_string() } else { message.username.clone() };
                view! { <div class:chat-message-self=own class="chat-message">
                    <time class="chat-message-time">{message.timestamp.format("%m/%d %H:%M").to_string()}</time>
                    <span class="chat-message-name">{speaker} ": "</span><span class="chat-message-message">{message.message}</span>
                </div> }
            }/>
            <div id="chat-history-scroll-anchor" node_ref=anchor></div>
        </div>
    }
}
