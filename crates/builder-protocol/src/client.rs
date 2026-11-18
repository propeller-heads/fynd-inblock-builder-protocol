//! The builder's end of the connection.

use builder_types::JitFrontrun;
use futures::{SinkExt, StreamExt};
use tokio_util::codec::Framed;

use crate::codec::{CodecError, MessageCodec};
use crate::handshake::{handshake, HandshakeError, PROTOCOL_VERSION};
use crate::transport::{connect, DuplexStream, ListenUrl};
use crate::{BuilderMessage, SolverMessage};

/// Why a builder could not reach a solver.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The socket could not be opened: nothing listening, a missing socket file, or a
    /// refused TCP connection.
    #[error("could not connect to {url}: {source}")]
    Io {
        /// The [`ListenUrl`] that was dialed, as text.
        url: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The connection opened but the [`Hello`](crate::Hello) exchange failed.
    #[error(transparent)]
    Handshake(#[from] HandshakeError),
}

/// A builder's connection to a solver running the streaming [`serve`](crate::serve).
///
/// Sending is `await`ed rather than dropped on backpressure: order intake is the builder's
/// own flow and losing an order costs the improvement on it. Frontruns travel the other way
/// and are dropped instead — see [`serve`](crate::serve).
pub struct BuilderClient {
    framed: Framed<Box<dyn DuplexStream>, MessageCodec<SolverMessage, BuilderMessage>>,
}

impl std::fmt::Debug for BuilderClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BuilderClient")
    }
}

impl BuilderClient {
    /// Dials `url` and completes the handshake.
    ///
    /// # Errors
    ///
    /// Returns [`ConnectError::Io`] when the socket cannot be opened and
    /// [`ConnectError::Handshake`] when the peer speaks another protocol version or is on
    /// another chain.
    pub async fn connect(url: &ListenUrl, chain_id: u64) -> Result<Self, ConnectError> {
        let stream = connect(url).await.map_err(|source| ConnectError::Io {
            url: url.to_string(),
            source,
        })?;
        Ok(Self {
            framed: handshake(stream, chain_id, PROTOCOL_VERSION).await?,
        })
    }

    /// Pushes one message to the solver.
    ///
    /// # Errors
    ///
    /// Returns a [`CodecError`] when the message cannot be encoded or the connection fails.
    pub async fn send(&mut self, message: BuilderMessage) -> Result<(), CodecError> {
        self.framed.send(message).await
    }

    /// The next frontrun, or `None` once the solver hangs up.
    ///
    /// `Some(Err(_))` is a frame that could not be decoded or a failed read; the connection
    /// is not usable after one.
    pub async fn next_frontrun(&mut self) -> Option<Result<JitFrontrun, CodecError>> {
        let message = self.framed.next().await?;
        Some(message.map(|SolverMessage::Frontrun(frontrun)| frontrun))
    }
}
