//! The protocol over a real socket: a builder connects, pushes, and takes a frontrun back.
//!
//! Everything here drives [`serve`] the way the binary does, over a Unix socket in the
//! system temp directory. The builder decodes on its own side, so the orders are assembled
//! here rather than derived from bytes the solver has to interpret — `decoder`'s own tests
//! cover the derivation.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unwrapping is idiomatic in tests"
)]

use std::path::PathBuf;
use std::time::Duration;

use alloy::primitives::{address, Address, Bytes, B256, U256};
use builder_protocol::{
    serve, serve_one_shot, BlockEnv, BuildEvent, BuilderClient, BuilderEvent, BuilderMessage,
    ClientFee, CloseReason, Frontrun, FyndJitSolverClient, Hello, JitFrontrun, JitOrderDetected,
    JitOrderEvent, ListenUrl, Listener, MessageCodec, RawTx, SolveRequest, SolveResponse, SwapKind,
    ONE_SHOT_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_util::codec::Framed;
use uuid::Uuid;

const CHAIN_ID: u64 = 8453;
const TOKEN_IN: Address = address!("4200000000000000000000000000000000000006");
const TOKEN_OUT: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

const TX_A: B256 = B256::repeat_byte(0x0A);
const TX_B: B256 = B256::repeat_byte(0x0B);

/// How long a test waits for something that should be immediate.
const SOON: Duration = Duration::from_secs(5);

/// A socket path short enough for `sun_path`, unique per test.
fn socket_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fynd-jit-{name}-{}.sock", std::process::id()))
}

/// An order as a builder assembles it: distinct callers make distinct orders, and the
/// transaction is named so that sharing one between orders can be expressed.
fn order(caller: Address, tx_hash: B256) -> JitOrderDetected {
    JitOrderDetected {
        order_ref: B256::ZERO,
        sender: caller,
        tx_hash,
        nonce: 1,
        token_in: TOKEN_IN,
        token_out: TOKEN_OUT,
        amount_in: U256::from(1u64),
        expected_amount_out: U256::from(1u64),
        min_amount_out: U256::from(1u64),
        receiver: TOKEN_IN,
        kind: SwapKind::Sequential,
        swaps: Bytes::from_static(&[0xde, 0xad]),
        permit2: false,
        client_fee: ClientFee {
            bps: 0,
            receiver: Address::ZERO,
            max_contribution: U256::ZERO,
            deadline: U256::ZERO,
            signature: Bytes::new(),
        },
        raw_tx: None,
    }
    .with_canonical_order_ref()
}

fn frontrun(target_block: u64) -> JitFrontrun {
    JitFrontrun {
        uuid: Uuid::from_u128(7),
        target_block,
        tx: RawTx {
            to: Some(TOKEN_IN),
            value: U256::ZERO,
            data: Bytes::from_static(&[0x01]),
            gas_limit: 100_000,
            max_fee_per_gas: 6_000_000,
            max_priority_fee_per_gas: 1_000_000,
        },
        improvement_wei: U256::from(2u64),
        cost_wei: U256::from(1u64),
        expected_gas: 26_348,
        covered: vec![B256::repeat_byte(0xAA)],
    }
}

/// A running solver-side transport, with the three channels the solve loop would own.
struct Solver {
    url: ListenUrl,
    orders: mpsc::Receiver<JitOrderEvent>,
    events: mpsc::Receiver<BuildEvent>,
    frontruns: mpsc::Sender<JitFrontrun>,
    served: tokio::task::JoinHandle<()>,
}

impl Solver {
    async fn start(name: &str) -> Self {
        let listener = Listener::bind(&ListenUrl::Unix(socket_path(name)))
            .await
            .unwrap();
        let url = listener.local_url().unwrap();
        let (order_tx, orders) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(16);
        let (frontruns, frontrun_rx) = mpsc::channel(16);
        let served = tokio::spawn(serve(listener, CHAIN_ID, order_tx, event_tx, frontrun_rx));
        Self {
            url,
            orders,
            events,
            frontruns,
            served,
        }
    }

