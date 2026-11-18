//! The solver's end: accept the builder, translate what it pushes, hand back frontruns.

use std::time::Duration;

use builder_types::{BuildEvent, JitFrontrun, JitOrderClosed, JitOrderEvent};
use futures::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;
use tracing::{debug, info, warn};

use crate::codec::MessageCodec;
use crate::handshake::{handshake, PROTOCOL_VERSION};
use crate::transport::{DuplexStream, Listener};
use crate::{BuilderMessage, SolverMessage};

/// A connection past its handshake, waiting to be installed.
type Handshaken = Framed<Box<dyn DuplexStream>, MessageCodec<BuilderMessage, SolverMessage>>;

/// Frames a connection may buffer from the builder before the solve loop drains them.
///
/// Order intake is bursty — a builder catching up after a reconnect pushes its whole live
/// set at once — and every one of them is cheap to apply, so this is generous.
const INBOUND_QUEUE: usize = 1_024;

/// Frontruns queued for one builder before they start being dropped.
///
/// Small on purpose: a frontrun is worthless once its target block has passed, so a deep
/// queue would only delay noticing that the builder stopped reading.
const OUTBOUND_QUEUE: usize = 16;

/// How long a peer has to send its hello before its connection is dropped.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Handshakes completed but not yet installed.
///
/// Small: the loop drains this on every pass, so it only ever holds the connections a
/// builder opened close enough together to complete their handshakes in the same turn.
const PENDING_QUEUE: usize = 8;

/// Serves the builder on `listener`, bridging its connections onto the solver's channels.
///
/// **Many sockets, one builder.** A builder opens as many connections as it has senders —
/// one per simulator thread, typically. All of them feed the same inbound channel, and every
/// frontrun is delivered on all of them, because nothing in a frontrun names the connection
/// that asked for it. The solver's state is process-wide, not per-connection, so the number
/// of sockets changes only how many places a message can arrive from.
///
/// **Stateless.** Everything the builder pushes is already decoded, and which orders a close
/// ends is the book's question — so the server holds no order state of its own and cannot
/// disagree with the book about what is open.
///
/// **Nothing in this loop waits on a peer.** Handshakes run in their own tasks and are
/// installed here when they finish, so a connection that never sends its hello costs one task
/// and a socket rather than the transport's attention.
///
/// Returns once the solver stops taking frontruns, which is how the binary shuts the
/// transport down.
pub async fn serve(
    listener: Listener,
    chain_id: u64,
    orders: mpsc::Sender<JitOrderEvent>,
    events: mpsc::Sender<BuildEvent>,
    mut frontruns: mpsc::Receiver<JitFrontrun>,
) {
    let (inbound_tx, mut inbound_rx) = mpsc::channel::<BuilderMessage>(INBOUND_QUEUE);
    let (ready_tx, mut ready_rx) = mpsc::channel::<(String, Handshaken)>(PENDING_QUEUE);
    let mut active: Vec<Connection> = Vec::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    // Handshaked in its own task, never awaited here: this is also the loop
                    // that delivers frontruns, so a peer that connects and then says nothing
                    // would hold up order intake and frontrun delivery for the whole timeout.
                    // Five seconds is two to three blocks on Base, and a frontrun is worthless
                    // outside its target block. A load-balancer health check or a port scan is
                    // enough to cause it, and to keep causing it.
                    Ok((stream, peer)) => {
                        tokio::spawn(greet(stream, peer, chain_id, ready_tx.clone()));
                    }
                    // A listener that cannot accept will not start doing so; leaving the
                    // loop is what surfaces that rather than spinning on the error.
                    Err(e) => {
                        warn!(error = %e, "listener stopped accepting");
                        break;
                    }
                }
            }
            ready = ready_rx.recv() => {
                // `ready_tx` is held for the life of the loop, so this closes only with it.
                let Some((peer, framed)) = ready else { break };
                let (sink, source) = framed.split();
                let (out_tx, out_rx) = mpsc::channel(OUTBOUND_QUEUE);
                let connection = Connection {
                    peer: peer.clone(),
                    out: out_tx,
                    reader: tokio::spawn(read_from_builder(source, inbound_tx.clone())),
                    writer: tokio::spawn(write_to_builder(sink, out_rx)),
                };
                active.push(connection);
                info!(peer = %peer, connections = active.len(), "builder connected");
            }
            message = inbound_rx.recv() => {
                let Some(message) = message else { break };
                if !apply(message, &orders, &events).await {
                    break;
                }
            }
            frontrun = frontruns.recv() => {
                let Some(frontrun) = frontrun else { break };
                // A builder that hung up leaves a connection whose halves have ended. Letting
                // those go here is what makes the next line say "no builder connected" instead
                // of accusing peers that are no longer there of not reading.
                active.retain_mut(|connection| {
                    let closed = connection.is_closed();
                    if closed {
                        info!(peer = %connection.peer, "builder disconnected");
                    }
                    !closed
                });
                if active.is_empty() {
                    warn!(
                        target_block = frontrun.target_block,
                        "no builder connected; frontrun dropped"
                    );
                    continue;
                }
                // Never awaited: a frontrun for a block that has passed is worthless, and
                // blocking here would stall every other order behind one slow consumer.
                for connection in &active {
                    if let Err(e) =
                        connection.out.try_send(SolverMessage::Frontrun(frontrun.clone()))
                    {
                        let SolverMessage::Frontrun(frontrun) = e.into_inner();
                        warn!(
                            target_block = frontrun.target_block,
                            peer = %connection.peer,
                            "builder is not draining frontruns; dropped"
                        );
                    }
                }
            }
        }
    }
    info!("builder transport stopped");
}

