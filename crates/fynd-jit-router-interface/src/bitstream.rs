//! The MSB-first bit stream over 256-bit words that a stored route is written in.
//!
//! [`BitReader`] reads a route's header and ops, and [`BitWriter`] writes them. The widths
//! of the fields are in [`route_format`](crate::route_format).

use alloy::primitives::B256;

/// The stream carries at most 64 bits per read/write, matching `u64`.
const MAX_BITS_PER_OP: u32 = 64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BitstreamError {
    #[error("read past the end of the bit stream")]
    OutOfBits,
    #[error("{n_bits} bits requested, at most {MAX_BITS_PER_OP} fit in one operation")]
    WidthTooLarge { n_bits: u32 },
    #[error("value {value:#x} does not fit in {n_bits} bits")]
    ValueTooWide { value: u64, n_bits: u32 },
}

#[derive(Debug, Default)]
pub struct BitWriter {
    bits: Vec<bool>,
}

impl BitWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pushes the low `n_bits` of `value`, most significant bit first.
    ///
    /// # Errors
    /// [`BitstreamError::WidthTooLarge`] when `n_bits` exceeds 64,
    /// [`BitstreamError::ValueTooWide`] when `value` has bits set above `n_bits` —
    /// silently dropping them would emit a stream the decoder reads back differently.
    pub fn push(&mut self, value: u64, n_bits: u32) -> Result<(), BitstreamError> {
        if n_bits > MAX_BITS_PER_OP {
            return Err(BitstreamError::WidthTooLarge { n_bits });
        }
        if n_bits < MAX_BITS_PER_OP && value >> n_bits != 0 {
            return Err(BitstreamError::ValueTooWide { value, n_bits });
        }
        for i in (0..n_bits).rev() {
            self.bits.push((value >> i) & 1 == 1);
        }
        Ok(())
    }

    /// # Errors
    /// Propagates [`Self::push`], which cannot fail for byte-sized writes.
    pub fn push_bytes(&mut self, data: &[u8]) -> Result<(), BitstreamError> {
        for b in data {
            self.push(u64::from(*b), 8)?;
        }
        Ok(())
    }

    #[must_use]
    pub fn bit_len(&self) -> usize {
        self.bits.len()
    }

    /// Packs into words, zero-padding the tail. An empty writer yields one zero word.
    #[must_use]
    pub fn into_words(self) -> Vec<B256> {
        let n_words = self.bits.len().div_ceil(256).max(1);
        let mut words = vec![B256::ZERO; n_words];
        for (i, bit) in self.bits.iter().enumerate() {
            if *bit {
                words[i / 256][(i % 256) / 8] |= 1 << (7 - (i % 8));
            }
        }
        words
    }
}

#[derive(Debug)]
pub struct BitReader<'a> {
    words: &'a [B256],
    pos: usize,
}

impl<'a> BitReader<'a> {
    #[must_use]
    pub fn new(words: &'a [B256]) -> Self {
        Self { words, pos: 0 }
    }

    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Reads `n_bits` (<= 64) MSB-first.
    ///
    /// # Errors
    /// [`BitstreamError::WidthTooLarge`] when `n_bits` exceeds 64,
    /// [`BitstreamError::OutOfBits`] when fewer than `n_bits` remain.
    pub fn read(&mut self, n_bits: u32) -> Result<u64, BitstreamError> {
        if n_bits > MAX_BITS_PER_OP {
            return Err(BitstreamError::WidthTooLarge { n_bits });
        }
        if self.pos + n_bits as usize > self.words.len() * 256 {
            return Err(BitstreamError::OutOfBits);
        }
        let mut v: u64 = 0;
        for _ in 0..n_bits {
            let byte = self.words[self.pos / 256][(self.pos % 256) / 8];
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | u64::from(bit);
            self.pos += 1;
        }
        Ok(v)
    }

    /// Reads `len` whole bytes.
    ///
    /// # Errors
    /// [`BitstreamError::OutOfBits`] when fewer than `len * 8` bits remain.
    pub fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>, BitstreamError> {
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            #[expect(clippy::cast_possible_truncation, reason = "read(8) fits in u8")]
            out.push(self.read(8)? as u8);
        }
        Ok(out)
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "unwrap is idiomatic in tests")]
mod tests {
    use alloy::primitives::B256;
    use proptest::prelude::*;

