//! A pure-Rust H.264 (MPEG-4 AVC) video decoder, for the Unflash web app to
//! read H.264 files in browsers whose WebCodecs has no H.264 decoder.
//!
//! Supported: the Constrained Baseline, Baseline (without FMO / ASO /
//! redundant slices), Main and High profiles for 4:2:0 8-bit pictures:
//! CAVLC and CABAC entropy coding, I / P / B slices with any
//! partitioning, multiple references, long-term references and memory
//! management control operations, weighted prediction (explicit and
//! implicit), spatial and temporal direct prediction, the 8x8 transform
//! and scaling matrices, interlaced coding (field pictures and MBAFF
//! frames), and the deblocking filter. 4:2:2 / 4:4:4, high bit depths,
//! data partitioning, slice groups and SP / SI slices are reported as
//! unsupported.
//!
//! The decoder is written to the standard (ITU-T H.264 / ISO/IEC 14496-10)
//! and tested bit-exact against ffmpeg's decoder on x264 streams that
//! exercise these tools (`tests/`).

pub mod bitreader;
mod cabac;
mod cavlc;
mod deblock;
pub mod decoder;
mod inter;
mod intra;
pub mod mb;
pub mod picture;
pub mod ps;
pub mod rewrite;
pub mod slice;
mod tables;
mod transform;
pub mod yuv;

pub use decoder::{DecodedFrame, Decoder};
pub use ps::{Pps, Sps};
pub use rewrite::{AvcRegistry, Rewriter};

/// Why a stream, a NAL unit or a picture could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A coding tool this decoder does not implement.
    Unsupported(&'static str),
    /// Malformed or truncated data.
    Bitstream(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "unsupported H.264 stream: {s}"),
            Error::Bitstream(s) => write!(f, "invalid H.264 data: {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
