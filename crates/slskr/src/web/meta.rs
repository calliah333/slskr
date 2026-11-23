//! How web UI files are served. Shared by `build.rs`, which embeds the UI, and the
//! directory mode of [`crate::web`], so both send the same headers.

/// `Content-Type` for a UI file. Extensionless files (`LICENSE`, `NOTICE`) are text.
pub fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    let Some((_, extension)) = name.rsplit_once('.') else {
        return "text/plain; charset=utf-8";
    };
    match extension.to_ascii_lowercase().as_str() {
        "html" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "wasm" => "application/wasm",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        _ => "application/octet-stream",
    }
}

/// Whether the file name carries a content hash (`app-1a2b3c4d5e6f7a8b_bg.wasm`,
/// `icons.38c6d8bab26db77d8c80.woff2`), so it can be cached forever: a changed file gets
/// a new name. Anything else (`index.html`, `LICENSE`) must be revalidated.
pub fn is_hashed(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.iter().enumerate().any(|(start, &byte)| {
        if byte != b'-' && byte != b'.' {
            return false;
        }
        let digits = bytes[start + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .count();
        let end = bytes.get(start + 1 + digits);
        (8..=64).contains(&digits) && matches!(end, Some(b'.' | b'_' | b'-' | b'/'))
    })
}

/// Whether compressing the file is worthwhile (already compressed formats are not).
/// Used by `build.rs` only.
#[allow(dead_code)]
pub fn is_compressible(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/wasm"
                | "application/json"
                | "application/manifest+json"
                | "image/svg+xml"
                | "image/x-icon"
                | "font/ttf"
                | "font/otf"
                | "application/vnd.ms-fontobject"
        )
}

/// `Cache-Control` for a UI file.
pub fn cache_control(immutable: bool) -> &'static str {
    if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}
