//! The web app: railwatch-cloud's browser side (ApplicationController,
//! InertiaController, the cloud-only controllers) over the control DB,
//! rendering the real React pages through inertia-omega.
//!
//! Per request, in order: the session middleware (`session.rs`) decrypts
//! the session cookie, resolves Current (session, user, account), checks
//! CSRF on non-GET, and writes cookies back; omega's Inertia layer runs
//! inside it; handlers (`controllers.rs`, `env_pages.rs`) render pages or
//! redirect with flash.

pub mod alerts;
pub mod api;
pub mod assets;
pub mod cable;
pub mod controllers;
pub mod cookies;
pub mod dashboard_bridge;
pub mod digest_mailer;
pub mod document;
pub mod env_pages;
pub mod google_oauth2;
pub mod inertia;
pub mod integrations;
pub mod issues;
pub mod mailer;
pub mod mcp;
mod mcp_summary;
pub mod params;
pub mod proxy_auth;
pub mod session;
pub mod sessions_oauth;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{ConnectInfo, Request};

use crate::config::{Config, Signup};
use crate::control::Control;
use crate::rate_limit::FixedWindow;
use crate::tenants::Tenants;

pub struct WebState {
    pub control: Arc<Control>,
    pub config: Config,
    pub keys: cookies::Keys,
    pub tenants: Tenants,
    /// Read-only connections to tenant files for the dashboard pages; ingest
    /// writes through `tenants`' writer threads, so a slow page never holds
    /// the write lock.
    pub readers: crate::dashboard::pool::ReadPools,
    /// Signs telemetry cursors (CursorPage); derived from secret_key_base.
    pub cursor_key: crate::dashboard::CursorKey,
    pub cable: cable::Hub,
    pub mailer: Option<mailer::Mailer>,
    /// SessionsController: 30 sign-in attempts per 5 minutes per IP.
    pub sign_in_limit: FixedWindow<IpAddr>,
    /// Rust-only: 10 failed sign-ins per 5 minutes per submitted email
    /// (lower-cased). Only failures count, and the right password always
    /// signs in, so knowing someone's email does not let you lock them out;
    /// a full bucket logs a warning (sessions::sign_in_create).
    pub sign_in_email_limit: FixedWindow<String>,
    /// UsersController: 10 sign-ups an hour per IP.
    pub sign_up_limit: FixedWindow<IpAddr>,
    /// PasswordResets/EmailVerifications: 20 an hour per IP.
    pub email_limit: FixedWindow<IpAddr>,
    /// Provisioning (accounts/applications/environments): 30/hour/user.
    pub provisioning_limit: FixedWindow<i64>,
    /// IntegrationsController#send_test: 5 a minute per user.
    pub send_test_limit: FixedWindow<i64>,
    /// Encrypts integration secrets (alerts::integration).
    pub settings_cipher: crate::alerts::integration::SettingsCipher,
    /// McpController's preauth-ip limit: 300 a minute per IP, before the
    /// token lookup.
    pub mcp_preauth_limit: FixedWindow<IpAddr>,
    /// Api::BaseController's rate_limit: 600 a minute per user, per controller.
    pub api_rate_limit: FixedWindow<(&'static str, i64)>,
}

impl WebState {
    pub fn new(control: Arc<Control>, config: Config, tenants: Tenants, cable: cable::Hub) -> Result<WebState, String> {
        let secret = config.secret()?;
        let keys = cookies::Keys::new(&secret);
        let cursor_key = crate::dashboard::CursorKey(cursor_key_bytes(&secret));
        let readers = crate::dashboard::pool::ReadPools::new(tenants.dir(), 4);
        let mailer = config.smtp.as_ref().map(mailer::Mailer::new).transpose()?;
        Ok(WebState {
            control,
            keys,
            readers,
            cursor_key,
            tenants,
            cable,
            mailer,
            sign_in_limit: FixedWindow::with_window(30, Duration::from_secs(300)),
            sign_in_email_limit: FixedWindow::with_window(10, Duration::from_secs(300)),
            sign_up_limit: FixedWindow::with_window(10, Duration::from_secs(3600)),
            email_limit: FixedWindow::with_window(20, Duration::from_secs(3600)),
            provisioning_limit: FixedWindow::with_window(30, Duration::from_secs(3600)),
            send_test_limit: FixedWindow::with_window(5, Duration::from_secs(60)),
            settings_cipher: crate::alerts::integration::SettingsCipher::new(&secret),
            mcp_preauth_limit: mcp::preauth_limit(),
            api_rate_limit: api::rate_limit(),
            config,
        })
    }

