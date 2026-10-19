//! One configuration surface: `RAILWATCH_<KEY>` environment variables, then
//! `railwatch.toml` (path from `RAILWATCH_CONFIG`, else `./railwatch.toml`),
//! then the default. Keys are the lower-case names below; in the TOML file
//! they are top-level keys (`data_dir = "/var/lib/railwatch"`).
//!
//! Phase 1's names are kept: `PORT`, `RAILWATCH_BIND`, `RAILWATCH_DATA_DIR`,
//! `RAILWATCH_RATE_LIMIT_IP`, `RAILWATCH_RATE_LIMIT_ENV`,
//! `RAILWATCH_TRUST_PROXY`, `RAILWATCH_TENANT_IDLE_SECONDS`, `RAILWATCH_LOG`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Who may create a user without an invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signup {
    Open,
    InviteOnly,
}

#[derive(Debug, Clone)]
pub struct Smtp {
    pub address: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// "starttls" (default), "tls" (implicit, port 465) or "none".
    pub security: String,
    pub from: String,
}

/// Google sign-in (web::google_oauth2). Both credentials or neither.
#[derive(Debug, Clone)]
pub struct Google {
    pub client_id: String,
    pub client_secret: String,
    /// Testing only: a stand-in OpenID provider at this base URL
    /// (`<issuer>/authorize`, `/token`, `/certs`; `iss` must equal it).
    /// None: Google itself.
    pub issuer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub bind: String,
    /// The public base URL (`https://railwatch.example.com`), used in emails,
    /// the printed RAILWATCH_INGEST_URL, the Secure cookie flag and the
    /// Origin check. Default `http://<bind>`.
    pub url: String,
    pub signup: Signup,
    pub smtp: Option<Smtp>,
    pub google: Option<Google>,
    /// Hex; default: generated once into `<data_dir>/secret_key_base`.
    pub secret_key_base: Option<String>,
    pub trust_proxy: bool,
    pub rate_limit_ip: u64,
    pub rate_limit_env: u64,
    pub tenant_idle_seconds: u64,
    /// Account#quota_exhausted? turns ingest away with 402. Off by default:
    /// a self-hosted install has no billing (docs/DIFFERENCES.md).
    pub enforce_quota: bool,
    /// Per-day tenants' retention tiers (storage::retention).
    pub tiers: crate::storage::retention::Tiers,
    /// S3-compatible backups (storage::backup); None = off (the default).
    pub backup: Option<crate::storage::backup::Settings>,
    /// `alert_delivery = "off"`: alerts still fire and are recorded, but
    /// every one is marked skipped instead of being sent. For a shadow
    /// install fed a mirror of another server's ingest, whose integrations
    /// point at real people.
    pub alert_delivery: bool,
    /// Built-in HTTPS (tls.rs): `tls_domain` turns on ACME; `tls_cert_file`
    /// and `tls_key_file` serve a certificate managed elsewhere. None: plain
    /// HTTP (behind a proxy, or local).
    pub tls: Option<crate::tls::Tls>,
    /// With TLS: where port 80's redirect-to-https listens (default
    /// `0.0.0.0:80` when bind is a :443 address, else off; "off" turns it
    /// off). Ignored when a second socket is inherited (LISTEN_FDS=2).
    pub tls_redirect_bind: Option<String>,
    /// Caps on new issues per environment and per account (grouping::Limits).
    pub new_issues: crate::grouping::Limits,
    /// HTTP timeouts and the connection cap (serve::Limits).
    pub http: crate::serve::Limits,
    /// The path prefix every page is served under (`/railwatch`), or empty
    /// for the root. Ingest (`/ingest`, `/up`) always stays at the root, so
    /// the gem's RAILWATCH_INGEST_URL does not change. For a reverse proxy
    /// that mounts the dashboard inside another app (the railwatch-server
    /// gem mounts it in the Rails app it monitors).
    pub base_path: String,
    /// `trusted_proxy_secret_file`: the proxy in front signs who is signed in
    /// (web::proxy_auth); a request with a valid `X-Railwatch-Proxy-Auth`
    /// header is that user, provisioned on first sight. None: off (default).
    pub trusted_proxy_secret: Option<Vec<u8>>,
    /// Where it was read from, for `railwatch doctor`.
    pub trusted_proxy_secret_file: Option<PathBuf>,
    /// Where the values came from, for `railwatch doctor`.
    pub sources: BTreeMap<&'static str, String>,
    pub file: Option<PathBuf>,
}

struct Lookup {
    toml: Option<toml::Table>,
    file: Option<PathBuf>,
}