/// Handshakes one peer and hands the framed connection back, or drops it saying why.
///
/// Spawned per connection so that [`serve`]'s loop never waits on a peer's first frame.
async fn greet(
    stream: Box<dyn DuplexStream>,
    peer: String,
    chain_id: u64,
    ready: mpsc::Sender<(String, Handshaken)>,
) {
    match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        handshake(stream, chain_id, PROTOCOL_VERSION),
    )
    .await
    {
        Ok(Ok(framed)) => {
            if ready.send((peer.clone(), framed)).await.is_err() {
                debug!(peer = %peer, "the transport stopped while this peer was connecting");
            }
        }
        Ok(Err(e)) => warn!(peer = %peer, error = %e, "handshake failed"),
        Err(_) => warn!(peer = %peer, "peer sent no hello; connection dropped"),
    }
}

/// One of the builder's live connections.
struct Connection {
    peer: String,
    out: mpsc::Sender<SolverMessage>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Connection {
    /// Whether either half has ended, which is how a hang-up reaches this side: the reader
    /// returns when the socket closes, and the writer when a send fails.
    fn is_closed(&mut self) -> bool {
        self.reader.is_finished() || self.writer.is_finished()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// Applies one builder message, returning whether the solver is still listening.
async fn apply(
    message: BuilderMessage,
    orders: &mpsc::Sender<JitOrderEvent>,
    events: &mpsc::Sender<BuildEvent>,
) -> bool {
    match message {
        // One event per order, so the batching is a framing choice the solver never sees.
        BuilderMessage::Orders(batch) => {
            for order in batch {
                if orders
                    .send(JitOrderEvent::Detected(Box::new(order)))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        }
        BuilderMessage::TransactionClosed { tx_hash, reason } => orders
            .send(JitOrderEvent::Closed(JitOrderClosed { tx_hash, reason }))
            .await
            .is_ok(),
        BuilderMessage::Build(event) => events.send(event).await.is_ok(),
    }
}

async fn read_from_builder<S>(mut source: S, inbound: mpsc::Sender<BuilderMessage>)
where
    S: Stream<Item = Result<BuilderMessage, crate::CodecError>> + Unpin,
{
    while let Some(message) = source.next().await {
        match message {
            Ok(message) => {
                if inbound.send(message).await.is_err() {
                    break;
                }
            }
            Err(e) => {
                warn!(error = %e, "builder connection failed while reading");
                break;
            }
        }
    }
}

async fn write_to_builder<S>(mut sink: S, mut outbound: mpsc::Receiver<SolverMessage>)
where
    S: Sink<SolverMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    while let Some(message) = outbound.recv().await {
        if let Err(e) = sink.send(message).await {
            warn!(error = %e, "builder connection failed while writing");
            break;
        }
    }
}
