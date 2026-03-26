//! Arithmetic encoder/decoder for DST.
//!
//! All bit operations use u32 wrapping arithmetic to match C unsigned 32-bit semantics.
//!
//! The encoder uses a streaming carry-propagation range coder that is the exact
//! inverse of the decoder. This is an independent implementation based on the
//! well-known range coding technique (G. N. N. Martin, 1979).

use crate::codec::constants::{ABITS, HALF, ONE, PBITS};

/// Compute ap = approximate (A * p / 256).
///
/// Formula: `((A >> PBITS) | ((A >> (PBITS-1)) & 1)) * p`
/// The `(A >> (PBITS-1)) & 1` term provides partial rounding.
#[inline(always)]
fn compute_ap(a: u32, p: u32) -> u32 {
    let a_factor = (a >> PBITS) | ((a >> (PBITS - 1)) & 1);
    a_factor.wrapping_mul(p)
}

/// Floor of log2 for a positive u32 (equivalent to `31 - leading_zeros`).
#[inline(always)]
fn log2_u32(v: u32) -> u32 {
    debug_assert!(v > 0);
    31 - v.leading_zeros()
}

/// Arithmetic encoder — streaming carry-propagation range coder.
///
/// Produces a bitstream that is decodable by `ACDecoder`.
/// Encodes symbols incrementally via `encode_bit()`, then finalizes with `flush()`.
///
/// Uses the pending-count technique to handle carry propagation in O(1) per bit.
/// A carry can only turn a 0-bit into a 1-bit (and cascade through trailing 1-bits
/// turning them to 0). We buffer one leading bit plus a count of subsequent 1-bits
/// that would flip on carry. When a new 0-bit arrives the buffered group is safe
/// from future carries and gets flushed.
pub struct ACEncoder {
    a: u32,
    c: u32,
    bits: Vec<u8>,
    /// The leading bit that might be flipped by a future carry.
    pending_bit: u8,
    /// How many consecutive 1-bits follow `pending_bit` (carry-vulnerable).
    pending_ones: u32,
    /// Whether we have a pending bit at all.
    has_pending: bool,
}

impl ACEncoder {
    pub fn new() -> Self {
        Self {
            a: ONE - 1,
            c: 0,
            bits: Vec::new(),
            pending_bit: 0,
            pending_ones: 0,
            has_pending: false,
        }
    }

    /// Commit the pending group to the output buffer.
    #[inline]
    fn flush_pending(&mut self) {
        if !self.has_pending {
            return;
        }
        self.bits.push(self.pending_bit);
        let n = self.pending_ones as usize;
        self.bits.extend(std::iter::repeat_n(1, n));
        self.has_pending = false;
        self.pending_ones = 0;
    }

    /// Emit one bit using pending-count bookkeeping.
    #[inline]
    fn emit_bit(&mut self, bit: u8) {
        if !self.has_pending {
            self.pending_bit = bit;
            self.pending_ones = 0;
            self.has_pending = true;
        } else if bit == 1 {
            // A 1-bit extends the carry-vulnerable chain.
            self.pending_ones += 1;
        } else {
            // bit == 0: the pending group is now safe from future carries — flush it.
            self.flush_pending();
            self.pending_bit = 0;
            self.pending_ones = 0;
            self.has_pending = true;
        }
    }

