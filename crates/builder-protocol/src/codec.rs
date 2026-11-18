//! Length-prefixed postcard frames.

use std::marker::PhantomData;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_util::bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

/// Largest frame either side will send or accept, in bytes.
///
/// The biggest real message is a [`BuildEvent::IterationComplete`](builder_types::BuildEvent)
/// carrying a `PostState`, which sits well inside this. The limit exists so a corrupt or
/// hostile length prefix is refused before anything is allocated for it.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// Width of the big-endian length prefix that precedes every body.
const PREFIX_LEN: usize = 4;

/// Why a frame could not be turned into a message, or a message into a frame.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    /// A length prefix, or a body about to be written, exceeds [`MAX_FRAME_LEN`].
    #[error("frame of {len} bytes exceeds the {MAX_FRAME_LEN}-byte limit")]
    FrameTooLarge {
        /// The offending length in bytes.
        len: usize,
    },
    /// A complete frame arrived but its body is not a valid postcard encoding of the
    /// expected message type. Usually a protocol-version mismatch the handshake did not see.
    #[error("could not decode a message body: {0}")]
    Decode(#[source] postcard::Error),
    /// Postcard refused to serialize the message.
    #[error("could not encode a message body: {0}")]
    Encode(#[source] postcard::Error),
    /// The underlying socket failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The read and write directions, as a type the codec can hold without owning either.
type Directions<In, Out> = PhantomData<(fn() -> In, fn(Out))>;

/// Reads `In` and writes `Out` over one connection.
///
/// The two directions are different types — a solver reads
/// [`BuilderMessage`](crate::BuilderMessage) and writes [`SolverMessage`](crate::SolverMessage),
/// a builder does the reverse — so the codec carries both rather than forcing one union type
/// onto the wire.
#[derive(Debug)]
pub struct MessageCodec<In, Out> {
    directions: Directions<In, Out>,
}

impl<In, Out> MessageCodec<In, Out> {
    /// A codec for one connection. Stateless, so any number can be made.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            directions: PhantomData,
        }
    }
}

impl<In, Out> Default for MessageCodec<In, Out> {
    fn default() -> Self {
        Self::new()
    }
}

impl<In: DeserializeOwned, Out> Decoder for MessageCodec<In, Out> {
    type Item = In;
    type Error = CodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<In>, CodecError> {
        let Some(prefix) = src.get(..PREFIX_LEN) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
        // Checked before the reserve below, so an implausible length costs nothing.
        if len > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge { len });
        }
        if src.len() < PREFIX_LEN + len {
            src.reserve(PREFIX_LEN + len - src.len());
            return Ok(None);
        }
        src.advance(PREFIX_LEN);
        let body = src.split_to(len);
        postcard::from_bytes(&body)
            .map(Some)
            .map_err(CodecError::Decode)
    }
}

impl<In, Out: Serialize> Encoder<Out> for MessageCodec<In, Out> {
    type Error = CodecError;

    fn encode(&mut self, item: Out, dst: &mut BytesMut) -> Result<(), CodecError> {
        let body = postcard::to_stdvec(&item).map_err(CodecError::Encode)?;
        let len =
            u32::try_from(body.len()).map_err(|_| CodecError::FrameTooLarge { len: body.len() })?;
        if body.len() > MAX_FRAME_LEN {
            return Err(CodecError::FrameTooLarge { len: body.len() });
        }
        dst.reserve(PREFIX_LEN + body.len());
        dst.put_u32(len);
        dst.put_slice(&body);
        Ok(())
    }
}
