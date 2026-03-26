//! DST-compressed DSDIFF streaming reader, writer, and CRC.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::{CladstError, Result};

use super::{
    build_prop_data, stream_read_chunk_id, stream_read_u16_be, stream_read_u32_be,
    stream_read_u64_be, stream_scan_dsdiff_chunks, write_chunk, write_chunk_header,
};
use crate::codec::constants::DST_FRAME_RATE;
use crate::format::metadata::DsdMetadata;
use crate::format::reader::DstFrameReader;

// ---------------------------------------------------------------------------
// DST-compressed DSDIFF Types
// ---------------------------------------------------------------------------

/// A single DST frame read from a DSDIFF file.
pub struct DstFrame {
    /// Raw encoded DST frame data (DSTF chunk payload).
    pub data: Vec<u8>,
    /// Optional CRC-32 from DSTC chunk (None if no DSTC follows this DSTF).
    pub crc: Option<u32>,
}

// ---------------------------------------------------------------------------
// DST-compressed DSDIFF Streaming Reader
// ---------------------------------------------------------------------------

/// Streaming reader for DST-compressed DSDIFF input.
///
/// Reads the header and FRTE on construction, then yields one DST frame at a
/// time by reading DSTF/DSTC chunk pairs sequentially.
pub struct DstDsdiffStreamReader<R: Read + Seek> {
    reader: BufReader<R>,
    metadata: DsdMetadata,
    /// End offset of the DST chunk data in the file.
    dst_end: u64,
    frames_yielded: usize,
}

impl<R: Read + Seek> DstDsdiffStreamReader<R> {
    /// Open a DST-compressed DSDIFF file for streaming reading.
    ///
    /// Parses the header, validates CMPR="DST ", reads FRTE, and positions
    /// the reader right after FRTE (ready for DSTF chunks).
    pub fn new(inner: R) -> Result<Self> {
        let mut reader = BufReader::new(inner);

        // Skip FRM8(4) + size(8) + form_type(4) = 16
        reader.seek(SeekFrom::Start(16))?;

        let (prop, data_offset, data_size) = stream_scan_dsdiff_chunks(&mut reader, b"DST ")?;

        if &prop.compression != b"DST " {
            return Err(CladstError::Format(format!(
                "not a DST-compressed DSDIFF: CMPR={:?} (expected 'DST ')",
                std::str::from_utf8(&prop.compression).unwrap_or("????"),
            )));
        }
        if prop.sample_rate == 0 || prop.n_channels == 0 || data_size == 0 {
            return Err(CladstError::Format(
                "incomplete DST DSDIFF: missing FS, CHNL, or DST data chunk".into(),
            ));
        }

        let dst_end = data_offset + data_size as u64;

        // Position at the start of DST sub-chunks and read FRTE
        reader.seek(SeekFrom::Start(data_offset))?;

        let frte_id = stream_read_chunk_id(&mut reader)?;
        let frte_size = stream_read_u64_be(&mut reader)? as usize;
        if &frte_id != b"FRTE" {
            return Err(CladstError::Format(format!(
                "expected FRTE chunk at start of DST data, got {:?}",
                std::str::from_utf8(&frte_id).unwrap_or("????"),
            )));
        }
        let n_frames = stream_read_u32_be(&mut reader)?;
        let frame_rate = stream_read_u16_be(&mut reader)?;
        if frame_rate != DST_FRAME_RATE as u16 {
            return Err(CladstError::Format(format!(
                "unexpected DST frame rate: {} (expected {})",
                frame_rate, DST_FRAME_RATE,
            )));
        }
        // Skip any remaining FRTE data + padding
        let frte_data_start = data_offset + 12; // after FRTE header
        let frte_next = frte_data_start + frte_size as u64 + (frte_size as u64 % 2);
        reader.seek(SeekFrom::Start(frte_next))?;

        // Compute metadata from n_frames
        let frame_bits = (prop.sample_rate / DST_FRAME_RATE) as usize;
        let total_samples = n_frames as u64 * frame_bits as u64;
        let metadata = DsdMetadata::from_params(prop.sample_rate, prop.n_channels, total_samples);

        Ok(Self {
            reader,
            metadata,
            dst_end,
            frames_yielded: 0,
        })
    }
}

impl<R: Read + Seek> DstFrameReader for DstDsdiffStreamReader<R> {
    fn metadata(&self) -> &DsdMetadata {
        &self.metadata
    }

