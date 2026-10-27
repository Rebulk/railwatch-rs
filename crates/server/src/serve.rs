//! The HTTP server loop: what `axum::serve` does (accept, one hyper HTTP/1
//! connection per task with upgrades, graceful shutdown that stops accepting
//! and lets in-flight connections finish), plus the limits it does not let a
//! caller set. Without them a client that opens connections and never
//! finishes a request holds a task and a file descriptor forever.
//!
//! - `header_timeout`: a request head must arrive in full within this, timed
//!   from when the connection is ready for one. hyper's timer, so it is also
//!   how long an idle keep-alive connection is kept between requests.
//! - `body_timeout`: a request body must arrive in full within this, timed
//!   from the handler's first read of it (600 s: the 32 MiB ingest ceiling
//!   at ~450 kbit/s; the gem's batches are far smaller).
//! - `idle_timeout`: no byte read or written for this long while hyper waits
//!   on the peer (a stalled upload, a client that stops reading the
//!   response, a dead websocket; cable pings every 3 s) closes it. Time the
//!   server spends on a request it has read in full is not the peer's: from
//!   the end of the request body (or the head, without one) until the
//!   response starts to be written, it does not count.
//! - `max_connections`: past this many open connections the server stops
//!   accepting; new ones wait in the kernel's backlog, as during a restart.
//! - `max_connections_per_ip`: one client address (an IPv6 /64) may hold at
//!   most this many of them; a further connection from it is closed as soon
//!   as it is accepted, so one source cannot take every slot and starve the
//!   others. The address is the TCP peer: X-Forwarded-For cannot be read
//!   before a request is, so with `trust_proxy` (every connection comes from
//!   the proxy) this cap is off and the proxy's own limits apply.
//!
//! Shutdown keeps the zero-drop restart: the listener stops accepting the
//! moment the signal fires and is dropped, so with socket activation every
//! connection not yet accepted stays queued for the next process.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ConnectInfo;
use http_body::Frame;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Semaphore, watch};
use tokio::time::{Instant, Sleep};
use tower::ServiceExt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub header_timeout: Duration,
    pub body_timeout: Duration,
    pub idle_timeout: Duration,
    pub max_connections: usize,
    /// 0: no per-address cap (`trust_proxy`: every peer is the proxy).
    pub max_connections_per_ip: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            header_timeout: Duration::from_secs(30),
            body_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(300),
            max_connections: 4096,
            max_connections_per_ip: 256,
        }
    }
}

/// Open connections per client address, for `max_connections_per_ip`.
#[derive(Clone, Default)]
struct PerIp(Arc<std::sync::Mutex<HashMap<IpAddr, usize>>>);

/// One open connection's place in `PerIp`; dropping it gives the place back.
struct IpSlot {
    counts: PerIp,
    key: IpAddr,
}

impl PerIp {
    /// The key one client is counted under: an IPv4 address (IPv4-mapped
    /// IPv6 included), or an IPv6 /64, which is what one host is given.
    fn key(ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V4(v4) => IpAddr::V4(v4),
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => IpAddr::V6(std::net::Ipv6Addr::from(u128::from(v6) & !((1u128 << 64) - 1))),
            },
        }
    }

    /// A slot for a connection from `ip`, or None when it has `max` open.
    fn take(&self, ip: IpAddr, max: usize) -> Option<IpSlot> {
        let key = Self::key(ip);
        let mut counts = self.0.lock().unwrap();
        let n = counts.entry(key).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
        Some(IpSlot { counts: self.clone(), key })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut counts = self.counts.0.lock().unwrap();
        if let Some(n) = counts.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                counts.remove(&self.key);
            }
        }
    }
}

/// Names the source on every response: an RFC 8288
/// `Link` to the repository.
pub fn source_link(router: axum::Router) -> axum::Router {
    let value = axum::http::HeaderValue::from_str(&format!("<{}>; rel=\"license source\"", crate::SOURCE_URL)).expect("ASCII URL");
    router.layer(axum::middleware::map_response(move |mut res: axum::response::Response| {
        let value = value.clone();
        async move {
            res.headers_mut().append(axum::http::header::LINK, value);
            res
        }
    }))
}

/// HSTS for a year (Rails' `force_ssl` default, without includeSubDomains:
/// the server does not know what else lives under its domain).
pub const HSTS: &str = "max-age=31536000";

