//! The parts of the `FyndJITRouter` contract that a block builder works with off chain.
//!
//! A frontrun is a `storeBatch` call. It stores one compressed route for each order it
//! covers, under the order key of that order.
//!
//! - [`abi`] decodes and encodes the `storeBatch` call, the `FyndOrderObserved` log and the
//!   `UnexpectedFirstIndex` error.
//! - [`observations`] turns the `FyndOrderObserved` log on in a simulation and reads it.
//! - [`order`] derives the order key that `storeBatch` stores a route under.
//! - [`route_format`] describes the compressed route that `storeBatch` stores.
//! - [`bitstream`] reads and writes the bit stream a route is written in.
//! - [`shift`] moves the dictionary entries of a `storeBatch` call to another first index,
//!   after it reverted with `UnexpectedFirstIndex`.

pub mod abi;
pub mod bitstream;
pub mod observations;
pub mod order;
pub mod route_format;
pub mod shift;
