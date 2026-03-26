//! Rice encoding and decoding for DST filter coefficients and Ptable entries.

use crate::codec::bitstream::{BitstreamReader, BitstreamWriter};
use crate::codec::constants::{
    CPRED_COEF, CPRED_ORDER, MAX_RICE_M_F, MAX_RICE_M_P, NROFFRICEMETHODS, SIZE_PREDCOEF,
    SIZE_RICEM, SIZE_RICEMETHOD, TableType,
};

/// Rice-encode a single signed integer.
pub fn rice_encode(writer: &mut BitstreamWriter, value: i32, m: usize) {
    let nr = value.unsigned_abs();
    let run_length = nr >> m;
    let lsbs = nr & ((1 << m) - 1);

    writer.write_unary(run_length);
    writer.write_bits(lsbs, m);

    if nr != 0 {
        writer.write_bit(if value < 0 { 1 } else { 0 });
    }
}

/// Rice-decode a single signed integer.
pub fn rice_decode(reader: &mut BitstreamReader, m: usize) -> i32 {
    let run_length = reader.read_unary();
    let lsbs = reader.read_bits(m);
    let nr = (run_length << m) + lsbs;

    if nr != 0 {
        let sign = reader.read_bit();
        if sign == 1 {
            return -(nr as i32);
        }
    }
    nr as i32
}

/// Compute Rice prediction for coefficient/entry at index.
fn predict_value(data: &[i32], index: usize, method: usize, table_type: TableType) -> i32 {
    let order = CPRED_ORDER[method];
    let coefs = &CPRED_COEF[table_type as usize][method];
    let mut x: i32 = 0;
    for tap in 0..order {
        if index > tap {
            x += coefs[tap] * data[index - tap - 1];
        }
    }
    x
}

/// Quantize prediction to integer (matching C decoder's rounding).
///
/// The C code does: if x >= 0: (x+4)/8 else: -((-x+3)/8)
fn quantize_prediction(x: i32) -> i32 {
    if x >= 0 { (x + 4) / 8 } else { -((-x + 3) / 8) }
}

/// Rice-encode a sequence of coefficients/entries with linear prediction.
///
/// First `CPRED_ORDER[method]` values are written directly (handled by caller).
/// This encodes the remaining values as Rice-coded residuals.
pub fn rice_encode_table(
    writer: &mut BitstreamWriter,
    data: &[i32],
    method: usize,
    m: usize,
    table_type: TableType,
) {
    let start = CPRED_ORDER[method];
    for i in start..data.len() {
        let x = predict_value(data, i, method, table_type);
        let predicted = quantize_prediction(x);
        // DST convention: stored = value + predicted
        let residual = data[i] + predicted;
        rice_encode(writer, residual, m);
    }
}

/// Rice-decode a sequence with linear prediction (appends to data).
pub fn rice_decode_table(
    reader: &mut BitstreamReader,
    data: &mut Vec<i32>,
    method: usize,
    m: usize,
    table_type: TableType,
    count: usize,
) {
    let start = CPRED_ORDER[method];
    for i in start..count {
        let x = predict_value(data, i, method, table_type);
        let predicted = quantize_prediction(x);
        let residual = rice_decode(reader, m);
        // DST convention: value = stored - predicted
        data.push(residual - predicted);
    }
}

/// Count bits needed to Rice-encode a single value.
fn count_rice_bits(value: i32, m: usize) -> usize {
    let nr = value.unsigned_abs() as usize;
    let run_length = nr >> m;
    // unary(run_length) = run_length + 1 bits
    // LSBs = m bits
    // sign = 1 bit if nr != 0, else 0
    run_length + 1 + m + if nr != 0 { 1 } else { 0 }
}

/// Select the best Rice method and m parameter for a data sequence.
///
/// Tries all method/m combinations and picks the one producing fewest bits.
/// Returns (best_method, best_m).
pub fn select_best_rice_method(data: &[i32], table_type: TableType) -> (usize, usize) {
    let max_m = match table_type {
        TableType::Filter => MAX_RICE_M_F,
        TableType::Ptable => MAX_RICE_M_P,
    };
    let mut best_method: usize = 0;
    let mut best_m: usize = 0;
    let mut best_bits: usize = usize::MAX;

    for (method, &start) in CPRED_ORDER.iter().enumerate().take(NROFFRICEMETHODS) {
        if start >= data.len() {
            continue;
        }

        for m in 0..=max_m {
            let mut total_bits = SIZE_RICEMETHOD + start * SIZE_PREDCOEF + SIZE_RICEM;

            for i in start..data.len() {
                let x = predict_value(data, i, method, table_type);
                let predicted = quantize_prediction(x);
                let residual = data[i] + predicted;
                total_bits += count_rice_bits(residual, m);
            }

            if total_bits < best_bits {
                best_bits = total_bits;
                best_method = method;
                best_m = m;
            }
        }
    }

    (best_method, best_m)
}
