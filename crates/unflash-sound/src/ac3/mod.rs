//! A pure-Rust AC-3 (Dolby Digital) and E-AC-3 (Dolby Digital Plus)
//! audio decoder, for the Unflash web app to play and re-encode the sound
//! of files whose audio browsers' WebCodecs cannot decode.
//!
//! Supported: AC-3 (bsid 0 to 8, the alternate bit stream information of
//! Annex D included) in every coding mode, 1+1 dual mono and the LFE
//! channel included, at 32, 44.1 and 48 kHz and every frame size: block
//! switching, coupling with phase flags, rematrixing, all exponent
//! strategies, the parametric bit allocation with delta bit allocation,
//! dither, dynamic range words (applied in full, as the standard's
//! default), skip fields and both CRCs. E-AC-3 (bsid 11 to 16):
//! independent substream 0 with 1, 2, 3 or 6 blocks per frame, at the
//! reduced rates too (16, 22.05, 24 kHz): frame and block exponent
//! strategies, the adaptive hybrid transform (vector and gain adaptive
//! quantization), spectral extension (bands, blending with noise,
//! attenuation), standard coupling, the bit allocation mode, SNR offset
//! strategies, fast gain codes, converter and block start fields, and all
//! the metadata (whose Lo/Ro mix levels the stereo downmix uses).
//! Dependent substreams and further independent substreams are skipped,
//! so a 7.1 stream plays its 5.1 core. Not supported: enhanced coupling
//! (such a frame is output as silence), bsid 9 and 10 (muted, as the
//! standard asks), heavy compression (compr), dialogue normalization and
//! transient pre-noise processing (parsed, not applied), the Lt/Rt
//! downmix.
//!
//! Output is 32-bit float at full scale ±1.0, per channel: every channel
//! of the stream in WAVE order ([`Output::Native`]) or a Lo/Ro stereo
//! downmix ([`Output::Stereo`]). A frame that fails its CRC, does not
//! parse or uses what is not supported is output as silence of its length
//! and counted in [`Decoded::damaged`]; decoding goes on with the next
//! frame, whose overlap starts from silence.
//!
//! The decoder is written from the standard (ATSC A/52:2018, with ETSI TS
//! 102 366 where A/52 is unclear); its tables are checked against the
//! standard's in unit tests, its transforms against their definitions
//! and by perfect reconstruction. The integration tests compare it with
//! ffmpeg's decoder, run at test time (they are skipped, saying so, when
//! `ffmpeg` and `ffprobe` are not installed): `tests/ac3_streams.rs` on
//! streams ffmpeg's encoders made (`tests/media/ac3/gen.sh`, every coding
//! mode and rate), `tests/ac3_random_frames.rs` on frames built at random
//! that use the tools no ffmpeg encoder does, and `examples/fate/ac3.rs`
//! on ffmpeg's FATE sample files (not in the repository). The comparison
//! is in the frequency domain: both outputs go through the forward MDCT
//! again, which recovers each block's coefficients; the decoder's trace
//! marks the coefficients that carry noise (dither, the transform's
//! dither, spectral extension noise), which are compared by their energy,
//! and every other coefficient must agree to 1e-5 of full scale (1e-4 for
//! those the spectral extension makes, 3e-5 on the random frames; the
//! FATE files agree to 5e-7). `tests/ac3_robust.rs` feeds it damaged, cut
//! and fuzzed data.
//!
//! Where the standard leaves the choice to the decoder: dither and noise
//! come from a xorshift generator of its own; the transform's zero-bit
//! coefficients are dithered in the DCT domain, those of a full bandwidth
//! channel as its dither flag of block 0 says, those of the coupling and
//! LFE channels always; E-AC-3 without mixing metadata downmixes with
//! -4.5 dB centre and -6 dB surround levels (as ffmpeg does); in 1+1 mode
//! the LFE channel takes dynrng2 (as ffmpeg does); the reduced rates use
//! the hearing threshold table of the rate they are half of.
//!
//! Where ffmpeg 6.1 decodes otherwise, this decoder follows the standard
//! (the tests keep to the cases both agree on): at a switched block
//! ffmpeg overlaps with another channel's previous block; it decodes the
//! transform's 32-level vector quantizer (hebap 4) with the next row of
//! the table; its dither of coupled coefficients is correlated between
//! the channels (§7.3.4 has it uncorrelated); it dithers the transform's
//! zero-bit coefficients whatever the dither flags say; it counts the
//! coupling channel's delta bit allocation bands from the coupling start
//! (A/52 §7.2.2.6 counts from band 0) and takes "reuse" of delta bit
//! allocation before any was sent in the frame for something else; in
//! E-AC-3 it keeps the previous fast gain codes where the standard resets
//! them to the default (Table E1.4), reads the flexible mixing data
//! (mixdef 3) as starting after mixdeflen (Annex E §3.10.4 counts
//! mixdeflen in), does not decode the per-block SNR offset strategies (1
//! and 2) as specified, keeps a stale band structure where coupling or
//! the spectral extension starts after block 0 of a frame (the standard
//! takes the default), and decodes blocks where coupling starts after
//! block 0 while the extension is in use, or where the extension's
//! strategy changes while coupling is in use, otherwise.

mod bitalloc;
mod crc;
mod decoder;
mod frame;
mod header;
mod imdct;
mod tables;
mod vq;

pub use crate::{Decoded, Output};
pub use decoder::{block_ends, Decoder};
pub use frame::{BlockTrace, ChannelTrace, Features};

/// The standard's tables and CRC, for tests that build frames.
#[doc(hidden)]
pub mod testing {
    pub use crate::ac3::crc::crc16;
    pub use crate::ac3::tables::{DEFAULT_CPL_BNDSTRC, DEFAULT_SPX_BNDSTRC, FRMEXPSTR, NFCHANS};

    /// A sync frame's length in bytes, from its first six (`None` where
    /// they cannot start a frame).
    pub fn frame_bytes(f: &[u8]) -> Option<usize> {
        crate::ac3::header::sync(f).map(|s| s.bytes)
    }
}

/// What a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    /// Output channels for `Output::Native`.
    pub channels: usize,
    /// E-AC-3 (bsid 11..=16) rather than AC-3.
    pub eac3: bool,
    pub acmod: u8,
    pub lfe: bool,
}

/// Why nothing could be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Not one sync frame of independent substream 0 was found.
    NoSync,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoSync => write!(f, "no AC-3 or E-AC-3 sync frame found"),
        }
    }
}

impl std::error::Error for Error {}
