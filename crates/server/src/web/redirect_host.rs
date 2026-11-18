//! Moving the dashboard to a new host name without breaking the old one.
//!
//! A request whose Host is one of `redirect_hosts` gets a 308 to the same
//! path and query on `url`, so a bookmark, an emailed link or a sign-in
//! started on the old name ends up on the new one, where the session
//! cookie and Google's redirect URI are.
//!
//! Machine traffic is never redirected: apps on older gems post telemetry
//! to the old name for as long as they run, agents keep their MCP URL, and
//! a redirect would at best cost them a round trip (a 308 is not followed
//! for a POST by every client) and at worst lose data. Those paths are
//! served on the old name exactly as before.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::WebState;

/// Path prefixes served on every host name: telemetry, health, the API and
/// MCP (bearer tokens), the live channel, discovery and the beacon.
const MACHINE_PREFIXES: &[&str] = &["/ingest", "/up", "/api", "/mcp", "/cable", "/.well-known", "/railwatch/beacon", "/beacon", "/source"];

fn is_machine(path: &str) -> bool {
    MACHINE_PREFIXES.iter().any(|p| path == *p || path.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')))
}

/// The request's host name, lower-cased, without a port.
fn host_of(req: &Request) -> Option<String> {
    let raw = req.headers().get(header::HOST)?.to_str().ok()?;
    let name = raw.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map_or(raw, |(h, _)| h);
    Some(name.trim_end_matches('.').to_ascii_lowercase())
}

/// Where `req` should go instead, or None to serve it here.
pub fn target(redirect_hosts: &[String], url: &str, req: &Request) -> Option<String> {
    // Nested under a base path, this router sees the path without it; the
    // redirect and the machine check use the request as it arrived.
    let uri = req.extensions().get::<axum::extract::OriginalUri>().map_or(req.uri(), |o| &o.0);
    if redirect_hosts.is_empty() || is_machine(uri.path()) {
        return None;
    }
    let host = host_of(req)?;
    if !redirect_hosts.contains(&host) {
        return None;
    }
    let path_and_query = uri.path_and_query().map_or("/", |pq| pq.as_str());
    Some(format!("{}{path_and_query}", url.trim_end_matches('/')))
}

pub async fn layer(State(state): State<Arc<WebState>>, req: Request, next: Next) -> Response {
    match target(&state.config.redirect_hosts, &state.config.url, &req) {
        None => next.run(req).await,
        Some(location) => match HeaderValue::from_str(&location) {
            // 308 keeps the method, so a form posted to the old name is
            // posted again on the new one rather than turned into a GET.
            Ok(v) => (StatusCode::PERMANENT_REDIRECT, [(header::LOCATION, v)]).into_response(),
            Err(_) => next.run(req).await,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req(host: &str, path: &str) -> Request {
        Request::builder().uri(path).header(header::HOST, host).body(Body::empty()).unwrap()
    }

    const NEW: &str = "https://app.railwatch.dev";

    fn hosts() -> Vec<String> {
        vec!["railwatch.rebulk.com".into()]
    }

    #[test]
    fn pages_on_an_old_host_move_to_the_url_with_their_path_and_query() {
        assert_eq!(target(&hosts(), NEW, &req("railwatch.rebulk.com", "/")).as_deref(), Some("https://app.railwatch.dev/"));
        assert_eq!(target(&hosts(), NEW, &req("railwatch.rebulk.com", "/sign_in")).as_deref(), Some("https://app.railwatch.dev/sign_in"));
        assert_eq!(
            target(&hosts(), NEW, &req("RailWatch.Rebulk.com:443", "/apps/1/issues?status=open")).as_deref(),
            Some("https://app.railwatch.dev/apps/1/issues?status=open"),
            "host matched without case or port"
        );
        assert_eq!(target(&hosts(), NEW, &req("railwatch.rebulk.com", "/auth/google_oauth2")).as_deref(), Some("https://app.railwatch.dev/auth/google_oauth2"));
    }

    #[test]
    fn telemetry_and_machine_endpoints_stay_on_the_old_host() {
        for path in ["/ingest", "/ingest/v1/batch", "/up", "/api/v1/issues", "/mcp", "/cable", "/.well-known/mcp.json", "/railwatch/beacon", "/source"] {
            assert_eq!(target(&hosts(), NEW, &req("railwatch.rebulk.com", path)), None, "{path}");
        }
        // A page whose name only starts like a machine prefix still moves.
        assert!(target(&hosts(), NEW, &req("railwatch.rebulk.com", "/upgrade-notes")).is_some());
        assert!(target(&hosts(), NEW, &req("railwatch.rebulk.com", "/apis")).is_some());
    }

    #[test]
    fn other_hosts_and_an_empty_list_are_served_here() {
        assert_eq!(target(&hosts(), NEW, &req("app.railwatch.dev", "/sign_in")), None);
        assert_eq!(target(&hosts(), NEW, &req("rs.railwatch.rebulk.com", "/sign_in")), None, "only listed names move");
        assert_eq!(target(&[], NEW, &req("railwatch.rebulk.com", "/sign_in")), None);
        let no_host = Request::builder().uri("/sign_in").body(Body::empty()).unwrap();
        assert_eq!(target(&hosts(), NEW, &no_host), None);
    }
}
