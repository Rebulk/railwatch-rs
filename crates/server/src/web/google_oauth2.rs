//! OmniAuth::Strategies::GoogleOauth2 (omniauth-google-oauth2 1.2.3 over
//! omniauth-oauth2 1.9.0, as railwatch-cloud configures it), as an OpenID
//! Connect authorization-code flow:
//!
//! - request phase: a random `state` (SecureRandom.hex(24)), a PKCE S256
//!   challenge and an OIDC `nonce`, all three kept in the encrypted session
//!   under OmniAuth's keys, then a 302 to Google;
//! - callback phase: `state` compared in constant time and taken out of the
//!   session (OmniAuth's csrf_detected), Google's `error` reported as the
//!   failure type, the code exchanged with the PKCE verifier, and the
//!   identity read from the ID token, whose RS256 signature is checked
//!   against Google's JWKS (cached an hour, refetched when a token names an
//!   unknown key, at most once a minute; stale keys served through an
//!   outage, as the gem's `cached_jwks`), with iss, aud, exp/nbf (60 s
//!   leeway), sub and the nonce verified.
//!
//! Rails reads the identity from the userinfo endpoint and trusts the ID
//! token without its signature (it came over TLS); here the verified ID
//! token is the only source (docs/DIFFERENCES.md).

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use oauth2::basic::{BasicErrorResponse, BasicRevocationErrorResponse, BasicTokenIntrospectionResponse, BasicTokenType};
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet, EndpointSet, ExtraTokenFields, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, RequestTokenError, Scope, StandardRevocableToken, StandardTokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use subtle::ConstantTimeEq;

use crate::config::Google;

/// OmniAuth's strategy name: the path segment and `provider` column.
pub const PROVIDER: &str = "google_oauth2";

/// Session keys (OmniAuth's names).
pub const STATE: &str = "omniauth.state";
pub const PKCE_VERIFIER: &str = "omniauth.pkce.verifier";
pub const NONCE: &str = "omniauth.nonce";
pub const PARAMS: &str = "omniauth.params";
pub const ORIGIN: &str = "omniauth.origin";

const GOOGLE_ISSUERS: [&str; 2] = ["accounts.google.com", "https://accounts.google.com"];
const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const GOOGLE_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";

/// The gem's JWKS_CACHE_TTL, JWKS_RETRY_INTERVAL and jwt_leeway.
const JWKS_CACHE_TTL: Duration = Duration::from_secs(3600);
const JWKS_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const JWT_LEEWAY: u64 = 60;

pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub jwks: String,
    pub issuers: Vec<String>,
}

/// Google's endpoints, or a stand-in provider's (`google_issuer`, tests).
pub fn endpoints(g: &Google) -> Endpoints {
    match &g.issuer {
        None => Endpoints {
            authorize: GOOGLE_AUTHORIZE_URL.into(),
            token: GOOGLE_TOKEN_URL.into(),
            jwks: GOOGLE_JWKS_URL.into(),
            issuers: GOOGLE_ISSUERS.iter().map(|s| s.to_string()).collect(),
        },
        Some(i) => Endpoints { authorize: format!("{i}/authorize"), token: format!("{i}/token"), jwks: format!("{i}/certs"), issuers: vec![i.clone()] },
    }
}

/// One shared client: no redirects (a token endpoint never redirects; the
/// oauth2 crate's advice against SSRF), bounded time, rustls.
fn http() -> &'static reqwest::Client {
    static C: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client")
    });
    &C
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdTokenField {
    id_token: Option<String>,
}
impl ExtraTokenFields for IdTokenField {}

type Client = oauth2::Client<
    BasicErrorResponse,
    StandardTokenResponse<IdTokenField, BasicTokenType>,
    BasicTokenIntrospectionResponse,
    StandardRevocableToken,
    BasicRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
>;

fn client(g: &Google, ep: &Endpoints, redirect_uri: &str) -> Result<Client, String> {
    Ok(oauth2::Client::new(ClientId::new(g.client_id.clone()))
        .set_client_secret(ClientSecret::new(g.client_secret.clone()))
        .set_auth_uri(AuthUrl::new(ep.authorize.clone()).map_err(|e| e.to_string())?)
        .set_token_uri(TokenUrl::new(ep.token.clone()).map_err(|e| e.to_string())?)
        .set_redirect_uri(RedirectUrl::new(redirect_uri.to_string()).map_err(|e| e.to_string())?))
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("random");
    crate::ingest::receipt_headers::hex(&b)
}

/// The request phase's redirect and what the session must keep for the
/// callback.
pub struct Authorize {
    pub url: String,
    pub state: String,
    pub pkce_verifier: String,
    pub nonce: String,
}

