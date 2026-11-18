//! The wire protocol between a block builder and the fynd JIT route solver.
//!
//! Running the solver in-process requires the builder to be a Rust program that links the
//! solver as a library. This crate lets a builder in a separate
//! process talk to the solver over a socket, on the same machine or the same local network.
//! The solver can then ship as an artefact a builder simply runs.
//!
//! # Shape
//!
//! The solver runs [`serve_one_shot`]. The builder dials it with
//! [`FyndJitSolverClient::connect`].
//!
//! The builder sends one [`BuilderEvent::Solve`] per order and gets one [`SolveResponse`]
//! back. The response carries the request's id and a [`Frontrun`], or `None` when the solver
//! declines. [`BuilderEvent::Included`] tells the solver that a frontrun landed. It gets no
//! response.
//!
//! A builder may open several connections. Each response goes back on the connection that
//! sent the request.
//!
//! Each side sends a [`Hello`] first and reads the peer's before anything else. Both hellos
//! must carry [`ONE_SHOT_PROTOCOL_VERSION`] and the same chain id, or the connection closes.
//! Frames are a 4-byte big-endian length prefix followed by a postcard body, capped at
//! [`MAX_FRAME_LEN`].
//!
//! Bump [`ONE_SHOT_PROTOCOL_VERSION`] on any change to how a one-shot message encodes. The
//! `protocol_version` unit test enforces it: it compares fixed samples of each message
//! against encodings recorded in `tests/fixtures/`, together with the version they were
//! recorded at. When the encoding changes, bump the version and re-record with
//! `UPDATE_FIXTURES=1 cargo test -p builder-protocol --lib protocol_version`.
//!
//! Postcard rather than bincode because its wire format is specified rather than
//! implementation-defined, and its varint integers are smaller. It is serde-native, so the
//! derives already on [`builder_types`] carry over untouched. The cost, taken knowingly, is
//! that a non-Rust builder reimplements the encoding by hand. Postcard also cannot carry
//! `#[serde(flatten)]` or an untagged enum, since both need `deserialize_any`. Nothing in
//! `builder_types` uses either. `tests/wire_format.rs` checks `builder-types/src/lib.rs` for
//! both.
//!
//! # The router reports the order, not the builder
//!
//! The builder does not decode orders. It simulates the order's transaction with
//! `FyndJITRouter`'s observations enabled: once alone at the top of the block, and once at the
//! end of a candidate iteration. Each run emits a `FyndOrderObserved` log. The
//! [`SolveRequest`] carries both, and the solver reads the order from them.
//!
//! This matters for wrapped calls. Most flow is expected to reach `FyndJITRouter` from an
//! integrator's own contract. The order key the router computes binds `msg.sender`, which on
//! a wrapped call is that contract, not the EOA that signed the transaction. The log carries
//! the order key and sender as the router saw them, so neither side has to reconstruct them.
//!
//! # The network is assumed to be trusted
//!
//! There is no authentication, and none is planned. The two processes are the same
//! operator's. They share a machine over a Unix socket, or a private network over TCP. The
//! [`ListenUrl`] the solver is given is the whole of the access control, exactly as it is for
//! the JSON-RPC endpoint the same builder already runs.
//!
//! The solver accepts every connection that completes the handshake. On a `tcp://` listener
//! reachable from outside, anyone who can dial it can send fabricated requests and read the
//! frontruns that come back. They can also send fabricated [`BuilderEvent::Included`]
//! events, which skew the solver's gas estimates. Bind to a Unix socket where the two
//! processes share a machine. Bind to a loopback or otherwise private address where they do
//! not.
//!
//! # The deprecated streaming API
//!
//! The crate also has a streaming API: [`serve`], [`BuilderClient`], [`BuilderMessage`] and
//! [`SolverMessage`]. There the builder pushes orders it decoded itself, and [`BuildEvent`]s.
//! The solver pushes frontruns back. [`serve`] keeps every connection and sends each frontrun
//! on all of them. Its hello carries [`PROTOCOL_VERSION`]. `fynd-jit-solver` does not serve
//! this API.
//!
//! # Everything payload-shaped is re-exported
//!
//! A builder depends on this crate alone and gets [`SolveRequest`], [`Frontrun`] and the rest
//! with it, which is what lets `builder-types` stay dependency-light.

#![warn(missing_docs)]

mod client;
mod codec;
mod handshake;
#[cfg(test)]
mod protocol_version;
mod server;
mod solve;
mod transport;

pub use builder_types::{
    AccountState, BlockEnv, BuildEvent, BuilderEvent, ClientFee, CloseReason, ExecutedTx, Frontrun,
    JitFrontrun, JitOrderClosed, JitOrderDetected, JitOrderEvent, PostState, RawTx, SolveRequest,
    SolveResponse, SwapKind,
};
pub use client::{BuilderClient, ConnectError};
pub use codec::{CodecError, MessageCodec, MAX_FRAME_LEN};
pub use handshake::{HandshakeError, Hello, PROTOCOL_VERSION};
pub use server::serve;
pub use solve::{
    serve as serve_one_shot, FyndJitSolverClient, Request as OneShotRequest,
    ONE_SHOT_PROTOCOL_VERSION,
};
pub use transport::{connect, DuplexStream, ListenUrl, ListenUrlError, Listener};

use alloy::primitives::B256;
use serde::{Deserialize, Serialize};

/// Builder → solver, in the streaming API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuilderMessage {
    /// Orders the builder decoded, in one batch.
    ///
    /// Several may share a [`tx_hash`](JitOrderDetected::tx_hash): one transaction can carry
    /// several router calls. Each order carries its own hash rather than the batch naming
    /// one, which keeps [`JitOrderEvent::Detected`] self-contained and stops the socket and
    /// in-process paths from diverging.
    Orders(Vec<JitOrderDetected>),
    /// A transaction the builder no longer holds, which ends every order it still carries.
    ///
    /// Names the transaction rather than the order because that is what a builder tracks,
    /// and the translation is one-to-many in both directions — see
    /// [`JitOrderClosed`].
    TransactionClosed {
        /// Hash of the transaction the builder dropped or saw land.
        tx_hash: B256,
        /// Whether the transaction landed in a block or was dropped.
        reason: CloseReason,
    },
    /// One block-build iteration event.
    Build(BuildEvent),
}

/// Solver → builder, in the streaming API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SolverMessage {
    /// One `storeBatch` transaction, unsigned, with the orders it covers. The builder signs
    /// it and places it directly before the covered user transactions in the target block.
    Frontrun(JitFrontrun),
}
