//! Uncompressed DSDIFF streaming reader and writer.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{CladstError, Result};

use super::{build_prop_data, stream_scan_dsdiff_chunks, write_chunk};
use crate::format::metadata::DsdMetadata;
use crate::format::reader::{DsdFrame, DsdFrameReader};

// ---------------------------------------------------------------------------
// Uncompressed DSDIFF Streaming Reader
// ---------------------------------------------------------------------------

/// Streaming reader for uncompressed DSDIFF input.
///
/// Reads the header on construction, then yields one DSD frame at a time.
/// DSDIFF stores byte-interleaved data (CH0_b0, CH1_b0, CH0_b1, CH1_b1, ...),
/// so each frame is read as `frame_bytes_per_ch * n_channels` contiguous bytes
/// and deinterleaved.
pub struct DsdDsdiffStreamReader<R: Read + Seek> {
    reader: BufReader<R>,
    metadata: DsdMetadata,
    n_channels: usize,
    /// Total bytes of DSD data available in the file.
    data_size: usize,
    /// Bytes already consumed from the data section.
    bytes_consumed: usize,
    frames_yielded: usize,
}

impl<R: Read + Seek> DsdDsdiffStreamReader<R> {
    /// Open a DSDIFF file for streaming reading.
    ///
    /// Parses the header, validates CMPR="DSD ", and positions the reader
    /// at the start of interleaved DSD data.
    pub fn new(inner: R) -> Result<Self> {
        let mut reader = BufReader::new(inner);

        // Skip FRM8(4) + size(8) + form_type(4) = 16
        reader.seek(SeekFrom::Start(16))?;

        let (prop, data_offset, data_size) = stream_scan_dsdiff_chunks(&mut reader, b"DSD ")?;

        if &prop.compression != b"DSD " {
            return Err(CladstError::Format(format!(
                "unsupported DSDIFF compression: {:?} (only uncompressed DSD is supported)",
                std::str::from_utf8(&prop.compression).unwrap_or("????"),
            )));
        }
        if prop.sample_rate == 0 || prop.n_channels == 0 || data_size == 0 {
            return Err(CladstError::Format(
                "incomplete DSDIFF: missing FS, CHNL, or DSD data chunk".into(),
            ));
        }

        let bytes_per_ch = data_size / prop.n_channels;
        let total_samples = (bytes_per_ch * 8) as u64;
        let metadata = DsdMetadata::from_params(prop.sample_rate, prop.n_channels, total_samples);

        // Position reader at data start
        reader.seek(SeekFrom::Start(data_offset))?;

        Ok(Self {
            reader,
            metadata,
            n_channels: prop.n_channels,
            data_size,
            bytes_consumed: 0,
            frames_yielded: 0,
        })
    }
}

impl<R: Read + Seek> DsdFrameReader for DsdDsdiffStreamReader<R> {
    fn metadata(&self) -> &DsdMetadata {
        &self.metadata
    }

    fn next_frame(&mut self) -> Result<Option<DsdFrame>> {
        if self.frames_yielded >= self.metadata.n_frames {
            return Ok(None);
        }

        let frame_bytes = self.metadata.frame_bytes_per_ch;
        let interleaved_size = frame_bytes * self.n_channels;
        let remaining = self.data_size - self.bytes_consumed;

        if remaining == 0 {
            return Ok(None);
        }

        // Read interleaved bytes (may be less for the last frame)
        let to_read = interleaved_size.min(remaining);
        let mut buf = vec![0u8; to_read];
        self.reader.read_exact(&mut buf)?;
        self.bytes_consumed += to_read;

        // Deinterleave into per-channel vectors
        let mut channels: Vec<Vec<u8>> = vec![Vec::with_capacity(frame_bytes); self.n_channels];
        for (i, &b) in buf.iter().enumerate() {
            let ch = i % self.n_channels;
            channels[ch].push(b);
        }

        // Zero-pad last frame if shorter than frame_bytes
        for ch in &mut channels {
            if ch.len() < frame_bytes {
                ch.resize(frame_bytes, 0);
            }
        }

        self.frames_yielded += 1;
        Ok(Some(DsdFrame { channels }))
    }
}