/// `request_phase`: Google's consent URL. `prompt` and `login_hint` pass
/// through from the form (the reauthentication button sends
/// `prompt=select_account`).
pub fn authorize(g: &Google, redirect_uri: &str, prompt: Option<&str>, login_hint: Option<&str>) -> Result<Authorize, String> {
    let ep = endpoints(g);
    let state = random_hex(24);
    let nonce = random_hex(24);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let s2 = state.clone();
    let client = client(g, &ep, redirect_uri)?;
    let mut req = client
        .authorize_url(move || CsrfToken::new(s2))
        .add_scopes(["openid", "email", "profile"].map(|s| Scope::new(s.into())))
        .set_pkce_challenge(challenge)
        .add_extra_param("nonce", nonce.clone());
    if let Some(p) = prompt.filter(|p| !p.is_empty()) {
        req = req.add_extra_param("prompt", p.to_string());
    }
    if let Some(h) = login_hint.filter(|h| !h.is_empty()) {
        req = req.add_extra_param("login_hint", h.to_string());
    }
    let (url, _) = req.url();
    Ok(Authorize { url: url.to_string(), state, pkce_verifier: verifier.secret().clone(), nonce })
}

/// What the callback learned: OmniAuth's auth hash, the parts the app reads.
#[derive(Debug, Clone)]
pub struct Identity {
    pub uid: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
}

/// `fail!(message_key)`: the `message` OmniAuth's failure endpoint puts in
/// `/auth/failure?message=...`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    CsrfDetected,
    /// Google's `error` (access_denied, ...).
    Provider(String),
    InvalidCredentials(String),
    Timeout,
    FailedToConnect,
}

impl Failure {
    pub fn message_key(&self) -> &str {
        match self {
            Failure::CsrfDetected => "csrf_detected",
            Failure::Provider(e) => e,
            Failure::InvalidCredentials(_) => "invalid_credentials",
            Failure::Timeout => "timeout",
            Failure::FailedToConnect => "failed_to_connect",
        }
    }
}

/// What the request phase left in the session, taken out by the callback.
pub struct Pending {
    pub state: Option<String>,
    pub pkce_verifier: Option<String>,
    pub nonce: Option<String>,
}

/// `callback_phase`.
pub async fn callback(
    g: &Google,
    redirect_uri: &str,
    code: &str,
    state: &str,
    error: Option<&str>,
    pending: Pending,
) -> Result<Identity, Failure> {
    let expected = pending.state.unwrap_or_default();
    if state.is_empty() || expected.is_empty() || !bool::from(state.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(Failure::CsrfDetected);
    }
    if let Some(e) = error.filter(|e| !e.is_empty()) {
        return Err(Failure::Provider(e.to_string()));
    }
    let (Some(verifier), Some(nonce)) = (pending.pkce_verifier, pending.nonce) else {
        return Err(Failure::CsrfDetected);
    };
    let ep = endpoints(g);
    let client = client(g, &ep, redirect_uri).map_err(Failure::InvalidCredentials)?;
    let token = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(http())
        .await
        .map_err(|e| match e {
            RequestTokenError::Request(oauth2::HttpClientError::Reqwest(r)) if r.is_timeout() => Failure::Timeout,
            RequestTokenError::Request(_) => Failure::FailedToConnect,
            RequestTokenError::ServerResponse(r) => Failure::InvalidCredentials(r.to_string()),
            other => Failure::InvalidCredentials(other.to_string()),
        })?;
    let id_token = token.extra_fields().id_token.clone().ok_or_else(|| Failure::InvalidCredentials("no id_token in the token response".into()))?;
    verify_id_token(&id_token, g, &ep, &nonce).await.map_err(Failure::InvalidCredentials)
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    email: Option<String>,
    email_verified: Option<Value>,
    name: Option<String>,
    nonce: Option<String>,
    azp: Option<String>,
}

