use std::collections::HashMap;

use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod solve;

pub use solve::{BuilderEvent, Frontrun, SolveRequest, SolveResponse};

/// Events emitted by the block builder for each building iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuildEvent {
    /// A new block-building iteration has started.
    IterationStart { uuid: Uuid, block: BlockEnv },
    /// A transaction was executed within this iteration.
    TxExecuted { uuid: Uuid, tx: ExecutedTx },
    /// The iteration completed successfully with final post-state.
    IterationComplete { uuid: Uuid, state: PostState },
    /// The iteration was aborted before completing.
    IterationAborted { uuid: Uuid },
}

/// Block-level environment for a building iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockEnv {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub base_fee_per_gas: u64,
}

/// A transaction executed within a builder iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutedTx {
    pub tx_hash: B256,
    pub from: Address,
    pub to: Option<Address>,
    pub logs: Vec<Log>,
    pub gas_used: u64,
    pub status: bool,
    /// State after this transaction, cumulative over the iteration so far.
    ///
    /// Having `PostState` at each transaction allows the solver to find an optimal position for
    /// the improved transaction. This functionality is not implemented yet. `PostState` after
    /// the one-before-last transaction (i.e. before the transaction containing the to-be-resolved
    /// order) is required.
    pub post_state: Option<PostState>,
}

/// Post-execution account state snapshot for a builder iteration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PostState {
    pub accounts: HashMap<Address, AccountState>,
}

/// Snapshot of a single account after execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountState {
    pub balance: U256,
    pub nonce: u64,
    pub code_hash: B256,
    pub storage: HashMap<U256, U256>,
}

/// An unsigned EIP-1559 transaction payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTx {
    pub to: Option<Address>,
    pub value: U256,
    pub data: Bytes,
    pub gas_limit: u64,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// A route-improvement transaction for the builder to place immediately before the user
/// transactions it covers.
///
/// This is not a profit opportunity for whoever signs it. The substituted call carries the
/// user's original `expectedAmountOut`, so `TychoRouterV3`'s fee calculator captures
/// everything the improved route returns above that quote — the improvement funds the
/// sponsorship rather than reaching either the user or the signer, who pays the gas. [`improvement_wei`](Self::improvement_wei) and
/// [`cost_wei`](Self::cost_wei) are both reported so the builder can verify the trade it is
/// being asked to make rather than take the solver's word for it.
///
/// Adjacency is a correctness requirement, not an optimization: the solver compared routes
/// against the state at this exact point in the block, and the stored route is keyed to
/// [`target_block`](Self::target_block) only.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitFrontrun {
    /// Correlation id of the builder iteration this was solved against.
    pub uuid: Uuid,
    /// The block the covered orders — and this transaction — must land in. A stored route
    /// is keyed to one block and is dead in every other.
    pub target_block: u64,
    /// `storeBatch` call to the `FyndJITRouter`: dictionary appends plus one route per
    /// covered order.
    pub tx: RawTx,
    /// Summed per-order output improvement, valued in ETH wei via a `tokenOut`→WETH quote
    /// from the same solve pass.
    ///
    /// What the router's fee receiver captures if this lands — not what the covered users gain,
    /// and not what the signer gains. Each substituted call carries the user's original
    /// `expectedAmountOut`, so everything above that quote goes to the fee receiver, which is
    /// what funds the sponsorship the signer is paying for. A builder deciding whether to sign
    /// is being asked to spend [`cost_wei`](Self::cost_wei) on someone else's revenue.
    pub improvement_wei: U256,
    /// Gas cost of this transaction at the target block's base fee — what the builder
    /// spends. Always below `improvement_wei` divided by the configured margin.
    pub cost_wei: U256,
    /// Estimated execution gas: intrinsic, calldata, route stores, dictionary appends.
    /// [`RawTx::gas_limit`] carries headroom on top.
    pub expected_gas: u64,
    /// `order_ref`s of the orders whose routes this transaction stores.
    pub covered: Vec<B256>,
}

/// Which `FyndJITRouter` entrypoint a swap was sent to.
///
/// The variant fixes how the `swaps` blob is laid out and, on a route-store miss, which
/// entrypoint the original route is forwarded to. Together with
/// [`JitOrderDetected::permit2`] it identifies one of the six entrypoints exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapKind {
    Single,
    Sequential,
    Split { n_tokens: U256 },
}

