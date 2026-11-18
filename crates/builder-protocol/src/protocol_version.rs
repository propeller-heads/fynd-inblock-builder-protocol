//! Guards [`ONE_SHOT_PROTOCOL_VERSION`] against wire changes that forget to bump it.
//!
//! Each one-shot message type has fixed samples whose postcard encodings are recorded under
//! `tests/fixtures/`, together with the version they were recorded at. Any change to the
//! encoding fails this test until the version is bumped and the fixtures are re-recorded
//! with `UPDATE_FIXTURES=1 cargo test -p builder-protocol --lib protocol_version`.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "unwrapping is idiomatic in tests"
)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use alloy::hex;
use alloy::primitives::{address, Address, Bytes, Log, LogData, B256, U256};
use serde::Serialize;

use crate::solve::{SolverMessage, ONE_SHOT_PROTOCOL_VERSION};
use crate::Hello;

const VERSION_FILE: &str = "protocol_version";
const RECORD_COMMAND: &str =
    "UPDATE_FIXTURES=1 cargo test -p builder-protocol --lib protocol_version";

const SENDER: Address = address!("1111111111111111111111111111111111111111");
const ROUTER: Address = address!("2222222222222222222222222222222222222222");
const TOKEN: Address = address!("3333333333333333333333333333333333333333");

#[test]
fn the_wire_format_changes_only_with_the_protocol_version() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let current_encodings = current_encodings();
    let changed: Vec<&str> = current_encodings
        .iter()
        .filter(|(name, current_encoding)| {
            let recorded_encoding = std::fs::read_to_string(fixture_path(&dir, name)).ok();
            recorded_encoding.as_ref() != Some(current_encoding)
        })
        .map(|(name, _)| *name)
        .collect();
    let recorded_version = std::fs::read_to_string(dir.join(VERSION_FILE))
        .ok()
        .map(|version| {
            version
                .trim()
                .parse::<u32>()
                .expect("a recorded version number")
        });
    let bump_needed = !changed.is_empty() && recorded_version == Some(ONE_SHOT_PROTOCOL_VERSION);

    assert!(
        !bump_needed,
        "the wire format of {changed:?} changed but ONE_SHOT_PROTOCOL_VERSION is still \
         {ONE_SHOT_PROTOCOL_VERSION}: bump it in src/solve.rs, then re-record the fixtures \
         with `{RECORD_COMMAND}`"
    );

    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(&dir).unwrap();
        for (name, current_encoding) in &current_encodings {
            std::fs::write(fixture_path(&dir, name), current_encoding).unwrap();
        }
        std::fs::write(
            dir.join(VERSION_FILE),
            format!("{ONE_SHOT_PROTOCOL_VERSION}\n"),
        )
        .unwrap();
        return;
    }

    assert!(
        changed.is_empty() && recorded_version == Some(ONE_SHOT_PROTOCOL_VERSION),
        "the fixtures were recorded at version {}, ONE_SHOT_PROTOCOL_VERSION is \
         {ONE_SHOT_PROTOCOL_VERSION}: re-record them with `{RECORD_COMMAND}`",
        recorded_version.map_or_else(|| "none".to_owned(), |version| version.to_string())
    );
}

fn fixture_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.hex"))
}

fn current_encodings() -> [(&'static str, String); 6] {
    [
        (
            "hello",
            encode(&[Hello {
                protocol_version: 0x0102_0304,
                chain_id: 0x0506_0708_090a,
            }]),
        ),
        (
            "builder_event",
            encode(&[
                builder_types::BuilderEvent::Solve(solve_request()),
                builder_types::BuilderEvent::Included {
                    order_key: B256::repeat_byte(0x0C),
                },
            ]),
        ),
        ("solve_request", encode(&[solve_request()])),
        (
            "solve_response",
            encode(&[
                builder_types::SolveResponse {
                    request_id: 7,
                    frontrun: Some(frontrun()),
                },
                builder_types::SolveResponse {
                    request_id: 8,
                    frontrun: None,
                },
            ]),
        ),
        ("frontrun", encode(&[frontrun()])),
        (
            "solver_message",
            encode(&[SolverMessage::Solved(builder_types::SolveResponse {
                request_id: 9,
                frontrun: Some(frontrun()),
            })]),
        ),
    ]
}

/// One hex line per sample, encoded the way the codec encodes a frame body.
fn encode<T: Serialize>(samples: &[T]) -> String {
    samples.iter().fold(String::new(), |mut lines, sample| {
        let body = postcard::to_stdvec(sample).unwrap();
        writeln!(lines, "{}", hex::encode(body)).unwrap();
        lines
    })
}

fn solve_request() -> builder_types::SolveRequest {
    builder_types::SolveRequest {
        request_id: 0x0123_4567,
        block: builder_types::BlockEnv {
            block_number: 49_959_633,
            block_timestamp: 1_770_000_000,
            base_fee_per_gas: 5_000_000,
        },
        iteration: vec![
            builder_types::ExecutedTx {
                tx_hash: B256::repeat_byte(0x0A),
                from: SENDER,
                to: Some(TOKEN),
                logs: vec![log(0x11)],
                gas_used: 119_724,
                status: true,
                post_state: Some(post_state()),
            },
            builder_types::ExecutedTx {
                tx_hash: B256::repeat_byte(0x0B),
                from: TOKEN,
                to: None,
                logs: Vec::new(),
                gas_used: 21_000,
                status: false,
                post_state: None,
            },
        ],
        top_of_block_logs: vec![log(0x44)],
    }
}

fn frontrun() -> builder_types::Frontrun {
    builder_types::Frontrun {
        order_key: B256::repeat_byte(0x0C),
        target_block: 49_959_634,
        tx: builder_types::RawTx {
            to: Some(ROUTER),
            value: U256::from(5u64),
            data: Bytes::from_static(&[0x01, 0x02, 0x03]),
            gas_limit: 680_000,
            max_fee_per_gas: 6_000_000,
            max_priority_fee_per_gas: 1_000_000,
        },
        expected_cost_wei: U256::from(158_088_000_000u64),
        expected_gas: 26_348,
    }
}

fn log(byte: u8) -> Log {
    Log {
        address: ROUTER,
        data: LogData::new_unchecked(
            vec![B256::repeat_byte(byte)],
            Bytes::from_static(&[0x22, 0x33]),
        ),
    }
}

/// One entry per map: `HashMap` iteration order, and so its encoding, is not fixed for more.
fn post_state() -> builder_types::PostState {
    builder_types::PostState {
        accounts: HashMap::from([(
            SENDER,
            builder_types::AccountState {
                balance: U256::from(3u64),
                nonce: 4,
                code_hash: B256::repeat_byte(0x55),
                storage: HashMap::from([(U256::from(1u64), U256::from(2u64))]),
            },
        )]),
    }
}
