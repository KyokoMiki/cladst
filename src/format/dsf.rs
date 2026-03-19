//! DSF file I/O: streaming reader and writer.
//!
//! All multi-byte values in DSF are little-endian.
//! DSD samples are stored LSB-first per byte (opposite of DSDIFF's MSB-first).

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{CladstError, Result};
use crate::format::metadata::DsdMetadata;
use crate::format::reader::{DsdFrame, DsdFrameReader};

/// Reverse bits within a byte (LSB-first <-> MSB-first).
fn bit_reverse(b: u8) -> u8 {
    let mut r: u8 = 0;
    for i in 0..8 {
        r = (r << 1) | ((b >> i) & 1);
    }
    r
}

/// Build the full 256-entry bit-reverse lookup table.
fn build_bit_reverse_table() -> [u8; 256] {
    let mut table = [0u8; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        *entry = bit_reverse(i as u8);
    }
    table
}

// ---------------------------------------------------------------------------
// DSF Streaming Reader
// ---------------------------------------------------------------------------

/// Streaming reader for DSF format input.
///
/// Reads the DSF header on construction, then yields one DSD frame at a time.
/// Handles the block/frame boundary mismatch via per-channel residual buffers.
pub struct DsfStreamReader<R: Read + Seek> {
    reader: BufReader<R>,
    metadata: DsdMetadata,
    bit_reverse_table: [u8; 256],
    /// Per-channel residual buffer for leftover bytes from the previous block.
    channel_residuals: Vec<Vec<u8>>,
    block_size: usize,
    n_channels: usize,
    /// Valid bytes per channel (derived from sample_count).
    valid_bytes_per_ch: usize,
    /// Total bytes read per channel so far (before residual drain).
    bytes_read_per_ch: usize,
    /// Number of frames already yielded.
    frames_yielded: usize,
}

/// Parse the DSF header and return metadata + data start position.
///
/// Reads DSD chunk (28 bytes), fmt chunk, and data chunk header.
/// The reader is left positioned at the start of audio data.
fn parse_dsf_header<R: Read + Seek>(
    reader: &mut BufReader<R>,
) -> Result<(u32, usize, u64, usize, usize)> {
    // --- DSD chunk (28 bytes) ---
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if &magic != b"DSD " {
        return Err(CladstError::Format(format!("not a DSF file: {:?}", magic)));
    }
    let mut buf8 = [0u8; 8];
    reader.read_exact(&mut buf8)?; // dsd_chunk_size
    reader.read_exact(&mut buf8)?; // total_file_size
    reader.read_exact(&mut buf8)?; // metadata_offset

    // --- fmt chunk ---
    reader.read_exact(&mut magic)?;
    if &magic != b"fmt " {
        return Err(CladstError::Format(format!(
            "expected fmt chunk: {:?}",
            magic,
        )));
    }
    reader.read_exact(&mut buf8)?;
    let fmt_chunk_size = u64::from_le_bytes(buf8);
    let fmt_data_len = (fmt_chunk_size - 12) as usize;
    let mut fmt_data = vec![0u8; fmt_data_len];
    reader.read_exact(&mut fmt_data)?;

    let channel_count = u32::from_le_bytes(
        fmt_data[12..16]
            .try_into()
            .map_err(|_| CladstError::Format("fmt chunk too small".into()))?,
    ) as usize;
    let sample_rate = u32::from_le_bytes(
        fmt_data[16..20]
            .try_into()
            .map_err(|_| CladstError::Format("fmt chunk too small".into()))?,
    );
    let sample_count = u64::from_le_bytes(
        fmt_data[24..32]
            .try_into()
            .map_err(|_| CladstError::Format("fmt chunk too small".into()))?,
    );
    let block_size = u32::from_le_bytes(
        fmt_data[32..36]
            .try_into()
            .map_err(|_| CladstError::Format("fmt chunk too small".into()))?,
    ) as usize;

    // --- data chunk ---
    reader.read_exact(&mut magic)?;
    if &magic != b"data" {
        return Err(CladstError::Format(format!(
            "expected data chunk: {:?}",
            magic,
        )));
    }
    reader.read_exact(&mut buf8)?; // data_chunk_size

    let valid_bytes_per_ch = (sample_count / 8) as usize;

    Ok((
        sample_rate,
        channel_count,
        sample_count,
        block_size,
        valid_bytes_per_ch,
    ))
}

