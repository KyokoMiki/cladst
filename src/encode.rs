//! DST encoding pipeline: DSD data to DST-compressed DSDIFF.

use std::io::{Seek, Write};

use rayon::prelude::*;

use crate::codec::frame::{decode_dst_frame, encode_dst_frame};
use crate::error::{CladstError, Result};
use crate::format::dsdiff::dst::DstDsdiffWriter;
use crate::format::reader::{DsdFrame, DsdFrameReader};

/// Encoding options.
pub struct EncodeOptions {
    /// FIR prediction order (1-128).
    pub pred_order: usize,
    /// Share filter coefficients across channels.
    pub share_filters: bool,
    /// Use HalfProb (p=128 for first pred_order samples).
    pub half_prob: bool,
    /// Verify encoding by decoding each frame inline.
    pub verify: bool,
    /// Number of threads for parallel encoding.
    /// 0 = use all available cores, 1 = single-threaded (no thread pool).
    pub threads: usize,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            pred_order: 128,
            share_filters: true,
            half_prob: true,
            verify: false,
            threads: 0,
        }
    }
}

/// Per-frame progress information passed to the progress callback.
pub struct EncodeProgress {
    /// Current frame index (0-based).
    pub frame: usize,
    /// Total number of frames.
    pub total_frames: usize,
    /// Frames encoded with DST compression so far.
    pub dst_frames: usize,
    /// Frames stored as raw DSD so far.
    pub raw_frames: usize,
    /// Total compressed bytes so far.
    pub compressed_bytes: usize,
    /// Raw (uncompressed) bytes per frame per channel * n_channels.
    pub raw_bytes_per_frame: usize,
}

/// Final statistics from an encoding run.
pub struct EncodeStats {
    /// Total number of frames processed.
    pub total_frames: usize,
    /// Frames encoded with DST compression.
    pub dst_frames: usize,
    /// Frames stored as raw DSD.
    pub raw_frames: usize,
    /// Overall compression ratio (compressed / raw).
    pub compression_ratio: f64,
}

/// Accumulates encoding statistics across frames.
struct EncodeAccum {
    n_frames: usize,
    dst_count: usize,
    raw_count: usize,
    total_compressed: usize,
    total_raw: usize,
}

impl EncodeAccum {
    fn new(n_frames: usize, total_raw: usize) -> Self {
        Self {
            n_frames,
            dst_count: 0,
            raw_count: 0,
            total_compressed: 0,
            total_raw,
        }
    }

    fn record(&mut self, frame_data: &[u8]) {
        if frame_data[0] & 0x80 != 0 {
            self.dst_count += 1;
        } else {
            self.raw_count += 1;
        }
        self.total_compressed += frame_data.len();
    }

    fn progress(&self, frame: usize) -> EncodeProgress {
        EncodeProgress {
            frame,
            total_frames: self.n_frames,
            dst_frames: self.dst_count,
            raw_frames: self.raw_count,
            compressed_bytes: self.total_compressed,
            raw_bytes_per_frame: self.total_raw,
        }
    }

    fn finish(self) -> EncodeStats {
        let overall_ratio = if self.total_raw > 0 {
            self.total_compressed as f64 / (self.n_frames as f64 * self.total_raw as f64)
        } else {
            0.0
        };
        EncodeStats {
            total_frames: self.n_frames,
            dst_frames: self.dst_count,
            raw_frames: self.raw_count,
            compression_ratio: overall_ratio,
        }
    }
}

/// Encode and optionally verify a single frame. Returns encoded bytes.
fn encode_single_frame(
    ch_refs: &[&[u8]],
    frame_bits: usize,
    frame_bytes_per_ch: usize,
    n_channels: usize,
    frame_idx: usize,
    options: &EncodeOptions,
) -> Result<Vec<u8>> {
    let frame_data = encode_dst_frame(
        ch_refs,
        frame_bits,
        options.pred_order,
        options.share_filters,
        options.half_prob,
    )?;

    if options.verify {
        let decoded = decode_dst_frame(&frame_data, n_channels, frame_bits)?;
        for ch in 0..n_channels {
            let expected = &ch_refs[ch][..frame_bytes_per_ch];
            if decoded[ch] != expected {
                let diffs: usize = decoded[ch]
                    .iter()
                    .zip(expected.iter())
                    .filter(|(a, b)| a != b)
                    .count();
                return Err(CladstError::VerifyFailed {
                    frame: frame_idx,
                    channel: ch,
                    diffs,
                });
            }
        }
    }

    Ok(frame_data)
}