/// The ID token, verified: RS256 only, signature against the issuer's
/// JWKS, iss, aud (and azp when present) = our client id, exp/nbf with the
/// gem's 60 s leeway, sub present, and our nonce.
pub async fn verify_id_token(token: &str, g: &Google, ep: &Endpoints, nonce: &str) -> Result<Identity, String> {
    ring_provider::install();
    let header = jsonwebtoken::decode_header(token).map_err(|e| format!("id_token header: {e}"))?;
    if header.alg != Algorithm::RS256 {
        return Err(format!("id_token alg {:?}, expected RS256", header.alg));
    }
    let kid = header.kid.ok_or("id_token has no kid")?;
    let key = signing_key(&ep.jwks, &kid).await?;
    let mut v = Validation::new(Algorithm::RS256);
    v.set_audience(&[&g.client_id]);
    v.set_issuer(&ep.issuers);
    v.set_required_spec_claims(&["iss", "aud", "exp", "sub"]);
    v.leeway = JWT_LEEWAY;
    v.validate_nbf = true;
    let claims = jsonwebtoken::decode::<Claims>(token, &key, &v).map_err(|e| format!("id_token: {e}"))?.claims;
    if !claims.nonce.as_deref().is_some_and(|n| bool::from(n.as_bytes().ct_eq(nonce.as_bytes()))) {
        return Err("id_token nonce does not match".into());
    }
    if claims.azp.as_deref().is_some_and(|a| a != g.client_id) {
        return Err("id_token azp is another client".into());
    }
    if claims.sub.is_empty() {
        return Err("id_token sub is empty".into());
    }
    // Google sends a boolean; some providers send "true".
    let email_verified = matches!(&claims.email_verified, Some(Value::Bool(true))) || matches!(&claims.email_verified, Some(Value::String(s)) if s == "true");
    Ok(Identity { uid: claims.sub, email: claims.email, email_verified, name: claims.name })
}

// ---------------------------------------------------------------------------
// JWKS cache (GoogleOauth2.cached_jwks), one per key-set URL.

#[derive(Default)]
struct JwksEntry {
    keys: Option<HashMap<String, DecodingKey>>,
    expires_at: Option<Instant>,
    retry_at: Option<Instant>,
    forced_at: Option<Instant>,
}

