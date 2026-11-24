//! Moves the dictionary entries of a `storeBatch` call to another first index.
//!
//! `storeBatch` appends its entries only at `expectedFirstIndex`: the dictionary length its
//! routes were compressed against. When another store in the same block appends entries
//! first, the call reverts with `UnexpectedFirstIndex(expected, actual)`. [`shift_store_batch`]
//! rewrites the call so that its entries go to `actual`. Each DICT op that references one of
//! its entries gets the new index. Every other bit of the routes stays as it was, so the routes
//! keep their length and their header.

use alloy::primitives::{B256, U256};

use crate::abi::storeBatchCall;
use crate::bitstream::{BitReader, BitWriter, BitstreamError};
use crate::route_format::{
    DICT_INDEX_BITS, HEADER_BITS, LIT_LEN_BITS, MAX_DICT_ENTRIES, MAX_OPS, OP_DICT, OP_END, OP_LIT,
    OP_TAG_BITS, VERSION,
};

/// Width of the version field, the first field of the header.
const VERSION_BITS: u32 = 3;
/// The most bits one read or write of the bit stream carries.
const MAX_BITS_PER_OP: usize = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShiftError {
    #[error("dictionary index {0} is past the dictionary's capacity")]
    FirstIndexTooLarge(U256),
    #[error("route version {0} is not the version this crate reads")]
    InvalidVersion(u64),
    #[error("invalid op tag {0:#b}")]
    InvalidOp(u64),
    #[error("the route has more ops than MAX_OPS")]
    TooManyOps,
    /// The route references an entry below its old first index, but at or above the new one.
    /// That entry is not on chain when the store runs.
    #[error("dictionary index {0} references an entry that is not on chain")]
    MissingEntry(u64),
    #[error("shifted dictionary index {0} does not fit in DICT_INDEX_BITS bits")]
    IndexTooLarge(u64),
    #[error(transparent)]
    Bitstream(#[from] BitstreamError),
}

/// Rewrites `call` so that its entries are appended at `first_index`. It sets
/// `expectedFirstIndex` to `first_index` and shifts every route with [`shift_route`].
///
/// A call without entries does not append, and the router ignores its `expectedFirstIndex`.
/// So it stays as it is.
///
/// # Errors
/// The errors of [`shift_route`] for any route, and [`ShiftError::FirstIndexTooLarge`] when
/// either first index is past [`MAX_DICT_ENTRIES`]. `call` stays as it was on an error.
pub fn shift_store_batch(call: &mut storeBatchCall, first_index: U256) -> Result<(), ShiftError> {
    if call.entries.is_empty() {
        return Ok(());
    }
    let expected = dictionary_index(call.expectedFirstIndex)?;
    let first = dictionary_index(first_index)?;
    let words = call
        .words
        .iter()
        .map(|route| shift_route(route, expected, first))
        .collect::<Result<Vec<_>, _>>()?;
    call.words = words;
    call.expectedFirstIndex = first_index;
    Ok(())
}

/// The route `words` with each DICT index at or above `expected_first_index` moved by
/// `first_index - expected_first_index`. Indices below both first indices stay. A disabling
/// route, whose first word is zero, stays as it is.
///
/// # Errors
/// - [`ShiftError::MissingEntry`] when an index is below `expected_first_index` but not below
///   `first_index`.
/// - [`ShiftError::IndexTooLarge`] when a shifted index does not fit in [`DICT_INDEX_BITS`].
/// - [`ShiftError::FirstIndexTooLarge`] when `first_index` is past [`MAX_DICT_ENTRIES`].
/// - [`ShiftError::InvalidVersion`], [`ShiftError::InvalidOp`], [`ShiftError::TooManyOps`] or
///   [`ShiftError::Bitstream`] when `words` is not a route of [`VERSION`].
pub fn shift_route(
    words: &[B256],
    expected_first_index: u64,
    first_index: u64,
) -> Result<Vec<B256>, ShiftError> {
    if words.first().is_none_or(B256::is_zero) {
        return Ok(words.to_vec());
    }
    dictionary_index(U256::from(first_index))?;

    let mut r = BitReader::new(words);
    let mut w = BitWriter::new();
    let version = r.read(VERSION_BITS)?;
    if version != VERSION {
        return Err(ShiftError::InvalidVersion(version));
    }
    w.push(version, VERSION_BITS)?;
    copy(&mut r, &mut w, HEADER_BITS - VERSION_BITS as usize)?;

    for _ in 0..MAX_OPS {
        let tag = r.read(OP_TAG_BITS)?;
        w.push(tag, OP_TAG_BITS)?;
        match tag {
            OP_DICT => {
                let index = r.read(DICT_INDEX_BITS)?;
                let shifted = shift_index(index, expected_first_index, first_index)?;
                w.push(shifted, DICT_INDEX_BITS)?;
            }
            OP_LIT => {
                let len = r.read(LIT_LEN_BITS)?;
                w.push(len, LIT_LEN_BITS)?;
                #[expect(clippy::cast_possible_truncation, reason = "a 6-bit field")]
                copy(&mut r, &mut w, (len as usize + 1) * 8)?;
            }
            OP_END => {
                // The padding after END is copied too, so the route keeps its word count.
                let padding = words.len() * 256 - r.position();
                copy(&mut r, &mut w, padding)?;
                return Ok(w.into_words());
            }
            other => return Err(ShiftError::InvalidOp(other)),
        }
    }
    Err(ShiftError::TooManyOps)
}

fn shift_index(index: u64, expected_first_index: u64, first_index: u64) -> Result<u64, ShiftError> {
    if index < expected_first_index {
        return if index < first_index {
            Ok(index)
        } else {
            Err(ShiftError::MissingEntry(index))
        };
    }
    let shifted = index - expected_first_index + first_index;
    if shifted >> DICT_INDEX_BITS != 0 {
        return Err(ShiftError::IndexTooLarge(shifted));
    }
    Ok(shifted)
}

fn dictionary_index(index: U256) -> Result<u64, ShiftError> {
    u64::try_from(index)
        .ok()
        .filter(|i| *i <= MAX_DICT_ENTRIES as u64)
        .ok_or(ShiftError::FirstIndexTooLarge(index))
}

fn copy(r: &mut BitReader<'_>, w: &mut BitWriter, n_bits: usize) -> Result<(), BitstreamError> {
    let mut left = n_bits;
    while left > 0 {
        let chunk = left.min(MAX_BITS_PER_OP);
        #[expect(clippy::cast_possible_truncation, reason = "at most 64")]
        let chunk_bits = chunk as u32;
        w.push(r.read(chunk_bits)?, chunk_bits)?;
        left -= chunk;
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "unwrap is idiomatic in tests")]
mod tests {
    use alloy::primitives::{Address, Bytes, B256, U256};

    use super::*;
    use crate::abi::OrderFields;
    use crate::route_format::FLOAT_BITS;

    #[derive(Debug, PartialEq, Eq)]
    enum Op {
        Dict(u64),
        Lit(Vec<u8>),
    }

    /// A route of `ops`. Its header is fixed: `shift_route` copies the header without reading
    /// its fields.
    fn route(ops: &[Op]) -> Vec<B256> {
        let mut w = unterminated(ops);
        w.push(OP_END, OP_TAG_BITS).unwrap();
        w.into_words()
    }

    /// The header and `ops` of a route, without END.
    fn unterminated(ops: &[Op]) -> BitWriter {
        let mut w = BitWriter::new();
        w.push(VERSION, VERSION_BITS).unwrap();
        w.push(0, 3).unwrap();
        w.push(1, 1).unwrap();
        w.push(5, 4).unwrap();
        w.push(0x12_3456_789A, FLOAT_BITS).unwrap();
        w.push(0x0F_EDCB_A987, FLOAT_BITS).unwrap();
        for op in ops {
            match op {
                Op::Dict(index) => {
                    w.push(OP_DICT, OP_TAG_BITS).unwrap();
                    w.push(*index, DICT_INDEX_BITS).unwrap();
                }
                Op::Lit(bytes) => {
                    w.push(OP_LIT, OP_TAG_BITS).unwrap();
                    w.push(bytes.len() as u64 - 1, LIT_LEN_BITS).unwrap();
                    w.push_bytes(bytes).unwrap();
                }
            }
        }
        w
    }

    fn ops(words: &[B256]) -> Vec<Op> {
        let mut r = BitReader::new(words);
        header_of(&mut r);
        let mut ops = Vec::new();
        loop {
            match r.read(OP_TAG_BITS).unwrap() {
                OP_DICT => ops.push(Op::Dict(r.read(DICT_INDEX_BITS).unwrap())),
                OP_LIT => {
                    let len = usize::try_from(r.read(LIT_LEN_BITS).unwrap()).unwrap() + 1;
                    ops.push(Op::Lit(r.read_bytes(len).unwrap()));
                }
                _ => return ops,
            }
        }
    }

    fn header(words: &[B256]) -> (u64, u64) {
        header_of(&mut BitReader::new(words))
    }

    fn header_of(r: &mut BitReader<'_>) -> (u64, u64) {
        let rest = u32::try_from(HEADER_BITS - 64).unwrap();
        (r.read(64).unwrap(), r.read(rest).unwrap())
    }

    #[test]
    fn indices_of_new_entries_move_and_older_ones_stay() {
        let words = route(&[
            Op::Dict(3),
            Op::Lit(vec![0xAA, 0xBB]),
            Op::Dict(8),
            Op::Dict(9),
        ]);

        let shifted = shift_route(&words, 8, 11).unwrap();

        assert_eq!(
            ops(&shifted),
            vec![
                Op::Dict(3),
                Op::Lit(vec![0xAA, 0xBB]),
                Op::Dict(11),
                Op::Dict(12)
            ]
        );
        assert_eq!(header(&shifted), header(&words));
        assert_eq!(shifted.len(), words.len());
    }

    #[test]
    fn a_lower_first_index_moves_new_entries_down() {
        let words = route(&[Op::Dict(3), Op::Dict(8)]);

        let shifted = shift_route(&words, 8, 6).unwrap();

        assert_eq!(ops(&shifted), vec![Op::Dict(3), Op::Dict(6)]);
    }

    #[test]
    fn an_entry_between_the_two_first_indices_is_not_on_chain() {
        let words = route(&[Op::Dict(7)]);

        assert_eq!(shift_route(&words, 8, 6), Err(ShiftError::MissingEntry(7)));
    }

    #[test]
    fn an_entry_at_a_lower_first_index_is_not_on_chain() {
        let words = route(&[Op::Dict(5), Op::Dict(6)]);

        assert_eq!(shift_route(&words, 8, 6), Err(ShiftError::MissingEntry(6)));
    }

    #[test]
    fn an_invalid_op_is_refused() {
        let mut w = unterminated(&[Op::Dict(8)]);
        w.push(0b11, OP_TAG_BITS).unwrap();

        assert_eq!(
            shift_route(&w.into_words(), 8, 11),
            Err(ShiftError::InvalidOp(0b11))
        );
    }

    #[test]
    fn a_route_without_end_is_refused() {
        let words = unterminated(&[Op::Lit(vec![0xFF; 20])]).into_words();

        assert!(matches!(
            shift_route(&words, 8, 11),
            Err(ShiftError::Bitstream(_))
        ));
    }

    #[test]
    fn a_route_has_at_most_max_ops_ops_with_end() {
        let longest: Vec<_> = (0..MAX_OPS - 1).map(|_| Op::Lit(vec![0xEE])).collect();
        let too_long: Vec<_> = (0..MAX_OPS).map(|_| Op::Lit(vec![0xEE])).collect();

        assert_eq!(ops(&shift_route(&route(&longest), 8, 11).unwrap()), longest);
        assert_eq!(
            shift_route(&route(&too_long), 8, 11),
            Err(ShiftError::TooManyOps)
        );
    }

    #[test]
    fn a_shifted_index_must_fit_its_field() {
        let max = MAX_DICT_ENTRIES as u64;
        let words = route(&[Op::Dict(8)]);

        assert_eq!(
            shift_route(&words, 8, max),
            Err(ShiftError::IndexTooLarge(max))
        );
        assert_eq!(
            shift_route(&words, 8, max + 1),
            Err(ShiftError::FirstIndexTooLarge(U256::from(max + 1)))
        );
    }

    #[test]
    fn a_disabling_route_stays() {
        let words = vec![B256::ZERO];

        assert_eq!(shift_route(&words, 8, 11).unwrap(), words);
    }

    #[test]
    fn a_route_of_another_version_is_refused() {
        let mut words = route(&[Op::Dict(8)]);
        words[0][0] |= 0b1110_0000;

        assert_eq!(
            shift_route(&words, 8, 11),
            Err(ShiftError::InvalidVersion(0b111))
        );
    }

    #[test]
    fn a_multi_word_route_keeps_its_word_count() {
        let literal = vec![0x5A; 20];
        let words = route(&[
            Op::Lit(literal.clone()),
            Op::Dict(8),
            Op::Lit(literal.clone()),
        ]);
        assert_eq!(words.len(), 2);

        let shifted = shift_route(&words, 8, 9).unwrap();

        assert_eq!(shifted.len(), 2);
        assert_eq!(
            ops(&shifted),
            vec![Op::Lit(literal.clone()), Op::Dict(9), Op::Lit(literal)]
        );
    }

    fn store_batch(entries: Vec<Bytes>, words: Vec<Vec<B256>>) -> storeBatchCall {
        let order = OrderFields {
            sender: Address::repeat_byte(1),
            tokenIn: Address::repeat_byte(2),
            tokenOut: Address::repeat_byte(3),
            amountIn: U256::from(1_000),
            expectedAmountOut: U256::from(900),
            minAmountOut: U256::from(800),
            receiver: Address::repeat_byte(1),
        };
        storeBatchCall {
            targetBlock: U256::from(100),
            entries,
            expectedFirstIndex: U256::from(8),
            orders: vec![order; words.len()],
            words,
        }
    }

    #[test]
    fn a_store_batch_moves_every_route_and_its_first_index() {
        let mut call = store_batch(
            vec![Bytes::from(vec![0xAB; 30])],
            vec![route(&[Op::Dict(8)]), route(&[Op::Dict(2), Op::Dict(8)])],
        );

        shift_store_batch(&mut call, U256::from(10)).unwrap();

        assert_eq!(call.expectedFirstIndex, U256::from(10));
        assert_eq!(ops(&call.words[0]), vec![Op::Dict(10)]);
        assert_eq!(ops(&call.words[1]), vec![Op::Dict(2), Op::Dict(10)]);
    }

    #[test]
    fn a_store_batch_without_entries_stays() {
        let words = vec![route(&[Op::Dict(8)])];
        let mut call = store_batch(vec![], words.clone());

        shift_store_batch(&mut call, U256::from(10)).unwrap();

        assert_eq!(call.expectedFirstIndex, U256::from(8));
        assert_eq!(call.words, words);
    }

    #[test]
    fn a_store_batch_refuses_a_first_index_past_the_dictionary() {
        let past_u64 = U256::from(u64::MAX) + U256::from(9);
        let mut call = store_batch(
            vec![Bytes::from(vec![0xAB; 30])],
            vec![route(&[Op::Dict(8)])],
        );

        assert_eq!(
            shift_store_batch(&mut call, past_u64),
            Err(ShiftError::FirstIndexTooLarge(past_u64))
        );

        let past_max = U256::from(MAX_DICT_ENTRIES + 1);
        call.expectedFirstIndex = past_max;
        assert_eq!(
            shift_store_batch(&mut call, U256::from(10)),
            Err(ShiftError::FirstIndexTooLarge(past_max))
        );
    }

    #[test]
    fn a_failed_shift_leaves_the_store_batch_as_it_was() {
        let words = vec![route(&[Op::Dict(8)]), route(&[Op::Dict(7)])];
        let mut call = store_batch(vec![Bytes::from(vec![0xAB; 30])], words.clone());

        assert_eq!(
            shift_store_batch(&mut call, U256::from(6)),
            Err(ShiftError::MissingEntry(7))
        );
        assert_eq!(call.expectedFirstIndex, U256::from(8));
        assert_eq!(call.words, words);
    }
}
