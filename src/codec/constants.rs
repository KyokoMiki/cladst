//! DST encoding/decoding constants.

/// Bits per DSD sample byte.
pub const RESOL: usize = 8;

// Frame header sizes (bits)
pub const SIZE_MAXFRAMELEN: usize = 4;
pub const SIZE_NROFCHANNELS: usize = 4;
pub const SIZE_DSTFRAMELEN: usize = 16;

// Prediction / filter coefficients
/// Bits for coded prediction order (max 128).
pub const SIZE_CODEDPREDORDER: usize = 7;
/// Bits per filter coefficient (signed, -256..+255).
pub const SIZE_PREDCOEF: usize = 9;

// Arithmetic coder
/// Probability resolution (256 levels).
pub const AC_BITS: usize = 8;
/// 256.
pub const AC_PROBS: usize = 1 << AC_BITS;
/// Histogram entries (max 64).
pub const AC_HISBITS: usize = 6;
/// 64.
pub const AC_HISMAX: usize = 1 << AC_HISBITS;
/// 3.
pub const AC_QSTEP: usize = SIZE_PREDCOEF - AC_HISBITS;

// Arithmetic coder internals
pub const PBITS: u32 = AC_BITS as u32;
/// Overhead bits (must be >= 2).
pub const NBITS: u32 = 4;
/// 12.
pub const ABITS: u32 = PBITS + NBITS;
/// 4096.
pub const ONE: u32 = 1 << ABITS;
/// 2048.
pub const HALF: u32 = 1 << (ABITS - 1);

// Rice coding
pub const NROFFRICEMETHODS: usize = 3;
/// Max prediction order for Rice prediction.
pub const MAXCPREDORDER: usize = 3;
/// Bits for method selection.
pub const SIZE_RICEMETHOD: usize = 2;
/// Bits for Rice m parameter.
pub const SIZE_RICEM: usize = 3;
/// Max Rice m for filters.
pub const MAX_RICE_M_F: usize = 6;
/// Max Rice m for Ptables.
pub const MAX_RICE_M_P: usize = 4;

// Segmentation
pub const MAXNROF_FSEGS: usize = 4;
pub const MAXNROF_PSEGS: usize = 8;
pub const MAXNROF_SEGS: usize = 8;
pub const MIN_FSEG_LEN: usize = 1024;
pub const MIN_PSEG_LEN: usize = 32;

// Frame/channel limits
pub const MAX_CHANNELS: usize = 6;
pub const MAX_DSDBYTES_INFRAME: usize = 588 * 64;
pub const MAX_DSDBITS_INFRAME: usize = MAX_DSDBYTES_INFRAME * RESOL;

/// Table type for Rice prediction coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableType {
    Filter = 0,
    Ptable = 1,
}

/// Rice prediction coefficients: CPRED_COEF[table_type][method][tap].
///
/// table_type: 0=FILTER, 1=PTABLE
pub const CPRED_COEF: [[[i32; 3]; 3]; 2] = [
    // FILTER
    [
        [-8, 0, 0],  // method 0: order 1
        [-16, 8, 0], // method 1: order 2
        [-9, -5, 6], // method 2: order 3
    ],
    // PTABLE
    [
        [-8, 0, 0],    // method 0: order 1
        [-16, 8, 0],   // method 1: order 2
        [-24, 24, -8], // method 2: order 3
    ],
];

/// CPredOrder per method (same for both table types).
pub const CPRED_ORDER: [usize; 3] = [1, 2, 3];

/// Frame lengths (bytes per channel per frame) by Fsample44 multiplier.
pub fn frame_length_for_rate(dsd_rate_multiplier: u32) -> Option<usize> {
    match dsd_rate_multiplier {
        64 => Some(4704),   // DSD64  (2.8 MHz)
        128 => Some(9408),  // DSD128 (5.6 MHz)
        256 => Some(18816), // DSD256 (11.2 MHz)
        _ => None,
    }
}

/// DST frame rate is always 75 fps for all DSD sample rates.
pub const DST_FRAME_RATE: u32 = 75;
