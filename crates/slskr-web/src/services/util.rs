//! Formatting and browser helpers.
//!
//! Number formatting follows JavaScript semantics (`Number.prototype.toFixed`,
//! `Number.prototype.toString`, `Math.round`).

use wasm_bindgen::{JsCast, JsValue};

const SIZES: [&str; 9] = ["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];

/// JavaScript `Number.prototype.toString()` for `value`.
pub fn js_number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return "0".to_string();
    }
    let magnitude = value.abs();
    if !(1e-6..1e21).contains(&magnitude) {
        // Rust renders `1.5e-7` / `1e21`; JavaScript adds an explicit `+` to positive exponents.
        let formatted = format!("{value:e}");
        return match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => {
                format!("{mantissa}e+{exponent}")
            }
            _ => formatted,
        };
    }
    format!("{value}")
}

/// JavaScript `Number.prototype.toFixed(digits)`: exact decimal rounding, ties away from zero.
pub fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return js_number_to_string(value);
    }
    let digits = digits.min(100);
    let negative = value < 0.0;
    // 1100 fractional digits is enough to print any f64 exactly.
    let exact = format!("{:.1100}", value.abs());
    let (integer, fraction) = exact.split_once('.').unwrap_or((&exact, ""));
    let fraction = fraction.as_bytes();

    let mut kept: Vec<u8> = integer
        .bytes()
        .chain(fraction[..digits].iter().copied())
        .collect();
    if fraction.get(digits).is_some_and(|next| *next >= b'5') {
        let mut index = kept.len();
        loop {
            if index == 0 {
                kept.insert(0, b'1');
                break;
            }
            index -= 1;
            if kept[index] == b'9' {
                kept[index] = b'0';
            } else {
                kept[index] += 1;
                break;
            }
        }
    }

    let split = kept.len() - digits;
    let mut out = String::with_capacity(kept.len() + 2);
    if negative {
        out.push('-');
    }
    out.push_str(std::str::from_utf8(&kept[..split]).unwrap_or("0"));
    if digits > 0 {
        out.push('.');
        out.push_str(std::str::from_utf8(&kept[split..]).unwrap_or(""));
    }
    out
}

/// `Number.parseFloat(value.toFixed(digits))`.
fn round_fixed(value: f64, digits: usize) -> f64 {
    to_fixed(value, digits).parse().unwrap_or(f64::NAN)
}

fn clamp_decimals(decimals: i32) -> usize {
    decimals.max(0) as usize
}

