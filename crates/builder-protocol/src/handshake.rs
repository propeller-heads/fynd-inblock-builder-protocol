//! The first frame each side sends, and what it protects against.

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_util::codec::Framed;

use crate::codec::{CodecError, MessageCodec};
use crate::transport::DuplexStream;

/// Bumped on any change to the message types that is not additive-at-the-end.
pub const PROTOCOL_VERSION: u32 = 3;

/// The first frame each side sends, before anything else.
///
/// Postcard carries no field names and no tags, so it has no forward compatibility at all:
/// a struct that gained a field does not fail to parse against an older peer, it misparses
/// into plausible garbage. A solver reading a `JitOrderDetected` with its fields shifted by
/// one would quote against numbers nobody sent. The handshake is what makes that impossible
/// rather than merely unlikely, which is why there is no negotiation and no downgrade path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// The [`PROTOCOL_VERSION`] the sender was built with. Must match exactly.
    pub protocol_version: u32,
    /// EVM chain id the sender operates on. Must match exactly: a route solved for one
    /// chain stored on another would be paid for and never hit.
    pub chain_id: u64,
}

impl Hello {
    /// This build's hello for `chain_id`.
    #[must_use]
    pub const fn new(chain_id: u64) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            chain_id,
        }
    }
}

/// Why a connection did not survive its first frame.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    /// The peer was built against another [`PROTOCOL_VERSION`]. Upgrade one side.
    #[error("peer speaks protocol version {theirs}, this build speaks {ours}")]
    ProtocolVersion {
        /// This side's version.
        ours: u32,
        /// The peer's version.
        theirs: u32,
    },
    /// The peer is configured for another chain.
    #[error("peer is on chain {theirs}, this side is on chain {ours}")]
    ChainId {
        /// This side's chain id.
        ours: u64,
        /// The peer's chain id.
        theirs: u64,
    },
    /// The peer hung up before sending its [`Hello`].
    #[error("the peer closed the connection before sending its hello")]
    Closed,
    /// The hello frame could not be read or written.
    #[error(transparent)]
    Codec(#[from] CodecError),
}

/// Exchanges hellos and hands back the connection framed for real traffic.
///
/// Both sides send before either reads, so neither waits on the other.
pub(crate) async fn handshake<In, Out>(
    stream: Box<dyn DuplexStream>,
    chain_id: u64,
    protocol_version: u32,
) -> Result<Framed<Box<dyn DuplexStream>, MessageCodec<In, Out>>, HandshakeError> {
    let mut hellos = Framed::new(stream, MessageCodec::<Hello, Hello>::new());
    hellos
        .send(Hello {
            protocol_version,
            chain_id,
        })
        .await?;
    let theirs = hellos.next().await.ok_or(HandshakeError::Closed)??;

    if theirs.protocol_version != protocol_version {
        return Err(HandshakeError::ProtocolVersion {
            ours: protocol_version,
            theirs: theirs.protocol_version,
        });
    }
    if theirs.chain_id != chain_id {
        return Err(HandshakeError::ChainId {
            ours: chain_id,
            theirs: theirs.chain_id,
        });
    }
    Ok(hellos.map_codec(|_| MessageCodec::new()))
}
