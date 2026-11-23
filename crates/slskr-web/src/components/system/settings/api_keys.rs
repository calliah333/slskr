//! Settings → API keys: list, create (the key is shown once) and delete.

use super::fields::use_form;
use crate::{
    services::{api, util::encode_uri_component},
    ui::{
        Modal, ModalAction,
        form::{Field, FieldGrid, FormMessage, ListEditor, Section, Select, TextInput},
    },
};
use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::{
    config::ApiKeyRole,
    settings::{CreateApiKey, CreatedApiKey},
};

fn role_name(role: ApiKeyRole) -> &'static str {
    match role {
        ApiKeyRole::ReadOnly => "readonly",
        ApiKeyRole::ReadWrite => "readwrite",
        ApiKeyRole::Admin => "admin",
    }
}

fn parse_role(text: &str) -> ApiKeyRole {
    match text {
        "readwrite" => ApiKeyRole::ReadWrite,
        "admin" => ApiKeyRole::Admin,
        _ => ApiKeyRole::ReadOnly,
    }
}

#[component]
pub fn ApiKeys() -> impl IntoView {
    let form = use_form();
    let name = RwSignal::new(String::new());
    let role = RwSignal::new("readonly".to_owned());
    let allow = RwSignal::new(Vec::<String>::new());
    let name_error = RwSignal::new(None::<String>);
    let allow_error = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    let creating = RwSignal::new(false);
    let created = RwSignal::new(None::<CreatedApiKey>);
    let copied = RwSignal::new(false);
    let deleting = RwSignal::new(None::<String>);
    let delete_open = Signal::derive(move || deleting.with(Option::is_some));
    let name_ref = NodeRef::<leptos::html::Input>::new();
    let key_ref = NodeRef::<leptos::html::Input>::new();

    let keys = move || {
        form.settings
            .with(|s| s.as_ref().map(|s| s.api_keys.clone()).unwrap_or_default())
    };

    let create = move || {
        let request = CreateApiKey {
            name: name.get_untracked().trim().to_owned(),
            role: parse_role(&role.get_untracked()),
            allow: allow.get_untracked(),
        };
        name_error.set(None);
        allow_error.set(None);
        error.set(None);
        if request.name.is_empty() {
            name_error.set(Some("Enter a name for the key".into()));
            if let Some(input) = name_ref.get_untracked() {
                let _ = input.focus();
            }
            return;
        }
        creating.set(true);
        spawn_local_scoped_with_cancellation(async move {
            match api::post::<_, CreatedApiKey>("/api-keys", &request).await {
                Ok(key) => {
                    created.set(Some(key));
                    copied.set(false);
                    name.set(String::new());
                    allow.set(Vec::new());
                    form.reload().await;
                    request_animation_frame(move || {
                        if let Some(input) = key_ref.get_untracked() {
                            let _ = input.focus();
                            input.select();
                        }
                    });
                }
                Err(failure) if failure.message.starts_with("allow") => {
                    allow_error.set(Some(failure.message));
                }
                Err(failure) if failure.message.starts_with("name") || failure.status == 409 => {
                    name_error.set(Some(failure.message));
                }
                Err(failure) => error.set(Some(failure.message)),
            }
            creating.set(false);
        });
    };

    let copy = move |_| {
        let Some(key) = created.with_untracked(|c| c.as_ref().map(|c| c.key.clone())) else {
            return;
        };
        if let Some(input) = key_ref.get_untracked() {
            input.select();
        }
        let clipboard = window().navigator().clipboard();
        let promise = clipboard.write_text(&key);
        // Without clipboard access the key stays selected for Ctrl+C.
        leptos::task::spawn_local(async move {
            let ok = wasm_bindgen_futures::JsFuture::from(promise).await.is_ok();
            copied.set(ok);
        });
    };

    let confirm_delete = move || {
        let Some(target) = deleting.get_untracked() else {
            return;
        };
        spawn_local_scoped_with_cancellation(async move {
            let path = format!("/api-keys/{}", encode_uri_component(&target));
            match api::request_text(gloo_net::http::Method::DELETE, &path, None).await {
                Ok(_) => {
                    if created.with_untracked(|c| c.as_ref().is_some_and(|c| c.name == target)) {
                        created.set(None);
                    }
                    form.reload().await;
                }
                Err(failure) => error.set(Some(failure.message)),
            }
            deleting.set(None);
        });
    };

    view! {
        <Section id="settings-api-keys" title="API keys"
            description="Keys let scripts use the API with the X-API-Key header. Changes here apply at once.">
            {move || error.get().map(|message| view! { <FormMessage kind="negative"><p>{message}</p></FormMessage> })}
            {move || created.get().map(|key| view! {
                <div class="ui warning message api-key-created" role="alert">
                    <div class="header">{format!("API key {} created", key.name)}</div>
                    <p>"Copy it now: it is shown only this once and can not be recovered."</p>
                    <div class="ui fluid action input">
                        <input node_ref=key_ref type="text" readonly aria-label="New API key"
                            class="api-key-value" prop:value=key.key.clone()/>
                        <button type="button" class="ui button api-key-copy" on:click=copy>
                            <i class="copy icon" aria-hidden="true"></i>
                            {move || if copied.get() { "Copied" } else { "Copy" }}
                        </button>
                    </div>
                    <button type="button" class="ui small basic button api-key-done"
                        on:click=move |_| created.set(None)>
                        "I have saved it"
                    </button>
                </div>
            })}
            <table class="ui celled table api-keys">
                <thead>
                    <tr><th scope="col">"Name"</th><th scope="col">"Role"</th><th scope="col">"Allowed networks"</th><th scope="col"><span class="visually-hidden">"Actions"</span></th></tr>
                </thead>
                <tbody>
                    {move || {
                        let keys = keys();
                        if keys.is_empty() {
                            return view! { <tr><td colspan="4">"No API keys yet."</td></tr> }.into_any();
                        }
                        keys.into_iter().map(|key| {
                            let target = key.name.clone();
                            view! {
                                <tr class="api-key-row">
                                    <td>{key.name.clone()}</td>
                                    <td>{role_name(key.role)}</td>
                                    <td>{key.allow.join(", ")}</td>
                                    <td class="collapsing">
                                        {if key.pinned {
                                            view! { <span class="ui basic label" title="Set in slskr.toml; remove it there">
                                                <i class="lock icon" aria-hidden="true"></i>"slskr.toml"
                                            </span> }.into_any()
                                        } else {
                                            view! {
                                                <button type="button" class="ui mini negative basic button api-key-delete"
                                                    aria-label=format!("Delete API key {}", key.name)
                                                    on:click=move |_| deleting.set(Some(target.clone()))>
                                                    "Delete"
                                                </button>
                                            }.into_any()
                                        }}
                                    </td>
                                </tr>
                            }
                        }).collect_view().into_any()
                    }}
                </tbody>
            </table>
            <form class="api-key-create" on:submit=move |event| { event.prevent_default(); create(); }>
                <h4 class="ui header">"Create a key"</h4>
                <FieldGrid>
                    <Field id="api-key-name" label="Name" error=name_error>
                        <TextInput value=name node_ref=name_ref on_input=move |t: String| name.set(t)
                            placeholder="e.g. soularr"/>
                    </Field>
                    <Field id="api-key-role" label="Role"
                        help="readonly reads; readwrite also searches, downloads and chats; admin also reads logs.">
                        <Select value=role on_change=move |r: String| role.set(r)
                            options=vec![
                                ("readonly".into(), "Read only".into()),
                                ("readwrite".into(), "Read and write".into()),
                                ("admin".into(), "Administrator".into()),
                            ]/>
                    </Field>
                </FieldGrid>
                <Field id="api-key-allow" label="Allowed networks" error=allow_error
                    help="CIDR ranges allowed to use the key; empty allows every address.">
                    <ListEditor items=allow on_change=Callback::new(move |items: Vec<String>| allow.set(items))
                        placeholder="e.g. 192.168.0.0/16"/>
                </Field>
                <button type="submit" class="ui primary button api-key-create-button" class:loading=creating
                    disabled=creating>
                    "Create key"
                </button>
            </form>
            <Modal open=delete_open size="mini"
                on_close=Callback::new(move |()| deleting.set(None))
                header=|| view! { <div class="header">"Delete API key"</div> }
                content="Scripts using this key will stop working. Delete it?"
                actions=vec![
                    ModalAction::new("Cancel"),
                    ModalAction::new("Delete").negative().on_click(confirm_delete),
                ]/>
        </Section>
    }
}
