//! The public status page (docs/uptime.md): `GET /status/<slug>` (HTML)
//! and `GET /status/<slug>.json`, unauthenticated.
//!
//! - Outside the session layer: no cookie is read or set, no CSRF, no
//!   Inertia, no dashboard bundle. One server-rendered page with inline
//!   CSS and no script, readable on a phone.
//! - Built only from `uptime::public_status`, which carries only what the
//!   admin marked public (display names, URLs where allowed, statuses,
//!   uptime, incident times). Both formats render that one value, so the
//!   HTML cannot show what the JSON does not.
//! - An unknown slug and a disabled page are the same 404, so a slug is
//!   not confirmed until its page is public. Slugs are unguessable by
//!   default (uptime::new_slug), or chosen by the admin.
//! - `Cache-Control: public, max-age=30`, so a proxy or CDN can absorb a
//!   crowd during an outage; the page says when it was generated.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use super::WebState;
use super::digest_mailer::html_escape as h;
use crate::uptime;

pub const MAX_AGE_SECS: u32 = 30;

pub fn routes() -> Router<Arc<WebState>> {
    Router::new().route("/status/{slug}", axum::routing::get(show))
}

async fn show(State(state): State<Arc<WebState>>, Path(slug): Path<String>) -> Response {
    let (slug, json) = match slug.strip_suffix(".json") {
        Some(s) => (s.to_string(), true),
        None => (slug, false),
    };
    if uptime::validate_slug(&slug).is_some() {
        return not_found(json);
    }
    let now = crate::time::now_micros();
    let found = state.db(move |c| c.with(|x| uptime::public_status(x, &slug, now))).await;
    let v = match found {
        Ok(Some(v)) => v,
        Ok(None) => return not_found(json),
        Err(e) => {
            tracing::error!(error = %e, "status page failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
        }
    };
    let (ctype, body) = if json { ("application/json", v.to_string()) } else { ("text/html; charset=utf-8", render(&v)) };
    let mut r = (StatusCode::OK, body).into_response();
    let hd = r.headers_mut();
    hd.insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    hd.insert(header::CACHE_CONTROL, HeaderValue::from_str(&format!("public, max-age={MAX_AGE_SECS}")).expect("header"));
    headers(hd);
    if json {
        // Read by other sites' status widgets.
        hd.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    }
    r
}

fn headers(hd: &mut axum::http::HeaderMap) {
    hd.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    hd.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    hd.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"),
    );
}

fn not_found(json: bool) -> Response {
    let mut r = if json {
        (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "application/json")], r#"{"error":"not found"}"#).into_response()
    } else {
        (StatusCode::NOT_FOUND, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], "<!doctype html><title>Not found</title><p>Not found</p>").into_response()
    };
    headers(r.headers_mut());
    r
}

fn overall_text(s: &str) -> (&'static str, &'static str) {
    match s {
        "operational" => ("All systems operational", "up"),
        "degraded" => ("Some systems are down", "degraded"),
        "down" => ("All systems are down", "down"),
        _ => ("Status not yet known", "unknown"),
    }
}

fn pct(v: &Value) -> String {
    v.as_f64().map_or("no data".into(), |p| if p >= 100.0 { "100%".into() } else { format!("{:.2}%", (p * 100.0).floor() / 100.0) })
}

/// A day's bar class: no data, all good, some failures, mostly down.
fn bar_class(v: &Value) -> &'static str {
    match v.as_f64() {
        None => "n",
        Some(p) if p >= 100.0 => "u",
        Some(p) if p >= 95.0 => "p",
        Some(_) => "d",
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// The HTML page for one `public_status` value.
pub fn render(v: &Value) -> String {
    let title = h(s(v, "title"));
    let (headline, cls) = overall_text(s(v, "status"));
    let mut out = String::with_capacity(16 * 1024);
    out.push_str(&format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <meta name=\"robots\" content=\"noindex\"><title>{title}</title><style>{CSS}</style></head><body><main>\
         <h1>{title}</h1><div class=\"banner {cls}\">{headline}</div><section><h2>Components</h2>"
    ));
    let components = v["components"].as_array().cloned().unwrap_or_default();
    if components.is_empty() {
        out.push_str("<p class=\"muted\">No components are listed yet.</p>");
    }
    for c in &components {
        let status = s(c, "status");
        let label = match status {
            "up" => "Operational",
            "down" => "Down",
            _ => "Unknown",
        };
        out.push_str(&format!("<div class=\"comp\"><div class=\"row\"><span class=\"name\">{}", h(s(c, "name"))));
        if let Some(url) = c["url"].as_str() {
            out.push_str(&format!(" <a href=\"{}\" rel=\"nofollow noopener\">{}</a>", h(url), h(url)));
        }
        out.push_str(&format!("</span><span class=\"st {status}\">{label}</span></div><div class=\"bars\" role=\"img\" aria-label=\"90 days of uptime\">"));
        for d in c["days"].as_array().into_iter().flatten() {
            let tip = format!("{}: {}", s(d, "date"), pct(&d["uptime"]));
            out.push_str(&format!("<i class=\"{}\" title=\"{}\"></i>", bar_class(&d["uptime"]), h(&tip)));
        }
        out.push_str(&format!(
            "</div><div class=\"legend\"><span>90 days ago</span><span>{} uptime</span><span>Today</span></div></div>",
            pct(&c["uptime_90d"])
        ));
    }
    out.push_str("</section><section><h2>Recent incidents</h2>");
    let incidents = v["incidents"].as_array().cloned().unwrap_or_default();
    if incidents.is_empty() {
        out.push_str("<p class=\"muted\">No incidents in the last 90 days.</p>");
    }
    for i in &incidents {
        let started = s(i, "started_at");
        let dur = uptime::duration_words(i["duration_seconds"].as_i64().unwrap_or(0));
        let (state, cls) = if i["ended_at"].is_null() { (format!("Ongoing, {dur} so far"), "down") } else { (format!("Resolved after {dur}"), "up") };
        out.push_str(&format!(
            "<div class=\"inc\"><div class=\"row\"><span class=\"name\">{} was down</span><span class=\"st {cls}\">{}</span></div>\
             <div class=\"muted\"><time datetime=\"{started}\">{}</time></div></div>",
            h(s(i, "component")),
            h(&state),
            h(&started.replace('T', " ").replace('Z', " UTC"))
        ));
    }
    out.push_str(&format!(
        "</section><footer>Updated <time datetime=\"{u}\">{}</time>. Checked from one location every minute or so. \
         <a href=\"{src}\">Railwatch</a></footer></main></body></html>",
        h(&s(v, "updated_at").replace('T', " ").replace('Z', " UTC")),
        u = h(s(v, "updated_at")),
        src = crate::SOURCE_URL
    ));
    out
}