/// Write encoded frames to output, updating statistics and calling progress.
fn write_batch<W: Write + Seek>(
    encoded_frames: Vec<Result<Vec<u8>>>,
    ch_refs_batch: &[Vec<&[u8]>],
    base_frame_idx: usize,
    dst_writer: &mut DstDsdiffWriter<W>,
    accum: &mut EncodeAccum,
    progress_fn: &Option<&dyn Fn(&EncodeProgress)>,
) -> Result<()> {
    for (j, result) in encoded_frames.into_iter().enumerate() {
        let frame_data = result?;
        let i = base_frame_idx + j;

        accum.record(&frame_data);
        dst_writer.write_frame(&frame_data, &ch_refs_batch[j])?;

        if let Some(cb) = progress_fn {
            cb(&accum.progress(i));
        }
    }
    Ok(())
}

/// Encode DSD data from a streaming reader to DST-compressed DSDIFF.
///
/// Reads frames in batches from `reader`, encodes them in parallel using Rayon,
/// and writes sequentially to `writer`. Only `batch_size` frames of DSD data
/// are held in memory at any time.
///
/// Calls `progress_fn` after each frame (if provided).
pub fn encode<W: Write + Seek>(
    reader: &mut impl DsdFrameReader,
    writer: W,
    options: &EncodeOptions,
    progress_fn: Option<&dyn Fn(&EncodeProgress)>,
) -> Result<EncodeStats> {
    let meta = reader.metadata().clone();
    let frame_bits = meta.frame_bits;
    let frame_bytes_per_ch = meta.frame_bytes_per_ch;
    let n_channels = meta.n_channels;
    let n_frames = meta.n_frames;

    // Open streaming writer
    let mut dst_writer =
        DstDsdiffWriter::new(writer, meta.sample_rate, n_channels, n_frames as u32)?;

    let total_raw = frame_bytes_per_ch * n_channels;
    let mut accum = EncodeAccum::new(n_frames, total_raw);

    // Build thread pool for multi-threaded encoding (None = single-threaded)
    let pool = if options.threads == 1 {
        None
    } else {
        Some(
            rayon::ThreadPoolBuilder::new()
                .num_threads(options.threads)
                .build()
                .map_err(|e| CladstError::Codec(format!("failed to create thread pool: {e}")))?,
        )
    };

    let batch_size = match &pool {
        Some(p) => p.current_num_threads() * 2,
        None => 1,
    };

    let mut frame_idx: usize = 0;

    loop {
        // 1. Read a batch of frames from the streaming reader
        let mut batch: Vec<DsdFrame> = Vec::with_capacity(batch_size);
        for _ in 0..batch_size {
            match reader.next_frame()? {
                Some(frame) => batch.push(frame),
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }

        // 2. Build channel references for encoding and writing
        let ch_refs_batch: Vec<Vec<&[u8]>> = batch
            .iter()
            .map(|f| f.channels.iter().map(|ch| ch.as_slice()).collect())
            .collect();

        // 3. Per-frame encoder closure
        let encode_frame = |j: usize, ch_refs: &Vec<&[u8]>| {
            encode_single_frame(
                ch_refs,
                frame_bits,
                frame_bytes_per_ch,
                n_channels,
                frame_idx + j,
                options,
            )
        };

        // 4. Parallel or sequential encoding
        let encoded_frames: Vec<Result<Vec<u8>>> = match &pool {
            Some(p) => p.install(|| {
                ch_refs_batch
                    .par_iter()
                    .enumerate()
                    .map(|(j, ch_refs)| encode_frame(j, ch_refs))
                    .collect()
            }),
            None => ch_refs_batch
                .iter()
                .enumerate()
                .map(|(j, ch_refs)| encode_frame(j, ch_refs))
                .collect(),
        };

        // 5. Sequential write
        write_batch(
            encoded_frames,
            &ch_refs_batch,
            frame_idx,
            &mut dst_writer,
            &mut accum,
            &progress_fn,
        )?;

        frame_idx += batch.len();
    }

    // Finalize: seek back and fill in chunk sizes
    dst_writer.finish()?;

    Ok(accum.finish())
}
