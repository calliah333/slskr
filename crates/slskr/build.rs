//! Embeds the web UI.
//!
//! `SLSKR_EMBED_UI` names a built UI (the trunk output directory, e.g. `dist/`; relative
//! paths resolve against the workspace root). Every file in it is compiled into the
//! binary with its content type, ETag and cache policy, plus brotli and gzip variants
//! where they are smaller. Without it, the binary carries a placeholder page that
//! explains how to build the UI, so plain `cargo build` and `cargo test` need no UI.

use std::{
    env,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

#[allow(dead_code)]
#[path = "src/web/meta.rs"]
mod meta;

const ENV: &str = "SLSKR_EMBED_UI";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/web/meta.rs");
    println!("cargo:rerun-if-changed=src/web/placeholder.html");
    println!("cargo:rerun-if-env-changed={ENV}");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let (files, built) = match env::var_os(ENV).filter(|value| !value.is_empty()) {
        Some(dir) => {
            let dir = manifest_dir.join("../..").join(dir);
            let dir = fs::canonicalize(&dir).unwrap_or_else(|error| {
                panic!("{ENV}={}: {error}", dir.display());
            });
            assert!(
                dir.join("index.html").is_file(),
                "{ENV}={}: no index.html; build the UI with trunk first",
                dir.display()
            );
            println!("cargo:rerun-if-changed={}", dir.display());
            let mut files = Vec::new();
            walk(&dir, &dir, &mut files);
            files.sort();
            (files, true)
        }
        None => (
            vec![(
                "index.html".to_owned(),
                manifest_dir.join("src/web/placeholder.html"),
            )],
            false,
        ),
    };

    let variants = out_dir.join("web-ui");
    fs::create_dir_all(&variants).expect("create variants directory");
    let mut code =
        format!("pub const BUILT: bool = {built};\npub static FILES: &[Embedded] = &[\n");
    for (index, (path, source)) in files.iter().enumerate() {
        let bytes = fs::read(source).unwrap_or_else(|e| panic!("{}: {e}", source.display()));
        let content_type = meta::content_type(path);
        // index.html is rewritten for `web.url_base` at startup, so precompressing it is
        // pointless; it is small anyway.
        let compress = built && path != "index.html" && meta::is_compressible(content_type);
        let br = compress
            .then(|| brotli(&bytes))
            .and_then(|data| write_variant(&variants, index, "br", &bytes, data));
        let gzip = compress
            .then(|| gzip(&bytes))
            .and_then(|data| write_variant(&variants, index, "gz", &bytes, data));
        writeln!(
            code,
            "    Embedded {{ path: {path:?}, content_type: {content_type:?}, etag: {etag:?}, \
             immutable: {immutable}, raw: include_bytes!({raw:?}), br: {br}, gzip: {gzip} }},",
            etag = etag(&bytes),
            immutable = meta::is_hashed(path),
            raw = path_str(source),
            br = include(br.as_deref()),
            gzip = include(gzip.as_deref()),
        )
        .unwrap();
    }
    code.push_str("];\n");
    fs::write(out_dir.join("web_ui.rs"), code).expect("write web_ui.rs");
}

/// Collects `(relative path with '/', absolute path)` for every file under `dir`,
/// skipping dotfiles.
fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, files);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .expect("walked path is under the root")
                .components()
                .map(|part| {
                    part.as_os_str()
                        .to_str()
                        .unwrap_or_else(|| panic!("{}: file name is not UTF-8", path.display()))
                })
                .collect::<Vec<_>>()
                .join("/");
            files.push((relative, path));
        }
    }
}

fn etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

fn brotli(bytes: &[u8]) -> Vec<u8> {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        lgwin: 22,
        ..Default::default()
    };
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut &bytes[..], &mut out, &params).expect("brotli compression");
    out
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).expect("gzip compression");
    encoder.finish().expect("gzip compression")
}

/// Keeps a compressed variant only when it saves at least a tenth of the size.
fn write_variant(
    dir: &Path,
    index: usize,
    extension: &str,
    original: &[u8],
    compressed: Vec<u8>,
) -> Option<PathBuf> {
    if compressed.len() * 10 > original.len() * 9 {
        return None;
    }
    let path = dir.join(format!("{index}.{extension}"));
    fs::write(&path, compressed).expect("write compressed variant");
    Some(path)
}

fn include(path: Option<&Path>) -> String {
    path.map_or_else(
        || "None".to_owned(),
        |path| format!("Some(include_bytes!({:?}))", path_str(path)),
    )
}

fn path_str(path: &Path) -> &str {
    path.to_str()
        .unwrap_or_else(|| panic!("{}: path is not UTF-8", path.display()))
}