    /// Apply a carry into the pending group.
    ///
    /// - The leading `pending_bit` gets +1 (0→1, or 1→0 with further carry into `bits`).
    /// - All `pending_ones` trailing 1-bits become 0-bits.
    #[inline]
    fn apply_carry(&mut self) {
        if !self.has_pending {
            // Nothing pending — propagate directly into committed bits.
            let mut i = self.bits.len();
            while i > 0 {
                i -= 1;
                if self.bits[i] == 0 {
                    self.bits[i] = 1;
                    return;
                }
                self.bits[i] = 0;
            }
            self.bits.insert(0, 1);
            return;
        }

        // The trailing 1-bits all become 0 — output them now.
        let n = self.pending_ones as usize;
        if self.pending_bit == 0 {
            // 0 + carry → 1, trailing 1s → 0s.
            self.bits.push(1);
            self.bits.extend(std::iter::repeat_n(0, n));
        } else {
            // 1 + carry → 0 with carry propagating further back.
            // First propagate carry into already-committed bits.
            let mut i = self.bits.len();
            while i > 0 {
                i -= 1;
                if self.bits[i] == 0 {
                    self.bits[i] = 1;
                    // Carry absorbed, now emit: leader becomes 0, trailing 1s become 0s.
                    self.bits.push(0);
                    self.bits.extend(std::iter::repeat_n(0, n));
                    self.has_pending = false;
                    self.pending_ones = 0;
                    return;
                }
                self.bits[i] = 0;
            }
            // Carry overflowed past the beginning.
            self.bits.insert(0, 1);
            self.bits.push(0);
            self.bits.extend(std::iter::repeat_n(0, n));
        }
        self.has_pending = false;
        self.pending_ones = 0;
    }

    /// Shift out the top bit of C, handling carry via pending-count.
    #[inline]
    fn shift_out(&mut self) {
        if self.c >= ONE {
            self.c -= ONE;
            self.apply_carry();
        }
        let out_bit = ((self.c >> (ABITS - 1)) & 1) as u8;
        self.emit_bit(out_bit);
        self.c = (self.c << 1) & (ONE - 1);
    }

    /// Encode a single bit with probability p (of bit=1).
    pub fn encode_bit(&mut self, bit: u8, p: u32) {
        let ap = compute_ap(self.a, p);
        let h = self.a.wrapping_sub(ap);

        if bit == 1 {
            self.a = h;
        } else {
            self.c += h;
            self.a = ap;
        }

        while self.a < HALF {
            self.shift_out();
            self.a <<= 1;
        }
    }

    /// Finalize encoding and return the coded bitstream.
    pub fn flush(&mut self) -> Vec<u8> {
        for _ in 0..ABITS {
            self.shift_out();
        }
        self.flush_pending();
        std::mem::take(&mut self.bits)
    }
}

impl Default for ACEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Arithmetic decoder consuming coded bits to recover original bits.
///
/// Ported from FFmpeg's dstdec.c (LGPL 2.1, Peter Ross).
pub struct ACDecoder<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, 0 = MSB
    a: u32,
    c: u32,
}

impl<'a> ACDecoder<'a> {
    /// Initialize decoder from packed byte data starting at the given bit offset.
    pub fn new(data: &'a [u8], bit_offset: usize) -> Self {
        let mut decoder = Self {
            data,
            byte_pos: bit_offset / 8,
            bit_pos: (bit_offset % 8) as u8,
            a: ONE - 1, // 4095
            c: 0,
        };
        // Init C = first ABITS bits
        let mut c: u32 = 0;
        for _ in 0..ABITS {
            c = (c << 1) | decoder.next_bit() as u32;
        }
        decoder.c = c;
        decoder
    }

    /// Read next coded bit, returns 0 past end of stream.
    #[inline(always)]
    fn next_bit(&mut self) -> u8 {
        if self.byte_pos >= self.data.len() {
            return 0;
        }
        let bit = (self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.byte_pos += 1;
            self.bit_pos = 0;
        }
        bit
    }

