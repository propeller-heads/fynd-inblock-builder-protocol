//! What actually goes on the wire.
//!
//! Postcard is not self-describing: a value decodes against whatever the reader's types say,
//! with no field names or tags to disagree with. Every check here exists because the failure
//! it prevents is silent — a misparse into plausible numbers rather than an error.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unwrapping is idiomatic in tests"
)]

use std::collections::HashMap;

use alloy::primitives::{address, Address, Bytes, Log, LogData, B256, U256};
use builder_protocol::{
    AccountState, BlockEnv, BuildEvent, BuilderMessage, ClientFee, CloseReason, ExecutedTx,
    JitFrontrun, JitOrderClosed, JitOrderDetected, JitOrderEvent, MessageCodec, PostState, RawTx,
    SolverMessage, SwapKind, MAX_FRAME_LEN,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};
use uuid::Uuid;

const SENDER: Address = address!("1111111111111111111111111111111111111111");
const TOKEN_IN: Address = address!("2222222222222222222222222222222222222222");
const TOKEN_OUT: Address = address!("3333333333333333333333333333333333333333");
const TX_HASH: B256 = B256::repeat_byte(0x0A);

/// Encodes and decodes through the real codec, so the length prefix is exercised too.
///
/// The payload types do not derive `PartialEq`, so the round trip is asserted on the debug
/// rendering — which names every field, and is exactly what a shifted-by-one misparse would
/// change.
fn round_trip<T>(value: T) -> T
where
    T: Serialize + DeserializeOwned,
{
    let mut codec = MessageCodec::<T, T>::new();
    let mut buffer = BytesMut::new();
    codec.encode(value, &mut buffer).unwrap();
    codec.decode(&mut buffer).unwrap().expect("a whole frame")
}

fn assert_round_trips<T>(value: T)
where
    T: Serialize + DeserializeOwned + std::fmt::Debug,
{
    let before = format!("{value:?}");

    assert_eq!(format!("{:?}", round_trip(value)), before);
}

fn order() -> JitOrderDetected {
    JitOrderDetected {
        order_ref: B256::ZERO,
        sender: SENDER,
        tx_hash: TX_HASH,
        nonce: 42,
        token_in: TOKEN_IN,
        token_out: TOKEN_OUT,
        amount_in: U256::from(1_000_000_000_000_000_000u64),
        expected_amount_out: U256::from(950u64),
        min_amount_out: U256::from(900u64),
        receiver: SENDER,
        kind: SwapKind::Split {
            n_tokens: U256::from(3u64),
        },
        swaps: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        permit2: true,
        client_fee: ClientFee {
            bps: 25,
            receiver: TOKEN_OUT,
            max_contribution: U256::from(7u64),
            deadline: U256::from(1_777_000_000u64),
            signature: Bytes::from_static(&[0xc0, 0xde]),
        },
        raw_tx: Some(Bytes::from_static(&[0x02, 0xff])),
    }
    .with_canonical_order_ref()
}

fn frontrun() -> JitFrontrun {
    JitFrontrun {
        uuid: Uuid::from_u128(0x1234_5678_9abc_def0_1234_5678_9abc_def0),
        target_block: 49_959_633,
        tx: RawTx {
            to: Some(TOKEN_IN),
            value: U256::ZERO,
            data: Bytes::from_static(&[0x01, 0x02, 0x03]),
            gas_limit: 680_000,
            max_fee_per_gas: 6_000_000,
            max_priority_fee_per_gas: 1_000_000,
        },
        improvement_wei: U256::from(10_000_000_000_000_000u64),
        cost_wei: U256::from(158_088_000_000u64),
        expected_gas: 26_348,
        covered: vec![B256::repeat_byte(0xAA), B256::repeat_byte(0xBB)],
    }
}

/// One executed transaction with a log, since `alloy::Log` is the one payload field whose
/// serde impl has a human-readable form that postcard must not take.
fn executed_tx() -> ExecutedTx {
    ExecutedTx {
        tx_hash: TX_HASH,
        from: SENDER,
        to: Some(TOKEN_OUT),
        logs: vec![Log {
            address: TOKEN_IN,
            data: LogData::new_unchecked(
                vec![B256::repeat_byte(0x11)],
                Bytes::from_static(&[0x22, 0x33]),
            ),
        }],
        gas_used: 119_724,
        status: true,
        post_state: Some(post_state()),
    }
}

fn post_state() -> PostState {
    let mut storage = HashMap::new();
    storage.insert(U256::from(1u64), U256::from(2u64));
    let mut accounts = HashMap::new();
    accounts.insert(
        SENDER,
        AccountState {
            balance: U256::from(3u64),
            nonce: 4,
            code_hash: B256::repeat_byte(0x55),
            storage,
        },
    );
    PostState { accounts }
}

#[test]
fn every_builder_message_round_trips() {
    let uuid = Uuid::from_u128(1);
    let mut second = order();
    second.nonce = 43;
    for message in [
        BuilderMessage::Orders(vec![order()]),
        // Two orders in one batch, sharing a transaction: the batching is what a `Vec` on
        // the wire has to survive, and the shared hash is the case it exists for.
        BuilderMessage::Orders(vec![order(), second]),
        BuilderMessage::Orders(Vec::new()),
        BuilderMessage::TransactionClosed {
            tx_hash: TX_HASH,
            reason: CloseReason::Included,
        },
        BuilderMessage::TransactionClosed {
            tx_hash: TX_HASH,
            reason: CloseReason::Dropped,
        },
        BuilderMessage::TransactionClosed {
            tx_hash: TX_HASH,
            reason: CloseReason::Expired,
        },
        BuilderMessage::Build(BuildEvent::IterationStart {
            uuid,
            block: BlockEnv {
                block_number: 49_959_633,
                block_timestamp: 1_770_000_000,
                base_fee_per_gas: 5_000_000,
            },
        }),
        BuilderMessage::Build(BuildEvent::TxExecuted {
            uuid,
            tx: executed_tx(),
        }),
        BuilderMessage::Build(BuildEvent::IterationComplete {
            uuid,
            state: post_state(),
        }),
        BuilderMessage::Build(BuildEvent::IterationAborted { uuid }),
    ] {
        assert_round_trips(message);
    }
}

