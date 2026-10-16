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
//!   on the socket (a stalled upload, a client that stops reading the
//!   response, a dead websocket; cable pings every 3 s) closes it.
//! - `max_connections`: past this many open connections the server stops
//!   accepting; new ones wait in the kernel's backlog, as during a restart.
//!
//! Shutdown keeps the zero-drop restart: the listener stops accepting the
//! moment the signal fires and is dropped, so with socket activation every
//! connection not yet accepted stays queued for the next process.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
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
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            header_timeout: Duration::from_secs(30),
            body_timeout: Duration::from_secs(600),
            idle_timeout: Duration::from_secs(300),
            max_connections: 4096,
        }
    }
}

/// Names the source on every response (AGPL-3.0 section 13): an RFC 8288
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
        let router = router.clone();
        let mut stop = stop_rx.clone();
        let close = close_rx.clone();
        tokio::spawn(async move {
            let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                let mut req = req.map(|b| Body::new(DeadlineBody::new(b, limits.body_timeout)));
                req.extensions_mut().insert(ConnectInfo(addr));
                router.clone().oneshot(req)
            });
            let io = TokioIo::new(Idle::new(io, limits.idle_timeout));
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
struct DeadlineBody {
    inner: Incoming,
    timeout: Duration,
    deadline: Option<Pin<Box<Sleep>>>,
}

impl DeadlineBody {
    fn new(inner: Incoming, timeout: Duration) -> Self {
        DeadlineBody { inner, timeout, deadline: None }
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
struct Idle<T> {
    inner: T,
    timeout: Duration,
    sleep: Pin<Box<Sleep>>,
}

impl<T> Idle<T> {
    fn new(inner: T, timeout: Duration) -> Self {
        Idle { inner, timeout, sleep: Box::pin(tokio::time::sleep(timeout)) }
    }

    fn moved(&mut self) {
        self.sleep.as_mut().reset(Instant::now() + self.timeout);
    }

    /// For a pending read/write: TimedOut if the idle deadline has passed.
    fn expired<R>(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<R>> {
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
