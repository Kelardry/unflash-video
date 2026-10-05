//! Pure-Rust decoders for the sound browsers' WebCodecs cannot decode, for
//! the Unflash web app to play and re-encode it: AC-3 and E-AC-3 (Dolby
//! Digital, Dolby Digital Plus) in [`ac3`], and the core of DTS Coherent
//! Acoustics (which DTS-HD streams carry too) in [`dts`]. Each module says
//! what it supports, how it is tested against ffmpeg's decoder, and where
//! the two differ.
//!
//! Both work alike: a `Decoder` takes whole frames and appends 32-bit float
//! samples at full scale ±1.0 to one vector per channel, in WAVE order
//! ([`Output::Native`]) or mixed down to Lo/Ro stereo ([`Output::Stereo`]);
//! a frame that does not decode is silence of its length, counted in
//! [`Decoded::damaged`]. What they share is written once, here: those two
//! types, the frame parsers' bit reader (`bits`), and the sizing and the
//! stereo mix of the output (`output`).

pub mod ac3;
mod bits;
pub mod dts;
mod output;

/// How a decoder arranges its output channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// Every channel the stream has (AC-3: its independent substream 0;
    /// DTS: its core), LFE included, in WAVE/SMPTE order: L R C LFE Ls Rs
    /// (those present, in that order; a single surround channel S, as in
    /// 2/1 and 3/1, comes where Ls would; mono is C; dual mono, AC-3's 1+1,
    /// is its two channels, Ch1 Ch2).
    Native,
    /// A stereo downmix Lo/Ro, the LFE channel left out, scaled so that no
    /// output's gains sum to more than 1. AC-3's is the one A/52 describes
    /// (§7.8, with the stream's cmixlev and surmixlev; dual mono as both
    /// channels); DTS's takes the stream's embedded Lo/Ro (or
    /// Lt/Rt) coefficients when its frames carry them, else centre and
    /// surrounds at -3 dB (a single surround -3 dB more, into both sides).
    /// Mono becomes two identical channels.
    Stereo,
}

/// What one call to a decoder's `decode` produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Decoded {
    /// Samples appended to each output channel.
    pub samples: usize,
    /// Frames (AC-3's sync frames, DTS's core frames) that were output as
    /// silence: a bad CRC (AC-3), cut off, unparseable, unsupported.
    pub damaged: u32,
}
