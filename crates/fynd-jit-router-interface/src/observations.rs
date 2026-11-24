//! The orders `FyndJITRouter` reports while its observations are on.
//!
//! For each swap, the router emits a `FyndOrderObserved` log. The log is gated: it is off on
//! chain, and a builder turns it on in its own simulations with [`enable_observations`].

use alloy::primitives::{Address, Log, B256, U256};
use alloy::rpc::types::state::StateOverride;
use alloy::sol_types::SolEvent;

use crate::abi::{FyndOrderObserved, OrderFields};

/// One order as `FyndJITRouter` saw it. The native form of a `FyndOrderObserved` log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedOrder {
    /// The router's key for this order in this block.
    pub order_key: B256,
    /// `msg.sender` as the router saw it.
    pub sender: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    /// Usually the quote the user was shown.
    pub expected_amount_out: U256,
    /// The user's floor.
    pub min_amount_out: U256,
    pub receiver: Address,
    /// What the receiver was paid.
    pub executed_amount_out: U256,
    /// What the route produced, before fees and surplus capture.
    pub gross_amount_out: U256,
    /// Whether a stored route ran instead of the user's own.
    pub substituted: bool,
    pub route_hash: B256,
    /// The route format version the router decodes. Compare it with
    /// [`route_format::VERSION`](crate::route_format::VERSION).
    pub route_version: u8,
}

impl From<FyndOrderObserved> for ObservedOrder {
    fn from(event: FyndOrderObserved) -> Self {
        Self {
            order_key: event.orderKey,
            sender: event.sender,
            token_in: event.tokenIn,
            token_out: event.tokenOut,
            amount_in: event.amountIn,
            expected_amount_out: event.expectedAmountOut,
            min_amount_out: event.minAmountOut,
            receiver: event.receiver,
            executed_amount_out: event.executedAmountOut,
            gross_amount_out: event.grossAmountOut,
            substituted: event.substituted,
            route_hash: event.routeHash,
            route_version: event.routeVersion,
        }
    }
}

impl ObservedOrder {
    /// The fields `FyndJITRouter.storeBatch` takes for this order. The
    /// contract hashes them with the block of the store, which gives `order_key` when the
    /// store is in the block of the observation.
    #[must_use]
    pub fn fields(&self) -> OrderFields {
        OrderFields {
            sender: self.sender,
            tokenIn: self.token_in,
            tokenOut: self.token_out,
            amountIn: self.amount_in,
            expectedAmountOut: self.expected_amount_out,
            minAmountOut: self.min_amount_out,
            receiver: self.receiver,
        }
    }

    /// The first order `router` reported in `logs`.
    ///
    /// Logs from any other emitter are skipped, so a contract that emits an event with the
    /// same signature cannot stand in for the router.
    pub fn first_in(logs: &[Log], router: Address) -> Option<Self> {
        logs.iter()
            .filter(|log| log.address == router)
            .find_map(|log| FyndOrderObserved::decode_log(log).ok())
            .map(|log| log.data.into())
    }
}

/// `FyndJITRouter.observationsEnabled`: a `bool` at byte 0 of storage slot 7. `paused` is
/// byte 1 of the same slot, so the word `1` written here also clears the pause in the
/// simulation. The contract's tests pin both.
pub const OBSERVATIONS_ENABLED_SLOT: B256 = B256::with_last_byte(7);

/// Turns `FyndJITRouter`'s observations on for `router` in `overrides`, so every swap through
/// it emits `FyndOrderObserved`. The gate is closed on chain, and a builder opens it only in
/// its own simulations.
///
/// Merged into whatever `overrides` already holds for the router. A full `state` gains the
/// slot, since adding a `state_diff` beside it is an error. Otherwise the slot goes into a
/// `state_diff`, so the router keeps the rest of its storage.
pub fn enable_observations(overrides: &mut StateOverride, router: Address) {
    let account = overrides.entry(router).or_default();
    match account.state.as_mut() {
        Some(state) => {
            state.insert(OBSERVATIONS_ENABLED_SLOT, one());
        }
        None => {
            account
                .state_diff
                .get_or_insert_default()
                .insert(OBSERVATIONS_ENABLED_SLOT, one());
        }
    }
}

fn one() -> B256 {
    B256::from(U256::from(1u64))
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "idiomatic in tests")]
mod tests {
    use alloy::primitives::{address, Address, Log, B256, U256};
    use alloy::rpc::types::state::AccountOverride;
    use alloy::sol_types::SolEvent;

    use super::*;

    const ROUTER: Address = address!("1111111111111111111111111111111111111111");