/// Formats a duration in seconds as `MM:SS`, or `HH:MM:SS` from one hour (hours wrap at 24).
pub fn format_seconds(seconds: f64) -> String {
    if !seconds.is_finite() {
        return "Invalid Date".to_string();
    }
    let total = (seconds.trunc() as i64).rem_euclid(86_400);
    let (hours, minutes, secs) = (total / 3_600, (total % 3_600) / 60, total % 60);
    if seconds >= 3_600.0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

/// Converts `bytes` to `unit` (`"B"`, `"KB"`, ... `"YB"`) rounded to `decimals`.
///
/// For `"B"` this returns `"{bytes} B"` exactly like the JavaScript original.
pub fn format_bytes_as_unit(bytes: f64, unit: &str, decimals: i32) -> String {
    if unit == "B" {
        return format!("{} {unit}", js_number_to_string(bytes));
    }
    let Some(power) = SIZES
        .iter()
        .position(|size| *size == unit)
        .filter(|power| *power > 0)
    else {
        return "NaN".to_string();
    };
    let value = bytes / 1_024f64.powi(power as i32);
    js_number_to_string(round_fixed(value, clamp_decimals(decimals)))
}

/// Formats a byte count with two decimals, e.g. `1.18 MB`.
pub fn format_bytes(bytes: f64) -> String {
    format_bytes_with(bytes, 2, 0, ' ')
}

/// Formats a byte count with `decimals` decimals, left-padding the number to `padding` characters.
pub fn format_bytes_with(
    bytes: f64,
    decimals: i32,
    padding: usize,
    padding_character: char,
) -> String {
    if bytes.is_nan() || bytes < 1.0 {
        return "0 B".to_string();
    }
    let index = ((bytes.ln() / 1_024f64.ln()).floor() as usize).min(SIZES.len() - 1);
    let number = round_fixed(
        bytes / 1_024f64.powi(index as i32),
        clamp_decimals(decimals),
    );
    let number = js_number_to_string(number);
    let width = number.chars().count();
    let mut padded = String::with_capacity(padding.max(width) + 3);
    padded.extend(std::iter::repeat_n(
        padding_character,
        padding.saturating_sub(width),
    ));
    padded.push_str(&number);
    format!("{padded} {}", SIZES[index])
}

/// Formats a transfer speed with one decimal, e.g. `1.2 MB/s`.
pub fn format_speed(bytes_per_second: f64) -> String {
    format_speed_with(bytes_per_second, 1)
}

/// Formats a transfer speed with `decimals` decimals.
pub fn format_speed_with(bytes_per_second: f64, decimals: i32) -> String {
    if bytes_per_second == 0.0 || bytes_per_second.is_nan() {
        return "0 B/s".to_string();
    }
    format!(
        "{}/s",
        format_bytes_with(bytes_per_second, decimals, 0, ' ')
    )
}

/// `new Date(date).toLocaleString()` in the browser's locale.
pub fn format_date(date: &str) -> String {
    let value = js_sys::Date::new(&JsValue::from_str(date));
    js_sys::Reflect::get(&value, &JsValue::from_str("toLocaleString"))
        .ok()
        .and_then(|function| function.dyn_into::<js_sys::Function>().ok())
        .and_then(|function| function.call0(&value).ok())
        .and_then(|result| result.as_string())
        .unwrap_or_default()
}

/// Truncates `text` to `max_length` UTF-16 units, appending `...` when shortened.
pub fn truncate(text: &str, max_length: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max_length {
        return text.to_string();
    }
    format!("{}...", String::from_utf16_lossy(&units[..max_length]))
}

/// Last path component, splitting on both `\` and `/`.
pub fn get_file_name(full_path: &str) -> &str {
    let name = full_path.rsplit('\\').next().unwrap_or(full_path);
    name.rsplit('/').next().unwrap_or(name)
}

/// Formats audio attributes: `24/96kHz` for lossless, `320 Kbps` / `245 Kbps, VBR` for lossy.
pub fn format_attributes(
    bit_rate: Option<u32>,
    is_variable_bit_rate: Option<bool>,
    bit_depth: Option<u32>,
    sample_rate: Option<u32>,
) -> String {
    let bit_rate = bit_rate.filter(|value| *value != 0);
    let bit_depth = bit_depth.filter(|value| *value != 0);
    let sample_rate = sample_rate.filter(|value| *value != 0);

    if let (Some(depth), Some(rate)) = (bit_depth, sample_rate) {
        return format!(
            "{depth}/{}kHz",
            js_number_to_string(f64::from(rate) / 1_000.0)
        );
    }

    let bit_rate = bit_rate.map_or_else(|| "undefined".to_string(), |value| value.to_string());
    if is_variable_bit_rate == Some(true) {
        return format!("{bit_rate} Kbps, VBR");
    }
    if bit_rate == "undefined" {
        String::new()
    } else {
        format!("{bit_rate} Kbps")
    }
}

/// Resolves after `milliseconds`.
pub async fn sleep(milliseconds: u32) {
    gloo_timers::future::TimeoutFuture::new(milliseconds).await;
}

/// JavaScript `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    const UNRESERVED: &[u8] = b"-_.!~*'()";
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Saves `data` as a file named `filename` through a temporary object-URL link.
pub fn download_file(data: &[u8], filename: &str, mime: Option<&str>) -> Result<(), JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?;

    let blob_constructor: js_sys::Function =
        js_sys::Reflect::get(&window, &JsValue::from_str("Blob"))?.dyn_into()?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(data));
    let options = js_sys::Object::new();
    js_sys::Reflect::set(
        &options,
        &JsValue::from_str("type"),
        &JsValue::from_str(mime.unwrap_or("application/octet-stream")),
    )?;
    let blob: web_sys::Blob =
        js_sys::Reflect::construct(&blob_constructor, &js_sys::Array::of2(&parts, &options))?
            .dyn_into()?;

    let blob_url = web_sys::Url::create_object_url_with_blob(&blob)?;
    let link = document.create_element("a")?;
    link.set_attribute("style", "display: none")?;
    link.set_attribute("href", &blob_url)?;
    link.set_attribute("download", filename)?;

    let body = document
        .body()
        .ok_or_else(|| JsValue::from_str("no body"))?;
    body.append_with_node_1(&link)?;
    link.unchecked_ref::<web_sys::HtmlElement>().click();
    link.remove();
    web_sys::Url::revoke_object_url(&blob_url)
}

