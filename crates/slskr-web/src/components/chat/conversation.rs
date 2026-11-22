use leptos::prelude::*;
use slskr_api::conversations::{Conversation, MessageDirection};

#[component]
pub fn ConversationView(conversation: Conversation) -> impl IntoView {
    let messages = conversation.messages.unwrap_or_default();
    view! {
        <div class="chat-history">
            <For each=move || messages.clone() key=|message| format!("{}:{}", message.timestamp, message.id) children=move |message| {
                let own = message.direction == MessageDirection::Out;
                let speaker = if own { "You".to_string() } else { message.username.clone() };
                view! { <div class:chat-message-self=own class="chat-message">
                    <time class="chat-message-time">{message.timestamp.format("%m/%d %H:%M").to_string()}</time>
                    <span class="chat-message-name">{speaker} ": "</span><span class="chat-message-message">{message.message}</span>
                </div> }
            }/>
            <div id="chat-history-scroll-anchor"></div>
        </div>
    }
}