impl<R: Read + Seek> DsfStreamReader<R> {
    /// Open a DSF file for streaming reading.
    ///
    /// Parses the header and positions the reader at the first data byte.
    pub fn new(inner: R) -> Result<Self> {
        let mut reader = BufReader::new(inner);
        let (sample_rate, channel_count, sample_count, block_size, valid_bytes_per_ch) =
            parse_dsf_header(&mut reader)?;

        let metadata = DsdMetadata::from_params(sample_rate, channel_count, sample_count);

        Ok(Self {
            reader,
            metadata,
            bit_reverse_table: build_bit_reverse_table(),
            channel_residuals: vec![Vec::new(); channel_count],
            block_size,
            n_channels: channel_count,
            valid_bytes_per_ch,
            bytes_read_per_ch: 0,
            frames_yielded: 0,
        })
    }

    /// Read the next block group (`n_channels × block_size` bytes) from the file,
    /// deinterleave, bit-reverse, and append to per-channel residuals.
    fn read_block_group(&mut self) -> Result<bool> {
        let group_size = self.n_channels * self.block_size;
        let mut buf = vec![0u8; group_size];
        match self.reader.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(e) => return Err(CladstError::Io(e)),
        }

        for ch in 0..self.n_channels {
            let offset = ch * self.block_size;
            // Determine how many bytes from this block are still valid
            let already = self.bytes_read_per_ch + self.channel_residuals[ch].len();
            let remaining_valid = self.valid_bytes_per_ch.saturating_sub(already);
            let usable = remaining_valid.min(self.block_size);
            // Bit-reverse and append usable bytes
            for &b in &buf[offset..offset + usable] {
                self.channel_residuals[ch].push(self.bit_reverse_table[b as usize]);
            }
        }

        Ok(true)
    }
}

impl<R: Read + Seek> DsdFrameReader for DsfStreamReader<R> {
    fn metadata(&self) -> &DsdMetadata {
        &self.metadata
    }

    fn next_frame(&mut self) -> Result<Option<DsdFrame>> {
        if self.frames_yielded >= self.metadata.n_frames {
            return Ok(None);
        }

        let frame_bytes = self.metadata.frame_bytes_per_ch;

        // Fill residuals until we have enough for one frame
        while self.channel_residuals[0].len() < frame_bytes {
            if !self.read_block_group()? {
                break;
            }
        }

        // Check if we have any data at all
        if self.channel_residuals[0].is_empty() {
            return Ok(None);
        }

        // Drain frame_bytes from each channel (zero-pad last frame if needed)
        let mut channels = Vec::with_capacity(self.n_channels);
        for ch_buf in &mut self.channel_residuals {
            let available = ch_buf.len().min(frame_bytes);
            let mut frame_data: Vec<u8> = ch_buf.drain(..available).collect();
            if frame_data.len() < frame_bytes {
                frame_data.resize(frame_bytes, 0);
            }
            channels.push(frame_data);
        }

        self.bytes_read_per_ch += frame_bytes;
        self.frames_yielded += 1;

        Ok(Some(DsdFrame { channels }))
    }
}

// ---------------------------------------------------------------------------
// DSF Writer
// ---------------------------------------------------------------------------

/// Standard DSF block size per channel (bytes).
const DSF_BLOCK_SIZE: usize = 4096;

/// Streaming writer for DSF format output.
///
/// DSF uses little-endian, LSB-first bit order (opposite of DSDIFF's MSB-first).
pub struct DsfWriter<W: Write + Seek> {
    writer: W,
    /// Offset of the total file size field in the DSD chunk header.
    total_size_offset: u64,
    /// Offset of the sample count field in the fmt chunk.
    sample_count_offset: u64,
    /// Offset of the data chunk size field.
    data_size_offset: u64,
    n_channels: usize,
    /// Accumulated per-channel DSD bytes (MSB-first), flushed in blocks.
    channel_bufs: Vec<Vec<u8>>,
    /// Total DSD bytes written to the data chunk so far.
    data_bytes_written: u64,
    bit_reverse_table: [u8; 256],
}

