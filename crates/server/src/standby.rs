//! `standby_relay`: a second `railwatch serve` on a data dir that another
//! server holds (serve.lock) serves its own address at once instead of
//! refusing connections until the first one exits.
//!
//! The case is two containers of one app sharing the data volume during a
//! blue/green deploy (the railwatch-server gem runs the server next to each
//! app process). Each container has its own loopback, so the new
//! container's agent cannot reach the old container's server, and its own
//! server must not write the data while the old one does. Until it holds
//! the lock, the standby splices every connection it accepts, byte for
//! byte, to `<data_dir>/serve.sock`, where the holder also listens; the
//! holder answers it like any other (ingest, pages, the live channel). A
//! Unix socket on the shared volume reaches across network namespaces:
//! the containers share the host's kernel and the volume's inode.
//!
//! The hand-over: the holder stops accepting, drains and exits, which
//! releases the lock; the standby takes it, starts its writers and
//! listens on serve.sock itself. A connection accepted in that gap waits:
//! its connect to serve.sock is retried every RETRY for up to PATIENCE, so
//! it reaches whichever server is listening by then.
//!
//! Without a holder in another namespace (two servers on one host and
//! loopback), the standby cannot bind the holder's address, and it waits
//! on the lock as before: the agents already reach the holder there.
//!
//! What the relay can and cannot do:
//! - It opens no new network listener: the standby binds the `bind` address
//!   it would serve anyway, and serve.sock is a Unix socket in the data dir.
//! - serve.sock is mode 0600, and both ends check the other's uid
//!   (SO_PEERCRED) against their own: only this user's processes connect,
//!   and a standby relays only to a server run by this user.
//! - It forwards bytes and nothing else: every request is authenticated by
//!   the holder exactly as if it had arrived on the holder's own port
//!   (cookies, tokens, X-Railwatch-Proxy-Auth, CSRF). It can reach nothing
//!   but the holder's serve.sock.
//! - At most `max` connections are relayed at once (the HTTP connection
//!   cap); more wait in the kernel's backlog. Each uses copy_bidirectional's
//!   fixed buffers; the holder's HTTP timeouts end idle ones.
//! - Ingest stays exactly-once across the hand-over: a batch is answered by
//!   whichever server holds the data, and one whose answer is lost to the
//!   holder's exit is retried by the agent under the same receipt
//!   (X-Railwatch-Batch-Id), which the next holder acknowledges as a
//!   duplicate rather than storing twice.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

pub const SOCKET: &str = "serve.sock";
/// sun_path holds 108 bytes on Linux and 104 on macOS, NUL included.
const MAX_SOCKET_PATH: usize = 103;
const RETRY: Duration = Duration::from_millis(50);
/// How long a connection accepted in the hand-over gap waits for a server
/// on serve.sock. The holder's drain is bounded by its HTTP limits; a
/// standby that has the lock is listening within a second.
const PATIENCE: Duration = Duration::from_secs(30);

/// `<data_dir>/serve.sock`, or None when the path is too long to bind.
pub fn socket_path(data_dir: &Path) -> Option<PathBuf> {
    let path = data_dir.join(SOCKET);
    (path.as_os_str().len() <= MAX_SOCKET_PATH).then_some(path)
}

/// For the lock holder: listen on serve.sock, replacing the socket a
/// server that died left behind (the lock says none is using it). Anything
/// there that is not a socket is left alone and refused. Owner-only, like
/// the data it serves.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => std::fs::remove_file(path)?,
        Ok(_) => return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} exists and is not a socket", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The other end of a serve.sock connection is a process of this user.
fn same_user(s: &UnixStream) -> bool {
    // SAFETY: geteuid cannot fail.
    let me = unsafe { libc::geteuid() };
    s.peer_cred().is_ok_and(|c| c.uid() == me)
}

/// serve.sock as a listener for serve::run. Its peers are this host's own
/// servers, so each connection is given the loopback address.
pub struct Socket(pub UnixListener);

