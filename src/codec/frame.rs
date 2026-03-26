//! DST frame encoder/decoder — packs FIR, Ptable, and AC data into a DST frame.
//!
//! Simplified: 1 segment per channel, per-channel filters/Ptables,
//! HalfProb=off, PSameSegAsF=1, PSameMapAsF=1.

use crate::error::{CladstError, Result};

use crate::codec::arithmetic::{ACDecoder, ACEncoder};
use crate::codec::bitstream::{BitstreamReader, BitstreamWriter};
use crate::codec::constants::{
    AC_BITS, AC_HISBITS, CPRED_ORDER, RESOL, SIZE_CODEDPREDORDER, SIZE_PREDCOEF, SIZE_RICEM,
    SIZE_RICEMETHOD, TableType,
};
use crate::codec::fir::{
    FilterLut, Status, build_lut, build_ptable_default, compute_fir_coefficients, fir_predict,
    predict_and_residual, ptable_lookup,
};
use crate::codec::rice::{rice_decode_table, rice_encode_table, select_best_rice_method};

/// Compute ceil(log2(x+1)), matching C decoder's Log2RoundUp.
fn log2_round_up(x: usize) -> usize {
    let mut y = 0;
    while x >= (1 << y) {
        y += 1;
    }
    y
}

/// Compute probability for the dst_x_bit symbol.
///
/// Matches ffmpeg's prob_dst_x_bit: (ff_reverse[c & 127] >> 1) + 1.
fn prob_dst_x_bit(coef: i16) -> u32 {
    let c = (coef as i32) & 127;
    // Reverse all 8 bits of the byte
    let mut r: u32 = 0;
    for i in 0..8 {
        r = (r << 1) | (((c as u32) >> i) & 1);
    }
    (r >> 1) + 1
}

/// Write simplified segment data (1 segment per channel, all same).
fn write_segment_data(writer: &mut BitstreamWriter, _n_channels: usize) {
    // PSameSegAsF = 1
    writer.write_bit(1);
    // Filter segments: SameSegAllCh = 1
    writer.write_bit(1);
    // EndOfChannel = 1 immediately (only 1 segment covering entire frame)
    writer.write_bit(1);
}

/// Write mapping data for filters and Ptables.
///
/// When `share_filters` is true, all channels share filter 0 (SameMapAllCh=1).
/// When false, each channel gets its own filter (SameMapAllCh=0).
fn write_mapping_data(
    writer: &mut BitstreamWriter,
    n_channels: usize,
    share_filters: bool,
    half_prob: bool,
) {
    // PSameMapAsF = 1
    writer.write_bit(1);

    if share_filters {
        // SameMapAllCh = 1: all channels share filter 0
        writer.write_bit(1);
    } else {
        // SameMapAllCh = 0: each channel uses own filter
        writer.write_bit(0);

        // Table assignments: channel 0 always gets table 0 (implicit).
        let mut count_tables: usize = 1;
        for ch in 0..n_channels {
            if ch == 0 {
                continue;
            }
            let n_bits = log2_round_up(count_tables);
            writer.write_bits(count_tables as u32, n_bits); // new table
            count_tables += 1;
        }
    }

    // HalfProb: per channel (ffmpeg reads `channels` bits here)
    for _ in 0..n_channels {
        writer.write_bit(u8::from(half_prob));
    }
}

/// Write one set of filter coefficients (Rice coded if beneficial).
fn write_filter_coef_set(writer: &mut BitstreamWriter, coefs: &[i16], pred_order: usize) {
    // PredOrder - 1 (7 bits)
    writer.write_bits((pred_order - 1) as u32, SIZE_CODEDPREDORDER);

    let coef_list: Vec<i32> = coefs[..pred_order].iter().map(|&c| c as i32).collect();

    // Uncoded cost: 1 (coded flag) + pred_order * 9 bits
    let uncoded_bits = 1 + pred_order * SIZE_PREDCOEF;

    // Try Rice coding
    let (best_method, best_m) = select_best_rice_method(&coef_list, TableType::Filter);
    let cpred_order = CPRED_ORDER[best_method];

    if cpred_order >= pred_order {
        // Cannot Rice-code: not enough values for prediction
        writer.write_bit(0); // Coded = 0
        for &c in &coef_list {
            writer.write_bits_signed(c, SIZE_PREDCOEF);
        }
        return;
    }

    // Estimate Rice coded bits
    let coded_overhead = 1 + SIZE_RICEMETHOD + cpred_order * SIZE_PREDCOEF + SIZE_RICEM;
    let mut trial = BitstreamWriter::new();
    rice_encode_table(
        &mut trial,
        &coef_list,
        best_method,
        best_m,
        TableType::Filter,
    );
    let coded_bits = coded_overhead + trial.bit_position();

    if coded_bits < uncoded_bits {
        // Rice coded
        writer.write_bit(1); // Coded = 1
        writer.write_bits(best_method as u32, SIZE_RICEMETHOD);
        for &c in &coef_list[..cpred_order] {
            writer.write_bits_signed(c, SIZE_PREDCOEF);
        }
        writer.write_bits(best_m as u32, SIZE_RICEM);
        rice_encode_table(writer, &coef_list, best_method, best_m, TableType::Filter);
    } else {
        // Uncoded
        writer.write_bit(0); // Coded = 0
        for &c in &coef_list {
            writer.write_bits_signed(c, SIZE_PREDCOEF);
        }
    }
}

