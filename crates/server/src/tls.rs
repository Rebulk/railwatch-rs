//! Built-in HTTPS (config `tls_domain`): the server terminates TLS itself
//! and gets its certificate from Let's Encrypt (or any ACME CA,
//! `tls_acme_directory`), so a self-hosted install needs no proxy package.
//!
//! - Challenge: TLS-ALPN-01 on the HTTPS port itself (rustls-acme), so a
//!   certificate needs nothing but port 443. Port 80, when given a socket
//!   (`tls_redirect_bind` or a second LISTEN_FDS socket), only redirects
//!   to https.
//! - Storage: the ACME account key and the certificate+key PEM live in
//!   `<data_dir>/acme/` (rustls-acme's DirCache: `cached_account_*`,
//!   `cached_cert_*`, mode 0700 dir). A restart deploys the cached
//!   certificate before accepting, so no handshake waits on the CA.
//! - Renewal: rustls-acme re-orders when a third of the validity is left
//!   (30 days of Let's Encrypt's 90) and swaps the certificate in place;
//!   existing connections keep theirs.
//! - Restarts: the listening socket is the same one LISTEN_FDS hands over
//!   in plain mode, so socket activation's zero-drop restart holds: the
//!   kernel queues TCP connections; the TLS handshake happens after accept.
//!
//! `tls_cert_file` + `tls_key_file` serve a certificate you manage instead
//! (re-read on SIGHUP is not implemented: restart to pick up a new one).

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::StreamExt;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, is_tls_alpn_challenge};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::LazyConfigAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio_rustls::server::TlsStream;

/// Let's Encrypt's production directory, the default CA.
pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";

/// How HTTPS is served (Config `tls`).
#[derive(Debug, Clone)]
pub enum Tls {
    /// ACME: certificate for `domain`, contact `email`, from `directory`.
    /// `ca_file` trusts a private CA for the directory itself (Pebble, step-ca).
    Acme { domain: String, email: Option<String>, directory: String, ca_file: Option<PathBuf> },
    /// A certificate managed outside (certbot, a corporate CA).
    Files { cert: PathBuf, key: PathBuf },
}

/// How long a client may take to finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The server's TLS state: the config real connections get, and (ACME) the
/// one TLS-ALPN-01 validation connections get.
#[derive(Clone)]
pub struct Acceptor {
    default: Arc<ServerConfig>,
    challenge: Option<Arc<ServerConfig>>,
}

fn provider() -> Arc<tokio_rustls::rustls::crypto::CryptoProvider> {
    Arc::new(tokio_rustls::rustls::crypto::ring::default_provider())
}

/// The ACME account and certificate directory under the data dir.
pub fn acme_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("acme")
}

impl Acceptor {
    /// Starts certificate management (ACME: loads the cached certificate,
    /// orders or renews in the background, logging every event) and returns
    /// the acceptor. Fails only on configuration errors (unreadable files).
    pub async fn start(tls: &Tls, data_dir: &Path) -> Result<Acceptor, String> {
        let mut alpn = vec![b"http/1.1".to_vec()];
        match tls {
            Tls::Files { cert, key } => {
                let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
                    .and_then(|i| i.collect::<Result<_, _>>())
                    .map_err(|e| format!("tls_cert_file {}: {e}", cert.display()))?;
                if certs.is_empty() {
                    return Err(format!("tls_cert_file {}: no certificate", cert.display()));
                }
                let key = PrivateKeyDer::from_pem_file(key).map_err(|e| format!("tls_key_file {}: {e}", key.display()))?;
                let mut c = ServerConfig::builder_with_provider(provider())
                    .with_safe_default_protocol_versions()
                    .map_err(|e| e.to_string())?
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .map_err(|e| format!("tls_cert_file/tls_key_file: {e}"))?;
                c.alpn_protocols = std::mem::take(&mut alpn);
                Ok(Acceptor { default: Arc::new(c), challenge: None })
            }
            Tls::Acme { domain, email, directory, ca_file } => {
                let dir = acme_dir(data_dir);
                std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                let mut config = AcmeConfig::new([domain.as_str()]).directory(directory).cache(DirCache::new(dir));
                if let Some(e) = email {
                    config = config.contact_push(format!("mailto:{e}"));
                }
                if let Some(ca) = ca_file {
                    config = config.client_tls_config(Arc::new(acme_client_config(ca)?));
                }
                let mut state = config.state();
                let mut c = (*state.default_rustls_config_with_provider(provider())).clone();
                c.alpn_protocols = alpn;
                let challenge = state.challenge_rustls_config_with_provider(provider());
                let domain = domain.clone();
                tokio::spawn(async move {
                    while let Some(event) = state.next().await {
                        match event {
                            Ok(ok) => tracing::info!(domain, event = ?ok, "acme"),
                            Err(err) => tracing::error!(domain, error = %err, "acme"),
                        }
                    }
                });
                Ok(Acceptor { default: Arc::new(c), challenge: Some(challenge) })
            }
        }
    }