/// Adds Strict-Transport-Security to every response when `on`.
pub fn hsts(router: axum::Router, on: bool) -> axum::Router {
    if !on {
        return router;
    }
    router.layer(axum::middleware::map_response(|mut res: axum::response::Response| async move {
        res.headers_mut().insert(axum::http::header::STRICT_TRANSPORT_SECURITY, axum::http::HeaderValue::from_static(HSTS));
        res
    }))
}

/// Serves `router` on `listener` until `shutdown` resolves, then waits for
/// the connections it accepted to finish. Handlers get `ConnectInfo<SocketAddr>`.
pub async fn run<L>(mut listener: L, router: axum::Router, limits: Limits, shutdown: impl Future<Output = ()> + Send + 'static)
where
    L: axum::serve::Listener<Addr = SocketAddr>,
{
    let permits = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let per_ip = PerIp::default();
    let mut per_ip_logged: Option<Instant> = None;
    // Dropped receivers tell connections to shut down; `close` counts them.
    let (stop_tx, stop_rx) = watch::channel(());
    let (close_tx, close_rx) = watch::channel(());
    let mut shutdown = std::pin::pin!(shutdown);
    let mut full_logged = false;
    loop {
        let permit = match permits.clone().try_acquire_owned() {
            Ok(p) => {
                full_logged = false;
                p
            }
            Err(_) => {
                if !full_logged {
                    tracing::warn!(max_connections = limits.max_connections, "connection limit reached: new connections wait in the backlog");
                    full_logged = true;
                }
                tokio::select! {
                    p = permits.clone().acquire_owned() => p.expect("semaphore is never closed"),
                    _ = &mut shutdown => break,
                }
            }
        };
        let (io, addr) = tokio::select! {
            c = listener.accept() => c,
            _ = &mut shutdown => break,
        };
        let ip_slot = if limits.max_connections_per_ip == 0 {
            None
        } else {
            match per_ip.take(addr.ip(), limits.max_connections_per_ip) {
                Some(slot) => Some(slot),
                None => {
                    // Dropping the stream closes it; at most one warning a minute.
                    if per_ip_logged.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
                        tracing::warn!(ip = %addr.ip(), max_connections_per_ip = limits.max_connections_per_ip, "per-address connection limit reached: closing new connections from it");
                        per_ip_logged = Some(Instant::now());
                    }
                    continue;
                }
            }
        };
        let router = router.clone();
        let mut stop = stop_rx.clone();
        let close = close_rx.clone();
        tokio::spawn(async move {
            let working = Arc::new(AtomicBool::new(false));
            let io = TokioIo::new(Idle::new(io, limits.idle_timeout, working.clone()));
            let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                if http_body::Body::is_end_stream(req.body()) {
                    working.store(true, Ordering::Relaxed);
                }
                let working = working.clone();
                let mut req = req.map(|b| Body::new(DeadlineBody::new(b, limits.body_timeout, working)));
                req.extensions_mut().insert(ConnectInfo(addr));
                router.clone().oneshot(req)
            });
            let mut builder = hyper::server::conn::http1::Builder::new();
            builder.timer(TokioTimer::new()).header_read_timeout(limits.header_timeout);
            let mut conn = std::pin::pin!(builder.serve_connection(io, svc).with_upgrades());
            let mut stopping = false;
            loop {
                tokio::select! {
                    r = conn.as_mut() => {
                        if let Err(e) = r {
                            tracing::trace!(error = %e, "connection ended");
                        }
                        break;
                    }
                    _ = stop.changed(), if !stopping => {
                        stopping = true;
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }
            drop(ip_slot);
            drop(permit);
            drop(close);
        });
    }
    drop(listener);
    drop(stop_tx);
    drop(close_rx);
    close_tx.closed().await;
}

/// A request body that fails once `timeout` has passed since its first read.
/// Sets `working` when it has been read to the end (see `Idle`).
struct DeadlineBody {
    inner: Incoming,
    timeout: Duration,
    deadline: Option<Pin<Box<Sleep>>>,
    working: Arc<AtomicBool>,
}

impl DeadlineBody {
    fn new(inner: Incoming, timeout: Duration, working: Arc<AtomicBool>) -> Self {
        DeadlineBody { inner, timeout, deadline: None, working }
    }
}