    fn next_frame(&mut self) -> Result<Option<DstFrame>> {
        if self.frames_yielded >= self.metadata.n_frames {
            return Ok(None);
        }

        // Read chunks until we find a DSTF (skip DSTI and other non-frame chunks)
        loop {
            if self.reader.stream_position()? >= self.dst_end {
                return Ok(None);
            }

            let chunk_id = match stream_read_chunk_id(&mut self.reader) {
                Ok(id) => id,
                Err(_) => return Ok(None),
            };
            let chunk_size = match stream_read_u64_be(&mut self.reader) {
                Ok(s) => s as usize,
                Err(_) => return Ok(None),
            };
            let chunk_data_start = self.reader.stream_position()?;

            match &chunk_id {
                b"DSTF" => {
                    // Read the frame data
                    let mut data = vec![0u8; chunk_size];
                    self.reader.read_exact(&mut data)?;

                    // Skip padding byte if odd size
                    if chunk_size % 2 != 0 {
                        self.reader.seek(SeekFrom::Current(1))?;
                    }

                    // Check if the next chunk is DSTC (CRC)
                    let crc = self.try_read_dstc()?;

                    self.frames_yielded += 1;
                    return Ok(Some(DstFrame { data, crc }));
                }
                _ => {
                    // Skip non-DSTF chunks (DSTI, etc.)
                    let next = chunk_data_start + chunk_size as u64 + (chunk_size as u64 % 2);
                    self.reader.seek(SeekFrom::Start(next))?;
                }
            }
        }
    }
}

impl<R: Read + Seek> DstDsdiffStreamReader<R> {
    /// Try to read a DSTC chunk immediately following a DSTF chunk.
    /// If the next chunk is not DSTC, seeks back so it can be read normally.
    fn try_read_dstc(&mut self) -> Result<Option<u32>> {
        let pos_before = self.reader.stream_position()?;
        if pos_before >= self.dst_end {
            return Ok(None);
        }

        let chunk_id = match stream_read_chunk_id(&mut self.reader) {
            Ok(id) => id,
            Err(_) => {
                self.reader.seek(SeekFrom::Start(pos_before))?;
                return Ok(None);
            }
        };

        if &chunk_id != b"DSTC" {
            // Not a CRC chunk — seek back
            self.reader.seek(SeekFrom::Start(pos_before))?;
            return Ok(None);
        }

        let chunk_size = stream_read_u64_be(&mut self.reader)? as usize;
        if chunk_size < 4 {
            return Err(CladstError::Format("DSTC chunk too small for CRC".into()));
        }
        let crc = stream_read_u32_be(&mut self.reader)?;
        // Skip remaining data + padding
        if chunk_size > 4 {
            let skip = (chunk_size - 4) + (chunk_size % 2);
            self.reader.seek(SeekFrom::Current(skip as i64))?;
        }
        Ok(Some(crc))
    }
}

// ---------------------------------------------------------------------------
// DST Frame CRC
// ---------------------------------------------------------------------------

/// CRC-32 polynomial for DST frame checksums (DSDIFF spec section 3.4.3).
///
/// G(x) = x^32 + x^31 + x^4 + 1
const DST_CRC_POLY: u32 = 0x8000_0011;