/// Client-fee parameters as they appeared on the original router call.
///
/// Carried verbatim so a substituted route can zero them (the fee is the client's, not
/// the builder's, to collect) while the miss path still forwards the original call
/// byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientFee {
    /// `uint32` since `TychoRouterV3` widened it — the value fits well inside `u16`, but the
    /// ABI does not, and the entrypoint selectors move with it.
    pub bps: u32,
    pub receiver: Address,
    pub max_contribution: U256,
    pub deadline: U256,
    pub signature: Bytes,
}

/// Order-intake events pushed by the block builder to a JIT route solver.
///
/// The builder is the single source of truth for order lifetime: an order is live in the
/// solver's book from [`Detected`](Self::Detected) until the matching
/// [`Closed`](Self::Closed) arrives.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JitOrderEvent {
    /// The builder saw a `FyndJITRouter` swap it wants improved. Boxed because a
    /// detection is an order of magnitude larger than a closure, and both travel the same
    /// channel.
    Detected(Box<JitOrderDetected>),
    /// A transaction the builder no longer holds, which ends every order it still carries.
    Closed(JitOrderClosed),
}

/// A `FyndJITRouter` swap the builder wants a better route for.
///
/// Detection is the builder's job — how the call was found (top-level tx, nested through
/// a multicall or relay) is deliberately not modelled here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitOrderDetected {
    /// Canonical identity of the order, derived by
    /// [`compute_order_ref`](JitOrderDetected::compute_order_ref) over the fields that
    /// make the swap what it is: the sender, its transaction nonce, both tokens,
    /// `amount_in`, `expected_amount_out`, `min_amount_out`, `receiver`, the entrypoint
    /// kind (including `n_tokens`) and a hash of the `swaps` blob.
    ///
    /// Not the transaction hash: a user who resends the same swap with a higher fee gets a
    /// new transaction hash but the same order, and the solver should keep working on it
    /// rather than treating it as new. The derivation therefore excludes
    ///
    /// - gas and fee-market fields — bumping them is exactly the case this is built for;
    /// - the Permit2 permit and its signature — wallets refresh deadlines and re-sign when
    ///   they replace a transaction, without changing what is being traded;
    /// - the client-fee params — commercial metadata carried on the call, not identity.
    ///
    /// The nonce is what keeps two otherwise identical swaps from one sender distinct, and
    /// what ties a ref to the single mempool slot a replacement can occupy. A replacement
    /// that changes the swap itself lands under a different ref, and the builder closes
    /// the old one.
    ///
    /// [`tx_hash`](Self::tx_hash) is excluded for the same reason, and the exclusion is
    /// load-bearing: a fee-bumped resend carries a new hash and has to keep its ref.
    pub order_ref: B256,
    /// `msg.sender` as `FyndJITRouter` sees it: the EOA on a direct call, the integrator's
    /// contract on a wrapped one.
    ///
    /// Binds into the order key the router computes, so a wrong value here produces a key
    /// that matches nothing — the route is stored, no swap has its key, and the original
    /// route is forwarded with no revert and no log. Only a builder's internal trace knows
    /// it; a transaction signature does not.
    pub sender: Address,
    /// The transaction the call was observed in, and what a close names.
    ///
    /// Closes name a transaction because that is what a builder tracks, so an order without
    /// this could never be closed. Deliberately outside
    /// [`compute_order_ref`](Self::compute_order_ref): a fee-bumped resend of the same swap
    /// arrives under a new hash and the same ref.
    pub tx_hash: B256,
    /// Nonce of the transaction carrying the swap. Part of the order's identity.
    pub nonce: u64,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    /// The quote the user was shown, as their calldata carries it.
    ///
    /// This is the baseline an improvement is measured against, and part of the order key.
    /// `TychoRouterV3` passes it through on a substitution, so output above it is captured by
    /// the router's fee receiver rather than reaching the user — which is what funds the
    /// builder's sponsorship of these transactions.
    ///
    /// Not `minAmountOut`, which is only the revert floor and sits far below the quote.
    pub expected_amount_out: U256,
    pub min_amount_out: U256,
    pub receiver: Address,
    /// Which entrypoint the swap was sent to, including `nTokens` for split routes.
    pub kind: SwapKind,
    /// The original encoded route, as passed to the entrypoint.
    pub swaps: Bytes,
    /// Whether the caller funded the swap through Permit2.
    pub permit2: bool,
    pub client_fee: ClientFee,
    /// The full original transaction, if the builder chose to attach it. Optional: the
    /// decoded fields above are everything the solver needs.
    pub raw_tx: Option<Bytes>,
}