#[derive(Debug)]
struct BodyTimeout;

impl std::fmt::Display for BodyTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("request body timed out")
    }
}

impl std::error::Error for BodyTimeout {}

impl http_body::Body for DeadlineBody {
    type Data = Bytes;
    type Error = axum::BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = &mut *self;
        if let Poll::Ready(r) = Pin::new(&mut this.inner).poll_frame(cx) {
            // A chunked body only says so with the final None.
            if r.is_none() || (matches!(r, Some(Ok(_))) && this.inner.is_end_stream()) {
                this.working.store(true, Ordering::Relaxed);
            }
            return Poll::Ready(r.map(|f| f.map_err(Into::into)));
        }
        let timeout = this.timeout;
        let deadline = this.deadline.get_or_insert_with(|| Box::pin(tokio::time::sleep(timeout)));
        match deadline.as_mut().poll(cx) {
            Poll::Ready(()) => {
                tracing::debug!(after = ?timeout, "request body timed out");
                Poll::Ready(Some(Err(BodyTimeout.into())))
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// An IO that fails with TimedOut when a read or write it was asked for has
/// waited `timeout` since the last byte moved either way.
///
/// Except while `working`: set once a request has been read in full, cleared
/// when hyper starts to write the response. In between hyper keeps a read
/// pending only to notice a hang-up; the peer is waiting on the server, so a
/// handler slower than `timeout` must not get the connection closed under it.
struct Idle<T> {
    inner: T,
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
    working: Arc<AtomicBool>,
}

impl<T> Idle<T> {
    fn new(inner: T, timeout: Duration, working: Arc<AtomicBool>) -> Self {
        Idle { inner, timeout, sleep: Box::pin(tokio::time::sleep(timeout)), working }
    }

    fn moved(&mut self) {
        self.sleep.as_mut().reset(Instant::now() + self.timeout);
    }

    /// hyper is writing: the server's turn is over, and the peer's idle time
    /// starts now, not when the request was read.
    fn writing(&mut self) {
        if self.working.swap(false, Ordering::Relaxed) {
            self.moved();
        }
    }

    /// For a pending read/write: TimedOut if the idle deadline has passed.
    fn expired<R>(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<R>> {
        if self.working.load(Ordering::Relaxed) {
            return Poll::Pending;
        }
        match self.sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(std::io::ErrorKind::TimedOut.into())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Idle<T> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(r) => {
                if buf.filled().len() > before {
                    this.moved();
                }
                Poll::Ready(r)
            }
            Poll::Pending => this.expired(cx),
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Idle<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.writing();
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(r) => {
                if matches!(r, Ok(n) if n > 0) {
                    this.moved();
                }
                Poll::Ready(r)
            }
            Poll::Pending => this.expired(cx),
        }
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[std::io::IoSlice<'_>]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.writing();
        match Pin::new(&mut this.inner).poll_write_vectored(cx, bufs) {
            Poll::Ready(r) => {
                if matches!(r, Ok(n) if n > 0) {
                    this.moved();
                }
                Poll::Ready(r)
            }
            Poll::Pending => this.expired(cx),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Not `writing`: hyper flushes on every turn, response or not.
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Pending => this.expired(cx),
            r => r,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves `/` on a loopback port under `limits`; the shutdown sender stops it.
    async fn server(limits: Limits) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(run(listener, router, limits, async move {
            let _ = rx.await;
        }));
        (addr, tx)
    }

    async fn connect_from(from: [u8; 4], to: SocketAddr) -> tokio::net::TcpStream {
        let s = tokio::net::TcpSocket::new_v4().unwrap();
        s.bind(SocketAddr::from((from, 0))).unwrap();
        s.connect(to).await.unwrap()
    }

    /// A GET / from `from`: true when "200 OK" comes back within 2 s.
    async fn served(from: [u8; 4], to: SocketAddr) -> bool {
        let attempt = async {
            let mut s = connect_from(from, to).await;
            s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").await.ok()?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.ok()?;
            Some(buf.starts_with(b"HTTP/1.1 200"))
        };
        tokio::time::timeout(Duration::from_secs(2), attempt).await.ok().flatten().unwrap_or(false)
    }

    /// Issue #3: one source opening connections and sending nothing held
    /// every slot of the global cap until the header timeout, and another
    /// source waited in the backlog. Linux routes all of 127/8 to loopback,
    /// so 127.0.0.1 and 127.0.0.2 are two sources.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_source_cannot_take_every_connection() {
        let limits = Limits { max_connections: 8, max_connections_per_ip: 4, ..Limits::default() };
        for (per_ip, victim_served) in [(0, false), (4, true)] {
            let (addr, stop) = server(Limits { max_connections_per_ip: per_ip, ..limits }).await;
            let mut held = Vec::new();
            for _ in 0..20 {
                held.push(connect_from([127, 0, 0, 1], addr).await);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(served([127, 0, 0, 2], addr).await, victim_served, "max_connections_per_ip {per_ip}");
            if per_ip > 0 {
                // The flooding source keeps its 4; its others were closed.
                let mut closed = 0;
                for s in &mut held {
                    let mut b = [0u8; 1];
                    if let Ok(Ok(0) | Err(_)) = tokio::time::timeout(Duration::from_millis(50), s.read(&mut b)).await {
                        closed += 1;
                    }
                }
                assert_eq!(closed, 16, "connections past the per-address cap are closed at once");
                drop(held);
                tokio::time::sleep(Duration::from_millis(100)).await;
                assert!(served([127, 0, 0, 1], addr).await, "closing them gives the source its slots back");
            }
            let _ = stop.send(());
        }
    }

    /// Serves `router` on a loopback port under `limits`.
    async fn serve_router(limits: Limits, router: axum::Router) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(run(listener, router, limits, async move {
            let _ = rx.await;
        }));
        (addr, tx)
    }

    /// Sends `req` and reads until the server closes: what came back.
    async fn exchange(addr: SocketAddr, req: &[u8]) -> String {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// A handler that takes longer than `idle_timeout` (a big ingest batch on
    /// a slow runner, a heavy dashboard query) still gets its response out:
    /// while the server works, hyper's pending read only watches for a hang-up,
    /// and the client, waiting for the response, is not idle. The connection
    /// used to be closed with no response at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_handler_slower_than_the_idle_timeout_still_responds() {
        let limits = Limits { idle_timeout: Duration::from_millis(300), ..Limits::default() };
        let slow = Duration::from_millis(1200);
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move {
                tokio::time::sleep(slow).await;
                "slow get"
            })
            .post(move |body: Bytes| async move {
                tokio::time::sleep(slow).await;
                format!("slow post {}", body.len())
            }),
        );
        let (addr, stop) = serve_router(limits, router).await;
        let get = exchange(addr, b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").await;
        assert!(get.starts_with("HTTP/1.1 200") && get.ends_with("slow get"), "GET: {get:?}");
        let post = exchange(addr, b"POST / HTTP/1.1\r\nhost: x\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello").await;
        assert!(post.starts_with("HTTP/1.1 200") && post.ends_with("slow post 5"), "POST: {post:?}");
        let _ = stop.send(());
    }

    /// The idle timeout still cuts off a client that stops reading the
    /// response once the server has started sending it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_client_that_stops_reading_the_response_is_cut_off() {
        const SIZE: usize = 64 << 20; // far more than both socket buffers hold
        let limits = Limits { idle_timeout: Duration::from_millis(300), ..Limits::default() };
        let router = axum::Router::new().route("/", axum::routing::get(|| async { vec![b'x'; SIZE] }));
        let (addr, stop) = serve_router(limits, router).await;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n").await.unwrap();
        let mut head = [0u8; 12];
        s.read_exact(&mut head).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut rest)).await;
        assert!(rest.len() < SIZE, "the server gave up on a client that stopped reading ({} bytes after the pause)", rest.len());
        let _ = stop.send(());
    }

    #[test]
    fn per_ip_key_is_the_ipv4_address_or_the_ipv6_64() {
        let key = |s: &str| PerIp::key(s.parse().unwrap()).to_string();
        assert_eq!(key("203.0.113.7"), "203.0.113.7");
        assert_eq!(key("::ffff:203.0.113.7"), "203.0.113.7");
        assert_eq!(key("2001:db8:1:2:aaaa:bbbb:cccc:dddd"), "2001:db8:1:2::");
        assert_eq!(key("2001:db8:1:2::1"), key("2001:db8:1:2:ffff::9"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
    }
}