const CSS: &str = "\
:root{color-scheme:light dark;--bg:#fafaf9;--fg:#1c1917;--mut:#78716c;--card:#fff;--bd:#e7e5e4;--up:#16a34a;--deg:#d97706;--down:#dc2626;--none:#d6d3d1}\
@media(prefers-color-scheme:dark){:root{--bg:#0c0a09;--fg:#f5f5f4;--mut:#a8a29e;--card:#1c1917;--bd:#292524;--none:#44403c}}\
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.5 system-ui,-apple-system,Segoe UI,Roboto,sans-serif}\
main{max-width:760px;margin:0 auto;padding:32px 16px}h1{font-size:24px;margin:0 0 16px}h2{font-size:15px;margin:32px 0 8px;color:var(--mut);font-weight:600}\
.banner{border-radius:10px;padding:16px 18px;font-weight:600;color:#fff}.banner.up{background:var(--up)}.banner.degraded{background:var(--deg)}\
.banner.down{background:var(--down)}.banner.unknown{background:var(--mut)}\
.comp,.inc{background:var(--card);border:1px solid var(--bd);border-radius:10px;padding:14px 16px;margin-bottom:10px}\
.row{display:flex;justify-content:space-between;gap:12px;align-items:baseline;flex-wrap:wrap}.name{font-weight:600;overflow-wrap:anywhere}\
.name a{font-weight:400;font-size:13px;color:var(--mut)}.st{font-size:13px;font-weight:600}.st.up{color:var(--up)}.st.down{color:var(--down)}.st.unknown{color:var(--mut)}\
.bars{display:flex;gap:2px;height:30px;margin:10px 0 4px}.bars i{flex:1;border-radius:2px;background:var(--none)}\
.bars i.u{background:var(--up)}.bars i.p{background:var(--deg)}.bars i.d{background:var(--down)}\
@media(max-width:520px){.bars{gap:1px}.bars i:nth-child(-n+60){display:none}.legend span:first-child{visibility:hidden}}\
.legend{display:flex;justify-content:space-between;font-size:12px;color:var(--mut)}.muted{color:var(--mut);font-size:13px}\
footer{margin-top:32px;font-size:12px;color:var(--mut)}footer a{color:inherit}";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_page_escapes_what_admins_type_and_shows_overall_status() {
        let v = json!({
            "title": "<script>alert(1)</script> status", "status": "degraded", "updated_at": "2026-11-01T10:00:00Z",
            "components": [{"name": "Shop & \"API\"", "status": "down", "uptime_90d": 99.5, "url": "https://x.example/?a=1&b=<2>",
                            "days": [{"date": "2026-10-31", "uptime": 100.0}, {"date": "2026-11-01", "uptime": 50.0}, {"date": "2026-10-30", "uptime": null}]}],
            "incidents": [{"component": "Shop", "started_at": "2026-11-01T09:00:00Z", "ended_at": null, "duration_seconds": 3600}],
        });
        let html = render(&v);
        assert!(!html.contains("<script>"), "admin text is escaped");
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt; status"));
        assert!(html.contains("Shop &amp; &quot;API&quot;"));
        assert!(html.contains("https://x.example/?a=1&amp;b=&lt;2&gt;"));
        assert!(html.contains("Some systems are down"));
        assert!(html.contains("99.50% uptime"));
        assert!(html.contains("Ongoing, 1 h so far"));
        assert!(html.contains("class=\"u\"") && html.contains("class=\"d\"") && html.contains("class=\"n\""));
        assert!(!html.contains("<script"), "no script at all");
    }

    #[test]
    fn uptime_is_never_rounded_up_to_100() {
        assert_eq!(pct(&json!(99.999)), "99.99%");
        assert_eq!(pct(&json!(100.0)), "100%");
        assert_eq!(pct(&Value::Null), "no data");
    }
}