    /// Completes the handshake. None: an ACME validation connection (served
    /// and closed here), a timeout, or a failed handshake (logged at debug:
    /// scanners make plenty of those).
    async fn handshake(&self, tcp: TcpStream) -> Option<TlsStream<TcpStream>> {
        let work = async {
            let start = LazyConfigAcceptor::new(Default::default(), tcp).await.ok()?;
            if let Some(challenge) = &self.challenge
                && is_tls_alpn_challenge(&start.client_hello())
            {
                tracing::info!("acme TLS-ALPN-01 validation connection");
                let mut tls = start.into_stream(challenge.clone()).await.ok()?;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut tls).await;
                return None;
            }
            start.into_stream(self.default.clone()).await.map_err(|e| tracing::debug!(error = %e, "tls handshake")).ok()
        };
        tokio::time::timeout(HANDSHAKE_TIMEOUT, work).await.ok().flatten()
    }
}

/// The ACME client's TLS config trusting webpki roots plus `ca_file`.
fn acme_client_config(ca_file: &Path) -> Result<tokio_rustls::rustls::ClientConfig, String> {
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in CertificateDer::pem_file_iter(ca_file).map_err(|e| format!("tls_acme_ca_file {}: {e}", ca_file.display()))? {
        roots.add(cert.map_err(|e| format!("tls_acme_ca_file: {e}"))?).map_err(|e| format!("tls_acme_ca_file: {e}"))?;
    }
    Ok(tokio_rustls::rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// A connection the server reads requests from: plain TCP, or TLS over it.
/// The TLS handshake runs lazily, on the connection's own task the first
/// time hyper reads or writes, so the accept loop never waits on a client,
/// and on shutdown axum stops accepting exactly as in plain mode: every
/// connection it did not accept stays queued in the kernel for the next
/// process (the zero-drop restart), every one it did is served.
pub enum Stream {
    Plain(TcpStream),
    Handshake(Pin<Box<dyn Future<Output = Option<TlsStream<TcpStream>>> + Send>>),
    Tls(Box<TlsStream<TcpStream>>),
    Closed,
}

impl Stream {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if let Stream::Handshake(f) = self {
            match f.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(tls)) => *self = Stream::Tls(Box::new(tls)),
                Poll::Ready(None) => *self = Stream::Closed,
            }
        }
        match self {
            Stream::Closed => Poll::Ready(Err(std::io::ErrorKind::ConnectionAborted.into())),
            _ => Poll::Ready(Ok(())),
        }
    }
}