    pub fn signup_open(&self) -> bool {
        self.config.signup == Signup::Open
    }

    /// The client address (Phase 1's rule: XFF only with trust_proxy).
    pub fn client_ip(&self, req: &Request) -> IpAddr {
        let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::from([127, 0, 0, 1]));
        if self.config.trust_proxy
            && let Some(xff) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(ip) = xff.split(',').filter_map(|s| s.trim().parse::<IpAddr>().ok()).next_back()
        {
            return ip;
        }
        peer
    }

    /// Runs a blocking control-DB closure off the async runtime.
    pub async fn db<T: Send + 'static>(self: &Arc<Self>, f: impl FnOnce(&Control) -> T + Send + 'static) -> T {
        let control = self.control.clone();
        tokio::task::spawn_blocking(move || f(&control)).await.expect("control DB task panicked")
    }
}

/// Everything browser-facing. The ingest router (http.rs) handles /ingest,
/// /up and is the fallback for anything this one does not route.
pub fn router(state: Arc<WebState>) -> Router {
    let mounted = !state.config.base_path.is_empty();
    let pages = controllers::routes()
        .merge(env_pages::routes())
        .merge(issues::routes())
        .merge(alerts::routes())
        .merge(integrations::routes())
        .merge(sessions_oauth::routes());
    let pages = inertia::layer(pages);
    let pages = session::layer(pages, state.clone());
    let router = Router::new()
        .merge(pages)
        // McpController: bearer-token auth, outside the session layer.
        .merge(mcp::routes())
        // /api/v1, the same tokens.
        .merge(api::routes())
        .route("/cable", axum::routing::get(cable::upgrade))
        .route("/vite/{*path}", axum::routing::get(controllers::asset))
        // AGPL-3.0 section 13: the Corresponding Source of this server.
        .route("/source", axum::routing::get(|| async { axum::response::Redirect::to(crate::SOURCE_URL) }))
        .route("/icon.png", axum::routing::get(controllers::icon))
        .route("/icon.svg", axum::routing::get(controllers::icon))
        // The dashboard's own browser client (lib/railwatch.ts) beacons its page
        // visits here. Rails cloud records them through the mounted gem engine;
        // this server does not monitor itself, so it answers like the gem's
        // BeaconController with beacon_enabled off: 204, nothing stored.
        .route("/railwatch/beacon", axum::routing::post(|| async { axum::http::StatusCode::NO_CONTENT }));
    // Mounted at /railwatch, the bundle's hard-coded /railwatch/beacon
    // arrives here as /beacon.
    let router = if mounted { router.route("/beacon", axum::routing::post(|| async { axum::http::StatusCode::NO_CONTENT })) } else { router };
    // Outermost: X-Railwatch-Proxy-Auth is checked and removed before any
    // route (pages, cable, MCP, API) sees the request.
    router.layer(axum::middleware::from_fn_with_state(state.clone(), proxy_auth::layer)).with_state(state)
}

/// `router` under `base_path` (when set), with `fallback` (the ingest API)
/// for everything else, so `/ingest` and `/up` stay at the root. The
/// ingest API never reads X-Railwatch-Proxy-Auth; it is dropped there too.
pub fn mounted(base_path: &str, pages: Router, fallback: Router) -> Router {
    let fallback = fallback.layer(axum::middleware::map_request(|mut req: Request| async move {
        req.headers_mut().remove(proxy_auth::HEADER);
        req
    }));
    if base_path.is_empty() { pages.fallback_service(fallback) } else { Router::new().nest(base_path, pages).fallback_service(fallback) }
}

/// The telemetry cursor signing key (CursorPage's message_verifier).
pub fn cursor_key_bytes(secret: &[u8]) -> Vec<u8> {
    hmac_derive(secret, b"telemetry_cursor")
}

/// HMAC-SHA256(secret, purpose): a per-purpose key, like Rails'
/// key_generator.generate_key(purpose).
fn hmac_derive(secret: &[u8], purpose: &[u8]) -> Vec<u8> {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(secret).expect("hmac key");
    m.update(purpose);
    m.finalize().into_bytes().to_vec()
}