    /// The next intake event, which a passing test always has waiting.
    async fn intake(&mut self) -> JitOrderEvent {
        soon(self.orders.recv()).await.expect("an intake event")
    }
}

impl Drop for Solver {
    fn drop(&mut self) {
        self.served.abort();
    }
}

async fn soon<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(SOON, future)
        .await
        .expect("this should not have taken five seconds")
}

/// The path the binary runs: a batch of orders and a build iteration in, a frontrun out.
#[tokio::test]
async fn a_builder_pushes_orders_and_takes_a_frontrun_back() {
    let mut solver = Solver::start("loopback").await;
    let mut builder = BuilderClient::connect(&solver.url, CHAIN_ID).await.unwrap();

    let orders = vec![order(TOKEN_IN, TX_A), order(TOKEN_OUT, TX_B)];
    builder
        .send(BuilderMessage::Orders(orders.clone()))
        .await
        .unwrap();

    // One event per order, so a batch on the wire is invisible to the solve loop.
    for pushed in &orders {
        let event = solver.intake().await;
        assert!(
            matches!(event, JitOrderEvent::Detected(order) if order.order_ref == pushed.order_ref),
            "each order in the batch should arrive as its own detection"
        );
    }

    let uuid = Uuid::from_u128(1);
    let iteration = BuildEvent::IterationStart {
        uuid,
        block: BlockEnv {
            block_number: 49_959_633,
            block_timestamp: 1_770_000_000,
            base_fee_per_gas: 5_000_000,
        },
    };
    builder
        .send(BuilderMessage::Build(iteration))
        .await
        .unwrap();
    let event = soon(solver.events.recv()).await.expect("a build event");
    assert!(matches!(
        event,
        BuildEvent::IterationStart { uuid: seen, .. } if seen == uuid
    ));

    solver.frontruns.send(frontrun(49_959_634)).await.unwrap();

    let taken = soon(builder.next_frontrun()).await.unwrap().unwrap();
    assert_eq!(taken.target_block, 49_959_634);
    assert_eq!(taken.covered, vec![B256::repeat_byte(0xAA)]);
}

/// A close crosses the connection as the transaction it names, with no interpretation on the
/// way. The server holds no order state, so which orders it ends is left to the book.
#[tokio::test]
async fn a_close_crosses_as_the_transaction_it_names() {
    let mut solver = Solver::start("closes").await;
    let mut builder = BuilderClient::connect(&solver.url, CHAIN_ID).await.unwrap();

    // Two orders in one transaction, which is what makes the translation many-to-many.
    builder
        .send(BuilderMessage::Orders(vec![
            order(TOKEN_IN, TX_A),
            order(TOKEN_OUT, TX_A),
        ]))
        .await
        .unwrap();
    for _ in 0..2 {
        solver.intake().await;
    }

    // A close for a transaction that was never a router call, then the shared one. Both
    // reach the solver: the server cannot tell them apart and does not try.
    for tx_hash in [TX_B, TX_A] {
        builder
            .send(BuilderMessage::TransactionClosed {
                tx_hash,
                reason: CloseReason::Included,
            })
            .await
            .unwrap();

        let event = solver.intake().await;
        assert!(
            matches!(
                event,
                JitOrderEvent::Closed(closed)
                    if closed.tx_hash == tx_hash && closed.reason == CloseReason::Included
            ),
            "the close should arrive naming {tx_hash} verbatim"
        );
    }
}