/// Write one probability table (Rice coded if beneficial).
///
/// Ptable entries are in [1, 128]. Stored as (value - 1) in 7 bits
/// for uncoded entries, or directly for Rice-coded entries.
fn write_probability_table(writer: &mut BitstreamWriter, ptable: &[u8]) {
    let ptable_len = ptable.len();

    // PtableLen - 1 (6 bits)
    writer.write_bits((ptable_len - 1) as u32, AC_HISBITS);

    if ptable_len == 1 {
        // Single entry defaults to 128 (0.5 probability) in decoder
        return;
    }

    let entry_list: Vec<i32> = ptable.iter().map(|&e| e as i32).collect();

    // Uncoded cost: coded(1) + ptable_len * 7 bits
    let uncoded_bits = 1 + ptable_len * (AC_BITS - 1);

    // Try Rice coding
    let (best_method, best_m) = select_best_rice_method(&entry_list, TableType::Ptable);
    let cpred_order = CPRED_ORDER[best_method];

    if cpred_order >= ptable_len {
        // Cannot Rice-code
        writer.write_bit(0); // Coded = 0
        for &e in &entry_list {
            writer.write_bits((e - 1) as u32, AC_BITS - 1);
        }
        return;
    }

    // Estimate Rice coded bits
    let coded_overhead = 1 + SIZE_RICEMETHOD + cpred_order * (AC_BITS - 1) + SIZE_RICEM;
    let mut trial = BitstreamWriter::new();
    rice_encode_table(
        &mut trial,
        &entry_list,
        best_method,
        best_m,
        TableType::Ptable,
    );
    let coded_bits = coded_overhead + trial.bit_position();

    if coded_bits < uncoded_bits {
        // Rice coded
        writer.write_bit(1); // Coded = 1
        writer.write_bits(best_method as u32, SIZE_RICEMETHOD);
        for &e in &entry_list[..cpred_order] {
            writer.write_bits((e - 1) as u32, AC_BITS - 1);
        }
        writer.write_bits(best_m as u32, SIZE_RICEM);
        rice_encode_table(writer, &entry_list, best_method, best_m, TableType::Ptable);
    } else {
        // Uncoded
        writer.write_bit(0); // Coded = 0
        for &e in &entry_list {
            writer.write_bits((e - 1) as u32, AC_BITS - 1);
        }
    }
}

