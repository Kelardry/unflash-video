//! A pure-Rust VP9 video decoder, for the Unflash web app to read VP9 files
//! in browsers whose WebCodecs has no VP9 decoder, and to add decoding
//! capacity next to a hardware decoder.
//!
//! Supported: profile 0 (8-bit 4:2:0) and profile 2 (10 and 12-bit 4:2:0),
//! with every coding tool of those profiles: key, inter and intra-only
//! frames, hidden frames in superframes and `show_existing_frame`, the four
//! probability contexts with their resets and backward adaptation,
//! segmentation (tree and temporally predicted maps, all four features),
//! the loop filter with its mode and reference deltas, delta quantisers and
//! lossless coding, tiles, every block size and partition, all transforms,
//! compound prediction, switchable interpolation filters and frame size
//! changes predicted from scaled references. Profiles 1 and 3 (4:2:2, 4:4:0
//! and 4:4:4) are reported as unsupported.
//!
//! The decoder is written to the VP9 Bitstream & Decoding Process
//! Specification (v0.6/v0.7) and tested bit-exact against ffmpeg's decoder
//! on libvpx streams that exercise these tools (`tests/`), and against the
//! libvpx test vectors (`examples/conformance.rs`).
//!
//! With the `simd` feature (the default), the interpolation, the loop
//! filter and, where 32-bit lanes multiply natively, the 8-bit inverse
//! transforms run in SIMD lanes through `wide`; tests hold each of these
//! kernels to its portable twin.

pub mod bits;
pub mod booldec;
pub mod decoder;
pub mod frame;
pub mod header;
pub mod idct;
pub mod inter;
pub mod intra;
#[cfg(feature = "simd")]
pub mod lanes;
pub mod loopfilter;
pub mod mvpred;
pub mod probs;
pub mod residual;
pub mod tables;
pub mod tile;

pub use decoder::{Decoder, Frame};

/// Why a stream or a frame could not be decoded.
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
            Error::Unsupported(s) => write!(f, "unsupported VP9 stream: {s}"),
            Error::Bitstream(s) => write!(f, "invalid VP9 data: {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
