//! `settings.json`: the settings changed from the web UI.
//!
//! The file holds only what was changed (a sparse tree shaped like
//! [`slskr_types::config::Config`]) plus the web password hash and the digests of API keys
//! created from the UI. It is written by the daemon alone, atomically with mode 0600. Unlike
//! `slskr.toml` it is read leniently: an unreadable file, unknown keys and invalid values are
//! logged and ignored, so the app-managed layer can never prevent startup.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use slskr_types::config::{Config, ManagedApiKey};
use toml::Table;

/// Settings store file name inside the data directory.
pub const FILE_NAME: &str = "settings.json";
/// Current `version` of the file format.
pub const VERSION: u32 = 1;

/// Dotted keys that `settings` may not hold: the web password is stored as a hash and API
/// keys as digests, outside the tree.
const MANAGED_ELSEWHERE: [&str; 2] = ["web.password", "web.api_keys"];

/// The contents of `settings.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoreFile {
    pub version: u32,
    /// Sparse configuration tree: only keys changed from the UI.
    pub settings: Map<String, Value>,
    /// argon2id PHC hash of the web password set from the UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub web_password_hash: String,
    /// API keys created from the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_keys: Vec<ManagedApiKey>,
}

impl StoreFile {
    /// Reads `path` leniently: anything that can not be used is logged and dropped.
    pub async fn load(path: &Path) -> Self {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read the settings store; starting without it");
                return Self::default();
            }
        };
        Self::parse(&text, &path.display().to_string())
    }

    /// Parses the file's text, dropping what can not be used (see [`Self::load`]).
    pub fn parse(text: &str, origin: &str) -> Self {
        let root = match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(root)) => root,
            Ok(_) => {
                tracing::warn!(
                    path = origin,
                    "settings store is not a JSON object; ignoring it"
                );
                return Self::default();
            }
            Err(error) => {
                tracing::warn!(path = origin, %error, "settings store is not valid JSON; ignoring it");
                return Self::default();
            }
        };
        let mut store = Self {
            version: VERSION,
            ..Self::default()
        };
        for (field, value) in root {
            match field.as_str() {
                "version" => match value.as_u64() {
                    Some(version) if version <= u64::from(VERSION) => {}
                    _ => tracing::warn!(
                        path = origin,
                        version = %value,
                        "settings store is from a newer slskr; unknown entries are ignored"
                    ),
                },
                "settings" => match value {
                    Value::Object(tree) => store.settings = sanitize(tree, origin),
                    _ => tracing::warn!(
                        path = origin,
                        "settings store: settings is not an object; ignoring it"
                    ),
                },
                "web_password_hash" => match value {
                    Value::String(hash) if crate::auth::valid_password_hash(&hash) => {
                        store.web_password_hash = hash;
                    }
                    _ => tracing::warn!(
                        path = origin,
                        "settings store: web_password_hash is not an argon2id hash; ignoring it"
                    ),
                },
                "api_keys" => {
                    let Value::Array(keys) = value else {
                        tracing::warn!(
                            path = origin,
                            "settings store: api_keys is not a list; ignoring it"
                        );
                        continue;
                    };
                    for key in keys {
                        match serde_json::from_value::<ManagedApiKey>(key) {
                            Ok(key) if crate::auth::parse_sha256_hex(&key.sha256).is_some() => {
                                store.api_keys.push(key);
                            }
                            Ok(key) => tracing::warn!(
                                path = origin,
                                name = %key.name,
                                "settings store: API key digest is not a SHA-256; ignoring the key"
                            ),
                            Err(error) => tracing::warn!(
                                path = origin,
                                %error,
                                "settings store: ignoring an invalid API key"
                            ),
                        }
                    }
                }
                _ => tracing::warn!(
                    path = origin,
                    field,
                    "settings store: ignoring unknown entry"
                ),
            }
        }
        store
    }

    /// The JSON written to disk.
    pub fn to_text(&self) -> String {
        let mut text = serde_json::to_string_pretty(&Self {
            version: VERSION,
            ..self.clone()
        })
        .expect("settings store serializes");
        text.push('\n');
        text
    }

    /// The settings tree as a TOML table, ready to merge under the file and environment.
    pub fn table(&self) -> Table {
        // Every leaf was checked to deserialize into `Config`, so it converts to TOML.
        Table::try_from(&self.settings).unwrap_or_default()
    }
}

/// Splits a dotted key; `None` when a segment is empty.
pub fn split_key(key: &str) -> Option<Vec<String>> {
    let path: Vec<String> = key.split('.').map(str::to_owned).collect();
    path.iter().all(|s| !s.is_empty()).then_some(path)
}

/// Leaves (non-object values) of a JSON tree with their paths. Empty objects have none.
pub fn leaves(tree: &Map<String, Value>) -> Vec<(Vec<String>, &Value)> {
    fn walk<'a>(
        tree: &'a Map<String, Value>,
        prefix: &mut Vec<String>,
        out: &mut Vec<(Vec<String>, &'a Value)>,
    ) {
        for (key, value) in tree {
            prefix.push(key.clone());
            match value {
                Value::Object(child) => walk(child, prefix, out),
                value => out.push((prefix.clone(), value)),
            }
            prefix.pop();
        }
    }
    let mut out = Vec::new();
    walk(tree, &mut Vec::new(), &mut out);
    out
}

