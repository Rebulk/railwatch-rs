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
