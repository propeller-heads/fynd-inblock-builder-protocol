//! The compressed route format that `FyndJITRouter.storeBatch` stores and the router decodes.
//!
//! A route is 1 to [`MAX_WORDS`] 32-byte words, read as one MSB-first bit stream. Its
//! [`HEADER_BITS`]-bit header comes first:
//!
//! ```text
//! [0..3)    version, equal to VERSION
//! [3..6)    wordCount - 1
//! [6..7)    strategy: 0 = sequentialSwap, 1 = splitSwap
//! [7..11)   nTokens, for splits only
//! [11..50)  baselineOut, float39, rounded up
//! [50..89)  ethPerTokenOut, float39, rounded down: wei per 1e18 units of tokenOut
//! ```
//!
//! Ops follow the header until END. Each op starts with an [`OP_TAG_BITS`]-bit tag:
//!
//! ```text
//! OP_DICT  + DICT_INDEX_BITS-bit dictionary index
//! OP_LIT   + LIT_LEN_BITS-bit (len - 1) + len bytes of payload
//! OP_END
//! ```
//!
//! A float39 field has a 7-bit exponent in its top bits and a [`FLOAT_MANTISSA_BITS`]-bit
//! mantissa in its low bits. Its value is `mantissa << exponent`.

/// The format version the router decodes. Routes carry it in the version field of the header.
pub const VERSION: u64 = 1;
/// Bits in the header: version, word count, strategy, nTokens and two float39 economics
/// fields. The first op starts here.
pub const HEADER_BITS: usize = 89;
/// Width of one float39 economics field: a 7-bit exponent above a 32-bit mantissa.
pub const FLOAT_BITS: u32 = 39;
/// Width of the mantissa of a float39 field.
pub const FLOAT_MANTISSA_BITS: u32 = 32;
/// Largest exponent of a float39 field. Every decoded value is below `2^128`.
pub const MAX_EXPONENT: u64 = 96;
/// The header's word-count field is 3 bits (up to 8), but routes are capped at 5
/// words: enough for a hop set plus one single-block-validity signature literal.
pub const MAX_WORDS: usize = 5;
/// Most ops in one route, END included.
pub const MAX_OPS: usize = 64;
/// Longest literal run of one LIT op, in bytes.
pub const MAX_LIT: usize = 64;
/// Longest dictionary entry, in bytes. 31 bytes = Solidity single-slot short-bytes layout; one
/// cold SSTORE per append.
pub const MAX_ENTRY_LEN: usize = 31;
/// Width of an op tag: [`OP_DICT`] | [`OP_LIT`] | [`OP_END`].
pub const OP_TAG_BITS: u32 = 2;
/// Width of the dictionary index carried by a DICT op.
pub const DICT_INDEX_BITS: u32 = 20;
/// Most entries the on-chain dictionary holds: one for each index a DICT op can carry. An
/// append past it reverts.
pub const MAX_DICT_ENTRIES: usize = 1 << DICT_INDEX_BITS;
/// Width of the length field carried by a LIT op. It holds `len - 1`, so a literal run
/// is 1..=[`MAX_LIT`] bytes.
pub const LIT_LEN_BITS: u32 = 6;

/// Tag of an op that copies one dictionary entry.
pub const OP_DICT: u64 = 0b00;
/// Tag of an op that copies literal bytes from the route.
pub const OP_LIT: u64 = 0b01;
/// Tag of the op that ends the route.
pub const OP_END: u64 = 0b10;
