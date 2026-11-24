//! State of the settings form: the loaded `GET /api/settings`, the changed keys and the
//! errors, keyed by dotted setting keys (`uploads.slots`, `groups.friends.members`).
//!
//! Field values are JSON values. A field shows its draft value when changed, else the
//! loaded value, else the default (keys of user-defined groups that are not saved yet).
//! Saving sends one sparse `PATCH` built from the draft; live `settings` updates replace the
//! loaded values, so fields that are not being edited follow them.

use std::collections::BTreeMap;

use leptos::prelude::*;
use serde_json::{Map, Value};
use slskr_types::{
    config::{Config, GroupConfig, duration},
    settings::Settings,
};

use crate::services::{api, forms};

/// Groups with fixed names: `[groups.default]` and `[groups.leechers]`.
pub const BUILT_IN_GROUPS: [&str; 2] = ["default", "leechers"];

/// How a field's text is turned into a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parse {
    /// Any text.
    Text,
    /// A whole number in `min..=max`.
    Number { min: u64, max: u64 },
    /// A duration such as `30s`; `optional` allows empty ("never").
    Duration { optional: bool },
}

impl Parse {
    /// The JSON value for `text`, or why it is invalid.
    pub fn value(self, text: &str) -> Result<Value, String> {
        let trimmed = text.trim();
        match self {
            Self::Text => Ok(Value::String(text.to_owned())),
            Self::Number { min, max } => match trimmed.parse::<u64>() {
                Ok(n) if (min..=max).contains(&n) => Ok(Value::from(n)),
                Ok(_) if max == u64::from(u32::MAX) => Err(format!("Enter a number from {min} up")),
                _ if trimmed.is_empty() => Err("Enter a number".to_owned()),
                Ok(_) => Err(format!("Enter a number from {min} to {max}")),
                Err(_) => Err("Enter a whole number".to_owned()),
            },
            Self::Duration { optional: true } if trimmed.is_empty() => {
                Ok(Value::String(String::new()))
            }
            Self::Duration { .. } => duration::parse(trimmed)
                .map(|d| Value::String(duration::format(d)))
                .map_err(|_| {
                    "Enter a duration such as 30s, 5m, 2h, 7d or 1h30m (units ms, s, m, h, d, w)"
                        .to_owned()
                }),
        }
    }
}

/// Text for a JSON value in a text field.
pub fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A duration as people write it: whole days stay days (`30d`, where the daemon writes
/// `4w2d`); anything else as the daemon formats it.
pub fn friendly_duration(text: &str) -> String {
    const DAY: u128 = 86_400_000;
    match duration::parse(text) {
        Ok(d)
            if d.as_millis() > 0 && d.as_millis() % DAY == 0 && d.as_millis() % (7 * DAY) != 0 =>
        {
            format!("{}d", d.as_millis() / DAY)
        }
        _ => text.to_owned(),
    }
}

/// Element id of the control for `key`.
pub fn field_id(key: &str) -> String {
    let mut id = String::from("setting-");
    id.extend(
        key.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }),
    );
    id
}

fn pointer(key: &str) -> String {
    key.split('.')
        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
        .collect()
}

/// The default value of `key` (user-defined groups use the group defaults).
fn default_value(key: &str) -> Option<Value> {
    let parts: Vec<&str> = key.split('.').collect();
    match parts.as_slice() {
        ["groups", name, rest @ ..] if !BUILT_IN_GROUPS.contains(name) => {
            let group = serde_json::to_value(GroupConfig::default()).ok()?;
            group.pointer(&pointer(&rest.join("."))).cloned()
        }
        _ => Config::default()
            .public_json()
            .pointer(&pointer(key))
            .cloned(),
    }
}

/// Inserts `value` at the dotted `key` of a JSON object tree.
fn insert(tree: &mut Map<String, Value>, key: &str, value: Value) {
    let mut parts = key.split('.').peekable();
    let mut node = tree;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            node.insert(part.to_owned(), value);
            return;
        }
        let child = node
            .entry(part.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        if !child.is_object() {
            *child = Value::Object(Map::new());
        }
        node = child.as_object_mut().expect("object");
    }
}

/// True when `a` is `b` or a key above or below it.
fn related(a: &str, b: &str) -> bool {
    a == b
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('.'))
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('.'))
}

