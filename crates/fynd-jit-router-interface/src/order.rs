//! The order key, as `FyndJITRouter.orderKey` derives it.

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::sol_types::SolValue;

use crate::abi::OrderFields;

/// `keccak256(abi.encode(sender, tokenIn, tokenOut, amountIn, expectedAmountOut,
/// minAmountOut, receiver, blockNumber))`
///
/// Every argument is taken verbatim from the user's router call — `receiver` included,
/// since the router rejects `address(0)` rather than reading it as the sender, and
/// `expected_amount_out` included, since the quote decides how much of a substituted
/// route's output reaches the receiver. The fields appear in the order `TychoRouterV3`'s
/// calldata carries them.
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors the Solidity orderKey argument list, which mirrors the router's calldata"
)]
pub fn order_key(
    sender: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    expected_amount_out: U256,
    min_amount_out: U256,
    receiver: Address,
    block_number: u64,
) -> B256 {
    keccak256(
        (
            sender,
            token_in,
            token_out,
            amount_in,
            expected_amount_out,
            min_amount_out,
            receiver,
            U256::from(block_number),
        )
            .abi_encode(),
    )
}

/// [`order_key`] for the order fields of a store in `block_number`: the key that
/// `FyndJITRouter.storeBatch` computes.
#[must_use]
pub fn order_key_of(order: &OrderFields, block_number: u64) -> B256 {
    order_key(
        order.sender,
        order.tokenIn,
        order.tokenOut,
        order.amountIn,
        order.expectedAmountOut,
        order.minAmountOut,
        order.receiver,
        block_number,
    )
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{address, b256, U256};

    use super::{order_key, order_key_of};
    use crate::abi::OrderFields;

    /// The key that `FyndJITRouter.orderKey` returns for these fields.
    #[test]
    fn order_key_of_matches_the_router() {
        let order = OrderFields {
            sender: address!("00000000000000000000000000000000000A11cE"),
            tokenIn: address!("4200000000000000000000000000000000000006"),
            tokenOut: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
            amountIn: U256::from(1_000_000_000_000_000_000u64),
            expectedAmountOut: U256::from(3_100_000_000u64),
            minAmountOut: U256::from(3_000_000_000u64),
            receiver: address!("00000000000000000000000000000000000B0b00"),
        };

        assert_eq!(
            order_key_of(&order, 123_456),
            b256!("3c4882a0b2a2e93c68e7770b6b8446dbc323c2ac40c128453cb8378a5e04b87f")
        );
    }

    #[test]
    fn order_key_is_stable_and_binds_every_field() {
        let sender = address!("1111111111111111111111111111111111111111");
        let token_in = address!("2222222222222222222222222222222222222222");
        let token_out = address!("3333333333333333333333333333333333333333");
        let receiver = address!("4444444444444444444444444444444444444444");
        let a = U256::from(10u64);
        let e = U256::from(10u64);
        let m = U256::from(9u64);
        let base = order_key(sender, token_in, token_out, a, e, m, receiver, 123);
        // every field perturbs the key
        assert_ne!(
            base,
            order_key(receiver, token_in, token_out, a, e, m, receiver, 123)
        );
        assert_ne!(
            base,
            order_key(sender, token_out, token_in, a, e, m, receiver, 123)
        );
        assert_ne!(
            base,
            order_key(
                sender,
                token_in,
                token_out,
                U256::from(11u64),
                e,
                m,
                receiver,
                123
            )
        );
        assert_ne!(
            base,
            order_key(
                sender,
                token_in,
                token_out,
                a,
                U256::from(11u64),
                m,
                receiver,
                123
            )
        );
        assert_ne!(
            base,
            order_key(
                sender,
                token_in,
                token_out,
                a,
                e,
                U256::from(8u64),
                receiver,
                123
            )
        );
        assert_ne!(
            base,
            order_key(sender, token_in, token_out, a, e, m, sender, 123)
        );
        assert_ne!(
            base,
            order_key(sender, token_in, token_out, a, e, m, receiver, 124)
        );
        // `expectedAmountOut` and `minAmountOut` are distinct positions, not a pair that
        // could be swapped without notice.
        assert_ne!(
            base,
            order_key(sender, token_in, token_out, a, m, e, receiver, 123)
        );
    }
}
