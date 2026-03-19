//! Audio metadata extracted from file headers.

use crate::codec::constants::DST_FRAME_RATE;

/// Audio metadata extracted from file headers (before any sample data).
///
/// All fields are available immediately after opening a file, without
/// reading any sample data.
#[derive(Debug, Clone)]
pub struct DsdMetadata {
    pub sample_rate: u32,
    pub n_channels: usize,
    pub total_samples: u64,
    /// Bytes per channel in one DST frame (`sample_rate / 75 / 8`).
    pub frame_bytes_per_ch: usize,
    /// Bits per channel in one DST frame (`sample_rate / 75`).
    pub frame_bits: usize,
    /// Total number of frames.
    pub n_frames: usize,
}

impl DsdMetadata {
    /// Compute metadata from basic audio parameters.
    pub fn from_params(sample_rate: u32, n_channels: usize, total_samples: u64) -> Self {
        let frame_bits = (sample_rate / DST_FRAME_RATE) as usize;
        let frame_bytes_per_ch = frame_bits / 8;
        let total_bytes_per_ch = (total_samples / 8) as usize;
        let n_frames = total_bytes_per_ch.div_ceil(frame_bytes_per_ch);
        Self {
            sample_rate,
            n_channels,
            total_samples,
            frame_bytes_per_ch,
            frame_bits,
            n_frames,
        }
    }
}
