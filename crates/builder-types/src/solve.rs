use alloy::primitives::{Log, B256, U256};
use serde::{Deserialize, Serialize};

use crate::{BlockEnv, ExecutedTx, RawTx};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuilderEvent {
    Solve(SolveRequest),
    /// The frontrun for `order_key` landed, so the storage slots it wrote are warm.
    Included {
        order_key: B256,
    },
}

/// One order to re-solve, at the end of a candidate iteration.
///
/// The builder simulates the order's transaction with `FyndJITRouter`'s observations enabled.
/// Each simulation emits a `FyndOrderObserved` log, which contains order details. The solver
/// reads the order from this log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolveRequest {
    pub request_id: u64,
    pub block: BlockEnv,
    /// The candidate iteration, in execution order. The order's own transaction is the last
    /// entry. The one before it must carry [`post_state`](ExecutedTx::post_state); when the
    /// order's transaction is the only entry, the solver quotes against the parent state.
    pub iteration: Vec<ExecutedTx>,
    /// Logs of the order's transaction simulated alone at the top of the block.
    pub top_of_block_logs: Vec<Log>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolveResponse {
    pub request_id: u64,
    pub frontrun: Option<Frontrun>,
}

/// A `storeBatch` transaction storing a re-solved route, with the economics in its header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontrun {
    pub order_key: B256,
    pub target_block: u64,
    pub tx: RawTx,
    pub expected_cost_wei: U256,
    pub expected_gas: u64,
}
