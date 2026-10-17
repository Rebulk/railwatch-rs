//! The built frontend (frontend/dist, by bin/build), embedded in the binary.
//! URL `/vite/<file>` serves `dist/<file>`; `/icon.png` and `/icon.svg`
//! serve the dist root files. Hashed files (under assets/) are cached for a
//! year, immutable; the precompressed `.br`/`.gz` sibling is chosen by
//! Accept-Encoding.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use include_dir::{Dir, include_dir};
use serde_json::Value;

static DIST: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../frontend/dist");

pub const BASE: &str = "/vite/";

/// The two entrypoints' URLs and the asset version (Inertia's), from the
/// embedded Vite manifest.
pub struct Manifest {
    pub script: String,
    pub css: Vec<String>,
    /// Statically imported chunks of the script, for modulepreload.
    pub preload: Vec<String>,
    pub fonts: Vec<String>,
    pub version: String,
}

pub fn manifest() -> &'static Manifest {
    static M: std::sync::OnceLock<Manifest> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let raw = DIST.get_file(".vite/manifest.json").expect("frontend/dist/.vite/manifest.json is missing: run bin/build before cargo build").contents();
        let m: Value = serde_json::from_slice(raw).expect("manifest JSON");
        let entry = &m["app/frontend/entrypoints/inertia.tsx"];
        let url = |f: &Value| format!("{BASE}{}", f.as_str().unwrap_or_default());
        let mut css: Vec<String> = m["app/frontend/entrypoints/application.css"]["file"].as_str().map(|f| vec![format!("{BASE}{f}")]).unwrap_or_default();
        css.extend(entry["css"].as_array().into_iter().flatten().map(url));
        let preload = entry["imports"].as_array().into_iter().flatten().filter_map(|k| m[k.as_str()?]["file"].as_str().map(|f| format!("{BASE}{f}"))).collect();
        let fonts = DIST
            .get_dir("assets")
            .map(|d| {
                let mut f: Vec<String> = d
                    .files()
                    .filter_map(|f| f.path().to_str())
                    .filter(|p| p.ends_with(".woff2") && (p.contains("Roboto-Regular") || p.contains("Roboto-Medium") || p.contains("CommitMono-Regular")))
                    .map(|p| format!("{BASE}{p}"))
                    .collect();
                f.sort();
                f
            })
            .unwrap_or_default();
        // The version changes whenever any built file does: the manifest
        // names every hashed file.
        let version = crate::ingest::receipt_headers::hex(&md5::Md5::digest_bytes(raw));
        Manifest { script: url(&entry["file"]), css, preload, fonts, version }
    })
}

trait DigestBytes {
    fn digest_bytes(b: &[u8]) -> [u8; 16];
}

impl DigestBytes for md5::Md5 {
    fn digest_bytes(b: &[u8]) -> [u8; 16] {
        use md5::Digest;
        md5::Md5::digest(b).into()
    }
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "woff2" => "font/woff2",
        "json" => "application/json",
        "map" => "application/json",
        "html" => "text/html; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Serves `dist/<path>`; None when it is not an embedded file.
pub fn serve(path: &str, headers: &HeaderMap) -> Option<Response> {
    if path.is_empty() || path.starts_with('.') || path.contains("..") || path.ends_with(".gz") || path.ends_with(".br") {
        return None;
    }
    let file = DIST.get_file(path)?;
    let accept = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("");
    let accepts = |enc: &str| accept.split(',').any(|p| p.split(';').next().is_some_and(|t| t.trim() == enc));
    let (body, encoding) = if let Some(br) = accepts("br").then(|| DIST.get_file(format!("{path}.br"))).flatten() {
        (br.contents(), Some("br"))
    } else if let Some(gz) = accepts("gzip").then(|| DIST.get_file(format!("{path}.gz"))).flatten() {
        (gz.contents(), Some("gzip"))
    } else {
        (file.contents(), None)
    };
    let hashed = path.starts_with("assets/");
    let mut res = (StatusCode::OK, Body::from(body)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type(path)));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(if hashed { "public, max-age=31536000, immutable" } else { "public, max-age=3600" }));
    if DIST.get_file(format!("{path}.br")).is_some() || DIST.get_file(format!("{path}.gz")).is_some() {
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if let Some(e) = encoding {
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(e));
    }
    Some(res)
}

/// Bytes embedded, for `railwatch doctor`.
pub fn embedded_bytes() -> (usize, usize) {
    fn walk(d: &Dir, n: &mut usize, b: &mut usize) {
        for f in d.files() {
            *n += 1;
            *b += f.contents().len();
        }
        for s in d.dirs() {
            walk(s, n, b);
        }
    }
    let (mut n, mut b) = (0, 0);
    walk(&DIST, &mut n, &mut b);
    (n, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_embedded_files_and_assets_are_cached_and_compressed() {
        let m = manifest();
        assert!(m.script.starts_with("/vite/assets/inertia-") && m.script.ends_with(".js"));
        assert!(m.css.iter().any(|c| c.contains("application-")));
        let path = m.script.strip_prefix(BASE).unwrap();
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate, br"));
        let r = serve(path, &h).unwrap();
        assert_eq!(r.headers()[header::CONTENT_ENCODING], "br");
        assert_eq!(r.headers()[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
        let plain = serve(path, &HeaderMap::new()).unwrap();
        assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
        assert!(serve(".vite/manifest.json", &h).is_none());
        assert!(serve("../Cargo.toml", &h).is_none());
        assert!(serve("icon.svg", &h).is_some());
    }
}
