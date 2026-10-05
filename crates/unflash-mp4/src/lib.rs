//! Small container readers and an MP4 writer, built for WebCodecs. The
//! readers (MP4 / QuickTime from the `moov` box and the `moof` boxes of
//! fragmented files; Matroska / WebM from its clusters; MPEG transport
//! streams from their packets) produce per-track codec strings, decoder
//! descriptions and sample tables (offset, size, dts, pts, sync), driven by
//! byte ranges so a browser can serve them from a `Blob` without loading
//! the file; the writer takes encoded chunks back and produces a playable
//! file.
//!
//! Nothing here decodes media.

mod annexb;
mod codec;
pub mod demux;
pub mod entry;
pub mod ivf;
pub mod mkv;
pub mod mux;
mod reader;
pub mod ts;

pub use annexb::avcc_record;
pub use demux::{Demuxer, Movie, Sample, Track, TrackKind};
pub use mux::{Muxer, TrackDesc};

/// Errors are plain strings: every failure is a malformed or unsupported
/// file, and the message is what the user sees.
pub type Error = String;
