//! FIR prediction, coefficient computation, and Ptable construction for DST.
//!
//! DSD data is stored as packed bytes (MSB-first, 8 bits per byte).
//! For FIR purposes, individual bits are mapped to {-1, +1}:
//!   bit 0 => sample -1, bit 1 => sample +1.

use crate::codec::constants::{AC_HISMAX, AC_PROBS, AC_QSTEP, RESOL, SIZE_PREDCOEF};

/// Extract the `i`-th bit from packed MSB-first bytes (0 or 1).
#[inline]
pub fn get_bit(packed: &[u8], i: usize) -> u8 {
    (packed[i >> 3] >> (7 - (i & 7))) & 1
}

/// Set the `i`-th bit in packed MSB-first bytes.
#[inline]
pub fn set_bit(packed: &mut [u8], i: usize, val: u8) {
    let byte_idx = i >> 3;
    let bit_idx = 7 - (i & 7);
    if val != 0 {
        packed[byte_idx] |= 1 << bit_idx;
    } else {
        packed[byte_idx] &= !(1 << bit_idx);
    }
}

/// Coefficient range: 9-bit signed => [-256, 255].
const COEF_MIN: i16 = -(1 << (SIZE_PREDCOEF - 1));
const COEF_MAX: i16 = (1 << (SIZE_PREDCOEF - 1)) - 1;

/// Compute autocorrelation of a DSD bit sequence (packed MSB-first bytes).
///
/// `n_bits` is the total number of valid bits in `packed`.
/// Uses XOR + popcount for byte-aligned lags and per-bit extraction for
/// sub-byte offsets. r[k] = matches - mismatches = n - k - 2*xor_popcount.
pub fn autocorrelation(packed: &[u8], n_bits: usize, order: usize) -> Vec<f64> {
    let mut r = vec![0.0f64; order + 1];
    r[0] = n_bits as f64;

    for (k, r_k) in r.iter_mut().enumerate().skip(1) {
        if k >= n_bits {
            continue;
        }
        let count = n_bits - k;

        // How many bits differ between sequence at offset 0 and offset k?
        let byte_shift = k / 8;
        let bit_shift = k % 8;
        let mut xor_ones: u64 = 0;

        if bit_shift == 0 {
            // Aligned: direct byte XOR
            let n_bytes = count / 8;
            for j in 0..n_bytes {
                let x = packed[j] ^ packed[j + byte_shift];
                xor_ones += x.count_ones() as u64;
            }
            // Remaining bits
            let remaining_start = n_bytes * 8;
            for i in remaining_start..count {
                let a = get_bit(packed, i);
                let b = get_bit(packed, i + k);
                xor_ones += (a ^ b) as u64;
            }
        } else {
            // Non-aligned: shift adjacent bytes to form XOR input
            let inv_shift = 8 - bit_shift;
            let n_full_bytes = count / 8;
            for j in 0..n_full_bytes {
                let src_byte = j + byte_shift;
                // Reconstruct the byte at bit offset k starting from bit j*8
                let shifted = if src_byte + 1 < packed.len() {
                    (packed[src_byte] << bit_shift) | (packed[src_byte + 1] >> inv_shift)
                } else {
                    packed[src_byte] << bit_shift
                };
                let x = packed[j] ^ shifted;
                xor_ones += x.count_ones() as u64;
            }
            // Remaining bits
            let remaining_start = n_full_bytes * 8;
            for i in remaining_start..count {
                let a = get_bit(packed, i);
                let b = get_bit(packed, i + k);
                xor_ones += (a ^ b) as u64;
            }
        }

        // matches = count - xor_ones, mismatches = xor_ones
        // r[k] = matches - mismatches = count - 2*xor_ones
        *r_k = (count as i64 - 2 * xor_ones as i64) as f64;
    }
    r
}

/// Levinson-Durbin recursion to compute FIR prediction coefficients.
///
/// Uses double-buffering to avoid allocation per iteration.
pub fn levinson_durbin(r: &[f64], order: usize) -> Vec<f64> {
    if r[0] == 0.0 {
        return vec![0.0; order];
    }

    let mut a = vec![0.0f64; order];
    let mut a_tmp = vec![0.0f64; order];
    let mut e = r[0];

    for i in 0..order {
        // Reflection coefficient
        let mut acc = r[i + 1];
        for j in 0..i {
            acc += a[j] * r[i - j];
        }
        let k = -acc / e;

        // Update coefficients into a_tmp
        a_tmp[i] = k;
        for j in 0..i {
            a_tmp[j] = a[j] + k * a[i - 1 - j];
        }
        std::mem::swap(&mut a, &mut a_tmp);

        e *= 1.0 - k * k;
        if e <= 0.0 {
            break;
        }
    }

    a
}