impl axum::serve::Listener for Socket {
    type Io = UnixStream;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (UnixStream, std::net::SocketAddr) {
        loop {
            match self.0.accept().await {
                Ok((s, _)) if same_user(&s) => return (s, std::net::SocketAddr::from(([127, 0, 0, 1], 0))),
                Ok(_) => tracing::warn!("serve.sock: refused a connection from another user"),
                Err(e) => {
                    tracing::warn!(error = %e, "serve.sock: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        Ok(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
    }
}

/// Relays every connection accepted on `listener` to `socket` until
/// `active` resolves (this server holds the lock), then hands the listener
/// back to be served directly. Connections already relayed finish on the
/// server that took them. At most `max` are relayed at once.
pub async fn relay<T>(listener: TcpListener, socket: PathBuf, max: usize, active: impl Future<Output = T>) -> (TcpListener, T) {
    let mut active = std::pin::pin!(active);
    let permits = Arc::new(tokio::sync::Semaphore::new(max.max(1)));
    loop {
        // A free slot first, so a full relay leaves connections queued in
        // the kernel's backlog for the server that takes over.
        let permit = tokio::select! {
            biased;
            held = &mut active => return (listener, held),
            p = permits.clone().acquire_owned() => p.expect("semaphore is never closed"),
        };
        tokio::select! {
            biased;
            held = &mut active => return (listener, held),
            accepted = listener.accept() => match accepted {
                Ok((client, _)) => {
                    let socket = socket.clone();
                    tokio::spawn(async move {
                        splice(client, socket).await;
                        drop(permit);
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "standby: accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
}

async fn splice(mut client: TcpStream, socket: PathBuf) {
    let _ = client.set_nodelay(true);
    let started = Instant::now();
    let mut upstream = loop {
        match UnixStream::connect(&socket).await {
            Ok(u) if same_user(&u) => break u,
            Ok(_) => {
                tracing::warn!(socket = %socket.display(), "standby: serve.sock belongs to another user; not relaying to it");
                return;
            }
            Err(_) if started.elapsed() < PATIENCE => tokio::time::sleep(RETRY).await,
            Err(e) => {
                tracing::warn!(error = %e, socket = %socket.display(), "standby: no server answered on serve.sock; closing the connection");
                return;
            }
        }
    };
    if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        tracing::trace!(error = %e, "standby: relayed connection ended");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn a_socket_path_too_long_to_bind_is_refused() {
        assert!(socket_path(Path::new("/rails/storage/railwatch")).is_some());
        assert!(socket_path(&PathBuf::from(format!("/{}", "d".repeat(100)))).is_none());
    }

    /// A connection accepted before anyone listens on the socket waits for
    /// the server that turns up there, and the bytes go through both ways.
    #[tokio::test]
    async fn a_connection_in_the_gap_reaches_the_server_that_turns_up() {
        let dir = std::env::temp_dir().join(format!("rw-standby-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(SOCKET);
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let relay = tokio::spawn(relay(tcp, socket.clone(), 4, stopped));

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let server = bind(&socket).unwrap();
        let (mut conn, _) = server.accept().await.unwrap();
        let mut got = [0u8; 4];
        conn.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        conn.write_all(b"pong").await.unwrap();
        client.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"pong");

        stop.send(()).unwrap();
        let (listener, _) = relay.await.unwrap();
        assert_eq!(listener.local_addr().unwrap(), addr, "the same listener comes back to be served");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_socket_is_owner_only_and_never_replaces_a_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rw-standby-bind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let socket = dir.join(SOCKET);
            let first = bind(&socket).unwrap();
            assert_eq!(std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777, 0o600);
            drop(first);
            bind(&socket).expect("a dead server's socket is replaced");
            let file = dir.join("not-a-socket");
            std::fs::write(&file, b"data").unwrap();
            assert!(bind(&file).is_err(), "a regular file at the path is refused, not deleted");
            assert_eq!(std::fs::read(&file).unwrap(), b"data");
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Past `max` relayed connections, the next one waits in the backlog
    /// instead of being relayed (or dropped).
    #[tokio::test]
    async fn relayed_connections_are_bounded() {
        let dir = std::env::temp_dir().join(format!("rw-standby-max-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join(SOCKET);
        let server = bind(&socket).unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = tcp.local_addr().unwrap();
        let (_stop, stopped) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(relay(tcp, socket.clone(), 1, stopped));

        let _a = TcpStream::connect(addr).await.unwrap();
        let (held, _) = server.accept().await.unwrap();
        let _b = TcpStream::connect(addr).await.unwrap();
        let second = tokio::time::timeout(Duration::from_millis(300), server.accept()).await;
        assert!(second.is_err(), "the second connection is not relayed while the first holds the only slot");
        drop(held);
        drop(_a);
        tokio::time::timeout(Duration::from_secs(5), server.accept()).await.expect("relayed once a slot frees").unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
