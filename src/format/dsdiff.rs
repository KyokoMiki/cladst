//! DSDIFF container format: shared chunk utilities and sub-modules.
//!
//! All multi-byte values in DSDIFF are big-endian.

pub mod dsd;
pub mod dst;

// Re-export public types for convenience.
pub use dsd::{DsdDsdiffStreamReader, DsdDsdiffWriter};
pub use dst::{DstDsdiffStreamReader, DstDsdiffWriter, DstFrame, dst_frame_crc};

use std::io::{Read, Seek, SeekFrom, Write};

use crate::error::{CladstError, Result};

// ---------------------------------------------------------------------------
// Chunk reading helpers (generic — used by streaming readers)
// ---------------------------------------------------------------------------

/// Read a big-endian u64 from any reader.
pub(crate) fn stream_read_u64_be(r: &mut (impl Read + Seek)) -> Result<u64> {
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

/// Read a big-endian u32 from any reader.
pub(crate) fn stream_read_u32_be(r: &mut (impl Read + Seek)) -> Result<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

/// Read a big-endian u16 from any reader.
pub(crate) fn stream_read_u16_be(r: &mut (impl Read + Seek)) -> Result<u16> {
    let mut buf = [0u8; 2];
    r.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

/// Read a 4-byte chunk ID from any reader.
pub(crate) fn stream_read_chunk_id(r: &mut (impl Read + Seek)) -> Result<[u8; 4]> {
    let mut id = [0u8; 4];
    r.read_exact(&mut id)?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// PROP chunk parsing
// ---------------------------------------------------------------------------

/// Parsed PROP chunk metadata from a DSDIFF file.
pub(crate) struct DsdiffProp {
    pub sample_rate: u32,
    pub n_channels: usize,
    pub compression: [u8; 4],
}

/// Parse the PROP chunk from a stream reader.
///
/// The reader must be positioned at the start of PROP data.
/// After return, the reader is positioned at `prop_start + chunk_size`.
pub(crate) fn stream_parse_prop_chunk(
    r: &mut (impl Read + Seek),
    chunk_size: usize,
) -> Result<DsdiffProp> {
    let chunk_data_start = r.stream_position()? as usize;
    let _prop_form = stream_read_chunk_id(r)?; // "SND "
    let prop_end = chunk_data_start + chunk_size;

    let mut sample_rate: u32 = 0;
    let mut n_channels: usize = 0;
    let mut compression = [0u8; 4];

    while (r.stream_position()? as usize) < prop_end {
        let sub_id = stream_read_chunk_id(r)?;
        let sub_size = stream_read_u64_be(r)? as usize;
        let sub_start = r.stream_position()? as usize;
        match &sub_id {
            b"FS  " => {
                sample_rate = stream_read_u32_be(r)?;
            }
            b"CHNL" => {
                n_channels = stream_read_u16_be(r)? as usize;
            }
            b"CMPR" => {
                r.read_exact(&mut compression)?;
            }
            _ => {}
        }
        let next = sub_start + sub_size + (sub_size % 2);
        r.seek(SeekFrom::Start(next as u64))?;
    }

    Ok(DsdiffProp {
        sample_rate,
        n_channels,
        compression,
    })
}

/// Scan DSDIFF top-level chunks from a stream reader to find PROP and a
/// specific data chunk (e.g. `b"DSD "` or `b"DST "`).
///
/// Returns `(prop, data_offset, data_size)`. The reader must be positioned
/// right after the FRM8 form type (offset 16).
pub(crate) fn stream_scan_dsdiff_chunks(
    r: &mut (impl Read + Seek),
    target_data_chunk: &[u8; 4],
) -> Result<(DsdiffProp, u64, usize)> {
    let mut prop = None;
    let mut data_offset: u64 = 0;
    let mut data_size: usize = 0;

    while let Ok(chunk_id) = stream_read_chunk_id(r) {
        let chunk_size = match stream_read_u64_be(r) {
            Ok(s) => s as usize,
            Err(_) => break,
        };
        let chunk_data_start = r.stream_position()?;

        match &chunk_id {
            b"PROP" => {
                prop = Some(stream_parse_prop_chunk(r, chunk_size)?);
            }
            id if id == target_data_chunk => {
                data_offset = chunk_data_start;
                data_size = chunk_size;
            }
            _ => {}
        }

        // If we found both, stop scanning
        if prop.is_some() && data_size > 0 {
            break;
        }

        let next = chunk_data_start as usize + chunk_size + (chunk_size % 2);
        r.seek(SeekFrom::Start(next as u64))?;
    }

    let prop =
        prop.ok_or_else(|| CladstError::Format("missing PROP chunk in DSDIFF file".into()))?;

    Ok((prop, data_offset, data_size))
}

// ---------------------------------------------------------------------------
// Chunk writing helpers
// ---------------------------------------------------------------------------

/// Write a chunk header (id + size) to a writer.
pub(crate) fn write_chunk_header(w: &mut impl Write, chunk_id: &[u8; 4], size: u64) -> Result<()> {
    w.write_all(chunk_id)?;
    w.write_all(&size.to_be_bytes())?;
    Ok(())
}

/// Write a complete small chunk: id(4) + size(8) + data + pad.
pub(crate) fn write_chunk(w: &mut impl Write, chunk_id: &[u8; 4], data: &[u8]) -> Result<()> {
    write_chunk_header(w, chunk_id, data.len() as u64)?;
    w.write_all(data)?;
    if !data.len().is_multiple_of(2) {
        w.write_all(&[0x00])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// PROP chunk builder
// ---------------------------------------------------------------------------

/// Standard channel IDs (4 bytes each).
const STEREO_CHANNEL_IDS: [[u8; 4]; 2] = [*b"SLFT", *b"SRGT"];
const MULTI_CHANNEL_IDS: [[u8; 4]; 6] =
    [*b"MLFT", *b"MRGT", *b"C   ", *b"LFE ", *b"LS  ", *b"RS  "];

/// Build the DSDIFF PROP chunk content for a given compression type.
///
/// `cmpr_id` is the 4-byte compression ID (b"DSD " or b"DST ").
/// `cmpr_name` is the human-readable name (e.g. b"not compressed\0" or b"DST Encoded").
pub(crate) fn build_prop_data(
    sample_rate: u32,
    n_channels: usize,
    cmpr_id: &[u8; 4],
    cmpr_name: &[u8],
) -> Vec<u8> {
    let channel_ids: Vec<[u8; 4]> = if n_channels == 2 {
        STEREO_CHANNEL_IDS.to_vec()
    } else if n_channels <= 6 {
        MULTI_CHANNEL_IDS[..n_channels].to_vec()
    } else {
        (0..n_channels)
            .map(|i| {
                let s = format!("C{i:03}");
                let mut id = [b' '; 4];
                id[..s.len().min(4)].copy_from_slice(&s.as_bytes()[..s.len().min(4)]);
                id
            })
            .collect()
    };

    let mut prop_data = Vec::new();
    prop_data.extend_from_slice(b"SND ");

    // FS sub-chunk
    let fs_data = sample_rate.to_be_bytes();
    prop_data.extend_from_slice(b"FS  ");
    prop_data.extend_from_slice(&(fs_data.len() as u64).to_be_bytes());
    prop_data.extend_from_slice(&fs_data);

    // CHNL sub-chunk
    let mut chnl_data = Vec::new();
    chnl_data.extend_from_slice(&(n_channels as u16).to_be_bytes());
    for id in &channel_ids {
        chnl_data.extend_from_slice(id);
    }
    prop_data.extend_from_slice(b"CHNL");
    prop_data.extend_from_slice(&(chnl_data.len() as u64).to_be_bytes());
    prop_data.extend_from_slice(&chnl_data);
    if chnl_data.len() % 2 != 0 {
        prop_data.push(0x00);
    }

    // CMPR sub-chunk
    let mut cmpr_data = Vec::new();
    cmpr_data.extend_from_slice(cmpr_id);
    cmpr_data.push(cmpr_name.len() as u8);
    cmpr_data.extend_from_slice(cmpr_name);
    prop_data.extend_from_slice(b"CMPR");
    prop_data.extend_from_slice(&(cmpr_data.len() as u64).to_be_bytes());
    prop_data.extend_from_slice(&cmpr_data);
    if cmpr_data.len() % 2 != 0 {
        prop_data.push(0x00);
    }

    prop_data
}