macro_rules! dispatch {
    ($self:ident, $cx:ident, $method:ident($($arg:expr),*)) => {{
        let this = $self.get_mut();
        if let Err(err) = std::task::ready!(this.poll_ready($cx)) {
            return Poll::Ready(Err(err));
        }
        match this {
            Stream::Plain(s) => Pin::new(s).$method($($arg),*),
            Stream::Tls(s) => Pin::new(s.as_mut()).$method($($arg),*),
            Stream::Handshake(_) | Stream::Closed => unreachable!("poll_ready resolved it"),
        }
    }};
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        dispatch!(self, cx, poll_read(cx, buf))
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        dispatch!(self, cx, poll_write(cx, buf))
    }
    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[std::io::IoSlice<'_>]) -> Poll<std::io::Result<usize>> {
        dispatch!(self, cx, poll_write_vectored(cx, bufs))
    }
    fn is_write_vectored(&self) -> bool {
        match self {
            Stream::Plain(s) => s.is_write_vectored(),
            Stream::Tls(s) => s.is_write_vectored(),
            _ => false,
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        dispatch!(self, cx, poll_flush(cx))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
            Stream::Handshake(_) | Stream::Closed => Poll::Ready(Ok(())),
        }
    }
}

/// The server's listener for `axum::serve`: TCP, with TLS when configured.
pub struct Listener {
    tcp: TcpListener,
    tls: Option<Acceptor>,
}

impl Listener {
    pub fn new(tcp: TcpListener, tls: Option<Acceptor>) -> Listener {
        Listener { tcp, tls }
    }
}

/// axum's own policy (serve/listener.rs): connection errors are the peer's,
/// anything else (EMFILE) is logged and backed off for a second.
async fn accept_error(e: std::io::Error) {
    use std::io::ErrorKind::*;
    if !matches!(e.kind(), ConnectionRefused | ConnectionAborted | ConnectionReset) {
        tracing::error!(error = %e, "accept error");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

impl axum::serve::Listener for Listener {
    type Io = Stream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Stream, SocketAddr) {
        loop {
            match self.tcp.accept().await {
                Ok((s, a)) => {
                    let _ = s.set_nodelay(true);
                    return match &self.tls {
                        None => (Stream::Plain(s), a),
                        Some(t) => {
                            let t = t.clone();
                            (Stream::Handshake(Box::pin(async move { t.handshake(s).await })), a)
                        }
                    };
                }
                Err(e) => accept_error(e).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

/// Port 80 with built-in TLS: every request is answered with a permanent
/// redirect to the same path on `https://<domain>`.
pub fn redirect_router(domain: String) -> axum::Router {
    axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let to = format!("https://{domain}{}", uri.path_and_query().map_or("/", |p| p.as_str()));
        async move { (axum::http::StatusCode::PERMANENT_REDIRECT, [(axum::http::header::LOCATION, to)]) }
    })
}

/// For `railwatch doctor`: (subject names, not_before, not_after unix seconds) of the
/// certificate the server would serve, read from the ACME cache or the file.
pub fn current_certificate(tls: &Tls, data_dir: &Path) -> Result<Option<(Vec<String>, i64, i64)>, String> {
    let pem = match tls {
        Tls::Files { cert, .. } => std::fs::read(cert).map_err(|e| format!("{}: {e}", cert.display()))?,
        Tls::Acme { .. } => {
            let dir = acme_dir(data_dir);
            let newest = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("cached_cert_"))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
            match newest {
                None => return Ok(None),
                Some(e) => std::fs::read(e.path()).map_err(|err| format!("{}: {err}", e.path().display()))?,
            }
        }
    };
    let leaf = CertificateDer::pem_slice_iter(&pem).next().ok_or("no certificate in the PEM")?.map_err(|e| e.to_string())?;
    let (_, cert) = x509_parser::parse_x509_certificate(&leaf).map_err(|e| e.to_string())?;
    let names = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|n| match n {
                    x509_parser::extensions::GeneralName::DNSName(d) => Some(d.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Some((names, cert.validity().not_before.timestamp(), cert.validity().not_after.timestamp())))
}