/// Encode a single DST frame from multi-channel packed DSD bytes.
///
/// Each element of `dsd_channels` is packed MSB-first DSD data.
/// `frame_len_bits` is the number of valid bits per channel.
/// `share_filters`: when true, compute one filter from channel 0 and share across all channels.
/// `half_prob`: when true, use p=128 for the first `pred_order` samples per channel.
/// Returns frame data as bytes. DSTCoded=1 if compression helps,
/// otherwise DSTCoded=0 with raw DSD data.
pub fn encode_dst_frame(
    dsd_channels: &[&[u8]],
    frame_len_bits: usize,
    pred_order: usize,
    share_filters: bool,
    half_prob: bool,
) -> Result<Vec<u8>> {
    let n_channels = dsd_channels.len();

    // Per-channel analysis
    let mut channel_predictions: Vec<Vec<i32>> = Vec::with_capacity(n_channels);
    let mut channel_residuals: Vec<Vec<u8>> = Vec::with_capacity(n_channels);

    // Compute filter coefficients: one shared set or per-channel
    let filter_coefs: Vec<Vec<i16>> = if share_filters {
        // Compute from channel 0 only, shared across all
        let coefs = compute_fir_coefficients(dsd_channels[0], frame_len_bits, pred_order);
        vec![coefs]
    } else {
        dsd_channels
            .iter()
            .map(|ch| compute_fir_coefficients(ch, frame_len_bits, pred_order))
            .collect()
    };

    // Run prediction per channel, using shared or per-channel coefficients
    for (ch_idx, ch_bytes) in dsd_channels.iter().enumerate() {
        let coef_idx = if share_filters { 0 } else { ch_idx };
        let (predictions, residuals) =
            predict_and_residual(ch_bytes, frame_len_bits, &filter_coefs[coef_idx]);
        channel_predictions.push(predictions);
        channel_residuals.push(residuals);
    }

    // Build probability tables: one shared or per-channel
    let ptables: Vec<Vec<u8>> = if share_filters {
        // Aggregate statistics from all channels for a single shared ptable
        let mut all_preds: Vec<i32> = Vec::new();
        let mut all_res: Vec<u8> = Vec::new();
        for (preds, res) in channel_predictions.iter().zip(channel_residuals.iter()) {
            all_preds.extend_from_slice(preds);
            all_res.extend_from_slice(res);
        }
        let mut ptable = build_ptable_default(&all_preds, &all_res);
        for p in &mut ptable {
            *p = (*p).clamp(1, 128);
        }
        vec![ptable]
    } else {
        channel_predictions
            .iter()
            .zip(channel_residuals.iter())
            .map(|(preds, res)| {
                let mut ptable = build_ptable_default(preds, res);
                for p in &mut ptable {
                    *p = (*p).clamp(1, 128);
                }
                ptable
            })
            .collect()
    };

    // Pack frame header
    let mut header = BitstreamWriter::new();

    // DSTCoded = 1
    header.write_bit(1);

    // Segment data
    write_segment_data(&mut header, n_channels);

    // Mapping data
    write_mapping_data(&mut header, n_channels, share_filters, half_prob);

    // Filter coefficient sets
    for coefs in &filter_coefs {
        write_filter_coef_set(&mut header, coefs, pred_order);
    }

    // Probability tables
    for ptable in &ptables {
        write_probability_table(&mut header, ptable);
    }

    // Validation bit: must be 0
    header.write_bit(0);

    // AC encode residuals in interleaved order
    let mut ac = ACEncoder::new();

    // dst_x_bit: extra dummy symbol
    let x_prob = prob_dst_x_bit(filter_coefs[0][0]);
    ac.encode_bit(0, x_prob);

    for bit_idx in 0..frame_len_bits {
        for ch_idx in 0..n_channels {
            let ptable_idx = if share_filters { 0 } else { ch_idx };
            let p = if half_prob && bit_idx < pred_order {
                128
            } else {
                ptable_lookup(&ptables[ptable_idx], channel_predictions[ch_idx][bit_idx]) as u32
            };
            ac.encode_bit(channel_residuals[ch_idx][bit_idx], p);
        }
    }

    let ac_bits = ac.flush();

    // Combine header + AC data
    header.write_bit_slice(&ac_bits);

    let frame_bytes = header.get_bytes();

    // Raw frame fallback disabled: ffmpeg's DST decoder does a tight memcpy
    // for DSTCoded=0 frames but reads with stride=channels*4, which is
    // inconsistent with the interleaved byte layout. Always emit DST-coded
    // frames to ensure correct decoding.
    //
    // let raw_frame_size = 1 + (frame_len_bits / RESOL) * n_channels;
    // if frame_bytes.len() >= raw_frame_size {
    //     return Ok(encode_raw_frame(dsd_channels, frame_len_bits));
    // }

    Ok(frame_bytes)
}

/// Encode a raw (uncompressed) DSD frame (DSTCoded=0).
///
/// `dsd_channels` contains packed MSB-first DSD bytes per channel.
/// Data is written as interleaved channel bytes per the DSDIFF spec.
///
/// NOTE: Not used by the encoder — ffmpeg's DST decoder (dstdec.c) does a
/// tight memcpy for DSTCoded=0 frames but then reads with stride=channels*4,
/// which is inconsistent with the interleaved byte layout and produces
/// incorrect output.
#[allow(dead_code)]
fn encode_raw_frame(dsd_channels: &[&[u8]], frame_len_bits: usize) -> Vec<u8> {
    let mut writer = BitstreamWriter::new();
    // DSTCoded = 0
    writer.write_bit(0);
    // DstXbits = 0
    writer.write_bit(0);
    // Stuffing = 0 (6 bits)
    writer.write_bits(0, 6);

    // Raw DSD data: for each byte position, write all channels
    let frame_len_bytes = frame_len_bits / RESOL;
    for byte_idx in 0..frame_len_bytes {
        for ch_bytes in dsd_channels {
            writer.write_bits(ch_bytes[byte_idx] as u32, RESOL);
        }
    }

    writer.get_bytes()
}