/// Separates order refs from every other keccak hash in the system — notably the router's
/// own order key, which is also a `B256` derived from overlapping fields.
const ORDER_REF_DOMAIN: &[u8] = b"fynd.jit.order_ref.v1";

impl JitOrderDetected {
    /// The canonical [`order_ref`](Self::order_ref) for this order.
    ///
    /// Builders and the reference decoder both derive refs through here, so the same swap
    /// gets the same ref whoever detected it. See [`order_ref`](Self::order_ref) for which
    /// fields take part and why the rest are left out.
    #[must_use]
    pub fn compute_order_ref(&self) -> B256 {
        let (kind_tag, n_tokens) = match self.kind {
            SwapKind::Single => (0u64, U256::ZERO),
            SwapKind::Sequential => (1u64, U256::ZERO),
            SwapKind::Split { n_tokens } => (2u64, n_tokens),
        };
        let mut preimage = ORDER_REF_DOMAIN.to_vec();
        preimage.extend_from_slice(
            &(
                self.sender,
                self.nonce,
                self.token_in,
                self.token_out,
                self.amount_in,
                self.expected_amount_out,
                self.min_amount_out,
                self.receiver,
                kind_tag,
                n_tokens,
                keccak256(&self.swaps),
            )
                .abi_encode(),
        );
        keccak256(preimage)
    }

    /// Sets [`order_ref`](Self::order_ref) to its canonical value.
    ///
    /// The way to build a detection by hand: fill every other field, then let the ref
    /// follow from them.
    #[must_use]
    pub fn with_canonical_order_ref(mut self) -> Self {
        self.order_ref = self.compute_order_ref();
        self
    }
}

/// A transaction the builder no longer holds.
///
/// Names the transaction rather than the order, because that is what a builder tracks. The
/// translation is not one-to-one in either direction — one transaction can carry several
/// router calls, and one order can be carried by several transactions once it has been
/// fee-bumped — so working out which orders this ends is the receiver's job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitOrderClosed {
    pub tx_hash: B256,
    pub reason: CloseReason,
}