    use super::{BitReader, BitWriter, BitstreamError};

    #[test]
    fn writer_produces_msb_first_words() {
        let mut w = BitWriter::new();
        w.push(0b001, 3).unwrap(); // version 1
        w.push(0b00, 2).unwrap(); // 1 word
        w.push(0b1, 1).unwrap(); // split
        w.push(0b0101, 4).unwrap(); // nTokens 5
        let words = w.into_words();
        assert_eq!(words.len(), 1);
        // bits: 001 00 1 0101 -> byte0 = 0b0010_0101 = 0x25, byte1 top 2 bits = 01
        assert_eq!(words[0][0], 0x25);
        assert_eq!(words[0][1] & 0xC0, 0x40);
    }

    #[test]
    fn reader_reads_across_word_boundary() {
        let mut w = BitWriter::new();
        w.push(0, 64).unwrap();
        w.push(0, 64).unwrap();
        w.push(0, 64).unwrap();
        w.push(0, 58).unwrap();
        w.push(0b1010_1010_1010, 12).unwrap(); // straddles the 256-bit boundary
        let words = w.into_words();
        assert_eq!(words.len(), 2);
        let mut r = BitReader::new(&words);
        for _ in 0..4 {
            r.read(62).unwrap();
        }
        r.read(2).unwrap();
        assert_eq!(r.read(12).unwrap(), 0b1010_1010_1010);
    }

    #[test]
    fn reader_errors_past_end() {
        let words = vec![B256::ZERO];
        let mut r = BitReader::new(&words);
        r.read(63).unwrap();
        r.read(64).unwrap();
        r.read(64).unwrap();
        r.read(64).unwrap();
        assert!(matches!(r.read(2), Err(BitstreamError::OutOfBits)));
    }

    #[test]
    fn writer_rejects_oversized_width_and_value() {
        let mut w = BitWriter::new();
        assert_eq!(
            w.push(0, 65),
            Err(BitstreamError::WidthTooLarge { n_bits: 65 })
        );
        assert_eq!(
            w.push(0b1_0000, 4),
            Err(BitstreamError::ValueTooWide {
                value: 0b1_0000,
                n_bits: 4
            })
        );
        // A rejected push leaves the stream untouched.
        assert_eq!(w.bit_len(), 0);
        w.push(u64::MAX, 64).unwrap();
        assert_eq!(w.bit_len(), 64);
    }

    #[test]
    fn reader_rejects_oversized_width() {
        let words = vec![B256::ZERO];
        let mut r = BitReader::new(&words);
        assert_eq!(
            r.read(65),
            Err(BitstreamError::WidthTooLarge { n_bits: 65 })
        );
        assert_eq!(r.position(), 0);
    }

    proptest! {
        #[test]
        fn roundtrip_arbitrary_pushes(values in prop::collection::vec((0u64..=u64::MAX, 1u32..=64), 1..200)) {
            let mut w = BitWriter::new();
            let mut expected = Vec::new();
            for (v, n) in &values {
                let v = if *n == 64 { *v } else { v & ((1u64 << n) - 1) };
                w.push(v, *n).unwrap();
                expected.push((v, *n));
            }
            let words = w.into_words();
            let mut r = BitReader::new(&words);
            for (v, n) in expected {
                prop_assert_eq!(r.read(n).unwrap(), v);
            }
        }

        #[test]
        fn roundtrip_bytes(data in prop::collection::vec(any::<u8>(), 0..300), offset in 0u32..17) {
            let mut w = BitWriter::new();
            w.push(0, offset).unwrap(); // misalign
            w.push_bytes(&data).unwrap();
            let words = w.into_words();
            let mut r = BitReader::new(&words);
            if offset > 0 {
                r.read(offset).unwrap();
            }
            prop_assert_eq!(r.read_bytes(data.len()).unwrap(), data);
        }
    }
}