impl Lookup {
    fn load() -> Result<Lookup, String> {
        let explicit = std::env::var("RAILWATCH_CONFIG").ok().map(PathBuf::from);
        let path = explicit.clone().or_else(|| Some(PathBuf::from("railwatch.toml")).filter(|p| p.exists()));
        let Some(path) = path else { return Ok(Lookup { toml: None, file: None }) };
        let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Lookup { toml: Some(table), file: Some(path) })
    }

    /// (value, source)
    fn get(&self, key: &str, env_names: &[&str]) -> Option<(String, String)> {
        for name in env_names {
            if let Ok(v) = std::env::var(name) {
                return Some((v, format!("env {name}")));
            }
        }
        let v = self.toml.as_ref()?.get(key)?;
        let s = match v {
            toml::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        Some((s, format!("{} {key}", self.file.as_ref().map_or_else(String::new, |p| p.display().to_string()))))
    }
}

/// An environment file (`RAILWATCH_ENV_FILE`, /etc/railwatch/railwatch.env
/// as installed): `KEY=VALUE` lines, the subset of systemd's EnvironmentFile
/// syntax both readers agree on. Blank lines and lines starting with `#` or
/// `;` are skipped; the value is the rest of the line, trimmed, with one pair
/// of matching surrounding quotes removed. Nothing is expanded, escaped or
/// executed: the file is data, never shell.
pub fn parse_env_file(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or_else(|| format!("line {}: expected KEY=VALUE", n + 1))?;
        let key = key.trim();
        let valid = key.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_') && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
        if !valid {
            return Err(format!("line {}: {key:?} is not a variable name", n + 1));
        }
        let value = value.trim();
        let value = match value.as_bytes() {
            [q @ (b'"' | b'\''), .., last] if q == last && value.len() >= 2 => &value[1..value.len() - 1],
            _ => value,
        };
        out.push((key.to_string(), value.to_string()));
    }
    Ok(out)
}

/// Where install.sh writes the configuration (root-only, 0600).
pub const INSTALLED_ENV_FILE: &str = "/etc/railwatch/railwatch.env";

/// Reads `RAILWATCH_ENV_FILE` into the process environment; variables
/// already set win. Without it, the installed file when this user can read
/// it (root: `sudo railwatch doctor` sees the service's configuration; the
/// service itself gets it from systemd). Call first thing in main, before
/// any thread exists.
pub fn load_env_file() -> Result<(), String> {
    let path = match std::env::var_os("RAILWATCH_ENV_FILE").filter(|p| !p.is_empty()) {
        Some(p) => PathBuf::from(p),
        None => match std::fs::File::open(INSTALLED_ENV_FILE) {
            Ok(_) => PathBuf::from(INSTALLED_ENV_FILE),
            Err(_) => return Ok(()),
        },
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    for (k, v) in parse_env_file(&text).map_err(|e| format!("{}: {e}", path.display()))? {
        if std::env::var_os(&k).is_none() {
            // SAFETY: called before any other thread exists (see above).
            unsafe { std::env::set_var(&k, v) };
        }
    }
    Ok(())
}

/// `""`, `"/"` -> `""`; `"railwatch/"` -> `"/railwatch"`. Only letters,
/// digits, `-`, `_`, `.` and `/` between segments: it is written into HTML
/// attributes, cookies and Location headers unescaped.
pub fn normalize_base_path(raw: &str) -> Result<String, String> {
    // One slash off each end; any more is an empty segment, refused below.
    let raw_trimmed = raw.trim();
    let trimmed = raw_trimmed.strip_prefix('/').unwrap_or(raw_trimmed);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let ok = trimmed
        .split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')));
    if !ok {
        return Err(format!("base_path: expected a path like /railwatch, got {raw:?}"));
    }
    // The ingest API answers these at the root whatever the base path is.
    let first = trimmed.split('/').next().unwrap_or("");
    if first == "up" || first == "ingest" || first.starts_with("ingest.") {
        return Err(format!("base_path: {raw:?} is where the ingest API answers (/ingest, /up); pick another prefix"));
    }
    Ok(format!("/{trimmed}"))
}

/// The trusted proxy's HMAC key: the file's bytes, surrounding whitespace
/// trimmed. Refused (the server does not start) when it cannot be read or is
/// shorter than 32 bytes, so a misconfigured proxy fails at boot rather than
/// on every request. A file others can read is allowed (container secret
/// mounts are often 0444) but warned about.
pub fn read_proxy_secret(path: &str) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::PermissionsExt;
    let raw = std::fs::read(path).map_err(|e| format!("trusted_proxy_secret_file {path}: {e}"))?;
    let secret = raw.trim_ascii().to_vec();
    if secret.len() < 32 {
        return Err(format!("trusted_proxy_secret_file {path}: the secret must be at least 32 bytes, got {}", secret.len()));
    }
    if std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o077 != 0) {
        eprintln!("railwatch: warning: trusted_proxy_secret_file {path} is readable by other users; chmod 600 it");
    }
    Ok(secret)
}