/// Formats a count with thousands separators: `12,345`.
pub fn group_digits(value: impl ToString) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (position, digit) in digits.chars().enumerate() {
        if position > 0 && (digits.len() - position).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A count in at most four characters for tight spots (tab counts): `9,999`, `22.8k`,
/// `555k`, `1.2M`.
pub fn short_count(value: usize) -> String {
    #[allow(clippy::cast_precision_loss)]
    let n = value as f64;
    match value {
        0..10_000 => group_digits(value),
        10_000..100_000 => format!("{:.1}k", n / 1e3),
        100_000..1_000_000 => format!("{:.0}k", n / 1e3),
        _ => format!("{:.1}M", n / 1e6),
    }
}

/// A view preference kept in this browser's local storage under `key`, so it survives
/// reloads. Starts `false` when nothing is stored or storage is unavailable.
pub fn stored_flag(key: &'static str) -> leptos::prelude::RwSignal<bool> {
    use leptos::prelude::{Effect, Get, RwSignal};
    let storage = || web_sys::window()?.local_storage().ok().flatten();
    let flag = RwSignal::new(
        storage()
            .and_then(|storage| storage.get_item(key).ok().flatten())
            .is_some_and(|value| value == "1"),
    );
    Effect::new(move |_| {
        let set = flag.get();
        if let Some(storage) = storage() {
            let _ = if set {
                storage.set_item(key, "1")
            } else {
                storage.remove_item(key)
            };
        }
    });
    flag
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_as_unit_converts_to_unit() {
        assert_eq!(format_bytes_as_unit(1_234_567.0, "MB", 2), "1.18");
        assert_eq!(format_bytes_as_unit(512.0, "B", 2), "512 B");
        assert_eq!(format_bytes_as_unit(1_536.0, "KB", 1), "1.5");
    }

    #[test]
    fn format_bytes_returns_zero_under_one_byte() {
        assert_eq!(format_bytes(0.5), "0 B");
        assert_eq!(format_bytes(0.0), "0 B");
    }

    #[test]
    fn format_bytes_formats_values_from_one() {
        assert_eq!(format_bytes(1.0), "1 B");
        assert_eq!(format_bytes(1_024.0), "1 KB");
        assert_eq!(format_bytes(1_234_567.0), "1.18 MB");
        assert_eq!(format_bytes_with(1_536.0, 1, 4, ' '), " 1.5 KB");
    }

    #[test]
    fn to_fixed_matches_javascript() {
        assert_eq!(to_fixed(0.25, 1), "0.3");
        assert_eq!(to_fixed(1.005, 2), "1.00");
        assert_eq!(to_fixed(9.995, 2), "9.99");
        assert_eq!(to_fixed(9.996, 2), "10.00");
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(-1.5, 0), "-2");
    }

    #[test]
    fn js_number_to_string_matches_javascript() {
        assert_eq!(js_number_to_string(1.0), "1");
        assert_eq!(js_number_to_string(44.1), "44.1");
        assert_eq!(js_number_to_string(-0.0), "0");
        assert_eq!(js_number_to_string(1e21), "1e+21");
        assert_eq!(js_number_to_string(1.5e-7), "1.5e-7");
    }

    #[test]
    fn formats_seconds() {
        assert_eq!(format_seconds(65.0), "01:05");
        assert_eq!(format_seconds(3_599.0), "59:59");
        assert_eq!(format_seconds(3_661.0), "01:01:01");
    }

    #[test]
    fn formats_speed() {
        assert_eq!(format_speed(0.0), "0 B/s");
        assert_eq!(format_speed(1_536.0), "1.5 KB/s");
    }

    #[test]
    fn truncates_text() {
        assert_eq!(truncate("", 3), "");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcdef", 3), "abc...");
    }

    #[test]
    fn splits_paths() {
        assert_eq!(get_file_name("@@a\\b\\c.mp3"), "c.mp3");
        assert_eq!(get_file_name("a/b/c.mp3"), "c.mp3");
    }

    #[test]
    fn formats_attributes() {
        assert_eq!(
            format_attributes(Some(1411), None, Some(24), Some(96_000)),
            "24/96kHz"
        );
        assert_eq!(
            format_attributes(None, None, Some(16), Some(44_100)),
            "16/44.1kHz"
        );
        assert_eq!(
            format_attributes(Some(245), Some(true), None, None),
            "245 Kbps, VBR"
        );
        assert_eq!(
            format_attributes(Some(320), Some(false), None, None),
            "320 Kbps"
        );
        assert_eq!(format_attributes(None, None, None, None), "");
    }

    #[test]
    fn short_counts() {
        assert_eq!(super::short_count(9_999), "9,999");
        assert_eq!(super::short_count(22_811), "22.8k");
        assert_eq!(super::short_count(555_133), "555k");
        assert_eq!(super::short_count(1_234_567), "1.2M");
    }

    #[test]
    fn encodes_paths() {
        assert_eq!(encode_uri_component("a b/ü"), "a%20b%2F%C3%BC");
    }
}