// ---------------------------------------------------------------------------
// Uncompressed DSDIFF Writer
// ---------------------------------------------------------------------------

/// Streaming writer for uncompressed DSDIFF output.
///
/// Produces FRM8/DSD form with FVER, PROP (CMPR="DSD "), and DSD sound data chunk.
pub struct DsdDsdiffWriter<W: Write + Seek> {
    writer: W,
    frm8_size_offset: u64,
    dsd_data_size_offset: u64,
    dsd_data_bytes: u64,
    n_channels: usize,
}

impl DsdDsdiffWriter<BufWriter<File>> {
    /// Create a new uncompressed DSDIFF writer writing to a file.
    pub fn from_path(path: &Path, sample_rate: u32, n_channels: usize) -> Result<Self> {
        let file = File::create(path).map_err(|e| {
            CladstError::Io(std::io::Error::new(
                e.kind(),
                format!("failed to create {}: {e}", path.display()),
            ))
        })?;
        Self::new(BufWriter::new(file), sample_rate, n_channels)
    }
}

impl<W: Write + Seek> DsdDsdiffWriter<W> {
    /// Create a new uncompressed DSDIFF writer.
    ///
    /// Writes FRM8 header (placeholder size), FVER, PROP, and DSD data chunk header
    /// (placeholder size). After this, call `write_frame()` for each decoded frame.
    pub fn new(mut writer: W, sample_rate: u32, n_channels: usize) -> Result<Self> {
        // FRM8 header with placeholder size
        writer.write_all(b"FRM8")?;
        let frm8_size_offset = 4u64;
        writer.write_all(&0u64.to_be_bytes())?;
        writer.write_all(b"DSD ")?;

        // FVER chunk
        write_chunk(&mut writer, b"FVER", &0x01050000u32.to_be_bytes())?;

        // PROP chunk (CMPR = "DSD ", name per DSDIFF spec)
        let prop_data = build_prop_data(sample_rate, n_channels, b"DSD ", b"not compressed\0");
        write_chunk(&mut writer, b"PROP", &prop_data)?;

        // DSD sound data chunk header with placeholder size
        writer.write_all(b"DSD ")?;
        let dsd_data_size_offset = writer.stream_position()?;
        writer.write_all(&0u64.to_be_bytes())?;

        Ok(DsdDsdiffWriter {
            writer,
            frm8_size_offset,
            dsd_data_size_offset,
            dsd_data_bytes: 0,
            n_channels,
        })
    }

    /// Write decoded DSD data for one frame as interleaved channel bytes.
    pub fn write_frame(
        &mut self,
        decoded_channels: &[Vec<u8>],
        frame_bytes_per_ch: usize,
    ) -> Result<()> {
        for byte_idx in 0..frame_bytes_per_ch {
            for ch in decoded_channels {
                self.writer.write_all(&[ch[byte_idx]])?;
            }
        }
        self.dsd_data_bytes += (frame_bytes_per_ch * self.n_channels) as u64;
        Ok(())
    }

    /// Finalize: seek back to fill in FRM8 and DSD data chunk sizes.
    pub fn finish(mut self) -> Result<()> {
        self.writer.flush()?;

        let end_pos = self.writer.stream_position()?;

        // FRM8 size = everything after FRM8 id(4) + size(8) = 12 bytes
        let frm8_size = end_pos - 12;
        self.writer.seek(SeekFrom::Start(self.frm8_size_offset))?;
        self.writer.write_all(&frm8_size.to_be_bytes())?;

        // DSD data chunk size
        self.writer
            .seek(SeekFrom::Start(self.dsd_data_size_offset))?;
        self.writer.write_all(&self.dsd_data_bytes.to_be_bytes())?;

        self.writer.flush()?;
        Ok(())
    }
}