/// A builder opens one connection per sender, so a second must join the first rather than
/// replace it: both push orders in, and every frontrun goes out on both.
#[tokio::test]
async fn connections_accumulate_and_every_one_takes_the_frontrun() {
    let mut solver = Solver::start("accumulate").await;
    let mut first = BuilderClient::connect(&solver.url, CHAIN_ID).await.unwrap();
    let mut second = BuilderClient::connect(&solver.url, CHAIN_ID).await.unwrap();

    // A connection is only active once the accept loop has taken it, which the solver
    // observing a message from it proves.
    for (client, tx_hash) in [(&mut first, TX_A), (&mut second, TX_B)] {
        client
            .send(BuilderMessage::Orders(vec![order(TOKEN_IN, tx_hash)]))
            .await
            .unwrap();
        solver.intake().await;
    }

    solver.frontruns.send(frontrun(100)).await.unwrap();

    for client in [&mut first, &mut second] {
        assert_eq!(
            soon(client.next_frontrun())
                .await
                .unwrap()
                .unwrap()
                .target_block,
            100,
            "every live connection should take the frontrun"
        );
    }
}

/// A peer that connects and says nothing must not cost the transport anything.
///
/// The accept loop is also the loop that applies orders and hands frontruns out, so
/// handshaking inline let one silent socket hold all three up for the full handshake timeout —
/// two to three blocks on Base, and a frontrun outside its target block is worthless. A
/// load-balancer health check or a port scan is enough to cause it, and to keep causing it.
///
/// The silent socket is held open for the whole test, and every wait below is deliberately
/// shorter than the handshake timeout the transport would have been stuck in — waiting that
/// timeout out is the failure being tested, so a deadline beyond it would let the bug pass.
#[tokio::test]
async fn a_peer_that_sends_no_hello_does_not_hold_up_the_transport() {
    /// Comfortable for a Unix socket round trip, far inside the five-second handshake timeout.
    const PROMPTLY: Duration = Duration::from_secs(1);

    let mut solver = Solver::start("silent").await;
    let _silent = builder_protocol::connect(&solver.url).await.unwrap();

    let mut builder = tokio::time::timeout(PROMPTLY, BuilderClient::connect(&solver.url, CHAIN_ID))
        .await
        .expect("a silent peer must not delay the next connection's handshake")
        .unwrap();
    builder
        .send(BuilderMessage::Orders(vec![order(TOKEN_IN, TX_A)]))
        .await
        .unwrap();

    let intake = tokio::time::timeout(PROMPTLY, solver.orders.recv())
        .await
        .expect("a silent peer must not delay order intake")
        .expect("an intake event");
    assert!(matches!(intake, JitOrderEvent::Detected(_)));

    solver.frontruns.send(frontrun(100)).await.unwrap();
    let delivered = tokio::time::timeout(PROMPTLY, builder.next_frontrun())
        .await
        .expect("a silent peer must not delay frontrun delivery")
        .unwrap()
        .unwrap();
    assert_eq!(delivered.target_block, 100);
}

/// Postcard has no forward compatibility, so a version mismatch is a hard stop rather than a
/// negotiation.
#[tokio::test]
async fn a_mismatched_protocol_version_closes_the_connection() {
    let solver = Solver::start("version").await;
    let stream = builder_protocol::connect(&solver.url).await.unwrap();
    let mut hellos = Framed::new(stream, MessageCodec::<Hello, Hello>::new());

    hellos
        .send(Hello {
            protocol_version: PROTOCOL_VERSION + 1,
            chain_id: CHAIN_ID,
        })
        .await
        .unwrap();

    // The solver's own hello arrives — it sends before it reads — and then the socket ends.
    assert!(soon(hellos.next()).await.is_some());
    assert!(soon(hellos.next()).await.is_none());
}

#[tokio::test]
async fn a_mismatched_chain_id_closes_the_connection() {
    let solver = Solver::start("chain").await;

    let error = BuilderClient::connect(&solver.url, CHAIN_ID + 1)
        .await
        .expect_err("a builder on another chain must not be served");

    assert!(
        format!("{error}").contains(&(CHAIN_ID + 1).to_string()),
        "the error should name both chains: {error}"
    );
}

/// The router's key for the order the solve tests use.
const ORDER_KEY: B256 = B256::repeat_byte(0x0C);

/// Empty solve request.
fn solve_request(request_id: u64, block: u64) -> SolveRequest {
    SolveRequest {
        request_id,
        block: BlockEnv {
            block_number: block,
            block_timestamp: 200,
            base_fee_per_gas: 300,
        },
        iteration: vec![],
        top_of_block_logs: vec![],
    }
}

