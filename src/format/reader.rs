//! Streaming reader traits and adapters for DSD/DST frame-by-frame reading.

use crate::codec::frame::decode_dst_frame;
use crate::error::Result;
use crate::format::dsdiff::dst::DstFrame;
use crate::format::metadata::DsdMetadata;

/// A single frame of uncompressed DSD data.
pub struct DsdFrame {
    /// Per-channel DSD bytes. Each `Vec` is exactly `frame_bytes_per_ch` long.
    pub channels: Vec<Vec<u8>>,
}

/// Streaming reader that yields one frame of uncompressed DSD data at a time.
pub trait DsdFrameReader {
    /// Audio metadata (available immediately after construction).
    fn metadata(&self) -> &DsdMetadata;

    /// Read the next frame. Returns `None` when all frames have been read.
    fn next_frame(&mut self) -> Result<Option<DsdFrame>>;
}

/// Streaming reader that yields one compressed DST frame at a time.
pub trait DstFrameReader {
    /// Audio metadata (available immediately after construction).
    fn metadata(&self) -> &DsdMetadata;

    /// Read the next DST frame. Returns `None` when all frames have been read.
    fn next_frame(&mut self) -> Result<Option<DstFrame>>;
}

/// Adapter that wraps a [`DstFrameReader`] and decodes DST frames on-the-fly
/// to produce uncompressed [`DsdFrame`]s.
///
/// Eliminates the need for a temp file when re-encoding DST→DST.
pub struct DstToDsdAdapter<R> {
    inner: R,
    metadata: DsdMetadata,
}

impl<R: DstFrameReader> DstToDsdAdapter<R> {
    pub fn new(inner: R) -> Self {
        let metadata = inner.metadata().clone();
        Self { inner, metadata }
    }
}

impl<R: DstFrameReader> DsdFrameReader for DstToDsdAdapter<R> {
    fn metadata(&self) -> &DsdMetadata {
        &self.metadata
    }

    fn next_frame(&mut self) -> Result<Option<DsdFrame>> {
        let meta = self.inner.metadata();
        let n_channels = meta.n_channels;
        let frame_bits = meta.frame_bits;

        match self.inner.next_frame()? {
            None => Ok(None),
            Some(dst_frame) => {
                let channels = decode_dst_frame(&dst_frame.data, n_channels, frame_bits)?;
                Ok(Some(DsdFrame { channels }))
            }
        }
    }
}
