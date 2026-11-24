//! Time formatting for the System tabs, in the browser's time zone.

use chrono::{DateTime, Local, Utc};

/// "just now", "5m ago", "3h ago", "2d ago", "in 30s"; older than a week: the date.
pub fn relative(time: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - time).num_seconds();
    let future = seconds < 0;
    let amount = seconds.unsigned_abs();
    let span = match amount {
        0..45 if !future => return "just now".to_owned(),
        0..60 => format!("{amount}s"),
        60..3_600 => format!("{}m", amount / 60),
        3_600..86_400 => format!("{}h", amount / 3_600),
        86_400..604_800 => format!("{}d", amount / 86_400),
        _ => return time.with_timezone(&Local).format("%b %-d, %Y").to_string(),
    };
    if future {
        format!("in {span}")
    } else {
        format!("{span} ago")
    }
}

/// The full local date and time, for `title` attributes: `2026-11-23 14:05:09 (UTC+01:00)`.
pub fn full(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M:%S (UTC%:z)")
        .to_string()
}

/// The local time of day with milliseconds: `14:05:09.123`.
pub fn clock(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local)
        .format("%H:%M:%S%.3f")
        .to_string()
}
