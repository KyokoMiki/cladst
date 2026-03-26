//! DST decoding and integrity testing pipelines.

use std::io::{Seek, Write};

use crate::codec::frame::decode_dst_frame;
use crate::error::{CladstError, Result};
use crate::format::dsdiff::DsdDsdiffWriter;
use crate::format::dsdiff::dst::dst_frame_crc;
use crate::format::dsf::DsfWriter;
use crate::format::reader::DstFrameReader;

/// Output format for decoding.
pub enum OutputFormat {
    /// DSF (Sony standard, little-endian, LSB-first).
    Dsf,
    /// Uncompressed DSDIFF (big-endian, MSB-first).
    DsdiffUncompressed,
}

/// Final statistics from a decode run.
pub struct DecodeStats {
    /// Total number of frames decoded.
    pub total_frames: usize,
    /// Number of frames that had CRC and passed verification.
    pub crc_checked: usize,
}

/// Final statistics from a test run.
pub struct TestStats {
    /// Total number of frames decoded.
    pub total_frames: usize,
    /// Number of frames that had CRC and passed verification.
    pub crc_checked: usize,
}

/// Verify CRC of a decoded frame against expected value.
fn verify_frame_crc(
    decoded_channels: &[Vec<u8>],
    frame_bytes_per_ch: usize,
    expected_crc: u32,
    frame_idx: usize,
) -> Result<()> {
    let n_channels = decoded_channels.len();
    let mut interleaved = Vec::with_capacity(frame_bytes_per_ch * n_channels);
    for byte_idx in 0..frame_bytes_per_ch {
        for ch in decoded_channels {
            interleaved.push(ch[byte_idx]);
        }
    }
    let computed = dst_frame_crc(&interleaved);
    if computed != expected_crc {
        return Err(CladstError::CrcMismatch {
            frame: frame_idx,
            expected: expected_crc,
            computed,
        });
    }
    Ok(())
}

/// Enum-dispatch DSD writer: static dispatch without trait object vtable.
enum DsdWriter<W: Write + Seek> {
    Dsf(Box<DsfWriter<W>>),
    Dsdiff(DsdDsdiffWriter<W>),
}

impl<W: Write + Seek> DsdWriter<W> {
    fn write_frame(
        &mut self,
        decoded_channels: &[Vec<u8>],
        frame_bytes_per_ch: usize,
    ) -> Result<()> {
        match self {
            Self::Dsf(w) => w.write_frame(decoded_channels, frame_bytes_per_ch),
            Self::Dsdiff(w) => w.write_frame(decoded_channels, frame_bytes_per_ch),
        }
    }

    fn finish(self, total_samples: u64) -> Result<()> {
        match self {
            Self::Dsf(w) => w.finish(total_samples),
            Self::Dsdiff(w) => w.finish(),
        }
    }
}

/// Decode DST-compressed data from a streaming reader, writing to `writer`.
///
/// Calls `progress_fn` after each frame (if provided).
pub fn decode<W: Write + Seek>(
    reader: &mut impl DstFrameReader,
    writer: W,
    format: OutputFormat,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> Result<DecodeStats> {
    let meta = reader.metadata().clone();
    let frame_bits = meta.frame_bits;
    let frame_bytes_per_ch = meta.frame_bytes_per_ch;
    let n_frames = meta.n_frames;
    let total_samples = n_frames as u64 * frame_bits as u64;

    let mut dsd_writer = match format {
        OutputFormat::Dsf => DsdWriter::Dsf(Box::new(DsfWriter::new(
            writer,
            meta.sample_rate,
            meta.n_channels,
        )?)),
        OutputFormat::DsdiffUncompressed => DsdWriter::Dsdiff(DsdDsdiffWriter::new(
            writer,
            meta.sample_rate,
            meta.n_channels,
        )?),
    };

    let mut crc_checked: usize = 0;
    let mut frames_decoded: usize = 0;

    while let Some(frame) = reader.next_frame()? {
        let decoded = decode_dst_frame(&frame.data, meta.n_channels, frame_bits)?;

        if let Some(expected_crc) = frame.crc {
            verify_frame_crc(&decoded, frame_bytes_per_ch, expected_crc, frames_decoded)?;
            crc_checked += 1;
        }

        dsd_writer.write_frame(&decoded, frame_bytes_per_ch)?;

        if let Some(ref cb) = progress_fn {
            cb(frames_decoded, n_frames);
        }
        frames_decoded += 1;
    }

    dsd_writer.finish(total_samples)?;

    Ok(DecodeStats {
        total_frames: frames_decoded,
        crc_checked,
    })
}

/// Test DST integrity: decode all frames, verify CRC, no output.
///
/// Calls `progress_fn` after each frame (if provided).
pub fn test(
    reader: &mut impl DstFrameReader,
    progress_fn: Option<&dyn Fn(usize, usize)>,
) -> Result<TestStats> {
    let meta = reader.metadata().clone();
    let frame_bits = meta.frame_bits;
    let frame_bytes_per_ch = meta.frame_bytes_per_ch;
    let n_frames = meta.n_frames;

    let mut crc_checked: usize = 0;
    let mut frames_decoded: usize = 0;

    while let Some(frame) = reader.next_frame()? {
        let decoded = decode_dst_frame(&frame.data, meta.n_channels, frame_bits)?;

        if let Some(expected_crc) = frame.crc {
            verify_frame_crc(&decoded, frame_bytes_per_ch, expected_crc, frames_decoded)?;
            crc_checked += 1;
        }

        if let Some(ref cb) = progress_fn {
            cb(frames_decoded, n_frames);
        }
        frames_decoded += 1;
    }

    Ok(TestStats {
        total_frames: frames_decoded,
        crc_checked,
    })
}
