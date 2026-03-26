//! Structured error types for the cladst library.

use std::io;

use thiserror::Error;

/// All errors produced by the cladst library.
#[derive(Debug, Error)]
pub enum CladstError {
    /// I/O error (file read/write/seek).
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Invalid or unsupported container format.
    #[error("invalid format: {0}")]
    Format(String),

    /// Codec-level error (bitstream, arithmetic coder, etc.).
    #[error("codec error: {0}")]
    Codec(String),

    /// CRC mismatch after decoding a frame.
    #[error("CRC mismatch at frame {frame}: expected {expected:#010x}, got {computed:#010x}")]
    CrcMismatch {
        frame: usize,
        expected: u32,
        computed: u32,
    },

    /// Inline verify detected a difference between encoded and original data.
    #[error("verify failed: frame {frame} ch{channel}: {diffs} bytes differ")]
    VerifyFailed {
        frame: usize,
        channel: usize,
        diffs: usize,
    },
}

/// Convenience alias used throughout the library.
pub type Result<T> = std::result::Result<T, CladstError>;
