use std::time::Duration;

use alloy::primitives::B256;
use builder_types::{BuilderEvent, SolveRequest, SolveResponse};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::codec::Framed;
use tracing::warn;

use crate::codec::{CodecError, MessageCodec};
use crate::handshake::handshake;
use crate::transport::connect;
use crate::{ConnectError, DuplexStream, ListenUrl, Listener};

/// The [`Hello`](crate::Hello) version both ends of a solver connection send.
pub const ONE_SHOT_PROTOCOL_VERSION: u32 = 4;

/// A builder event and its connection's reply channel.
pub type Request = (BuilderEvent, mpsc::Sender<SolveResponse>);

/// Solver → builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SolverMessage {
    /// The result of one solve request.
    Solved(SolveResponse),
}

/// Serves solve requests on each builder connection.
pub async fn serve(listener: Listener, chain_id: u64, requests: mpsc::Sender<Request>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            () = requests.closed() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let requests = requests.clone();
                    connections.spawn(async move {
                        if let Err(error) = connection(stream, chain_id, requests).await {
                            warn!(%peer, %error, "request-response connection closed");
                        }
                    });
                }
                Err(error) => {
                    warn!(%error, "request-response listener stopped accepting");
                    break;
                }
            },
            finished = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = finished {
                    warn!(%error, "request-response connection task failed");
                }
            }
        }
    }
    connections.shutdown().await;
}

async fn connection(
    stream: Box<dyn DuplexStream>,
    chain_id: u64,
    requests: mpsc::Sender<Request>,
) -> Result<(), crate::HandshakeError> {
    let Ok(framed) = tokio::time::timeout(
        Duration::from_secs(5),
        handshake::<BuilderEvent, SolverMessage>(stream, chain_id, ONE_SHOT_PROTOCOL_VERSION),
    )
    .await
    else {
        warn!("request-response peer sent no hello; connection dropped");
        return Ok(());
    };
    let mut framed = framed?;
    let (reply, mut responses) = mpsc::channel::<SolveResponse>(16);
    loop {
        tokio::select! {
            () = requests.closed() => break,
            message = framed.next() => {
                let Some(message) = message else { break };
                if requests.send((message?, reply.clone())).await.is_err() {
                    break;
                }
            }
            response = responses.recv() => {
                let Some(response) = response else { break };
                framed.send(SolverMessage::Solved(response)).await?;
            }
        }
    }
    Ok(())
}

/// A builder's connection to a solver.
pub struct FyndJitSolverClient {
    connection: Framed<Box<dyn DuplexStream>, MessageCodec<SolverMessage, BuilderEvent>>,
}

impl std::fmt::Debug for FyndJitSolverClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FyndJitSolverClient")
    }
}

impl FyndJitSolverClient {
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
            connection: handshake(stream, chain_id, ONE_SHOT_PROTOCOL_VERSION).await?,
        })
    }

    /// Sends one event to the solver.
    ///
    /// # Errors
    ///
    /// Returns a [`CodecError`] when the event cannot be encoded or the connection fails.
    pub async fn send(&mut self, event: BuilderEvent) -> Result<(), CodecError> {
        self.connection.send(event).await
    }

    /// Tells the solver the frontrun for `order_key` landed.
    ///
    /// # Errors
    ///
    /// Returns a [`CodecError`] when the connection fails.
    pub async fn included(&mut self, order_key: B256) -> Result<(), CodecError> {
        self.send(BuilderEvent::Included { order_key }).await
    }

    /// The next response, or `None` once the solver hangs up.
    ///
    /// `Some(Err(_))` is a frame that could not be decoded or a failed read; the connection
    /// is not usable after one.
    pub async fn next_response(&mut self) -> Option<Result<SolveResponse, CodecError>> {
        let message = self.connection.next().await?;
        Some(message.map(|SolverMessage::Solved(response)| response))
    }

    /// Sends `request` and waits for the response carrying its id, or `None` once the solver
    /// hangs up first.
    ///
    /// Responses to other request ids are skipped: they answer requests sent earlier on this
    /// connection that nobody waits for any more.
    pub async fn solve(
        &mut self,
        request: SolveRequest,
    ) -> Option<Result<SolveResponse, CodecError>> {
        let request_id = request.request_id;
        if let Err(error) = self.send(BuilderEvent::Solve(request)).await {
            return Some(Err(error));
        }
        loop {
            match self.next_response().await? {
                Ok(response) if response.request_id != request_id => {}
                answered => return Some(answered),
            }
        }
    }
}