// ---------------------------------------------------------------------------
// Frame decoder (for self-testing)
// ---------------------------------------------------------------------------

/// Read simplified segment data.
fn read_segment_data(reader: &mut BitstreamReader, _n_channels: usize) -> Result<usize> {
    let p_same_seg_as_f = reader.read_bit();
    if p_same_seg_as_f != 1 {
        return Err(CladstError::Codec("only PSameSegAsF=1 supported".into()));
    }

    let same_seg_all_ch = reader.read_bit();
    if same_seg_all_ch != 1 {
        return Err(CladstError::Codec("only SameSegAllCh=1 supported".into()));
    }

    let end_of_channel = reader.read_bit();
    if end_of_channel != 1 {
        return Err(CladstError::Codec("only 1 segment supported".into()));
    }

    Ok(1) // NrOfSegments = 1
}

/// Read mapping data for per-channel filters.
///
/// Returns (half_prob, table_map).
fn read_mapping_data(
    reader: &mut BitstreamReader,
    n_channels: usize,
) -> Result<(Vec<u8>, Vec<usize>)> {
    let p_same_map_as_f = reader.read_bit();
    if p_same_map_as_f != 1 {
        return Err(CladstError::Codec("only PSameMapAsF=1 supported".into()));
    }

    let same_map_all_ch = reader.read_bit();

    let mut table_map: Vec<usize> = vec![0]; // channel 0 always maps to table 0
    let mut count_tables: usize = 1;

    if same_map_all_ch == 1 {
        table_map = vec![0; n_channels];
    } else {
        for _ch in 1..n_channels {
            let n_bits = log2_round_up(count_tables);
            let table_nr = if n_bits > 0 {
                reader.read_bits(n_bits) as usize
            } else {
                0
            };
            if table_nr == count_tables {
                count_tables += 1;
            }
            table_map.push(table_nr);
        }
    }

    // HalfProb: per channel (matches ffmpeg decoder)
    let mut half_prob: Vec<u8> = Vec::with_capacity(n_channels);
    for _ in 0..n_channels {
        half_prob.push(reader.read_bit());
    }

    Ok((half_prob, table_map))
}

/// Read one set of filter coefficients.
fn read_filter_coef_set(reader: &mut BitstreamReader) -> Vec<i16> {
    let pred_order = reader.read_bits(SIZE_CODEDPREDORDER) as usize + 1;
    let coded = reader.read_bit();

    if coded == 0 {
        return (0..pred_order)
            .map(|_| reader.read_bits_signed(SIZE_PREDCOEF) as i16)
            .collect();
    }

    let method = reader.read_bits(SIZE_RICEMETHOD) as usize;
    let cpred_order = CPRED_ORDER[method];
    let mut coefs: Vec<i32> = (0..cpred_order)
        .map(|_| reader.read_bits_signed(SIZE_PREDCOEF))
        .collect();
    let m = reader.read_bits(SIZE_RICEM) as usize;
    rice_decode_table(reader, &mut coefs, method, m, TableType::Filter, pred_order);

    coefs.iter().map(|&c| c as i16).collect()
}

/// Read one probability table.
fn read_probability_table(reader: &mut BitstreamReader) -> Vec<u8> {
    let ptable_len = reader.read_bits(AC_HISBITS) as usize + 1;

    if ptable_len == 1 {
        return vec![128];
    }

    let coded = reader.read_bit();

    if coded == 0 {
        return (0..ptable_len)
            .map(|_| (reader.read_bits(AC_BITS - 1) + 1) as u8)
            .collect();
    }

    let method = reader.read_bits(SIZE_RICEMETHOD) as usize;
    let cpred_order = CPRED_ORDER[method];
    let mut entries: Vec<i32> = (0..cpred_order)
        .map(|_| (reader.read_bits(AC_BITS - 1) + 1) as i32)
        .collect();
    let m = reader.read_bits(SIZE_RICEM) as usize;
    rice_decode_table(
        reader,
        &mut entries,
        method,
        m,
        TableType::Ptable,
        ptable_len,
    );

    entries.iter().map(|&e| e as u8).collect()
}