impl JwksEntry {
    /// Whether to fetch now. `force`: a token named a kid we do not hold,
    /// honoured at most once per retry interval (the sender picks the kid,
    /// so it must not drive unlimited fetches). With nothing cached, a
    /// failed fetch backs off for the retry interval.
    fn should_fetch(&mut self, force: bool, now: Instant) -> Result<bool, String> {
        let force = force && self.forced_at.is_none_or(|t| now >= t + JWKS_RETRY_INTERVAL);
        if force || self.keys.is_none() || self.expires_at.is_none_or(|t| now >= t) {
            if self.keys.is_none() && self.retry_at.is_some_and(|t| now < t) {
                return Err("JWKS unavailable (within the retry backoff)".into());
            }
            if force {
                self.forced_at = Some(now);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// A fetch's outcome. A failure keeps serving the stale set until the
    /// retry interval passes; with no set at all it is the caller's error.
    fn record(&mut self, fetched: Result<HashMap<String, DecodingKey>, String>, now: Instant) -> Result<(), String> {
        match fetched {
            Ok(keys) => {
                self.keys = Some(keys);
                self.expires_at = Some(now + JWKS_CACHE_TTL);
                Ok(())
            }
            Err(e) => {
                self.retry_at = Some(now + JWKS_RETRY_INTERVAL);
                if self.keys.is_none() {
                    return Err(e);
                }
                tracing::warn!(error = %e, "JWKS refresh failed; serving the cached keys");
                self.expires_at = self.retry_at;
                Ok(())
            }
        }
    }

    fn get(&self, kid: &str) -> Option<DecodingKey> {
        self.keys.as_ref()?.get(kid).cloned()
    }
}

static JWKS: LazyLock<tokio::sync::Mutex<HashMap<String, JwksEntry>>> = LazyLock::new(Default::default);

/// The key `kid` from the set at `url`. The lock is held across the fetch,
/// as the gem's: concurrent sign-ins wait for one request.
async fn signing_key(url: &str, kid: &str) -> Result<DecodingKey, String> {
    let mut all = JWKS.lock().await;
    let entry = all.entry(url.to_string()).or_default();
    for force in [false, true] {
        if entry.should_fetch(force, Instant::now())? {
            let fetched = fetch_jwks(url).await;
            entry.record(fetched, Instant::now())?;
        }
        if let Some(k) = entry.get(kid) {
            return Ok(k);
        }
    }
    Err(format!("id_token kid {kid:?} is not in the key set"))
}

/// The RSA keys of the set at `url`, by kid (also `railwatch doctor`).
pub async fn fetch_jwks(url: &str) -> Result<HashMap<String, DecodingKey>, String> {
    let res = http().get(url).send().await.map_err(|e| format!("JWKS fetch: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("JWKS fetch: HTTP {}", res.status()));
    }
    let set: jsonwebtoken::jwk::JwkSet = res.json().await.map_err(|e| format!("JWKS response: {e}"))?;
    let mut keys = HashMap::new();
    for jwk in &set.keys {
        let (Some(kid), jsonwebtoken::jwk::AlgorithmParameters::RSA(rsa)) = (&jwk.common.key_id, &jwk.algorithm) else { continue };
        if let Ok(k) = DecodingKey::from_rsa_components(&rsa.n, &rsa.e) {
            keys.insert(kid.clone(), k);
        }
    }
    if keys.is_empty() {
        return Err("JWKS response had no RSA keys".into());
    }
    Ok(keys)
}

/// jsonwebtoken's crypto backend, on ring (already in the tree for rustls):
/// RS256 verification only. Its own backends are aws-lc-rs or the `rsa`
/// crate (RUSTSEC-2023-0071); neither is needed to verify.
mod ring_provider {
    use jsonwebtoken::crypto::{CryptoProvider, JwtSigner, JwtVerifier, KeyUtils};
    use jsonwebtoken::errors::{ErrorKind, Result};
    use jsonwebtoken::signature::{Error, Verifier};
    use jsonwebtoken::{Algorithm, DecodingKey, DecodingKeyKind, EncodingKey};

    struct Rs256(DecodingKey);

    impl Verifier<Vec<u8>> for Rs256 {
        fn verify(&self, msg: &[u8], signature: &Vec<u8>) -> std::result::Result<(), Error> {
            let DecodingKeyKind::RsaModulusExponent { n, e } = self.0.kind() else { return Err(Error::new()) };
            ring::signature::RsaPublicKeyComponents { n, e }
                .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, msg, signature)
                .map_err(|_| Error::new())
        }
    }

    impl JwtVerifier for Rs256 {
        fn algorithm(&self) -> Algorithm {
            Algorithm::RS256
        }
    }

    fn verifier(alg: &Algorithm, key: &DecodingKey) -> Result<Box<dyn JwtVerifier>> {
        match alg {
            Algorithm::RS256 => Ok(Box::new(Rs256(key.clone()))),
            _ => Err(ErrorKind::InvalidAlgorithm.into()),
        }
    }

    fn signer(_: &Algorithm, _: &EncodingKey) -> Result<Box<dyn JwtSigner>> {
        Err(ErrorKind::InvalidAlgorithm.into())
    }

    static PROVIDER: CryptoProvider = CryptoProvider { signer_factory: signer, verifier_factory: verifier, key_utils: KeyUtils::new_unimplemented() };

    pub fn install() {
        // Err only when already installed (by an earlier call).
        let _ = PROVIDER.install_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> HashMap<String, DecodingKey> {
        HashMap::from([("k1".to_string(), DecodingKey::from_rsa_raw_components(&[1, 0, 1], &[1, 0, 1]))])
    }

    #[test]
    fn jwks_cache_refreshes_hourly_and_on_unknown_kids_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut e = JwksEntry::default();
        assert_eq!(e.should_fetch(false, t0), Ok(true), "nothing cached");
        e.record(Ok(keys()), t0).unwrap();
        assert_eq!(e.should_fetch(false, t0 + Duration::from_secs(3599)), Ok(false), "fresh for an hour");
        assert_eq!(e.should_fetch(false, t0 + Duration::from_secs(3600)), Ok(true), "stale after an hour");
        assert_eq!(e.should_fetch(true, t0 + Duration::from_secs(10)), Ok(true), "an unknown kid forces a refresh");
        assert_eq!(e.should_fetch(true, t0 + Duration::from_secs(30)), Ok(false), "but not again within a minute");
        assert_eq!(e.should_fetch(true, t0 + Duration::from_secs(70)), Ok(true), "a minute later it may");
    }

    #[test]
    fn jwks_cache_serves_stale_keys_through_an_outage_and_backs_off_when_empty() {
        let t0 = Instant::now();
        let mut e = JwksEntry::default();
        assert!(e.record(Err("down".into()), t0).is_err(), "nothing to serve");
        assert!(e.should_fetch(false, t0 + Duration::from_secs(5)).is_err(), "backing off");
        assert_eq!(e.should_fetch(false, t0 + Duration::from_secs(61)), Ok(true));
        e.record(Ok(keys()), t0 + Duration::from_secs(61)).unwrap();
        let later = t0 + Duration::from_secs(61 + 3600);
        assert_eq!(e.should_fetch(false, later), Ok(true));
        e.record(Err("down again".into()), later).unwrap();
        assert!(e.get("k1").is_some(), "stale set still served");
        assert_eq!(e.should_fetch(false, later + Duration::from_secs(30)), Ok(false), "no refetch storm");
        assert_eq!(e.should_fetch(false, later + Duration::from_secs(60)), Ok(true));
    }

    #[test]
    fn failure_message_keys_are_omniauths() {
        assert_eq!(Failure::CsrfDetected.message_key(), "csrf_detected");
        assert_eq!(Failure::Provider("access_denied".into()).message_key(), "access_denied");
        assert_eq!(Failure::InvalidCredentials("x".into()).message_key(), "invalid_credentials");
    }
}