/// The settings form.
#[derive(Clone, Copy)]
pub struct Form {
    /// The last `GET /api/settings` (or `PATCH`) response.
    pub settings: RwSignal<Option<Settings>>,
    /// `settings.values` as JSON (secrets left out).
    original: RwSignal<Value>,
    /// Changed keys and their new values; `null` removes (a user-defined group).
    pub draft: RwSignal<BTreeMap<String, Value>>,
    /// Text typed into number and duration fields, kept while editing.
    raw: RwSignal<BTreeMap<String, String>>,
    /// Problems found before saving; they block saving.
    pub client_errors: RwSignal<BTreeMap<String, String>>,
    /// Problems the daemon reported for the last save.
    pub server_errors: RwSignal<BTreeMap<String, String>>,
    /// An error that names no field.
    pub top_error: RwSignal<Option<String>>,
    pub notice: RwSignal<Option<String>>,
    pub saving: RwSignal<bool>,
    reload_revision: RwSignal<u64>,
    /// Keys of the fields on the page, in display order.
    fields: StoredValue<Vec<String>>,
}

impl Form {
    pub fn new() -> Self {
        Self {
            settings: RwSignal::new(None),
            original: RwSignal::new(Value::Null),
            draft: RwSignal::new(BTreeMap::new()),
            raw: RwSignal::new(BTreeMap::new()),
            client_errors: RwSignal::new(BTreeMap::new()),
            server_errors: RwSignal::new(BTreeMap::new()),
            top_error: RwSignal::new(None),
            notice: RwSignal::new(None),
            saving: RwSignal::new(false),
            reload_revision: RwSignal::new(0),
            fields: StoredValue::new(Vec::new()),
        }
    }

    /// Shows a fresh `GET /api/settings`: changes that now match it are no longer changes.
    pub fn load(&self, settings: Settings) {
        let original = settings.values.public_json();
        let pinned = settings.pinned.clone();
        self.draft.update(|draft| {
            draft.retain(|key, value| {
                !pinned.iter().any(|p| related(p, key))
                    && original.pointer(&pointer(key)) != Some(value)
            });
        });
        self.original.set(original);
        self.settings.set(Some(settings));
    }

    /// Loads `GET /api/settings`.
    pub fn invalidate_reload(&self) {
        self.reload_revision
            .update(|revision| *revision = revision.wrapping_add(1));
    }

    pub async fn reload(self) {
        let Some(revision) = self.reload_revision.try_update(|value| {
            *value = value.wrapping_add(1);
            *value
        }) else {
            return;
        };
        let result = api::get::<Settings>("/settings").await;
        if self.reload_revision.try_get_untracked() != Some(revision) {
            return;
        }
        match result {
            Ok(settings) => self.load(settings),
            Err(error) => {
                self.top_error.try_set(Some(error.message));
            }
        }
    }

    /// Registers a field so errors about its key are shown next to it.
    pub fn register(&self, key: &str) {
        self.fields.update_value(|fields| {
            if !fields.iter().any(|field| field == key) {
                fields.push(key.to_owned());
            }
        });
    }

    fn saved_value(&self, key: &str) -> Option<Value> {
        self.original
            .with(|original| original.pointer(&pointer(key)).cloned())
    }

    /// The value a field shows.
    pub fn value(&self, key: &str) -> Value {
        if let Some(value) = self.draft.with(|draft| draft.get(key).cloned()) {
            return value;
        }
        self.saved_value(key)
            .or_else(|| default_value(key))
            .unwrap_or(Value::Null)
    }

    /// The text a text field shows: what is being typed, else the value.
    pub fn text(&self, key: &str) -> String {
        self.raw
            .with(|raw| raw.get(key).cloned())
            .unwrap_or_else(|| text_of(&self.value(key)))
    }

    /// The text a duration field shows.
    pub fn duration_text(&self, key: &str) -> String {
        self.raw
            .with(|raw| raw.get(key).cloned())
            .unwrap_or_else(|| friendly_duration(&text_of(&self.value(key))))
    }

    /// Changes a field. A value equal to the saved one is no change.
    pub fn set(&self, key: &str, value: Value) {
        let unchanged = self.saved_value(key).is_some_and(|saved| saved == value);
        self.draft.update(|draft| {
            if unchanged {
                draft.remove(key);
            } else {
                draft.insert(key.to_owned(), value);
            }
        });
        self.server_errors.update(|errors| {
            errors.remove(key);
        });
        self.notice.set(None);
    }