#[test]
fn every_solver_message_round_trips() {
    assert_round_trips(SolverMessage::Frontrun(frontrun()));
}

/// The payload types travel inside the messages above, but they are also what a builder
/// hands its own code, so their round trip is asserted on its own terms.
#[test]
fn every_order_event_round_trips() {
    assert_round_trips(JitOrderEvent::Detected(Box::new(order())));
    assert_round_trips(JitOrderEvent::Closed(JitOrderClosed {
        tx_hash: TX_HASH,
        reason: CloseReason::Dropped,
    }));
}

/// `alloy::Log` serializes flattened for JSON and unflattened for a binary format. Postcard
/// has to get the second one, and a value that took the first would be unreadable rather
/// than wrong — so this pins the property directly rather than trusting the round trip above
/// to have exercised it.
#[test]
fn logs_survive_the_binary_form_of_their_serde_impl() {
    let tx = round_trip(executed_tx());

    assert_eq!(tx.logs.len(), 1);
    assert_eq!(tx.logs[0].address, TOKEN_IN);
    assert_eq!(tx.logs[0].topics(), [B256::repeat_byte(0x11)]);
    assert_eq!(tx.logs[0].data.data, Bytes::from_static(&[0x22, 0x33]));
}

#[test]
fn a_length_above_the_limit_is_refused_without_allocating_for_it() {
    let claimed = MAX_FRAME_LEN + 1;
    let mut buffer = BytesMut::from(&u32::try_from(claimed).unwrap().to_be_bytes()[..]);
    let capacity_before = buffer.capacity();

    let error = MessageCodec::<BuilderMessage, BuilderMessage>::new()
        .decode(&mut buffer)
        .unwrap_err();

    assert!(
        format!("{error}").contains(&claimed.to_string()),
        "the error should name the claimed length: {error}"
    );
    assert_eq!(buffer.capacity(), capacity_before);
}

#[test]
fn a_frame_at_the_limit_is_accepted() {
    let mut codec = MessageCodec::<BuilderMessage, BuilderMessage>::new();
    let mut buffer = BytesMut::new();
    // An order whose route blob lands the body just under the cap once postcard's length
    // prefixes are on it: the guard is on the frame, not on the payload.
    let swaps = Bytes::from(vec![0x11u8; MAX_FRAME_LEN - 512]);
    let mut order = order();
    order.swaps = swaps.clone();
    codec
        .encode(BuilderMessage::Orders(vec![order]), &mut buffer)
        .unwrap();

    let decoded = codec.decode(&mut buffer).unwrap().expect("a whole frame");

    assert!(
        matches!(decoded, BuilderMessage::Orders(orders) if orders[0].swaps == swaps),
        "a frame just under the cap should survive"
    );
}

#[test]
fn a_partial_frame_yields_nothing_and_completes_later() {
    let mut codec = MessageCodec::<BuilderMessage, BuilderMessage>::new();
    let mut whole = BytesMut::new();
    codec
        .encode(
            BuilderMessage::TransactionClosed {
                tx_hash: TX_HASH,
                reason: CloseReason::Included,
            },
            &mut whole,
        )
        .unwrap();

    let mut buffer = whole.split_to(whole.len() - 1);
    assert!(codec.decode(&mut buffer).unwrap().is_none());

    buffer.extend_from_slice(&whole);
    assert!(codec.decode(&mut buffer).unwrap().is_some());
}

#[test]
fn frames_in_one_buffer_decode_in_order() {
    let mut codec = MessageCodec::<BuilderMessage, BuilderMessage>::new();
    let mut buffer = BytesMut::new();
    for byte in [0xAAu8, 0xBB] {
        codec
            .encode(
                BuilderMessage::TransactionClosed {
                    tx_hash: B256::repeat_byte(byte),
                    reason: CloseReason::Dropped,
                },
                &mut buffer,
            )
            .unwrap();
    }

    let mut seen = Vec::new();
    while let Some(BuilderMessage::TransactionClosed { tx_hash, .. }) =
        codec.decode(&mut buffer).unwrap()
    {
        seen.push(tx_hash[0]);
    }

    assert_eq!(seen, [0xAA, 0xBB]);
    assert!(buffer.is_empty());
}

/// Postcard cannot deserialize `#[serde(flatten)]` or an untagged enum — both need
/// `deserialize_any`, which a non-self-describing format does not have. Adding either to a
/// payload type would break the wire at run time rather than at compile time, so the source
/// is checked directly: a type that no message here happens to carry would slip past a
/// round-trip test.
#[test]
fn no_builder_types_payload_carries_flatten_or_an_untagged_enum() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../builder-types/src/lib.rs"),
    )
    .expect("builder-types sits beside this crate");

    for forbidden in ["flatten", "untagged"] {
        let offender = source
            .lines()
            .find(|line| line.contains("serde(") && line.contains(forbidden));

        assert!(
            offender.is_none(),
            "builder-types uses serde({forbidden}), which postcard cannot carry: {offender:?}"
        );
    }
}
