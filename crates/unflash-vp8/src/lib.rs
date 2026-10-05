//! A pure-Rust VP8 video decoder, for the Unflash web app to read VP8 files
//! (WebM, IVF) in browsers whose WebCodecs has no VP8 decoder, and to add
//! decoding capacity next to the hardware decoder.
//!
//! Supported: all of VP8 as RFC 6386 specifies it. Key and inter frames,
//! segmentation (quantiser and loop filter level per segment, absolute or
//! relative, with maps that persist between frames), the normal and simple
//! loop filters with sharpness and reference / mode deltas, one to eight
//! token partitions, probability updates kept or discarded per frame
//! (`refresh_entropy_probs`), every intra mode, six-tap and bilinear inter
//! prediction (the version field chooses, version 3 with whole-sample
//! chroma), split motion vectors, the golden and alt-ref frames with their
//! sign bias and buffer copies, invisible frames, and frames of any size
//! (a key frame may change it; the scaling bits are informational). A
//! frame whose data runs out part way is decoded as far as it goes, the
//! rest copied from the last frame, and returned marked damaged; malformed
//! headers are errors, and nothing in any input makes the decoder panic.
//! Only a picture too large to allocate is refused as unsupported.
//!
//! The decoder follows the RFC's reference decoder and is tested bit-exact
//! against ffmpeg's VP8 decoder on libvpx streams that exercise these tools
//! (`tests/`) and on the VP8 test vectors (`examples/conformance.rs`). Where
//! ffmpeg and libvpx read a malformed or reserved field differently, it
//! does what ffmpeg does (noted where it happens).

pub mod bool_decoder;
pub mod decoder;
pub mod header;
pub mod inter;
pub mod intra;
pub mod loopfilter;
pub mod modes;
pub mod picture;
pub mod tables;
pub mod tokens;
pub mod transform;

pub use decoder::{Decoder, Frame};

/// Why a stream or a frame could not be decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// A coding tool or picture this decoder does not handle.
    Unsupported(&'static str),
    /// Malformed or truncated data.
    Bitstream(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unsupported(s) => write!(f, "unsupported VP8 stream: {s}"),
            Error::Bitstream(s) => write!(f, "invalid VP8 data: {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