/// Compute the DST frame CRC over interleaved DSD channel bytes.
///
/// Input is the original (uncompressed) DSD data as interleaved channel bytes
/// (CH0_b0, CH1_b0, CH0_b1, CH1_b1, ...), processed MSB-first per byte.
/// Returns a 4-byte big-endian CRC (c31..c0).
pub fn dst_frame_crc(interleaved: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in interleaved {
        for bit in (0..8).rev() {
            let input_bit = ((byte >> bit) & 1) as u32;
            let feedback = (crc >> 31) ^ input_bit;
            crc = (crc << 1) ^ (feedback * DST_CRC_POLY);
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// DSDIFF DST Writer
// ---------------------------------------------------------------------------

/// Streaming DSDIFF writer that writes DST-compressed frames.
///
/// Uses seek-back to fill in FRM8 and DST chunk sizes after all frames are written.
/// This eliminates the need to buffer all frame data in memory.
pub struct DstDsdiffWriter<W: Write + Seek> {
    writer: W,
    /// File offset of FRM8 size field (right after "FRM8", 4 bytes in).
    frm8_size_offset: u64,
    /// File offset of DST chunk size field.
    dst_size_offset: u64,
    /// Running total of bytes written inside the DST chunk (after the DST size field).
    dst_data_bytes: u64,
    /// DSTI index entries: (offset_in_dst, dstf_size) per frame.
    frame_index: Vec<(u64, u32)>,
}

impl DstDsdiffWriter<BufWriter<File>> {
    /// Create a new streaming DSDIFF DST writer writing to a file.
    pub fn from_path(
        path: &Path,
        sample_rate: u32,
        n_channels: usize,
        n_frames: u32,
    ) -> Result<Self> {
        let file = File::create(path).map_err(|e| {
            CladstError::Io(std::io::Error::new(
                e.kind(),
                format!("failed to create {}: {e}", path.display()),
            ))
        })?;
        Self::new(BufWriter::new(file), sample_rate, n_channels, n_frames)
    }
}

impl<W: Write + Seek> DstDsdiffWriter<W> {
    /// Create a new streaming DSDIFF writer.
    ///
    /// Writes FRM8 header (placeholder size), FVER, PROP, DST header (placeholder size),
    /// and FRTE chunk. After this, call `write_frame()` for each encoded frame.
    pub fn new(mut writer: W, sample_rate: u32, n_channels: usize, n_frames: u32) -> Result<Self> {
        // --- FRM8 header with placeholder size ---
        writer.write_all(b"FRM8")?;
        let frm8_size_offset = 4u64; // offset where the 8-byte size lives
        writer.write_all(&0u64.to_be_bytes())?; // placeholder
        writer.write_all(b"DSD ")?;

        // --- FVER chunk (fixed) ---
        write_chunk(&mut writer, b"FVER", &0x01050000u32.to_be_bytes())?;

        // --- PROP chunk ---
        let prop_data = build_prop_data(sample_rate, n_channels, b"DST ", b"DST Encoded");
        write_chunk(&mut writer, b"PROP", &prop_data)?;

        // --- DST chunk header with placeholder size ---
        writer.write_all(b"DST ")?;
        // Current position is where the DST size field starts
        let dst_size_offset = writer.stream_position()?;
        writer.write_all(&0u64.to_be_bytes())?; // placeholder

        // --- FRTE sub-chunk (fixed, n_frames known upfront) ---
        let mut frte_data = Vec::new();
        frte_data.extend_from_slice(&n_frames.to_be_bytes());
        frte_data.extend_from_slice(&DST_FRAME_RATE.to_be_bytes()[2..]); // u16
        write_chunk(&mut writer, b"FRTE", &frte_data)?;

        // Track how many bytes we've written inside DST after its size field
        // FRTE chunk = 12 (header) + 6 (data) = 18 bytes
        let frte_total = 12 + frte_data.len() as u64;

        Ok(DstDsdiffWriter {
            writer,
            frm8_size_offset,
            dst_size_offset,
            dst_data_bytes: frte_total,
            frame_index: Vec::with_capacity(n_frames as usize),
        })
    }

    /// Write a single DSTF chunk followed by a DSTC (CRC) chunk.
    ///
    /// `frame_data` is the encoded DST frame. `dsd_channels` contains the
    /// original DSD bytes per channel for this frame, used to compute the CRC
    /// over the interleaved representation.
    pub fn write_frame(&mut self, frame_data: &[u8], dsd_channels: &[&[u8]]) -> Result<()> {
        // Record frame index: offset of DSTF within DST chunk, and DSTF total size
        let dstf_offset_in_dst = self.dst_data_bytes;
        let dstf_total = 12 + frame_data.len() as u64 + (frame_data.len() as u64 % 2);
        self.frame_index
            .push((dstf_offset_in_dst, dstf_total as u32));

        // DSTF chunk
        write_chunk(&mut self.writer, b"DSTF", frame_data)?;

        // Compute CRC over interleaved DSD data (CH0_b0, CH1_b0, CH0_b1, ...)
        let frame_bytes = dsd_channels[0].len();
        let n_channels = dsd_channels.len();
        let mut interleaved = Vec::with_capacity(frame_bytes * n_channels);
        for byte_idx in 0..frame_bytes {
            for ch in dsd_channels {
                interleaved.push(ch[byte_idx]);
            }
        }
        let crc = dst_frame_crc(&interleaved);

        // DSTC chunk (4 bytes big-endian CRC)
        write_chunk(&mut self.writer, b"DSTC", &crc.to_be_bytes())?;
        let dstc_total = 12 + 4u64; // header + 4 bytes data (always even, no pad)

        self.dst_data_bytes += dstf_total + dstc_total;
        Ok(())
    }

    /// Finalize the file: write DSTI chunk, then seek back to fill in FRM8 and DST sizes.
    pub fn finish(mut self) -> Result<()> {
        self.writer.flush()?;

        // --- DSTI chunk (DST Frame Index) ---
        // Each entry: 8-byte offset (within DST chunk) + 4-byte size = 12 bytes
        let dsti_data_len = self.frame_index.len() * 12;
        write_chunk_header(&mut self.writer, b"DSTI", dsti_data_len as u64)?;
        for &(offset, size) in &self.frame_index {
            self.writer.write_all(&offset.to_be_bytes())?;
            self.writer.write_all(&size.to_be_bytes())?;
        }

        let end_pos = self.writer.stream_position()?;

        // FRM8 size = everything after FRM8 header (after id + size = 12 bytes)
        let frm8_size = end_pos - 12;
        self.writer.seek(SeekFrom::Start(self.frm8_size_offset))?;
        self.writer.write_all(&frm8_size.to_be_bytes())?;

        // DST chunk size = all data inside DST (FRTE + all DSTF/DSTC chunks)
        self.writer.seek(SeekFrom::Start(self.dst_size_offset))?;
        self.writer.write_all(&self.dst_data_bytes.to_be_bytes())?;

        self.writer.flush()?;
        Ok(())
    }
}