    /// Read `n` coded bits at once (n <= 12), MSB first, returning them as a u32.
    #[inline(always)]
    fn read_bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 12);
        if self.byte_pos + 2 < self.data.len() {
            // Fast path: build 24-bit window from 3 bytes (enough for bit_pos + n <= 20)
            let window: u32 = (self.data[self.byte_pos] as u32) << 16
                | (self.data[self.byte_pos + 1] as u32) << 8
                | (self.data[self.byte_pos + 2] as u32);
            let shift = 24 - self.bit_pos as u32 - n;
            let result = (window >> shift) & ((1u32 << n) - 1);

            let new_bit_pos = self.bit_pos as u32 + n;
            self.byte_pos += (new_bit_pos / 8) as usize;
            self.bit_pos = (new_bit_pos % 8) as u8;
            result
        } else {
            // Near end of data, fall back to per-bit
            let mut v: u32 = 0;
            for _ in 0..n {
                v = (v << 1) | self.next_bit() as u32;
            }
            v
        }
    }

    /// Decode a single bit given probability p of bit=1.
    #[inline(always)]
    pub fn decode_bit(&mut self, p: u32) -> u8 {
        let ap = compute_ap(self.a, p);
        let h = self.a.wrapping_sub(ap);

        let bit;
        if self.c >= h {
            bit = 0;
            self.c = self.c.wrapping_sub(h);
            self.a = ap;
        } else {
            bit = 1;
            self.a = h;
        }

        // Renormalize: batch-shift when A < HALF (from FFmpeg dstdec.c, LGPL 2.1).
        if self.a < HALF {
            let n = (ABITS - 1) - log2_u32(self.a);
            self.a <<= n;
            self.c = (self.c << n) | self.read_bits(n);
        }

        bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pack individual bit values into MSB-first bytes (for test only).
    fn pack_bits(bits: &[u8]) -> Vec<u8> {
        let n_bytes = bits.len().div_ceil(8);
        let mut packed = vec![0u8; n_bytes];
        for (i, &bit) in bits.iter().enumerate() {
            if bit != 0 {
                packed[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        packed
    }

    /// Encode symbols, then decode, verify round-trip.
    fn roundtrip(symbols: &[(u8, u32)]) {
        let mut enc = ACEncoder::new();
        for &(bit, p) in symbols {
            enc.encode_bit(bit, p);
        }
        let coded = enc.flush();
        let packed = pack_bits(&coded);

        let mut dec = ACDecoder::new(&packed, 0);
        for (i, &(expected_bit, p)) in symbols.iter().enumerate() {
            let decoded_bit = dec.decode_bit(p);
            assert_eq!(
                decoded_bit, expected_bit,
                "mismatch at symbol {i}: expected {expected_bit}, got {decoded_bit}"
            );
        }
    }

    #[test]
    fn test_all_zeros_uniform() {
        // 10 zeros with p=128 (50/50)
        let symbols: Vec<(u8, u32)> = vec![(0, 128); 10];
        roundtrip(&symbols);
    }

    #[test]
    fn test_all_ones_uniform() {
        // 10 ones with p=128
        let symbols: Vec<(u8, u32)> = vec![(1, 128); 10];
        roundtrip(&symbols);
    }

    #[test]
    fn test_alternating() {
        let symbols: Vec<(u8, u32)> = (0..20).map(|i| ((i % 2) as u8, 128)).collect();
        roundtrip(&symbols);
    }

    #[test]
    fn test_skewed_probability() {
        // bit=0 with very low p (p=1 means bit=1 is very unlikely)
        let symbols: Vec<(u8, u32)> = vec![(0, 1); 50];
        roundtrip(&symbols);
    }

    #[test]
    fn test_skewed_high() {
        // bit=1 with high p (p=120 means bit=1 is likely)
        let symbols: Vec<(u8, u32)> = vec![(1, 120); 50];
        roundtrip(&symbols);
    }

    #[test]
    fn test_mixed_probabilities() {
        let symbols = vec![
            (0, 64),
            (1, 64),
            (0, 200),
            (1, 10),
            (0, 128),
            (1, 128),
            (0, 1),
            (1, 255),
            (0, 100),
            (1, 50),
        ];
        roundtrip(&symbols);
    }

    #[test]
    fn test_long_sequence() {
        // Simulate a realistic frame: 1000 symbols with varying probabilities
        let symbols: Vec<(u8, u32)> = (0..1000)
            .map(|i| {
                let p = (i % 127) as u32 + 1; // p in [1, 127]
                let bit = ((i * 7 + 3) % 2) as u8;
                (bit, p)
            })
            .collect();
        roundtrip(&symbols);
    }

    #[test]
    fn test_single_symbol() {
        roundtrip(&[(0, 128)]);
        roundtrip(&[(1, 128)]);
        roundtrip(&[(0, 1)]);
        roundtrip(&[(1, 1)]);
    }

    #[test]
    fn test_empty() {
        let mut enc = ACEncoder::new();
        let coded = enc.flush();
        // Should produce ABITS zero bits
        assert_eq!(coded.len(), ABITS as usize);
    }
}