/// Compute quantized FIR prediction coefficients for packed DSD bytes.
///
/// `packed` is MSB-first packed DSD data, `n_bits` is the number of valid bits.
/// Combines autocorrelation, Levinson-Durbin, negation, and quantization.
pub fn compute_fir_coefficients(packed: &[u8], n_bits: usize, order: usize) -> Vec<i16> {
    let r = autocorrelation(packed, n_bits, order);
    let coefs = levinson_durbin(&r, order);
    let scale = (1 << (SIZE_PREDCOEF - 1)) as f64; // 256
    // Negate + quantize in one pass: DST convention uses predict ≈ x[n] (not -x[n]).
    coefs
        .iter()
        .map(|&c| {
            let scaled = (-c * scale).round() as i16;
            scaled.clamp(COEF_MIN, COEF_MAX)
        })
        .collect()
}

/// Maximum number of LUT tables (pred_order up to 128, 128/8 = 16).
pub const MAX_LUT_TABLES: usize = 16;

/// Fixed-size filter LUT: `lut[table_nr][byte_value]`.
///
/// Uses `i16` matching FFmpeg's `int16_t filter[16][256]`.
/// Each entry is the dot product of 8 coefficients with the 8 bits of `byte_value`.
pub type FilterLut = [[i16; 256]; MAX_LUT_TABLES];

/// Build coefficient lookup table for 8-bit-at-a-time FIR evaluation.
///
/// Returns a fixed-size `FilterLut` where `lut[table_nr][byte_value]` gives
/// the partial prediction for that group of 8 coefficients.
pub fn build_lut(coefs: &[i16]) -> FilterLut {
    let order = coefs.len();
    let n_tables = order.div_ceil(RESOL);

    let mut lut = [[0i16; 256]; MAX_LUT_TABLES];

    for (t, lut_entry) in lut.iter_mut().enumerate().take(n_tables) {
        let group_start = t * RESOL;
        for byte_val in 0..256u32 {
            let mut total: i32 = 0;
            for bit_idx in 0..RESOL {
                let coef_idx = group_start + bit_idx;
                let c = if coef_idx < order {
                    coefs[coef_idx] as i32
                } else {
                    0
                };
                let bit = (byte_val >> bit_idx) & 1;
                let sample: i32 = if bit != 0 { 1 } else { -1 };
                total += sample * c;
            }
            lut_entry[byte_val as usize] = total as i16;
        }
    }

    lut
}

/// Status register stored as two u64 words for fast 128-bit shift.
///
/// `lo` holds status bytes [0..7], `hi` holds [8..15] (little-endian byte order
/// matching FFmpeg's `AV_WL64A` / `AV_RL64A` convention).
/// The LUT is indexed by individual bytes extracted from these words.
pub struct Status {
    pub lo: u64,
    pub hi: u64,
}

impl Status {
    /// Create a new status register initialized to 0xAA pattern (matching DST spec).
    pub fn new(n_bytes: usize) -> Self {
        // Fill used bytes with 0xAA, unused bytes with 0
        let mut lo: u64 = 0;
        let mut hi: u64 = 0;
        for i in 0..n_bytes.min(8) {
            lo |= 0xAAu64 << (i * 8);
        }
        for i in 8..n_bytes.min(16) {
            hi |= 0xAAu64 << ((i - 8) * 8);
        }
        Self { lo, hi }
    }

    /// Shift a new bit into the status register (128-bit left shift by 1).
    #[inline]
    pub fn update(&mut self, new_bit: u8) {
        self.hi = (self.hi << 1) | (self.lo >> 63);
        self.lo = (self.lo << 1) | (new_bit as u64 & 1);
    }

    /// Get the byte at index `i` (0 = lowest byte of `lo`).
    #[inline]
    pub fn byte_at(&self, i: usize) -> u8 {
        if i < 8 {
            (self.lo >> (i * 8)) as u8
        } else {
            (self.hi >> ((i - 8) * 8)) as u8
        }
    }
}