#[tokio::test]
async fn colliding_request_ids_reply_only_on_their_own_socket() {
    let listener = Listener::bind(&ListenUrl::Unix(socket_path("requests")))
        .await
        .unwrap();
    let url = listener.local_url().unwrap();
    let (sender, mut requests) = mpsc::channel(16);
    let served = tokio::spawn(serve_one_shot(listener, CHAIN_ID, sender));
    assert!(matches!(
        soon(BuilderClient::connect(&url, CHAIN_ID)).await,
        Err(builder_protocol::ConnectError::Handshake(
            builder_protocol::HandshakeError::ProtocolVersion {
                ours: PROTOCOL_VERSION,
                theirs: ONE_SHOT_PROTOCOL_VERSION,
            }
        ))
    ));
    let _idle = builder_protocol::connect(&url).await.unwrap();
    let mut clients = Vec::new();
    for block in [100, 101] {
        let mut client = tokio::time::timeout(
            Duration::from_secs(1),
            FyndJitSolverClient::connect(&url, CHAIN_ID),
        )
        .await
        .unwrap()
        .unwrap();
        client
            .send(BuilderEvent::Solve(solve_request(7, block)))
            .await
            .unwrap();
        clients.push(client);
    }
    let mut pending = [
        soon(requests.recv()).await.unwrap(),
        soon(requests.recv()).await.unwrap(),
    ];
    pending.reverse();
    for (event, reply) in pending {
        let BuilderEvent::Solve(request) = event else {
            unreachable!()
        };
        let frontrun = frontrun(100);
        let target_block = request.block.block_number;
        reply
            .try_send(SolveResponse {
                request_id: request.request_id,
                frontrun: (target_block == 100).then_some(Frontrun {
                    order_key: ORDER_KEY,
                    target_block,
                    tx: frontrun.tx,
                    expected_cost_wei: frontrun.cost_wei,
                    expected_gas: frontrun.expected_gas,
                }),
            })
            .unwrap();
    }
    let first = soon(clients[0].next_response()).await.unwrap().unwrap();
    let second = soon(clients[1].next_response()).await.unwrap().unwrap();
    assert_eq!(first.request_id, 7);
    assert_eq!(first.frontrun.unwrap().order_key, ORDER_KEY);
    assert_eq!(second.request_id, 7);
    assert!(second.frontrun.is_none());
    clients[0].included(ORDER_KEY).await.unwrap();
    let (event, _) = soon(requests.recv()).await.unwrap();
    assert!(matches!(event, BuilderEvent::Included { order_key } if order_key == ORDER_KEY));
    drop(requests);
    soon(served).await.unwrap();
    for client in &mut clients {
        assert!(matches!(
            soon(client.next_response()).await,
            None | Some(Err(_))
        ));
    }
}

/// A response to an earlier request nobody waits for any more is not taken as the answer.
#[tokio::test]
async fn a_solve_waits_for_the_response_carrying_its_own_request_id() {
    let listener = Listener::bind(&ListenUrl::Unix(socket_path("solve")))
        .await
        .unwrap();
    let url = listener.local_url().unwrap();
    let (sender, mut requests) = mpsc::channel(16);
    let _served = tokio::spawn(serve_one_shot(listener, CHAIN_ID, sender));
    let mut client = FyndJitSolverClient::connect(&url, CHAIN_ID).await.unwrap();

    let solver = tokio::spawn(async move {
        let (event, reply) = soon(requests.recv()).await.unwrap();
        let BuilderEvent::Solve(request) = event else {
            unreachable!()
        };
        for request_id in [request.request_id - 1, request.request_id] {
            reply
                .send(SolveResponse {
                    request_id,
                    frontrun: None,
                })
                .await
                .unwrap();
        }
        requests
    });
    let response = soon(client.solve(solve_request(8, 100)))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.request_id, 8);
    drop(soon(solver).await.unwrap());
}