    /// Text typed into a field: kept as typed, applied when it parses.
    pub fn input(&self, key: &str, parse: Parse, text: String) {
        let parsed = parse.value(&text);
        self.raw.update(|raw| {
            raw.insert(key.to_owned(), text);
        });
        match parsed {
            Ok(value) => {
                self.client_errors.update(|errors| {
                    errors.remove(key);
                });
                self.set(key, value);
            }
            Err(message) => {
                self.client_errors.update(|errors| {
                    errors.insert(key.to_owned(), message);
                });
                // Still a change: Save stays visible and is refused until it is fixed.
                self.draft.update(|draft| {
                    draft.entry(key.to_owned()).or_insert(Value::Null);
                });
            }
        }
    }

    /// Sets or clears a client-side problem of a field.
    pub fn set_client_error(&self, key: &str, error: Option<String>) {
        self.client_errors.update(|errors| match error {
            Some(error) => {
                errors.insert(key.to_owned(), error);
            }
            None => {
                errors.remove(key);
            }
        });
    }

    /// Drops the change of one field (and of keys below it).
    pub fn revert(&self, key: &str) {
        let below = format!("{key}.");
        self.draft.update(|draft| {
            draft.retain(|k, _| k != key && !k.starts_with(&below));
        });
        self.raw
            .update(|raw| raw.retain(|k, _| k != key && !k.starts_with(&below)));
        for map in [self.client_errors, self.server_errors] {
            map.update(|errors| errors.retain(|k, _| k != key && !k.starts_with(&below)));
        }
    }

    /// Drops every change.
    pub fn discard(&self) {
        self.draft.set(BTreeMap::new());
        self.raw.set(BTreeMap::new());
        self.client_errors.set(BTreeMap::new());
        self.server_errors.set(BTreeMap::new());
        self.top_error.set(None);
    }

    pub fn is_dirty(&self, key: &str) -> bool {
        self.draft.with(|draft| draft.contains_key(key))
    }

    pub fn dirty_count(&self) -> usize {
        self.draft.with(BTreeMap::len)
    }

    /// The pinned key that locks `key` (it, a key above or a key below it).
    pub fn pinned_by(&self, key: &str) -> Option<String> {
        self.settings.with(|settings| {
            settings
                .as_ref()?
                .pinned
                .iter()
                .find(|pinned| related(pinned, key))
                .cloned()
        })
    }

    /// True when `key` (or a key below it) is saved in `settings.json`.
    pub fn is_stored(&self, key: &str) -> bool {
        let below = format!("{key}.");
        self.settings.with(|settings| {
            settings.as_ref().is_some_and(|settings| {
                settings
                    .stored
                    .iter()
                    .any(|stored| stored == key || stored.starts_with(&below))
            })
        })
    }

    /// True when the secret `key` has a value.
    pub fn secret_set(&self, key: &str) -> bool {
        self.settings.with(|settings| {
            settings
                .as_ref()
                .is_some_and(|settings| settings.secrets_set.iter().any(|s| s == key))
        })
    }

    pub fn error(&self, key: &str) -> Option<String> {
        self.client_errors
            .with(|errors| errors.get(key).cloned())
            .or_else(|| self.server_errors.with(|errors| errors.get(key).cloned()))
    }