fn truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

impl Config {
    pub fn load() -> Result<Config, String> {
        let l = Lookup::load()?;
        let mut sources = BTreeMap::new();
        let mut get = |key: &'static str, env: &[&str]| -> Option<String> {
            let (v, src) = l.get(key, env)?;
            sources.insert(key, src);
            Some(v)
        };
        let num = |v: Option<String>, key: &str, default: u64| -> Result<u64, String> {
            v.map_or(Ok(default), |s| s.trim().parse().map_err(|_| format!("{key}: not a number: {s:?}")))
        };

        let data_dir = PathBuf::from(get("data_dir", &["RAILWATCH_DATA_DIR"]).unwrap_or_else(|| "./data".into()));
        let port = get("port", &["PORT"]).unwrap_or_else(|| "8790".into());
        let bind = get("bind", &["RAILWATCH_BIND"]).unwrap_or_else(|| format!("127.0.0.1:{port}"));
        let url_given = get("url", &["RAILWATCH_URL"]);
        let url = url_given.clone().unwrap_or_else(|| format!("http://{bind}")).trim_end_matches('/').to_string();
        let signup = match get("signup", &["RAILWATCH_SIGNUP"]).as_deref().map(str::trim) {
            None | Some("invite") | Some("invite-only") | Some("invite_only") => Signup::InviteOnly,
            Some("open") => Signup::Open,
            Some(other) => return Err(format!("signup: expected \"open\" or \"invite\", got {other:?}")),
        };
        let smtp = match get("smtp_address", &["RAILWATCH_SMTP_ADDRESS"]) {
            None => None,
            Some(address) => {
                let port = get("smtp_port", &["RAILWATCH_SMTP_PORT"]);
                let port = num(port, "smtp_port", 587)?;
                let security = get("smtp_security", &["RAILWATCH_SMTP_SECURITY"]).unwrap_or_else(|| "starttls".into());
                if !matches!(security.as_str(), "starttls" | "tls" | "none") {
                    return Err(format!("smtp_security: expected starttls, tls or none, got {security:?}"));
                }
                Some(Smtp {
                    address,
                    port: u16::try_from(port).map_err(|_| "smtp_port out of range".to_string())?,
                    username: get("smtp_username", &["RAILWATCH_SMTP_USERNAME"]),
                    password: get("smtp_password", &["RAILWATCH_SMTP_PASSWORD"]),
                    security,
                    from: get("smtp_from", &["RAILWATCH_SMTP_FROM"]).unwrap_or_else(|| "Railwatch <railwatch@localhost>".into()),
                })
            }
        };
        let google = match (
            get("google_client_id", &["RAILWATCH_GOOGLE_CLIENT_ID"]).filter(|v| !v.trim().is_empty()),
            get("google_client_secret", &["RAILWATCH_GOOGLE_CLIENT_SECRET"]).filter(|v| !v.trim().is_empty()),
        ) {
            (None, None) => None,
            (Some(client_id), Some(client_secret)) => Some(Google {
                client_id: client_id.trim().to_string(),
                client_secret: client_secret.trim().to_string(),
                issuer: get("google_issuer", &["RAILWATCH_GOOGLE_ISSUER"]).map(|i| i.trim().trim_end_matches('/').to_string()).filter(|i| !i.is_empty()),
            }),
            _ => return Err("Google sign-in needs both google_client_id and google_client_secret".into()),
        };
        let secret_key_base = get("secret_key_base", &["RAILWATCH_SECRET_KEY_BASE"]);
        let trust_proxy = get("trust_proxy", &["RAILWATCH_TRUST_PROXY"]).is_some_and(|v| truthy(&v));
        let rate_limit_ip = num(get("rate_limit_ip", &["RAILWATCH_RATE_LIMIT_IP"]), "rate_limit_ip", 2_400)?;
        let rate_limit_env = num(get("rate_limit_env", &["RAILWATCH_RATE_LIMIT_ENV"]), "rate_limit_env", 600)?;
        let tenant_idle_seconds = num(get("tenant_idle_seconds", &["RAILWATCH_TENANT_IDLE_SECONDS"]), "tenant_idle_seconds", 300)?;
        let enforce_quota = get("enforce_quota", &["RAILWATCH_ENFORCE_QUOTA"]).is_some_and(|v| truthy(&v));
        let d = crate::storage::retention::Tiers::default();
        let tiers = crate::storage::retention::Tiers {
            detail_days: num(get("detail_retention_days", &["RAILWATCH_DETAIL_RETENTION_DAYS"]), "detail_retention_days", d.detail_days as u64)? as i64,
            core_days_max: num(get("core_retention_days_max", &["RAILWATCH_CORE_RETENTION_DAYS_MAX"]), "core_retention_days_max", d.core_days_max as u64)?
                as i64,
            exemplars_per_hour: num(get("exemplars_per_hour", &["RAILWATCH_EXEMPLARS_PER_HOUR"]), "exemplars_per_hour", d.exemplars_per_hour as u64)? as usize,
        };
        if tiers.detail_days < 1 || tiers.core_days_max < 1 {
            return Err("detail_retention_days and core_retention_days_max must be at least 1".into());
        }
        let backup = match (
            get("backup_s3_endpoint", &["RAILWATCH_BACKUP_S3_ENDPOINT"]),
            get("backup_s3_bucket", &["RAILWATCH_BACKUP_S3_BUCKET"]),
            get("backup_s3_access_key_id", &["RAILWATCH_BACKUP_S3_ACCESS_KEY_ID"]),
            get("backup_s3_secret_access_key", &["RAILWATCH_BACKUP_S3_SECRET_ACCESS_KEY"]),
        ) {
            (None, None, None, None) => None,
            (Some(endpoint), Some(bucket), Some(access_key), Some(secret_key)) => {
                let mut prefix = get("backup_s3_prefix", &["RAILWATCH_BACKUP_S3_PREFIX"]).unwrap_or_default();
                if !prefix.is_empty() && !prefix.ends_with('/') {
                    prefix.push('/');
                }
                Some(crate::storage::backup::Settings {
                    s3: crate::storage::s3::S3 {
                        endpoint,
                        region: get("backup_s3_region", &["RAILWATCH_BACKUP_S3_REGION"]).unwrap_or_else(|| "us-east-1".into()),
                        bucket,
                        access_key,
                        secret_key,
                        prefix,
                    },
                    interval_secs: num(get("backup_interval_seconds", &["RAILWATCH_BACKUP_INTERVAL_SECONDS"]), "backup_interval_seconds", 300)?.max(30),
                })
            }
            _ => return Err("backups need all of backup_s3_endpoint, backup_s3_bucket, backup_s3_access_key_id, backup_s3_secret_access_key".into()),
        };
        let alert_delivery = match get("alert_delivery", &["RAILWATCH_ALERT_DELIVERY"]).as_deref().map(str::trim) {
            None | Some("on") => true,
            Some("off") => false,
            Some(other) => return Err(format!("alert_delivery: expected \"on\" or \"off\", got {other:?}")),
        };
        let tls_domain = get("tls_domain", &["RAILWATCH_TLS_DOMAIN"]).filter(|d| !d.trim().is_empty());
        let tls_cert = get("tls_cert_file", &["RAILWATCH_TLS_CERT_FILE"]);
        let tls_key = get("tls_key_file", &["RAILWATCH_TLS_KEY_FILE"]);
        let tls = match (tls_domain, tls_cert, tls_key) {
            (None, None, None) => None,
            (Some(domain), None, None) => {
                let domain = domain.trim().to_ascii_lowercase();
                if domain.contains(['/', ':', ' ']) || !domain.contains('.') {
                    return Err(format!("tls_domain: expected a host name like monitor.example.com, got {domain:?}"));
                }
                Some(crate::tls::Tls::Acme {
                    domain,
                    email: get("tls_email", &["RAILWATCH_TLS_EMAIL"]).filter(|e| !e.trim().is_empty()),
                    directory: get("tls_acme_directory", &["RAILWATCH_TLS_ACME_DIRECTORY"]).unwrap_or_else(|| crate::tls::LETS_ENCRYPT.into()),
                    ca_file: get("tls_acme_ca_file", &["RAILWATCH_TLS_ACME_CA_FILE"]).map(PathBuf::from),
                })
            }
            (None, Some(cert), Some(key)) => Some(crate::tls::Tls::Files { cert: cert.into(), key: key.into() }),
            _ => return Err("TLS: set either tls_domain (automatic certificates) or both tls_cert_file and tls_key_file".into()),
        };
        let tls_redirect_bind = match (get("tls_redirect_bind", &["RAILWATCH_TLS_REDIRECT_BIND"]), &tls) {
            (_, None) => None,
            (Some(v), _) if v.trim() == "off" => None,
            (Some(v), _) => Some(v),
            (None, Some(_)) => bind.ends_with(":443").then(|| format!("{}:80", bind.rsplit_once(':').map_or("0.0.0.0", |(h, _)| h))),
        };
        let dl = crate::grouping::Limits::default();
        let new_issues = crate::grouping::Limits {
            new_issues_per_hour: num(get("new_issues_per_hour", &["RAILWATCH_NEW_ISSUES_PER_HOUR"]), "new_issues_per_hour", dl.new_issues_per_hour as u64)?
                as usize,
            new_issues_per_batch: num(get("new_issues_per_batch", &["RAILWATCH_NEW_ISSUES_PER_BATCH"]), "new_issues_per_batch", dl.new_issues_per_batch as u64)?
                as usize,
            new_issues_per_account_per_hour: num(
                get("new_issues_per_account_per_hour", &["RAILWATCH_NEW_ISSUES_PER_ACCOUNT_PER_HOUR"]),
                "new_issues_per_account_per_hour",
                dl.new_issues_per_account_per_hour as u64,
            )? as usize,
        };
        let dh = crate::serve::Limits::default();
        let secs = |v: Option<String>, key: &str, d: std::time::Duration| -> Result<std::time::Duration, String> {
            let n = num(v, key, d.as_secs())?;
            if n == 0 {
                return Err(format!("{key} must be at least 1"));
            }
            Ok(std::time::Duration::from_secs(n))
        };
        let http = crate::serve::Limits {
            header_timeout: secs(
                get("http_header_timeout_seconds", &["RAILWATCH_HTTP_HEADER_TIMEOUT_SECONDS"]),
                "http_header_timeout_seconds",
                dh.header_timeout,
            )?,
            body_timeout: secs(get("http_body_timeout_seconds", &["RAILWATCH_HTTP_BODY_TIMEOUT_SECONDS"]), "http_body_timeout_seconds", dh.body_timeout)?,
            idle_timeout: secs(get("http_idle_timeout_seconds", &["RAILWATCH_HTTP_IDLE_TIMEOUT_SECONDS"]), "http_idle_timeout_seconds", dh.idle_timeout)?,
            max_connections: num(get("max_connections", &["RAILWATCH_MAX_CONNECTIONS"]), "max_connections", dh.max_connections as u64)?.max(1) as usize,
            // Behind a trusted proxy every connection's peer is the proxy.
            max_connections_per_ip: if trust_proxy {
                0
            } else {
                num(get("max_connections_per_ip", &["RAILWATCH_MAX_CONNECTIONS_PER_IP"]), "max_connections_per_ip", dh.max_connections_per_ip as u64)? as usize
            },
        };
        let base_path = normalize_base_path(&get("base_path", &["RAILWATCH_BASE_PATH"]).unwrap_or_default())?;
        let trusted_proxy_secret_file =
            get("trusted_proxy_secret_file", &["RAILWATCH_TRUSTED_PROXY_SECRET_FILE"]).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let trusted_proxy_secret = trusted_proxy_secret_file.as_deref().map(read_proxy_secret).transpose()?;
        // With automatic certificates the public URL is the domain's.
        let url = match (&tls, url_given) {
            (Some(crate::tls::Tls::Acme { domain, .. }), None) => format!("https://{domain}"),
            _ => url,
        };
        Ok(Config {
            data_dir,
            bind,
            url,
            signup,
            smtp,
            google,
            secret_key_base,
            trust_proxy,
            rate_limit_ip,
            rate_limit_env,
            tenant_idle_seconds,
            enforce_quota,
            tiers,
            backup,
            alert_delivery,
            tls,
            tls_redirect_bind,
            new_issues,
            http,
            base_path,
            trusted_proxy_secret,
            trusted_proxy_secret_file: trusted_proxy_secret_file.map(PathBuf::from),
            sources,
            file: l.file,
        })
    }

    pub fn control_path(&self) -> PathBuf {
        self.data_dir.join("control.sqlite3")
    }

    pub fn tenants_dir(&self) -> PathBuf {
        self.data_dir.join("telemetry")
    }

    /// The OAuth redirect URI to register with Google (Rails' OmniAuth
    /// callback path, so a migrated install keeps its registration).
    pub fn google_redirect_uri(&self) -> String {
        format!("{}/auth/google_oauth2/callback", self.pages_url())
    }

    /// The Path every cookie is set with: the base path, so cookies of the
    /// app it is mounted in (its own `XSRF-TOKEN`, `session_token`) are not
    /// overwritten.
    pub fn cookie_path(&self) -> &str {
        if self.base_path.is_empty() { "/" } else { &self.base_path }
    }

    /// A site path (`/dashboard`) as the browser sees it (`/railwatch/dashboard`).
    pub fn page_path(&self, path: &str) -> String {
        format!("{}{path}", self.base_path)
    }

    /// The public URL pages live under: `url` plus the base path. Links in
    /// emails and alerts, the MCP endpoint and Google's callback start here;
    /// the ingest URL is `url` alone.
    pub fn pages_url(&self) -> String {
        format!("{}{}", self.url, self.base_path)
    }

    pub fn secure_cookies(&self) -> bool {
        self.url.starts_with("https://")
    }

    /// Send Strict-Transport-Security: this server terminates TLS itself, or
    /// its url is https behind a proxy it trusts (which forwards only https,
    /// as the installer's setup does). Never for a plain-http url.
    pub fn hsts(&self) -> bool {
        self.tls.is_some() || (self.trust_proxy && self.url.starts_with("https://"))
    }

    /// The server is evidently reached through a proxy (an https url, no
    /// built-in TLS, bound to a loopback or private address) but does not
    /// trust X-Forwarded-For: every client then has the proxy's address, so
    /// the per-IP limits (sign-in, sign-up, ingest, MCP) are one global bucket.
    pub fn proxy_warning(&self) -> Option<String> {
        if self.trust_proxy || self.tls.is_some() || !self.url.starts_with("https://") {
            return None;
        }
        let host = self.bind.rsplit_once(':').map_or(self.bind.as_str(), |(h, _)| h).trim_matches(['[', ']']);
        let behind = match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback() || v4.is_private(),
            Ok(std::net::IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
            Err(_) => host == "localhost",
        };
        behind.then(|| {
            format!(
                "url is https and bind {} is local, so a proxy terminates TLS, but trust_proxy is off: every client looks like the proxy and per-IP rate limits become global. Set RAILWATCH_TRUST_PROXY=1 if the proxy sets X-Forwarded-For.",
                self.bind
            )
        })
    }

    /// The configured secret, or the one generated into the data dir on
    /// first use (0600). 64 random bytes, hex.
    pub fn secret(&self) -> Result<Vec<u8>, String> {
        if let Some(s) = &self.secret_key_base {
            if s.len() < 32 {
                return Err("secret_key_base must be at least 32 characters".into());
            }
            return Ok(s.as_bytes().to_vec());
        }
        let path = self.data_dir.join("secret_key_base");
        if let Ok(s) = std::fs::read_to_string(&path) {
            return Ok(s.trim().as_bytes().to_vec());
        }
        std::fs::create_dir_all(&self.data_dir).map_err(|e| e.to_string())?;
        let mut bytes = [0u8; 64];
        getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
        let hex = crate::ingest::receipt_headers::hex(&bytes);
        write_private(&path, hex.as_bytes())?;
        Ok(hex.into_bytes())
    }

    /// For `railwatch doctor`: every setting with its source; secrets masked.
    pub fn describe(&self) -> Vec<(String, String, String)> {
        let src = |k: &str| self.sources.get(k).cloned().unwrap_or_else(|| "default".into());
        let mut rows = vec![
            ("data_dir".into(), self.data_dir.display().to_string(), src("data_dir")),
            ("bind".into(), self.bind.clone(), src("bind")),
            ("url".into(), self.url.clone(), src("url")),
            ("signup".into(), format!("{:?}", self.signup), src("signup")),
            ("trust_proxy".into(), self.trust_proxy.to_string(), src("trust_proxy")),
            ("rate_limit_ip".into(), self.rate_limit_ip.to_string(), src("rate_limit_ip")),
            ("rate_limit_env".into(), self.rate_limit_env.to_string(), src("rate_limit_env")),
            ("tenant_idle_seconds".into(), self.tenant_idle_seconds.to_string(), src("tenant_idle_seconds")),
            ("enforce_quota".into(), self.enforce_quota.to_string(), src("enforce_quota")),
            (
                "http".into(),
                format!(
                    "header {} s, body {} s, idle {} s, max {} connections, {} per IP",
                    self.http.header_timeout.as_secs(),
                    self.http.body_timeout.as_secs(),
                    self.http.idle_timeout.as_secs(),
                    self.http.max_connections,
                    match self.http.max_connections_per_ip {
                        0 => "no limit".to_string(),
                        n => n.to_string(),
                    }
                ),
                src("max_connections"),
            ),
            ("new_issues_per_hour".into(), self.new_issues.new_issues_per_hour.to_string(), src("new_issues_per_hour")),
            ("new_issues_per_batch".into(), self.new_issues.new_issues_per_batch.to_string(), src("new_issues_per_batch")),
            ("new_issues_per_account_per_hour".into(), self.new_issues.new_issues_per_account_per_hour.to_string(), src("new_issues_per_account_per_hour")),
            ("detail_retention_days".into(), self.tiers.detail_days.to_string(), src("detail_retention_days")),
            ("core_retention_days_max".into(), self.tiers.core_days_max.to_string(), src("core_retention_days_max")),
            ("exemplars_per_hour".into(), self.tiers.exemplars_per_hour.to_string(), src("exemplars_per_hour")),
            (
                "backup".into(),
                match &self.backup {
                    None => "(off)".into(),
                    Some(b) => format!("{}/{}/{} every {} s", b.s3.endpoint, b.s3.bucket, b.s3.prefix, b.interval_secs),
                },
                src("backup_s3_endpoint"),
            ),
            (
                "tls".into(),
                match &self.tls {
                    None => "(off: plain http)".into(),
                    Some(crate::tls::Tls::Acme { domain, email, directory, .. }) => {
                        format!("acme {domain} ({}) from {directory}", email.as_deref().unwrap_or("no contact email"))
                    }
                    Some(crate::tls::Tls::Files { cert, key }) => format!("files {} + {}", cert.display(), key.display()),
                },
                src(if self.sources.contains_key("tls_domain") { "tls_domain" } else { "tls_cert_file" }),
            ),
            ("tls_redirect_bind".into(), self.tls_redirect_bind.clone().unwrap_or_else(|| "(off)".into()), src("tls_redirect_bind")),
            ("base_path".into(), if self.base_path.is_empty() { "(none: pages at the root)".into() } else { self.base_path.clone() }, src("base_path")),
            (
                "trusted_proxy".into(),
                match &self.trusted_proxy_secret_file {
                    None => "(off)".into(),
                    Some(f) => format!("on: X-Railwatch-Proxy-Auth signed with {}", f.display()),
                },
                src("trusted_proxy_secret_file"),
            ),
            ("alert_delivery".into(), if self.alert_delivery { "on" } else { "off (alerts recorded as skipped, never sent)" }.into(), src("alert_delivery")),
            ("secret_key_base".into(), if self.secret_key_base.is_some() { "(set)".into() } else { "(generated file)".into() }, src("secret_key_base")),
        ];
        match &self.google {
            None => rows.push(("google".into(), "(not configured: no \"Continue with Google\" button)".into(), "default".into())),
            Some(g) => {
                rows.push(("google_client_id".into(), g.client_id.clone(), src("google_client_id")));
                rows.push(("google_client_secret".into(), "(set)".into(), src("google_client_secret")));
                rows.push(("google_redirect_uri".into(), self.google_redirect_uri(), "url".into()));
                if let Some(i) = &g.issuer {
                    rows.push(("google_issuer".into(), format!("{i} (NOT Google: testing only)"), src("google_issuer")));
                }
            }
        }
        match &self.smtp {
            None => rows.push(("smtp".into(), "(not configured: emails are not sent)".into(), "default".into())),
            Some(s) => {
                rows.push(("smtp_address".into(), format!("{}:{} ({})", s.address, s.port, s.security), src("smtp_address")));
                rows.push(("smtp_username".into(), s.username.clone().unwrap_or_default(), src("smtp_username")));
                rows.push(("smtp_password".into(), if s.password.is_some() { "(set)".into() } else { String::new() }, src("smtp_password")));
                rows.push(("smtp_from".into(), s.from.clone(), src("smtp_from")));
            }
        }
        rows
    }
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f =
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(bind: &str, url: &str, trust_proxy: bool) -> Config {
        unsafe {
            std::env::remove_var("RAILWATCH_CONFIG");
        }
        let mut c = Config::load().unwrap();
        c.bind = bind.into();
        c.url = url.into();
        c.trust_proxy = trust_proxy;
        c.tls = None;
        c
    }

    #[test]
    fn base_path_is_one_clean_prefix_or_nothing() {
        assert_eq!(normalize_base_path("").unwrap(), "");
        assert_eq!(normalize_base_path("/").unwrap(), "");
        assert_eq!(normalize_base_path("railwatch/").unwrap(), "/railwatch");
        assert_eq!(normalize_base_path("/admin/railwatch").unwrap(), "/admin/railwatch");
        // Traversal, empty segments, encodings, anything that needs escaping
        // in HTML, a cookie Path or a Location header.
        for bad in ["/a//b", "//evil.example", "/../x", "/railwatch/..", "/./x", "/a b", "/a\"b", "/x?y", "/x#y", "/%2e", "/a%2fb", "/a\\b", "/a;b", "/é"] {
            assert!(normalize_base_path(bad).is_err(), "{bad:?}");
        }
        // The ingest API's own paths cannot be shadowed.
        for taken in ["/up", "/ingest", "/ingest/x", "/ingest.json"] {
            assert!(normalize_base_path(taken).is_err(), "{taken:?}");
        }
        assert_eq!(normalize_base_path("/upgrade").unwrap(), "/upgrade", "only the exact segments are taken");
        let mut c = cfg("127.0.0.1:8790", "http://127.0.0.1:8790", false);
        assert_eq!((c.cookie_path(), c.page_path("/dashboard").as_str(), c.pages_url().as_str()), ("/", "/dashboard", "http://127.0.0.1:8790"));
        c.base_path = "/railwatch".into();
        assert_eq!((c.cookie_path(), c.page_path("/dashboard").as_str()), ("/railwatch", "/railwatch/dashboard"));
        assert_eq!(c.pages_url(), "http://127.0.0.1:8790/railwatch");
        assert_eq!(c.google_redirect_uri(), "http://127.0.0.1:8790/railwatch/auth/google_oauth2/callback");
    }

    #[test]
    fn a_proxy_secret_must_be_readable_and_long_enough() {
        let dir = std::env::temp_dir().join(format!("rw-proxy-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, body: &[u8]| {
            let p = dir.join(name);
            std::fs::write(&p, body).unwrap();
            p.to_str().unwrap().to_string()
        };
        assert_eq!(read_proxy_secret(&file("ok", b"  0123456789abcdef0123456789abcdef\n")).unwrap(), b"0123456789abcdef0123456789abcdef");
        let short = read_proxy_secret(&file("short", b"0123456789abcdef0123456789abcde\n")).unwrap_err();
        assert!(short.contains("at least 32 bytes"), "{short}");
        assert!(read_proxy_secret(&file("blank", b"   \n")).is_err(), "whitespace is not a secret");
        assert!(read_proxy_secret(dir.join("missing").to_str().unwrap()).is_err(), "an unreadable file stops the server");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn warns_when_proxied_without_trust_proxy() {
        assert!(cfg("127.0.0.1:8790", "https://rw.example.com", false).proxy_warning().is_some());
        assert!(cfg("10.0.0.5:8790", "https://rw.example.com", false).proxy_warning().is_some());
        assert!(cfg("[::1]:8790", "https://rw.example.com", false).proxy_warning().is_some());
        assert!(cfg("127.0.0.1:8790", "https://rw.example.com", true).proxy_warning().is_none());
        assert!(cfg("127.0.0.1:8790", "http://127.0.0.1:8790", false).proxy_warning().is_none());
        assert!(cfg("0.0.0.0:443", "https://rw.example.com", false).proxy_warning().is_none());
    }

    #[test]
    fn env_files_are_data_not_shell() {
        let text = "# comment\n; also\n\nRAILWATCH_URL=https://rw.example.com\nRAILWATCH_SMTP_FROM = Railwatch <rw@example.com>  \nA=\"quoted value\"\nB='$(touch /tmp/x)'\nC=$HOME\nD=\n";
        let v = parse_env_file(text).unwrap();
        let get = |k: &str| v.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("RAILWATCH_URL"), Some("https://rw.example.com"));
        assert_eq!(get("RAILWATCH_SMTP_FROM"), Some("Railwatch <rw@example.com>"));
        assert_eq!(get("A"), Some("quoted value"));
        assert_eq!(get("B"), Some("$(touch /tmp/x)"), "nothing is executed");
        assert_eq!(get("C"), Some("$HOME"), "nothing is expanded");
        assert_eq!(get("D"), Some(""));
        assert!(parse_env_file("export X=1").is_err());
        assert!(parse_env_file("no equals sign").is_err());
        assert!(parse_env_file("1X=2").is_err());
    }

    #[test]
    fn hsts_only_when_https_is_certain() {
        assert!(cfg("127.0.0.1:8790", "https://rw.example.com", true).hsts());
        assert!(!cfg("127.0.0.1:8790", "https://rw.example.com", false).hsts());
        assert!(!cfg("127.0.0.1:8790", "http://127.0.0.1:8790", true).hsts());
        let mut tls = cfg("0.0.0.0:443", "https://rw.example.com", false);
        tls.tls = Some(crate::tls::Tls::Files { cert: "c".into(), key: "k".into() });
        assert!(tls.hsts());
    }
}
