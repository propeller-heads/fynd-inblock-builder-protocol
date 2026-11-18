//! Where the solver listens and the builder connects.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tracing::warn;

/// A byte stream either transport can produce.
///
/// Boxed rather than made generic because the transport is chosen at runtime from a URL,
/// and nothing above this layer cares which one it got.
pub trait DuplexStream: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> DuplexStream for T {}

/// The address the solver listens on and the builder dials.
///
/// The scheme picks the transport: `unix:///run/fynd-jit.sock` for the same machine,
/// `tcp://127.0.0.1:9000` for the same local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenUrl {
    /// A Unix domain socket at this path. The solver removes a stale file before binding
    /// and unlinks it on shutdown.
    Unix(PathBuf),
    /// A TCP `host:port`, as `TcpListener::bind` and `TcpStream::connect` take it.
    Tcp(String),
}

/// Why a string is not a [`ListenUrl`].
#[derive(Debug, thiserror::Error)]
#[error("{url} is not a listen url: expected unix://<path> or tcp://<host>:<port>")]
pub struct ListenUrlError {
    url: String,
}

impl FromStr for ListenUrl {
    type Err = ListenUrlError;

    fn from_str(s: &str) -> Result<Self, ListenUrlError> {
        if let Some(path) = s.strip_prefix("unix://") {
            if !path.is_empty() {
                return Ok(Self::Unix(PathBuf::from(path)));
            }
        }
        if let Some(addr) = s.strip_prefix("tcp://") {
            if !addr.is_empty() {
                return Ok(Self::Tcp(addr.to_owned()));
            }
        }
        Err(ListenUrlError { url: s.to_owned() })
    }
}

impl fmt::Display for ListenUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
            Self::Tcp(addr) => write!(f, "tcp://{addr}"),
        }
    }
}

/// A bound listener, of whichever transport the URL named.
#[derive(Debug)]
pub struct Listener {
    kind: ListenerKind,
}

#[derive(Debug)]
enum ListenerKind {
    /// The path is kept so the socket file can be unlinked on drop; a leftover file makes
    /// the next bind fail with `EADDRINUSE` on a socket nothing is listening to.
    Unix(UnixListener, PathBuf),
    Tcp(TcpListener),
}

impl Listener {
    /// Binds `url`, removing a stale Unix socket file first.
    ///
    /// Removing before binding is safe in the only sense that matters here: a second solver
    /// pointed at the same socket would be a second owner of state that cannot be shared,
    /// so the last one to start winning is the same rule the connection handling already
    /// applies.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the path or address cannot be bound.
    pub async fn bind(url: &ListenUrl) -> io::Result<Self> {
        match url {
            ListenUrl::Unix(path) => {
                remove_stale_socket(path);
                Ok(Self {
                    kind: ListenerKind::Unix(UnixListener::bind(path)?, path.clone()),
                })
            }
            ListenUrl::Tcp(addr) => Ok(Self {
                kind: ListenerKind::Tcp(TcpListener::bind(addr).await?),
            }),
        }
    }

    /// The address actually bound, which for `tcp://127.0.0.1:0` is the port the OS chose.
    ///
    /// # Errors
    ///
    /// Returns the OS error when a TCP listener cannot report its local address.
    pub fn local_url(&self) -> io::Result<ListenUrl> {
        match &self.kind {
            ListenerKind::Unix(_, path) => Ok(ListenUrl::Unix(path.clone())),
            ListenerKind::Tcp(listener) => Ok(ListenUrl::Tcp(listener.local_addr()?.to_string())),
        }
    }

    /// Accepts one connection, with a printable peer address for logs.
    ///
    /// # Errors
    ///
    /// Returns the OS error when the listener cannot accept.
    pub async fn accept(&self) -> io::Result<(Box<dyn DuplexStream>, String)> {
        match &self.kind {
            ListenerKind::Unix(listener, _) => {
                let (stream, peer) = listener.accept().await?;
                Ok((Box::new(stream), format!("{peer:?}")))
            }
            ListenerKind::Tcp(listener) => {
                let (stream, peer) = listener.accept().await?;
                Ok((Box::new(stream), peer.to_string()))
            }
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let ListenerKind::Unix(_, path) = &self.kind {
            if let Err(e) = std::fs::remove_file(path) {
                warn!(path = %path.display(), error = %e, "could not remove the listening socket");
            }
        }
    }
}

/// Dials a listening solver. [`FyndJitSolverClient::connect`](crate::FyndJitSolverClient::connect)
/// and [`BuilderClient::connect`](crate::BuilderClient::connect) do this and the handshake in
/// one step.
///
/// # Errors
///
/// Returns the OS error when nothing is listening at `url`.
pub async fn connect(url: &ListenUrl) -> io::Result<Box<dyn DuplexStream>> {
    match url {
        ListenUrl::Unix(path) => Ok(Box::new(UnixStream::connect(path).await?)),
        ListenUrl::Tcp(addr) => Ok(Box::new(TcpStream::connect(addr).await?)),
    }
}

fn remove_stale_socket(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => warn!(path = %path.display(), "removed a stale socket file before binding"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "could not remove the socket file"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::str::FromStr;

    use super::ListenUrl;

    #[test]
    fn the_scheme_picks_the_transport() {
        assert_eq!(
            ListenUrl::from_str("unix:///run/fynd-jit.sock").ok(),
            Some(ListenUrl::Unix(PathBuf::from("/run/fynd-jit.sock")))
        );
        assert_eq!(
            ListenUrl::from_str("tcp://127.0.0.1:9000").ok(),
            Some(ListenUrl::Tcp("127.0.0.1:9000".to_owned()))
        );
    }

    #[test]
    fn a_url_round_trips_through_display() {
        for url in ["unix:///run/fynd-jit.sock", "tcp://127.0.0.1:9000"] {
            assert_eq!(
                ListenUrl::from_str(url)
                    .ok()
                    .map(|u| u.to_string())
                    .as_deref(),
                Some(url)
            );
        }
    }

    #[test]
    fn anything_else_is_refused() {
        for url in [
            "",
            "/run/fynd-jit.sock",
            "http://localhost:9000",
            "unix://",
            "tcp://",
        ] {
            assert!(ListenUrl::from_str(url).is_err(), "{url} should be refused");
        }
    }
}