/// Decode a DST frame back to multi-channel packed DSD bytes.
///
/// Returns one `Vec<u8>` per channel containing packed MSB-first DSD data.
pub fn decode_dst_frame(
    frame_data: &[u8],
    n_channels: usize,
    frame_len_bits: usize,
) -> Result<Vec<Vec<u8>>> {
    let mut reader = BitstreamReader::new(frame_data);
    let frame_len_bytes = frame_len_bits.div_ceil(RESOL);

    let dst_coded = reader.read_bit();
    if dst_coded == 0 {
        // Raw DSD frame — already packed bytes in the stream
        let _dst_xbits = reader.read_bit();
        let _stuffing = reader.read_bits(6);

        let mut channels: Vec<Vec<u8>> = (0..n_channels)
            .map(|_| vec![0u8; frame_len_bytes])
            .collect();

        for byte_idx in 0..frame_len_bytes {
            for channel in channels.iter_mut() {
                channel[byte_idx] = reader.read_bits(RESOL) as u8;
            }
        }
        return Ok(channels);
    }

    // DST compressed frame
    read_segment_data(&mut reader, n_channels)?;
    let (half_prob, table_map) = read_mapping_data(&mut reader, n_channels)?;

    let n_filters = *table_map.iter().max().unwrap_or(&0) + 1;

    // Read filter coefficient sets
    let mut filter_coefs: Vec<Vec<i16>> = Vec::with_capacity(n_filters);
    for _ in 0..n_filters {
        filter_coefs.push(read_filter_coef_set(&mut reader));
    }

    // Read probability tables
    let mut ptables: Vec<Vec<u8>> = Vec::with_capacity(n_filters);
    for _ in 0..n_filters {
        ptables.push(read_probability_table(&mut reader));
    }

    // Validation bit
    let validation_bit = reader.read_bit();
    if validation_bit != 0 {
        return Err(CladstError::Codec("validation bit must be 0".into()));
    }

    // AC decode directly from packed frame data at current bit offset
    let mut ac_decoder = ACDecoder::new(frame_data, reader.bit_position());

    // Decode dst_x_bit dummy symbol
    let x_prob = prob_dst_x_bit(filter_coefs[0][0]);
    ac_decoder.decode_bit(x_prob);

    // Per-channel decode state, packed for iteration.
    struct ChannelState {
        lut: FilterLut,
        status: Status,
        ptable_idx: usize,
        half_prob: bool,
        pred_order: usize,
        output: Vec<u8>,
    }

    let mut ch_states: Vec<ChannelState> = table_map
        .iter()
        .enumerate()
        .map(|(ch_idx, &f_idx)| {
            let coefs = &filter_coefs[f_idx];
            let pred_order = coefs.len();
            let n_tables = pred_order.div_ceil(RESOL);
            ChannelState {
                lut: build_lut(coefs),
                status: Status::new(n_tables),
                ptable_idx: f_idx,
                half_prob: half_prob[ch_idx] != 0,
                pred_order,
                output: vec![0u8; frame_len_bytes],
            }
        })
        .collect();

    // Decode loop: accumulate 8 bits per channel, write whole bytes.
    // This eliminates per-bit set_bit() calls (was 17.7% of decode time).
    let n_ch = ch_states.len();
    let mut accum = vec![0u8; n_ch]; // bit accumulator per channel
    let mut bit_in_byte: u32 = 0; // counts 0..7
    let mut byte_idx: usize = 0; // output byte position

    for bit_idx in 0..frame_len_bits {
        for (ch_i, ch) in ch_states.iter_mut().enumerate() {
            let predict = fir_predict(&ch.lut, &ch.status);

            let p = if ch.half_prob && bit_idx < ch.pred_order {
                128
            } else {
                ptable_lookup(&ptables[ch.ptable_idx], predict)
            };

            let residual = ac_decoder.decode_bit(p as u32);
            let predicted_bit = ((predict >> 15) & 1) as u8;
            let actual_bit = residual ^ predicted_bit;

            // Accumulate MSB-first: bit 0 goes to position 7, bit 7 to position 0
            accum[ch_i] = (accum[ch_i] << 1) | actual_bit;

            ch.status.update(actual_bit);
        }

        bit_in_byte += 1;
        if bit_in_byte == 8 {
            for (ch_i, ch) in ch_states.iter_mut().enumerate() {
                ch.output[byte_idx] = accum[ch_i];
            }
            bit_in_byte = 0;
            byte_idx += 1;
        }
    }

    // Flush remaining bits (frame_len_bits not multiple of 8)
    if bit_in_byte > 0 {
        let shift = 8 - bit_in_byte;
        for (ch_i, ch) in ch_states.iter_mut().enumerate() {
            ch.output[byte_idx] = accum[ch_i] << shift;
        }
    }

    Ok(ch_states.into_iter().map(|ch| ch.output).collect())
}
