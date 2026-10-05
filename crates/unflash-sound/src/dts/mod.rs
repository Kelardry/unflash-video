//! A pure-Rust decoder for the core of DTS Coherent Acoustics (the sound
//! of most Blu-ray rips), for the Unflash web app to play and re-encode
//! the sound of files whose audio browsers' WebCodecs cannot decode.
//!
//! Supported: the core substream (ETSI TS 102 114, clause 5) at every
//! sample rate (8 to 48 kHz) and bit rate, frames of 256 to 4096 samples
//! in up to 16 subframes, the channel arrangements of up to six channels
//! (AMODE 0 to 12: mono, dual mono, stereo, sum/difference and Lt/Rt
//! stereo, 3/0, 2/1, 3/1, 2/2, 3/2 and the six-channel ones) with or
//! without the LFE channel; in the side information, subband activity,
//! high frequency VQ, joint intensity, transient modes, scale factors in
//! every code book and both tables, bit allocation and quantizer
//! selection with the scale factor adjustments, dynamic range
//! coefficients; in the audio data, ADPCM prediction (with or without the
//! previous frame's history), subband samples in Huffman codes, block
//! codes or plain, the lossy and lossless step sizes, high frequency
//! vectors, joint intensity, sum/difference of the front and surround
//! pairs, the LFE channel interpolated 64 or 128 times; both 32-band
//! synthesis filter banks (perfect and non-perfect reconstruction); the
//! embedded downmix coefficients of the auxiliary data; frames in 16-bit
//! words either way round and in 14-bit words (CD and WAV rips).
//! DTS-HD's extension substreams and the core extensions (XCh, X96,
//! XXCh) are stepped over: a 6.1, 7.1, 96 kHz or lossless stream plays
//! its core (5.1 at 48 kHz at most). Not supported: streams without a
//! core (DTS-HD Master Audio or DTS Express without one: `decode` says
//! so with `Error::NoCore`), arrangements of more than six channels or
//! user defined ones, and encoder revisions above 7 (both output as
//! silence, as the standard asks for the latter); the dialogue
//! normalization is read, not applied; the dynamic range coefficients
//! are applied only on request.
//!
//! Output is 32-bit float at full scale ±1.0, per channel: every channel
//! in WAVE order ([`Output::Native`]) or a Lo/Ro stereo downmix
//! ([`Output::Stereo`]). A frame that does not parse is output as silence
//! of its length and counted in [`Decoded::damaged`]; decoding goes on
//! with the next frame, whose filter banks and predictors start from
//! silence. Each frame gives 32 samples per block of its header; the
//! first samples of a stream are the filter bank's start from silence,
//! as ffmpeg's decoder gives them (sample `i` of this decoder's output is
//! sample `i` of ffmpeg's).
//!
//! The decoder is written from the standard; its tables were extracted
//! from the standard's text, V1.6.1 (2019) and V1.2.1 (2002) separately,
//! and compared (see `huffman` and `fir` for where they differ), and are
//! checked in unit tests; the two vector quantizer code books, which the
//! standard does not print, are described in `vq`. The integration tests
//! compare it with ffmpeg's decoder, run at test time (they are skipped,
//! saying so, when `ffmpeg` and `ffprobe` are not installed):
//! `tests/dts_streams.rs` on streams ffmpeg's encoder made
//! (`tests/media/dts/gen.sh`), `tests/dts_random_frames.rs` on frames
//! built at random that use what ffmpeg's encoder does not, and
//! `examples/fate/dts.rs` on ffmpeg's FATE sample files (not in the
//! repository). ffmpeg's core decoder dequantizes and predicts in fixed
//! point, this one in floating point: the outputs agree to 1e-4 of full
//! scale (within 2.5e-6 on the encoder's streams, 6e-6 on the random
//! frames and 2.2e-6 on the cores of FATE's files). `tests/dts_robust.rs`
//! feeds it damaged, cut and fuzzed data.
//!
//! Where the standard leaves the choice to the decoder: the output scale
//! (C.3.6 leaves the synthesis gain out) is ffmpeg's and the reference
//! decoder's; the stereo downmix of streams without embedded coefficients
//! is the one described at [`Output::Stereo`]; the dynamic range
//! coefficients are applied only on request, and the dialogue
//! normalization not at all, as ffmpeg does. Where the standard's
//! arithmetic has no bounds, this decoder holds subband samples (as
//! dequantized, predicted, scaled for joint intensity or looked up as
//! high frequency vectors) and decimated LFE samples to 24 bits, full
//! scale, as ffmpeg's does, as a fixed point decoder's words would (loud
//! streams of DTS's own encoder go a little past full scale: FATE's
//! `xll_51_24_48_768.dtshd` has a decimated LFE sample of -1.004). The
//! output itself is not clipped: it goes past ±1.0 where the synthesis
//! filter bank adds up loud subbands.
//!
//! Where ffmpeg 6.1 decodes otherwise, this decoder follows the standard
//! (the tests keep to the cases both agree on): ffmpeg interpolates the
//! LFE channel 128 times (LFF 1) with the taps of D.8's filter in another
//! order in the second half of each decimated sample's outputs (its
//! response is a sawtooth where the standard's is smooth: 1 to 2 % of
//! the LFE channel's level apart); it undoes sum/difference coding on the
//! frame's output rather than on its subband samples, which differs where
//! SUMF or SUMS changes from one frame to the next; it loses the ADPCM
//! and LFE history at a frame longer than all before it, and keeps too
//! little LFE history in frames of 256 samples; it plays encoder
//! revisions above 7, which Table 5-16 mutes; it refuses termination
//! frames, and frames with the reserved bit after RATE set (V1.2.1's
//! embedded downmix flag, whose downmix indexes this decoder steps over);
//! and of embedded downmix coefficients that feed L into Ro or R into Lo,
//! which a Lo/Ro downmix does not have, it takes others, as it mixes in
//! place.

mod decoder;
mod fir;
mod frame;
mod header;
mod huffman;
mod qmf;
mod tables;
mod vq;

pub use crate::{Decoded, Output};
pub use decoder::Decoder;
pub use frame::Features;

/// The standard's tables and CRC, for tests that build frames.
#[doc(hidden)]
pub mod testing {
    pub use crate::dts::header::crc16;
    pub use crate::dts::huffman::{Book, AUDIO, BIT_ALLOC, SCALES, TMODE};
    pub use crate::dts::tables::{AMODE_CHANNELS, RMS6};
}

/// What a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    /// Output channels for `Output::Native`.
    pub channels: usize,
    /// The audio channel arrangement (Table 5-4).
    pub amode: u8,
    pub lfe: bool,
    /// Samples per channel in a frame.
    pub frame_samples: usize,
}

/// Why nothing could be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not one DTS frame was found.
    NoSync,
    /// Only DTS-HD extension substreams, no core frame: DTS-HD Master
    /// Audio or DTS Express without a core.
    NoCore,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoSync => write!(f, "no DTS frame found"),
            Error::NoCore => write!(f, "a DTS-HD stream without a core (DTS-HD Master Audio or DTS Express without one), which this decoder cannot play"),
        }
    }
}

impl std::error::Error for Error {}