/// Compute FIR prediction value using the lookup table and Status register.
///
/// Matches FFmpeg's unrolled `F(0)+F(1)+...+F(15)` pattern.
#[inline(always)]
pub fn fir_predict(lut: &FilterLut, status: &Status) -> i32 {
    let lo = status.lo;
    let hi = status.hi;
    // Unrolled 16 table lookups, matching FFmpeg's macro expansion
    lut[0][(lo) as u8 as usize] as i32
        + lut[1][(lo >> 8) as u8 as usize] as i32
        + lut[2][(lo >> 16) as u8 as usize] as i32
        + lut[3][(lo >> 24) as u8 as usize] as i32
        + lut[4][(lo >> 32) as u8 as usize] as i32
        + lut[5][(lo >> 40) as u8 as usize] as i32
        + lut[6][(lo >> 48) as u8 as usize] as i32
        + lut[7][(lo >> 56) as u8 as usize] as i32
        + lut[8][(hi) as u8 as usize] as i32
        + lut[9][(hi >> 8) as u8 as usize] as i32
        + lut[10][(hi >> 16) as u8 as usize] as i32
        + lut[11][(hi >> 24) as u8 as usize] as i32
        + lut[12][(hi >> 32) as u8 as usize] as i32
        + lut[13][(hi >> 40) as u8 as usize] as i32
        + lut[14][(hi >> 48) as u8 as usize] as i32
        + lut[15][(hi >> 56) as u8 as usize] as i32
}

/// Run FIR prediction over packed DSD bytes, computing residuals.
///
/// `packed` is MSB-first packed DSD data, `n_bits` is the number of valid bits.
/// Returns (predictions, residuals).
pub fn predict_and_residual(packed: &[u8], n_bits: usize, coefs: &[i16]) -> (Vec<i32>, Vec<u8>) {
    let order = coefs.len();
    let n_tables = order.div_ceil(RESOL);

    let lut = build_lut(coefs);
    let mut status = Status::new(n_tables);

    let mut predictions = vec![0i32; n_bits];
    let mut residuals = vec![0u8; n_bits];

    for i in 0..n_bits {
        let predict = fir_predict(&lut, &status);
        predictions[i] = predict;

        let actual_bit = get_bit(packed, i);

        // Predicted bit: sign bit of predict (0 if >=0, 1 if <0)
        let predicted_bit = ((predict >> 15) & 1) as u8;
        residuals[i] = predicted_bit ^ actual_bit;

        status.update(actual_bit);
    }

    (predictions, residuals)
}

/// Build a probability table from prediction values and residual bits.
///
/// Maps |prediction| >> AC_QSTEP to the probability used by the AC.
pub fn build_ptable(predictions: &[i32], residuals: &[u8], max_len: usize) -> Vec<u8> {
    let mut count_zeros = vec![0i64; max_len];
    let mut count_total = vec![0i64; max_len];

    for (&pred, &res) in predictions.iter().zip(residuals.iter()) {
        let idx = ((pred.unsigned_abs() as usize) >> AC_QSTEP).min(max_len - 1);
        count_total[idx] += 1;
        if res == 0 {
            count_zeros[idx] += 1;
        }
    }

    // Find actual table length (trim trailing empty bins)
    let mut ptable_len = max_len;
    while ptable_len > 1 && count_total[ptable_len - 1] == 0 {
        ptable_len -= 1;
    }

    let mut ptable = vec![128u8; ptable_len];

    for idx in 0..ptable_len {
        if count_total[idx] > 0 {
            let p_val = (count_zeros[idx] * AC_PROBS as i64 / count_total[idx]) as i32;
            ptable[idx] = p_val.clamp(1, 255) as u8;
        }
    }

    ptable
}

/// Build a probability table using default max_len (AC_HISMAX = 64).
pub fn build_ptable_default(predictions: &[i32], residuals: &[u8]) -> Vec<u8> {
    build_ptable(predictions, residuals, AC_HISMAX)
}

/// Look up probability from Ptable given a prediction value.
#[inline]
pub fn ptable_lookup(ptable: &[u8], predict_value: i32) -> u8 {
    let mut idx = (predict_value.unsigned_abs() as usize) >> AC_QSTEP;
    if idx >= ptable.len() {
        idx = ptable.len() - 1;
    }
    ptable[idx]
}
