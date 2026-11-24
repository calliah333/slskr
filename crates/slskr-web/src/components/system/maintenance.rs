//! System → Maintenance: clearing finished transfers from the lists (formerly "Data").

use leptos::prelude::*;

use crate::{
    services::api,
    ui::{Alert, Card, Icon, Modal, ModalAction},
};

#[component]
pub fn Maintenance() -> impl IntoView {
    let message = RwSignal::new(None::<(&'static str, String)>);
    let busy = RwSignal::new(false);
    let clear = move |direction: &'static str| {
        busy.set(true);
        message.set(None);
        leptos::task::spawn_local(async move {
            let path = format!("/transfers/{direction}s/completed");
            let result = match api::delete::<serde_json::Value>(&path).await {
                Ok(_) => ("success", format!("Completed {direction}s cleared.")),
                Err(err) => ("danger", err.to_string()),
            };
            message.try_set(Some(result));
            busy.try_set(false);
        });
    };
    let action = move |direction: &'static str, title: &'static str| {
        view! {
            <Modal size="sm" title=format!("Clear completed {direction}s?")
                trigger=move || view! {
                    <button type="button" class="btn btn--danger-outline" disabled=move || busy.get()>
                        <Icon name="trash-2"/>{title}
                    </button>
                }
                content=format!(
                    "Finished, cancelled and failed {direction}s are removed from the list. Files on disk are kept."
                )
                actions=vec![
                    ModalAction::new("Cancel"),
                    ModalAction::new("Clear").negative().on_click(move || clear(direction)),
                ]/>
        }
    };
    view! {
        <div class="system-maintenance stack">
            <Card title="Transfer lists"
                description="Remove completed transfers if the Downloads or Uploads pages become slow.">
                <div class="cluster">
                    {action("download", "Clear completed downloads")}
                    {action("upload", "Clear completed uploads")}
                </div>
                <div role="status" class="maintenance-message">
                    {move || message.get().map(|(variant, text)| view! {
                        <Alert variant=variant role="none">{text}</Alert>
                    })}
                </div>
            </Card>
        </div>
    }
}