    fn event(order_key: B256) -> FyndOrderObserved {
        FyndOrderObserved {
            orderKey: order_key,
            sender: address!("2222222222222222222222222222222222222222"),
            tokenIn: address!("3333333333333333333333333333333333333333"),
            tokenOut: address!("4444444444444444444444444444444444444444"),
            amountIn: U256::from(1_000u64),
            expectedAmountOut: U256::from(950u64),
            minAmountOut: U256::from(900u64),
            receiver: address!("5555555555555555555555555555555555555555"),
            executedAmountOut: U256::from(960u64),
            grossAmountOut: U256::from(970u64),
            substituted: true,
            routeHash: B256::repeat_byte(0x77),
            routeVersion: 1,
        }
    }

    fn log(emitter: Address, event: &FyndOrderObserved) -> Log {
        Log {
            address: emitter,
            data: event.encode_log_data(),
        }
    }

    #[test]
    fn a_raw_log_decodes_to_the_observed_order() {
        let key = B256::repeat_byte(0xAA);

        let observed = ObservedOrder::first_in(&[log(ROUTER, &event(key))], ROUTER);

        assert_eq!(
            observed,
            Some(ObservedOrder {
                order_key: key,
                sender: address!("2222222222222222222222222222222222222222"),
                token_in: address!("3333333333333333333333333333333333333333"),
                token_out: address!("4444444444444444444444444444444444444444"),
                amount_in: U256::from(1_000u64),
                expected_amount_out: U256::from(950u64),
                min_amount_out: U256::from(900u64),
                receiver: address!("5555555555555555555555555555555555555555"),
                executed_amount_out: U256::from(960u64),
                gross_amount_out: U256::from(970u64),
                substituted: true,
                route_hash: B256::repeat_byte(0x77),
                route_version: 1,
            })
        );
    }

    #[test]
    fn the_store_fields_are_the_order_fields_of_the_log() {
        let observed: ObservedOrder = event(B256::repeat_byte(0xAA)).into();

        let fields = observed.fields();

        assert_eq!(fields.sender, observed.sender);
        assert_eq!(fields.tokenIn, observed.token_in);
        assert_eq!(fields.tokenOut, observed.token_out);
        assert_eq!(fields.amountIn, observed.amount_in);
        assert_eq!(fields.expectedAmountOut, observed.expected_amount_out);
        assert_eq!(fields.minAmountOut, observed.min_amount_out);
        assert_eq!(fields.receiver, observed.receiver);
    }

    #[test]
    fn the_first_of_two_logs_is_taken() {
        let first = B256::repeat_byte(0xAA);
        let second = B256::repeat_byte(0xBB);
        let logs = [log(ROUTER, &event(first)), log(ROUTER, &event(second))];

        let observed = ObservedOrder::first_in(&logs, ROUTER);

        assert_eq!(observed.map(|o| o.order_key), Some(first));
    }

    #[test]
    fn a_log_from_another_emitter_is_ignored() {
        let other = address!("9999999999999999999999999999999999999999");
        let logs = [
            log(other, &event(B256::repeat_byte(0xAA))),
            log(ROUTER, &event(B256::repeat_byte(0xBB))),
        ];

        assert_eq!(
            ObservedOrder::first_in(&logs, ROUTER).map(|o| o.order_key),
            Some(B256::repeat_byte(0xBB))
        );
        assert_eq!(ObservedOrder::first_in(&logs[..1], ROUTER), None);
    }

    #[test]
    fn logs_of_other_events_are_skipped() {
        let unrelated = Log::new_unchecked(ROUTER, vec![B256::repeat_byte(0x01)], vec![].into());

        let logs = [unrelated, log(ROUTER, &event(B256::repeat_byte(0xAA)))];

        assert_eq!(
            ObservedOrder::first_in(&logs, ROUTER).map(|o| o.order_key),
            Some(B256::repeat_byte(0xAA))
        );
    }

    #[test]
    fn observations_join_a_full_state_rather_than_sitting_beside_it() {
        let router = address!("1111111111111111111111111111111111111111");
        let kept = B256::with_last_byte(1);
        let mut overrides = StateOverride::default();
        overrides.insert(
            router,
            AccountOverride {
                state: Some(std::iter::once((kept, one())).collect()),
                ..Default::default()
            },
        );

        enable_observations(&mut overrides, router);

        let state = overrides[&router].state.as_ref().expect("a full state");
        assert_eq!(state[&OBSERVATIONS_ENABLED_SLOT], one());
        assert_eq!(state[&kept], one());
        assert!(overrides[&router].state_diff.is_none());
    }

    #[test]
    fn observations_on_an_untouched_router_override_only_the_gate() {
        let router = address!("1111111111111111111111111111111111111111");
        let mut overrides = StateOverride::default();

        enable_observations(&mut overrides, router);

        let account = &overrides[&router];
        assert_eq!(account.code, None);
        assert_eq!(account.balance, None);
        let diff = account.state_diff.as_ref().expect("a state diff");
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[&OBSERVATIONS_ENABLED_SLOT], one());
    }
}
