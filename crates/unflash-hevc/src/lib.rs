//! A pure-Rust HEVC (H.265 / MPEG-H Part 2) video decoder, for the Unflash
//! web app to read HEVC files in browsers whose WebCodecs cannot decode
//! them, and to add decoding capacity next to the hardware decoder.
//!
//! Supported: the Main, Main 10 and Main Still Picture profiles (and
//! streams of other profiles that use only their tools) for 4:2:0 and
//! 4:0:0 pictures of 8 to 12 bits: every slice and NAL unit type, short-
//! and long-term reference pictures, tiles, wavefront parallel processing
//! (decoded serially), dependent slice segments, all partitionings
//! including asymmetric motion partitions, merge and AMVP with temporal
//! motion vector prediction, explicit weighted prediction, transform skip,
//! scaling lists, sign data hiding, lossless and PCM coding units, the
//! deblocking filter and sample adaptive offset. 4:2:2 / 4:4:4, the range
//! extension coding tools, screen content coding and the multi-layer
//! extensions are reported as unsupported (multi-layer streams decode
//! their base layer).
//!
//! The decoder is written to the standard (ITU-T H.265 / ISO/IEC 23008-2)
//! and tested bit-exact against ffmpeg's decoder on x265 streams that
//! exercise these tools (`tests/`) and on the JCT-VC conformance streams
//! (`examples/conformance.rs`), which it can also check against their
//! decoded picture hash messages (`Decoder::set_check_hashes`). It runs
//! on one thread; the `simd` feature (on by default) runs the
//! interpolation and loop filter rows eight samples at a time through
//! `wide` (wasm simd128, SSE2 or NEON), with the same output.

pub mod bitreader;
pub mod cabac;
pub mod ctu;
pub mod deblock;
pub mod decoder;
pub mod dpb;
pub mod hash;
pub mod inter;
pub mod intra;
pub mod meta;
pub mod mv;
pub mod picture;
pub mod ps;
pub mod residual;
pub mod sao;
pub mod slice;
pub mod tables;
pub mod transform;

pub use decoder::{Decoder, Frame};

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
            Error::Unsupported(s) => write!(f, "unsupported HEVC stream: {s}"),
            Error::Bitstream(s) => write!(f, "invalid HEVC data: {s}"),
        }
    }
}

impl std::error::Error for Error {}

/// The result of decoding.
pub type Result<T> = std::result::Result<T, Error>;
