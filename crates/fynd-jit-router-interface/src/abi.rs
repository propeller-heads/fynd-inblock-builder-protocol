//! The items of `FyndJITRouter`'s ABI that a builder works with, generated from the contract's
//! ABI.
//!
//! `abi/FyndJITRouter.json` holds the subset of the ABI that this crate binds:
//!
//! - `storeBatch`, the frontrun, and every error it can revert with. [`FyndJITRouterErrors`]
//!   decodes the revert data of a failed frontrun.
//! - `dictionaryLength`, the `expectedFirstIndex` of the next `storeBatch` that appends.
//! - `ROUTE_CAPACITY`: two orders whose keys are equal modulo it share one route index.
//! - `BUILDER_ROLE` and `hasRole`, to check that an account may call `storeBatch`.
//! - `FyndOrderObserved`, the order a simulation reports while observations are on.
//! - `FyndSubstitutionSettled`, what a substitution paid, the coinbase tip included.

alloy::sol!(
    #[sol(all_derives)]
    FyndJITRouter,
    concat!(env!("CARGO_MANIFEST_DIR"), "/abi/FyndJITRouter.json")
);

pub use FyndJITRouter::{
    dictionaryLengthCall, hasRoleCall, storeBatchCall, AccessControlUnauthorizedAccount,
    BUILDER_ROLECall, BatchLengthMismatch, DictionaryFull, EntryLengthOutOfBounds,
    FyndJITRouterErrors, FyndJITRouterEvents, FyndOrderObserved, FyndSubstitutionSettled,
    InvalidEconomics, InvalidWordCount, OrderFields, ROUTE_CAPACITYCall, UnexpectedFirstIndex,
    WrongBlock,
};