/// Sets `path` to `value`, creating (or replacing non-object) parents.
pub fn insert(tree: &mut Map<String, Value>, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut node = tree;
    for key in parents {
        let child = node
            .entry(key.clone())
            .or_insert_with(|| Value::Object(Map::new()));
        if !child.is_object() {
            *child = Value::Object(Map::new());
        }
        node = child.as_object_mut().expect("created object");
    }
    node.insert(last.clone(), value);
}

/// Removes `path` (a leaf or a whole subtree) and prunes parents left empty. Returns true
/// when something was removed.
pub fn remove(tree: &mut Map<String, Value>, path: &[String]) -> bool {
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return tree.remove(first).is_some();
    }
    let Some(Value::Object(child)) = tree.get_mut(first) else {
        return false;
    };
    let removed = remove(child, rest);
    if child.is_empty() {
        tree.remove(first);
    }
    removed
}

/// `Ok` when `value` at `path` is a valid configuration value on its own; errors name the
/// key.
pub fn check_leaf(path: &[String], value: &Value) -> Result<(), String> {
    let key = path.join(".");
    if MANAGED_ELSEWHERE.contains(&key.as_str()) {
        return Err(format!("{key} can not be stored as a setting"));
    }
    let mut single = Map::new();
    insert(&mut single, path, value.clone());
    let single = Value::Object(single);
    serde_json::from_value::<Config>(single.clone()).map_err(|error| format!("{key}: {error}"))?;
    toml::Value::try_from(&single).map_err(|error| format!("{key}: {error}"))?;
    Ok(())
}

/// Drops leaves that are unknown keys or invalid values, with a warning each.
fn sanitize(tree: Map<String, Value>, origin: &str) -> Map<String, Value> {
    let mut clean = Map::new();
    for (path, value) in leaves(&tree) {
        match check_leaf(&path, value) {
            Ok(()) => insert(&mut clean, &path, value.clone()),
            Err(error) => {
                tracing::warn!(path = origin, %error, "settings store: ignoring setting");
            }
        }
    }
    clean
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    #[test]
    fn sparse_round_trip() {
        let mut store = StoreFile::default();
        insert(
            &mut store.settings,
            &["uploads".into(), "slots".into()],
            json!(5),
        );
        insert(
            &mut store.settings,
            &["groups".into(), "friends".into(), "members".into()],
            json!(["alice"]),
        );
        store.web_password_hash = crate::auth::hash_password("pw").unwrap();
        let text = store.to_text();
        let back = StoreFile::parse(&text, "test");
        assert_eq!(back.settings, store.settings);
        assert_eq!(back.web_password_hash, store.web_password_hash);
        assert_eq!(back.version, VERSION);
        let json: Value = serde_json::from_str(&text).unwrap();
        // Only the changed keys are written, not a full dump.
        assert_eq!(
            json["settings"],
            json!({"uploads": {"slots": 5}, "groups": {"friends": {"members": ["alice"]}}})
        );
        assert!(json.get("api_keys").is_none());
        assert_eq!(store.table()["uploads"]["slots"].as_integer(), Some(5));

        assert!(remove(
            &mut store.settings,
            &["uploads".into(), "slots".into()]
        ));
        assert!(store.settings.get("uploads").is_none());
        assert!(!remove(
            &mut store.settings,
            &["uploads".into(), "slots".into()]
        ));
        assert!(remove(&mut store.settings, &["groups".into()]));
        assert!(store.settings.is_empty());
    }

    #[test]
    fn unknown_and_invalid_entries_are_dropped() {
        let text = json!({
            "version": 99,
            "future": {"x": 1},
            "settings": {
                "uploads": {"slots": 3, "nope": 1},
                "web": {"listen": "not an address", "password": "plain", "api_keys": []},
                "downloads": {"retry_delay": "5 parsecs", "resume": false},
                "brand_new_section": {"a": 1}
            },
            "web_password_hash": "not-a-hash",
            "api_keys": [
                {"name": "ok", "sha256": "00".repeat(32), "role": "admin", "allow": ["0.0.0.0/0"]},
                {"name": "bad", "sha256": "xyz", "role": "admin", "allow": []},
                {"name": "worse"}
            ]
        })
        .to_string();
        let store = StoreFile::parse(&text, "test");
        assert_eq!(
            Value::Object(store.settings),
            json!({"uploads": {"slots": 3}, "downloads": {"resume": false}})
        );
        assert_eq!(store.web_password_hash, "");
        assert_eq!(store.api_keys.len(), 1);
        assert_eq!(store.api_keys[0].name, "ok");

        for garbage in ["", "{", "[1,2]", "null", "\"x\""] {
            assert_eq!(StoreFile::parse(garbage, "test"), StoreFile::default());
        }
    }

    #[test]
    fn leaf_checks_name_the_key() {
        let path = |key: &str| split_key(key).unwrap();
        assert!(check_leaf(&path("uploads.slots"), &json!(4)).is_ok());
        assert!(check_leaf(&path("groups.friends.priority"), &json!(1)).is_ok());
        for (key, value) in [
            ("uploads.slots", json!("many")),
            ("uploads.slotz", json!(1)),
            ("web.listen", json!("x")),
            ("web.password", json!("x")),
            ("web.api_keys", json!([])),
        ] {
            let error = check_leaf(&path(key), &value).unwrap_err();
            assert!(error.starts_with(key), "{key}: {error}");
        }
        assert!(split_key("a..b").is_none());
        assert_eq!(
            leaves(&tree(json!({"a": {"b": 1, "c": {}}, "d": [1]})))
                .into_iter()
                .map(|(p, _)| p.join("."))
                .collect::<Vec<_>>(),
            ["a.b", "d"]
        );
    }
}