impl DsfWriter<BufWriter<File>> {
    /// Create a new DSF writer writing to a file.
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

impl<W: Write + Seek> DsfWriter<W> {
    /// Create a new DSF writer.
    ///
    /// Writes DSD chunk, fmt chunk, and data chunk header with placeholder sizes.
    /// After this, call `write_frame()` for each decoded frame, then `finish()`.
    pub fn new(mut writer: W, sample_rate: u32, n_channels: usize) -> Result<Self> {
        // --- DSD chunk (28 bytes) ---
        writer.write_all(b"DSD ")?;
        writer.write_all(&28u64.to_le_bytes())?; // chunk size = 28
        let total_size_offset = writer.stream_position()?;
        writer.write_all(&0u64.to_le_bytes())?; // total file size placeholder
        writer.write_all(&0u64.to_le_bytes())?; // metadata offset = 0

        // --- fmt chunk (52 bytes) ---
        writer.write_all(b"fmt ")?;
        writer.write_all(&52u64.to_le_bytes())?; // chunk size = 52
        writer.write_all(&1u32.to_le_bytes())?; // format version
        writer.write_all(&0u32.to_le_bytes())?; // format ID: DSD raw
        // Channel type: 2=stereo, 6=5.1, etc.
        let channel_type: u32 = match n_channels {
            1 => 1, // mono
            2 => 2, // stereo
            3 => 3, // 3 channels
            4 => 4, // quad
            5 => 5, // 5 channels
            6 => 6, // 5.1
            _ => 0, // undefined
        };
        writer.write_all(&channel_type.to_le_bytes())?;
        writer.write_all(&(n_channels as u32).to_le_bytes())?;
        writer.write_all(&sample_rate.to_le_bytes())?;
        writer.write_all(&1u32.to_le_bytes())?; // bits per sample
        let sample_count_offset = writer.stream_position()?;
        writer.write_all(&0u64.to_le_bytes())?; // sample count placeholder
        writer.write_all(&(DSF_BLOCK_SIZE as u32).to_le_bytes())?; // block size per channel
        writer.write_all(&0u32.to_le_bytes())?; // reserved

        // --- data chunk header ---
        writer.write_all(b"data")?;
        let data_size_offset = writer.stream_position()?;
        writer.write_all(&0u64.to_le_bytes())?; // data chunk size placeholder

        Ok(DsfWriter {
            writer,
            total_size_offset,
            sample_count_offset,
            data_size_offset,
            n_channels,
            channel_bufs: vec![Vec::new(); n_channels],
            data_bytes_written: 0,
            bit_reverse_table: build_bit_reverse_table(),
        })
    }

    /// Write decoded DSD data for one frame (MSB-first packed bytes per channel).
    ///
    /// Data is buffered and flushed in DSF block-interleaved format when enough
    /// bytes accumulate.
    pub fn write_frame(
        &mut self,
        decoded_channels: &[Vec<u8>],
        frame_bytes_per_ch: usize,
    ) -> Result<()> {
        for (ch, ch_data) in decoded_channels.iter().enumerate() {
            self.channel_bufs[ch].extend_from_slice(&ch_data[..frame_bytes_per_ch]);
        }

        // Flush complete blocks
        while self.channel_bufs[0].len() >= DSF_BLOCK_SIZE {
            self.flush_block()?;
        }

        Ok(())
    }

    /// Flush one DSF_BLOCK_SIZE block from each channel buffer.
    fn flush_block(&mut self) -> Result<()> {
        for ch_buf in &mut self.channel_bufs {
            let block: Vec<u8> = ch_buf
                .drain(..DSF_BLOCK_SIZE)
                .map(|b| self.bit_reverse_table[b as usize])
                .collect();
            self.writer.write_all(&block)?;
            self.data_bytes_written += DSF_BLOCK_SIZE as u64;
        }
        Ok(())
    }

    /// Finalize: flush remaining data, pad last block, seek back to fill sizes.
    ///
    /// `total_samples` is the exact number of DSD samples per channel (for the
    /// sample count field). If 0, it is computed from total data written.
    pub fn finish(mut self, total_samples: u64) -> Result<()> {
        // Flush remaining partial block (pad with zeros to DSF_BLOCK_SIZE)
        let remaining = self.channel_bufs[0].len();
        if remaining > 0 {
            for ch_buf in &mut self.channel_bufs {
                let mut block: Vec<u8> = ch_buf
                    .drain(..)
                    .map(|b| self.bit_reverse_table[b as usize])
                    .collect();
                block.resize(DSF_BLOCK_SIZE, 0);
                self.writer.write_all(&block)?;
                self.data_bytes_written += DSF_BLOCK_SIZE as u64;
            }
        }

        self.writer.flush()?;
        let end_pos = self.writer.stream_position()?;

        let sample_count = if total_samples > 0 {
            total_samples
        } else {
            (self.data_bytes_written / self.n_channels as u64) * 8
        };

        // Total file size
        self.writer.seek(SeekFrom::Start(self.total_size_offset))?;
        self.writer.write_all(&end_pos.to_le_bytes())?;

        // Sample count
        self.writer
            .seek(SeekFrom::Start(self.sample_count_offset))?;
        self.writer.write_all(&sample_count.to_le_bytes())?;

        // Data chunk size (includes its own 12-byte header)
        let data_chunk_size = 12 + self.data_bytes_written;
        self.writer.seek(SeekFrom::Start(self.data_size_offset))?;
        self.writer.write_all(&data_chunk_size.to_le_bytes())?;

        self.writer.flush()?;
        Ok(())
    }
}
