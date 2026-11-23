//! Helpers shared by the setup wizard and the settings page: mapping the daemon's
//! validation messages to the fields they name.
//!
//! Validation errors are `{"error": "<message>"}` where each problem starts with the dotted
//! key it is about (`uploads.slots must be at least 1`, `shares.exclude[0]: ...`,
//! `shares: /x does not exist`); `POST /api/setup` joins several with `"; "`.

/// Top-level configuration sections: a message starting with one of these names a key.
const SECTIONS: [&str; 11] = [
    "soulseek",
    "web",
    "downloads",
    "uploads",
    "shares",
    "groups",
    "blacklist",
    "search",
    "retention",
    "logging",
    "metrics",
];

/// The dotted key at the start of `text`, if any (`shares.exclude[0]: x` gives
/// `shares.exclude`).
pub fn leading_key(text: &str) -> Option<String> {
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
        .unwrap_or(text.len());
    let key = text[..end].trim_end_matches('.');
    let section = key.split('.').next().unwrap_or_default();
    SECTIONS.contains(&section).then(|| key.to_owned())
}

/// Splits a validation message into its problems, each with the key it names. A part that
/// names no key continues the previous one (a message may itself contain `"; "`).
pub fn error_parts(message: &str) -> Vec<(Option<String>, String)> {
    let mut parts: Vec<(Option<String>, String)> = Vec::new();
    for part in message.split("; ") {
        let key = leading_key(part);
        match parts.last_mut() {
            Some(last) if key.is_none() => {
                last.1.push_str("; ");
                last.1.push_str(part);
            }
            _ => parts.push((key, part.to_owned())),
        }
    }
    parts
}

/// The field (one of `fields`, dotted keys in display order) an error about `key` belongs
/// to: the field itself, the closest field above it (`shares.exclude` for
/// `shares.exclude.0`), or else the first field below it (`groups.friends.priority` for
/// `groups.friends`).
pub fn field_for<'a>(fields: &'a [String], key: &str) -> Option<&'a String> {
    if let Some(exact) = fields.iter().find(|field| *field == key) {
        return Some(exact);
    }
    let above = fields
        .iter()
        .filter(|field| {
            key.strip_prefix(field.as_str())
                .is_some_and(|rest| rest.starts_with('.') || rest.starts_with('['))
        })
        .max_by_key(|field| field.len());
    above.or_else(|| {
        let below = format!("{key}.");
        fields.iter().find(|field| field.starts_with(&below))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_found_at_the_start() {
        assert_eq!(
            leading_key("uploads.slots must be at least 1").as_deref(),
            Some("uploads.slots")
        );
        assert_eq!(
            leading_key("shares.exclude[0]: regex parse error").as_deref(),
            Some("shares.exclude")
        );
        assert_eq!(
            leading_key("shares: /x does not exist").as_deref(),
            Some("shares")
        );
        assert_eq!(
            leading_key("web.password must be at least 8 characters.").as_deref(),
            Some("web.password")
        );
        assert_eq!(leading_key("Invalid setup code"), None);
        assert_eq!(leading_key("use ms, s, m, h, d or w"), None);
    }

    #[test]
    fn messages_split_into_parts() {
        let parts = error_parts(
            "web.username may not be blank; shares: /x does not exist; shares: /y is not a directory",
        );
        let keys: Vec<_> = parts.iter().map(|(key, _)| key.as_deref()).collect();
        assert_eq!(keys, [Some("web.username"), Some("shares"), Some("shares")]);
        let parts =
            error_parts("downloads.retry_delay: invalid duration unit in \"5x\"; use ms, s or m");
        assert_eq!(parts.len(), 1);
        assert!(parts[0].1.ends_with("use ms, s or m"));
        assert_eq!(error_parts("Setup is already complete")[0].0, None);
    }

    #[test]
    fn errors_find_their_field() {
        let fields: Vec<String> = [
            "uploads.slots",
            "shares.exclude",
            "groups.friends.priority",
            "groups.friends.members",
        ]
        .map(str::to_owned)
        .to_vec();
        let field = |key| field_for(&fields, key).map(String::as_str);
        assert_eq!(field("uploads.slots"), Some("uploads.slots"));
        assert_eq!(field("shares.exclude"), Some("shares.exclude"));
        assert_eq!(field("groups.friends"), Some("groups.friends.priority"));
        assert_eq!(
            field("groups.friends.members"),
            Some("groups.friends.members")
        );
        assert_eq!(field("uploads.speed_limit"), None);
        assert_eq!(field("uploads"), Some("uploads.slots"));
    }
}
