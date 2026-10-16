//! The HTML document of a first visit: railwatch-cloud's
//! layouts/application.html.erb without the marketing meta tags (title,
//! viewport, csrf-token meta, icons, the dark-mode script, font preloads,
//! the Vite entrypoints) around omega's page script and root element.

use super::assets;

/// View data key for the request's CSRF token (`csrf_meta_tags`).
pub const CSRF: &str = "csrf_token";

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
    let mut tags = String::new();
    for f in &m.fonts {
        tags.push_str(&format!("<link rel=\"preload\" href=\"{f}\" as=\"font\" type=\"font/woff2\" crossorigin>\n    "));
    }
    for c in &m.css {
        tags.push_str(&format!("<link rel=\"stylesheet\" href=\"{c}\">\n    "));
    }
    for p in &m.preload {
        tags.push_str(&format!("<link rel=\"modulepreload\" href=\"{p}\">\n    "));
    }
    tags.push_str(&format!("<script type=\"module\" src=\"{}\"></script>", m.script));
    format!(
        r#"<!DOCTYPE html>
<html>
  <head>
    <title data-inertia>Railwatch</title>
    <meta name="viewport" content="width=device-width,initial-scale=1">
    <meta name="application-name" content="Railwatch">
    <meta name="source" content="{source}">
    <link rel="license" href="{source}/blob/main/LICENSE" title="AGPL-3.0-only: source at {source}">
    <meta name="csrf-param" content="authenticity_token">
    <meta name="csrf-token" content="{csrf}">
    <link rel="icon" href="/icon.png" type="image/png">
    <link rel="icon" href="/icon.svg" type="image/svg+xml">
    <link rel="apple-touch-icon" href="/icon.png">
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