/// Why a transaction left the builder's view.
///
/// There is no `Replaced` variant: because [`JitOrderDetected::order_ref`] is stable
/// across a fee bump, a replacement for the same swap is a re-detection under the same ref
/// rather than a close. A replacement that changes the swap arrives under a different ref
/// and leaves the original to be closed as [`Dropped`](Self::Dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CloseReason {
    /// Landed in a block.
    Included,
    /// Left the mempool without being included.
    Dropped,
    /// Passed its deadline.
    Expired,
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, Address, Bytes, B256, U256};

    use super::{ClientFee, JitOrderDetected, SwapKind};

    const SENDER: Address = address!("1111111111111111111111111111111111111111");
    const TOKEN_IN: Address = address!("2222222222222222222222222222222222222222");
    const TOKEN_OUT: Address = address!("3333333333333333333333333333333333333333");
    const RECEIVER: Address = address!("4444444444444444444444444444444444444444");

    /// A detection with a canonical ref, as a builder would assemble it.
    fn order() -> JitOrderDetected {
        JitOrderDetected {
            order_ref: B256::ZERO,
            sender: SENDER,
            tx_hash: B256::repeat_byte(0x0A),
            nonce: 42,
            token_in: TOKEN_IN,
            token_out: TOKEN_OUT,
            amount_in: U256::from(1_000u64),
            expected_amount_out: U256::from(950u64),
            min_amount_out: U256::from(900u64),
            receiver: RECEIVER,
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

    #[test]
    fn with_canonical_order_ref_fills_the_field() {
        let order = order();

        assert_ne!(order.order_ref, B256::ZERO);
        assert_eq!(order.order_ref, order.compute_order_ref());
    }

    /// The property the whole intake design rests on. A fee-bumped resend is a different
    /// transaction carrying the same order, so a hash reaching the derivation would make
    /// every replacement look like a new order — and the solver would stop recognising the
    /// one it is already working on.
    #[test]
    fn the_transaction_hash_does_not_enter_the_order_ref() {
        let original = order();

        let mut resent = original.clone();
        resent.tx_hash = B256::repeat_byte(0x0B);

        assert_eq!(resent.compute_order_ref(), original.order_ref);
    }

    /// The reason the ref exists: a fee-bumped resend of the same swap is the same order.
    #[test]
    fn a_fee_bump_keeps_the_order_ref() {
        let original = order();

        let mut resent = original.clone();
        resent.tx_hash = B256::repeat_byte(0x0B);
        resent.raw_tx = Some(Bytes::from_static(&[0x02, 0xff]));

        assert_eq!(resent.compute_order_ref(), original.order_ref);
    }

    /// Wallets refresh the Permit2 deadline and re-sign when they replace a transaction.
    #[test]
    fn refreshed_permit2_and_client_fee_signatures_keep_the_order_ref() {
        let original = order();

        let mut resent = original.clone();
        resent.client_fee.deadline = U256::from(1_777_000_999u64);
        resent.client_fee.signature = Bytes::from_static(&[0xaa, 0xbb]);

        assert_eq!(resent.compute_order_ref(), original.order_ref);
    }

    #[test]
    fn every_identity_field_changes_the_order_ref() {
        let base = order();
        let base_ref = base.order_ref;

        let mut with_sender = base.clone();
        with_sender.sender = RECEIVER;
        let mut with_nonce = base.clone();
        with_nonce.nonce = 43;
        let mut with_token_in = base.clone();
        with_token_in.token_in = TOKEN_OUT;
        let mut with_token_out = base.clone();
        with_token_out.token_out = TOKEN_IN;
        let mut with_amount_in = base.clone();
        with_amount_in.amount_in = U256::from(1_001u64);
        let mut with_expected = base.clone();
        with_expected.expected_amount_out = U256::from(951u64);
        let mut with_min = base.clone();
        with_min.min_amount_out = U256::from(901u64);
        let mut with_receiver = base.clone();
        with_receiver.receiver = SENDER;
        let mut with_kind = base.clone();
        with_kind.kind = SwapKind::Single;
        let mut with_swaps = base.clone();
        with_swaps.swaps = Bytes::from_static(&[0xde, 0xae]);

        for perturbed in [
            with_sender,
            with_nonce,
            with_token_in,
            with_token_out,
            with_amount_in,
            with_expected,
            with_min,
            with_receiver,
            with_kind,
            with_swaps,
        ] {
            assert_ne!(perturbed.compute_order_ref(), base_ref);
        }
    }

    /// `Split { n_tokens: 0 }` must not collide with the tagless kinds.
    #[test]
    fn swap_kinds_do_not_collide_through_n_tokens() {
        let mut single = order();
        single.kind = SwapKind::Single;
        let mut sequential = order();
        sequential.kind = SwapKind::Sequential;
        let mut split_zero = order();
        split_zero.kind = SwapKind::Split {
            n_tokens: U256::ZERO,
        };
        let mut split_two = order();
        split_two.kind = SwapKind::Split {
            n_tokens: U256::from(2u64),
        };

        let refs = [
            single.compute_order_ref(),
            sequential.compute_order_ref(),
            split_zero.compute_order_ref(),
            split_two.compute_order_ref(),
        ];

        for (i, a) in refs.iter().enumerate() {
            for b in &refs[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// The blob is hashed, not concatenated, so a byte moving across the boundary between
    /// the blob and the field after it cannot produce the same preimage.
    #[test]
    fn swaps_blob_length_is_bound_into_the_order_ref() {
        let mut short = order();
        short.swaps = Bytes::from_static(&[0xde]);
        let mut long = order();
        long.swaps = Bytes::from_static(&[0xde, 0x00]);

        assert_ne!(short.compute_order_ref(), long.compute_order_ref());
    }
}
