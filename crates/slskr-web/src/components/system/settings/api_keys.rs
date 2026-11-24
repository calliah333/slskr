//! Settings → API keys: list, create (the key is shown once, in the dialog) and delete.

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::{
    config::ApiKeyRole,
    settings::{CreateApiKey, CreatedApiKey},
};

use super::{account::ConfigBadge, fields::use_form};
use crate::{
    services::{api, util::encode_uri_component},
    ui::{
        Badge, EmptyState, Icon, Modal, ModalAction,
        form::{ChipInput, Field, FormMessage, Section, Select, TextInput},
    },
};

fn role_badge(role: ApiKeyRole) -> &'static str {
    match role {
        ApiKeyRole::ReadOnly => "neutral",
        ApiKeyRole::ReadWrite => "primary",
        ApiKeyRole::Admin => "purple",
    }
}

fn role_name(role: ApiKeyRole) -> &'static str {
    match role {
        ApiKeyRole::ReadOnly => "Read only",
        ApiKeyRole::ReadWrite => "Read & write",
        ApiKeyRole::Admin => "Admin",
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
    let open = RwSignal::new(false);
    let deleting = RwSignal::new(None::<String>);
    let delete_open = Signal::derive(move || deleting.with(Option::is_some));
    let error = RwSignal::new(None::<String>);

    let keys = move || {
        form.settings
            .with(|s| s.as_ref().map(|s| s.api_keys.clone()).unwrap_or_default())
    };

    let confirm_delete = move || {
        let Some(target) = deleting.get_untracked() else {
            return;
        };
        spawn_local_scoped_with_cancellation(async move {
            let path = format!("/api-keys/{}", encode_uri_component(&target));
            match api::request_text(gloo_net::http::Method::DELETE, &path, None).await {
                Ok(_) => form.reload().await,
                Err(failure) => error.set(Some(failure.message)),
            }
            deleting.set(None);
        });
    };

    let new_key = move |_| {
        error.set(None);
        open.set(true);
    };

    view! {
        <Section id="settings-api-keys" title="API keys"
            description="Let scripts use the API with the X-API-Key header. Changes apply at once."
            actions=ViewFn::from(move || view! {
                <button type="button" class="btn btn--sm api-key-new" on:click=new_key>
                    <Icon name="plus"/>"Create key"
                </button>
            })>
            {move || error.get().map(|message| view! { <FormMessage kind="danger"><p>{message}</p></FormMessage> })}
            {move || {
                let keys = keys();
                if keys.is_empty() {
                    return view! {
                        <EmptyState icon="key" title="No API keys yet"
                            description="Create a key for each script or tool that talks to slskr."/>
                    }.into_any();
                }
                view! {
                    <div class="table-wrap settings-table">
                        <table class="table api-keys">
                            <thead>
                                <tr>
                                    <th scope="col">"Name"</th>
                                    <th scope="col">"Role"</th>
                                    <th scope="col">"Allowed networks"</th>
                                    <th scope="col" class="actions"><span class="sr-only">"Actions"</span></th>
                                </tr>
                            </thead>
                            <tbody>
                                {keys.into_iter().map(|key| {
                                    let target = key.name.clone();
                                    let networks = if key.allow.is_empty() {
                                        view! { <span class="muted">"Any address"</span> }.into_any()
                                    } else {
                                        key.allow.iter().map(|cidr| view! {
                                            <code class="settings-cidr">{cidr.clone()}</code>
                                        }).collect_view().into_any()
                                    };
                                    view! {
                                        <tr class="api-key-row">
                                            <td class="font-medium">
                                                <span class="row"><Icon name="key" class="muted"/>{key.name.clone()}</span>
                                            </td>
                                            <td><Badge variant=role_badge(key.role) text=role_name(key.role)/></td>
                                            <td><span class="cluster">{networks}</span></td>
                                            <td class="actions">
                                                {if key.pinned {
                                                    view! { <ConfigBadge title="Set in slskr.toml; remove it there"/> }.into_any()
                                                } else {
                                                    view! {
                                                        <button type="button" class="btn btn--action is-danger api-key-delete"
                                                            aria-label=format!("Delete API key {}", key.name)
                                                            title="Delete"
                                                            on:click=move |_| deleting.set(Some(target.clone()))>
                                                            <Icon name="trash-2"/>
                                                        </button>
                                                    }.into_any()
                                                }}
                                            </td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    </div>
                }.into_any()
            }}
            <CreateKeyDialog open/>
            <Modal open=delete_open size="sm"
                on_close=Callback::new(move |()| deleting.set(None))
                title="Delete API key?"
                content="Scripts using this key stop working at once. This can not be undone."
                actions=vec![
                    ModalAction::new("Cancel"),
                    ModalAction::new("Delete").negative().on_click(confirm_delete),
                ]/>
        </Section>
    }
}

/// The Create key dialog; after creating, the same dialog shows the key once.
#[component]
fn CreateKeyDialog(open: RwSignal<bool>) -> impl IntoView {
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
    let name_ref = NodeRef::<leptos::html::Input>::new();
    let key_ref = NodeRef::<leptos::html::Input>::new();

    let close = move || {
        open.set(false);
        created.set(None);
        name_error.set(None);
        allow_error.set(None);
        error.set(None);
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
                    role.set("readonly".to_owned());
                    allow.set(Vec::new());
                    form.reload().await;
                    request_animation_frame(move || {
                        if let Some(input) = key_ref.try_get_untracked().flatten() {
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
        let promise = window().navigator().clipboard().write_text(&key);
        // Without clipboard access the key stays selected for Ctrl+C.
        leptos::task::spawn_local(async move {
            let ok = wasm_bindgen_futures::JsFuture::from(promise).await.is_ok();
            copied.set(ok);
        });
    };

    view! {
        <Modal open=open on_close=Callback::new(move |()| close())
            title="Create API key" class="api-key-dialog"
            description="Keys are shown once. Give each script its own key."
            footer=ViewFn::from(move || view! {
                <Show when=move || created.with(Option::is_none)
                    fallback=move || view! {
                        <button type="button" class="btn btn--primary api-key-done" on:click=move |_| close()>
                            <Icon name="check"/>"Done"
                        </button>
                    }>
                    <button type="button" class="btn" on:click=move |_| close()>"Cancel"</button>
                    <button type="submit" form="api-key-form" class="btn btn--primary api-key-create-button"
                        disabled=creating aria-busy=move || creating.get().then_some("true")>
                        {move || if creating.get() {
                            view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                        } else {
                            view! { <Icon name="key"/> }.into_any()
                        }}
                        "Create key"
                    </button>
                </Show>
            })>
            {move || match created.get() {
                Some(key) => view! {
                    <div class="stack api-key-created">
                        <div class="alert alert--warning" role="alert">
                            <Icon name="triangle-alert"/>
                            <div class="alert__body">
                                <div class="alert__title">{format!("Copy the key for {} now", key.name)}</div>
                                <p>"It is shown only this once and can not be recovered. Store it in your script's configuration."</p>
                            </div>
                        </div>
                        <div class="input-group">
                            <input node_ref=key_ref type="text" readonly aria-label="New API key"
                                class="input input--mono api-key-value" prop:value=key.key.clone()/>
                            <button type="button" class="btn api-key-copy" on:click=copy>
                                <Icon name=Signal::derive(move || if copied.get() { "check" } else { "copy" }.to_owned())/>
                                {move || if copied.get() { "Copied" } else { "Copy" }}
                            </button>
                        </div>
                        <p class="help">"Send it in the X-API-Key header."</p>
                    </div>
                }.into_any(),
                None => view! {
                    <form id="api-key-form" class="stack api-key-create" novalidate
                        on:submit=move |event| { event.prevent_default(); create(); }>
                        {move || error.get().map(|message| view! { <FormMessage kind="danger"><p>{message}</p></FormMessage> })}
                        <Field id="api-key-name" label="Name" error=name_error
                            help="To recognise the key, e.g. the script using it.">
                            <TextInput value=name node_ref=name_ref on_input=move |t: String| name.set(t)
                                placeholder="e.g. soularr"/>
                        </Field>
                        <Field id="api-key-role" label="Role"
                            help="Read only reads; read & write also searches, downloads and chats; admin also reads logs.">
                            <Select value=role on_change=move |r: String| role.set(r)
                                options=vec![
                                    ("readonly".into(), "Read only".into()),
                                    ("readwrite".into(), "Read & write".into()),
                                    ("admin".into(), "Admin".into()),
                                ]/>
                        </Field>
                        <Field id="api-key-allow" label="Allowed networks" error=allow_error
                            help="CIDR ranges that may use the key; none allows every address.">
                            <ChipInput items=allow mono=true placeholder="e.g. 192.168.0.0/16, then Enter"
                                on_change=Callback::new(move |items: Vec<String>| allow.set(items))/>
                        </Field>
                    </form>
                }.into_any(),
            }}
        </Modal>
    }
}
