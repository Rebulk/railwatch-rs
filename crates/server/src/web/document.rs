//! The HTML document of a first visit: railwatch-cloud's
//! layouts/application.html.erb without the marketing meta tags (title,
//! viewport, csrf-token meta, icons, the dark-mode script, font preloads,
//! the Vite entrypoints) around omega's page script and root element.

use super::assets;

/// View data key for the request's CSRF token (`csrf_meta_tags`).
pub const CSRF: &str = "csrf_token";
/// View data key for Config::base_path: prefixes every URL in the document,
/// and is published as `railwatch-mount` (the route helpers' script_name,
/// as the gem's embedded engine does) and `action-cable-url`.
pub const BASE_PATH: &str = "base_path";

pub fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

const DARK_MODE_SCRIPT: &str = r#"
      document.documentElement.classList.toggle(
        "dark",
        localStorage.appearance === "dark" ||
          (!("appearance" in localStorage) && window.matchMedia("(prefers-color-scheme: dark)").matches),
      );
    "#;

/// The pages' Content-Security-Policy: scripts from this server and the
/// dark-mode script above by its hash (never 'unsafe-inline'), connections
/// (fetch, the live channel) and form posts back to this server.
/// `form_origin`: the sign-in provider's origin, where "Continue with
/// Google"'s form post is redirected (browsers check a form's redirects
/// against form-action too).
pub fn content_security_policy(form_origin: Option<&str>) -> String {
    use base64::Engine;
    use sha2::Digest;
    let hash = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(DARK_MODE_SCRIPT.as_bytes()));
    let forms = form_origin.map_or(String::new(), |o| format!(" {o}"));
    format!("base-uri 'self'; object-src 'none'; frame-ancestors 'none'; script-src 'self' 'sha256-{hash}'; connect-src 'self'; form-action 'self'{forms}")
}

pub fn render(view: &omega::View<'_>) -> String {
    let m = assets::manifest();
    let csrf = view.data.get(CSRF).and_then(|v| v.as_str()).unwrap_or("");
    // config::normalize_base_path allows nothing that needs escaping.
    let base = view.data.get(BASE_PATH).and_then(|v| v.as_str()).unwrap_or("");
    let mut tags = String::new();
    if !base.is_empty() {
        tags.push_str(&format!("<meta name=\"railwatch-mount\" content=\"{base}\">\n    "));
    }
    tags.push_str(&format!("<meta name=\"action-cable-url\" content=\"{base}/cable\">\n    "));
    for f in &m.fonts {
        tags.push_str(&format!("<link rel=\"preload\" href=\"{base}{f}\" as=\"font\" type=\"font/woff2\" crossorigin>\n    "));
    }
    for c in &m.css {
        tags.push_str(&format!("<link rel=\"stylesheet\" href=\"{base}{c}\">\n    "));
    }
    for p in &m.preload {
        tags.push_str(&format!("<link rel=\"modulepreload\" href=\"{base}{p}\">\n    "));
    }
    tags.push_str(&format!("<script type=\"module\" src=\"{base}{}\"></script>", m.script));
    format!(
        r#"<!DOCTYPE html>
<html>
  <head>
    <title data-inertia>Railwatch</title>
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="application-name" content="Railwatch">
    <meta name="source" content="{source}">
    <link rel="license" href="{source}/blob/main/LICENSE" title="FSL-1.1-ALv2: source at {source}">
    <meta name="csrf-param" content="authenticity_token">
    <meta name="csrf-token" content="{csrf}">
    <link rel="icon" href="{base}/icon.png" type="image/png">
    <link rel="icon" href="{base}/icon.svg" type="image/svg+xml">
    <link rel="apple-touch-icon" href="{base}/icon.png">
    <script>{DARK_MODE_SCRIPT}</script>
    {tags}
    {head}
  </head>
  <body>
    {body}
  </body>
</html>
"#,
        csrf = escape_attr(csrf),
        source = escape_attr(crate::SOURCE_URL),
        head = view.head,
        body = view.body,
    )
}