    /// Names of the user-defined groups shown: saved ones not being removed, then new ones.
    pub fn custom_groups(&self) -> Vec<String> {
        let saved: Vec<String> = self.original.with(|original| {
            original
                .get("groups")
                .and_then(Value::as_object)
                .map(|groups| {
                    groups
                        .keys()
                        .filter(|name| !BUILT_IN_GROUPS.contains(&name.as_str()))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        });
        self.draft.with(|draft| {
            let removed = |name: &str| draft.get(&format!("groups.{name}")) == Some(&Value::Null);
            let mut names: Vec<String> = saved
                .iter()
                .filter(|name| !removed(name))
                .cloned()
                .collect();
            for key in draft.keys() {
                let mut parts = key.splitn(3, '.');
                if let (Some("groups"), Some(name), Some(_)) =
                    (parts.next(), parts.next(), parts.next())
                    && !BUILT_IN_GROUPS.contains(&name)
                    && !names.iter().any(|n| n == name)
                {
                    names.push(name.to_owned());
                }
            }
            names
        })
    }

    /// Adds a user-defined group (saved with the next save).
    pub fn add_group(&self, name: &str) {
        self.draft.update(|draft| {
            draft.remove(&format!("groups.{name}"));
            draft.insert(format!("groups.{name}.members"), Value::Array(Vec::new()));
        });
    }

    /// Removes a user-defined group (saved with the next save).
    pub fn remove_group(&self, name: &str) {
        let key = format!("groups.{name}");
        self.revert(&key);
        if self.saved_value(&key).is_some() {
            self.draft.update(|draft| {
                draft.insert(key, Value::Null);
            });
        }
    }

    /// The `PATCH /api/settings` body: the changed keys as a sparse tree.
    pub fn patch(&self) -> Value {
        let mut tree = Map::new();
        self.draft.with(|draft| {
            for (key, value) in draft {
                insert(&mut tree, key, value.clone());
            }
        });
        Value::Object(tree)
    }

    /// Attaches the errors of a failed save to their fields; the rest goes to the top.
    pub fn show_errors(&self, message: &str) {
        let fields = self.fields.get_value();
        let mut by_field = BTreeMap::new();
        let mut rest = Vec::new();
        for (key, text) in forms::error_parts(message) {
            match key
                .as_deref()
                .and_then(|key| forms::field_for(&fields, key))
            {
                Some(field) => {
                    by_field.insert(field.clone(), text);
                }
                None => rest.push(text),
            }
        }
        self.server_errors.set(by_field);
        self.top_error
            .set((!rest.is_empty()).then(|| rest.join("; ")));
    }

    /// True when the change touches the web sign-in, which signs every browser out.
    pub fn changes_sign_in(&self) -> bool {
        self.draft
            .with(|draft| draft.contains_key("web.password") || draft.contains_key("web.username"))
    }

    /// Clears the change state after a successful save.
    pub fn saved(&self, settings: Settings) {
        self.discard();
        self.load(settings);
    }

    /// True when a key below `prefix` has an error.
    pub fn has_errors_below(&self, prefix: &str) -> bool {
        let below = format!("{prefix}.");
        let any = |errors: &BTreeMap<String, String>| errors.keys().any(|k| k.starts_with(&below));
        self.client_errors.with(any) || self.server_errors.with(any)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing() {
        let count = Parse::Number {
            min: 0,
            max: u64::from(u32::MAX),
        };
        assert_eq!(count.value(" 5 "), Ok(Value::from(5)));
        assert!(count.value("-1").is_err());
        assert!(count.value("").is_err());
        let port = Parse::Number {
            min: 1024,
            max: 65535,
        };
        assert!(port.value("80").unwrap_err().contains("1024 to 65535"));
        let optional = Parse::Duration { optional: true };
        assert_eq!(optional.value(""), Ok(Value::String(String::new())));
        assert_eq!(optional.value("60m"), Ok(Value::String("1h".into())));
        assert!(Parse::Duration { optional: false }.value("").is_err());
        assert!(optional.value("5x").is_err());
        assert_eq!(friendly_duration("4w2d"), "30d");
        assert_eq!(friendly_duration("1w"), "1w");
        assert_eq!(friendly_duration("1h30m"), "1h30m");
        assert_eq!(friendly_duration(""), "");
    }

    #[test]
    fn defaults_and_ids() {
        assert_eq!(default_value("uploads.slots"), Some(Value::from(20)));
        assert_eq!(
            default_value("groups.friends.priority"),
            Some(Value::from(500))
        );
        assert_eq!(
            default_value("groups.leechers.priority"),
            Some(Value::from(999))
        );
        assert_eq!(
            field_id("groups.my friends.slots"),
            "setting-groups-my-friends-slots"
        );
        let mut tree = Map::new();
        insert(&mut tree, "uploads.limits.daily_files", Value::from(1));
        insert(&mut tree, "uploads.slots", Value::from(2));
        assert_eq!(
            Value::Object(tree),
            serde_json::json!({"uploads": {"slots": 2, "limits": {"daily_files": 1}}})
        );
        assert!(related("web.listen", "web.listen"));
        assert!(related("groups.friends.members", "groups.friends"));
        assert!(!related("web.listen", "web.listener"));
    }
}
