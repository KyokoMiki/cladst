//! Container format I/O: DSF, DSDIFF (uncompressed and DST-compressed).

pub mod dsdiff;
pub mod dsf;
pub mod metadata;
pub mod reader;

use std::io::{Read, Seek, SeekFrom};

use crate::error::{CladstError, Result};

use dsdiff::dsd::DsdDsdiffStreamReader;
use dsdiff::dst::DstDsdiffStreamReader;
use dsf::DsfStreamReader;

// ---------------------------------------------------------------------------
// Streaming reader enum dispatch
// ---------------------------------------------------------------------------

/// Streaming DSD reader — dispatches to DSF or uncompressed DSDIFF reader.
pub enum DsdStreamReader<R: Read + Seek> {
    Dsf(Box<DsfStreamReader<R>>),
    Dsdiff(DsdDsdiffStreamReader<R>),
}

impl<R: Read + Seek> reader::DsdFrameReader for DsdStreamReader<R> {
    fn metadata(&self) -> &metadata::DsdMetadata {
        match self {
            Self::Dsf(r) => r.metadata(),
            Self::Dsdiff(r) => r.metadata(),
        }
    }

    fn next_frame(&mut self) -> Result<Option<reader::DsdFrame>> {
        match self {
            Self::Dsf(r) => r.next_frame(),
            Self::Dsdiff(r) => r.next_frame(),
        }
    }
}

/// Streaming input — either uncompressed DSD or DST-compressed.
pub enum StreamInput<R: Read + Seek> {
    /// Uncompressed DSD (DSF or uncompressed DSDIFF).
    Dsd(DsdStreamReader<R>),
    /// DST-compressed DSDIFF.
    Dst(DstDsdiffStreamReader<R>),
}

/// Open a DSD file for streaming reading.
///
/// Reads only the file header to detect the format, then returns the
/// appropriate streaming reader. No sample data is loaded into memory.
pub fn open_dsd_file<R: Read + Seek>(mut reader: R) -> Result<StreamInput<R>> {
    // Read magic bytes
    let mut magic = [0u8; 4];
    reader
        .read_exact(&mut magic)
        .map_err(|_| CladstError::Format("file too small to be a valid DSD file".into()))?;

    // Reset to start — the stream readers expect to parse from the beginning
    reader.seek(SeekFrom::Start(0))?;

    match &magic {
        b"DSD " => {
            let dsf = DsfStreamReader::new(reader)?;
            Ok(StreamInput::Dsd(DsdStreamReader::Dsf(Box::new(dsf))))
        }
        b"FRM8" => {
            // Need to detect compression type by scanning PROP/CMPR.
            let compression = stream_detect_compression(&mut reader)?;
            reader.seek(SeekFrom::Start(0))?;

            match &compression {
                b"DSD " => {
                    let dsdiff = DsdDsdiffStreamReader::new(reader)?;
                    Ok(StreamInput::Dsd(DsdStreamReader::Dsdiff(dsdiff)))
                }
                b"DST " => {
                    let dst = DstDsdiffStreamReader::new(reader)?;
                    Ok(StreamInput::Dst(dst))
                }
                other => Err(CladstError::Format(format!(
                    "unsupported DSDIFF compression: {:?}",
                    std::str::from_utf8(other).unwrap_or("????"),
                ))),
            }
        }
        _ => Err(CladstError::Format(format!(
            "unrecognized file format (magic: {:?}), expected DSF or DSDIFF",
            std::str::from_utf8(&magic).unwrap_or("????"),
        ))),
    }
}

/// Detect DSDIFF compression type by streaming through chunk headers.
fn stream_detect_compression<R: Read + Seek>(reader: &mut R) -> Result<[u8; 4]> {
    reader.seek(SeekFrom::Start(16))?;

    let (prop, _, _) = dsdiff::stream_scan_dsdiff_chunks(reader, b"PROP")?;

    Ok(prop.compression)
}
