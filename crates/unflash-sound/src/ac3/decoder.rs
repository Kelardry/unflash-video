//! The public decoder: finding sync frames, checking their CRCs, choosing
//! independent substream 0, and arranging or downmixing the channels.

use crate::ac3::crc::crc16;
use crate::ac3::frame::{BlockTrace, ChannelTrace, Features, FrameDecoder, FrameError};
use crate::ac3::header::{self, Header, Sync};
use crate::ac3::{Error, StreamInfo};
use crate::output::{fit, limit_gains, mix};
use crate::{Decoded, Output};

/// An AC-3 / E-AC-3 decoder (see the module documentation).
pub struct Decoder {
    output: Output,
    frame: FrameDecoder,
    info: Option<StreamInfo>,
    /// One buffer per coded channel for the frame being decoded.
    pcm: Vec<Vec<f32>>,
    /// Frames of other substreams (dependent ones, or other programs)
    /// skipped so far.
    skipped: u64,
}

/// The coded channels (A/52 Table 5.8 order; the LFE channel, when on,
/// has index `nfchans`) in WAVE order: front channels L R C, LFE,
/// surrounds.
pub(crate) fn wave_order(acmod: u8, lfe: bool) -> Vec<usize> {
    let nfch = crate::ac3::tables::NFCHANS[acmod as usize];
    let (front, surround): (&[usize], &[usize]) = match acmod {
        0 | 2 => (&[0, 1], &[]),
        1 => (&[0], &[]),
        3 => (&[0, 2, 1], &[]),
        4 => (&[0, 1], &[2]),
        5 => (&[0, 2, 1], &[3]),
        6 => (&[0, 1], &[2, 3]),
        _ => (&[0, 2, 1], &[3, 4]),
    };
    let mut v = front.to_vec();
    if lfe {
        v.push(nfch);
    }
    v.extend_from_slice(surround);
    v
}

/// The Lo/Ro downmix (§7.8.2) as the gains of each coded channel into the
/// left and right outputs, scaled so that neither output's gains sum to
/// more than 1: Lo = L + clev C + slev Ls (a single surround S with -3 dB
/// more, into both), and Ro alike; dual mono as left and right; mono into
/// both at full level. The LFE channel is left out.
pub(crate) fn downmix_gains(acmod: u8, clev: f32, slev: f32) -> [[f32; 6]; 2] {
    let mut g = [[0f32; 6]; 2];
    let s3 = std::f32::consts::FRAC_1_SQRT_2;
    match acmod {
        0 | 2 => {
            g[0][0] = 1.0;
            g[1][1] = 1.0;
        }
        1 => {
            g[0][0] = 1.0;
            g[1][0] = 1.0;
        }
        _ => {
            let has_c = acmod & 1 != 0;
            let (l, r) = (0, if has_c { 2 } else { 1 });
            g[0][l] = 1.0;
            g[1][r] = 1.0;
            if has_c {
                g[0][1] = clev;
                g[1][1] = clev;
            }
            match acmod {
                4 | 5 => {
                    let s = if has_c { 3 } else { 2 };
                    g[0][s] = slev * s3;
                    g[1][s] = slev * s3;
                }
                6 | 7 => {
                    let (ls, rs) = if has_c { (3, 4) } else { (2, 3) };
                    g[0][ls] = slev;
                    g[1][rs] = slev;
                }
                _ => {}
            }
            limit_gains(&mut g);
        }
    }
    g
}

fn crc_ok(frame: &[u8], s: &Sync) -> bool {
    if frame.len() < 4 {
        return false;
    }
    if s.eac3 {
        // one CRC over the whole frame after the sync word (Annex E §3.2)
        crc16(&frame[2..]) == 0
    } else {
        // crc1 over the first 5/8 of the frame, crc2 over the rest (§7.10.1)
        let words = frame.len() / 2;
        let five_eighths = ((words >> 1) + (words >> 3)) * 2;
        crc16(&frame[2..five_eighths]) == 0 && crc16(&frame[five_eighths..]) == 0
    }
}

fn is_sync(data: &[u8], pos: usize) -> bool {
    pos + 1 < data.len() && data[pos] == 0x0b && data[pos + 1] == 0x77
}

fn info_of(h: &Header) -> StreamInfo {
    StreamInfo { sample_rate: h.sample_rate, channels: h.nfchans + h.lfeon as usize, eac3: h.eac3, acmod: h.acmod, lfe: h.lfeon }
}

impl Decoder {
    pub fn new(output: Output) -> Decoder {
        Decoder { output, frame: FrameDecoder::new(), info: None, pcm: Vec::new(), skipped: 0 }
    }

    /// Decode every sync frame in `data` (a whole number of them: an MP4
    /// sample, a Matroska block, or a run of frames from an elementary
    /// stream) and append each output channel's samples to `out[ch]`.
    ///
    /// `out` is first resized to the output channel count: 2 for
    /// `Output::Stereo`; for `Output::Native`, the count of the first
    /// frame of independent substream 0 in `data`. Should the stream
    /// change its channel layout at a later frame (in this call or a later
    /// one), `out` is resized there to the new count: new channels start
    /// with silence as long as the others so far, and channels past the
    /// new count are removed with their samples. Channels are filled by
    /// position in the WAVE order of each frame's own layout.
    ///
    /// Errs only when not one sync frame of independent substream 0 could
    /// be found; damaged frames give silence and count in
    /// `Decoded::damaged`.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<Vec<f32>>) -> Result<Decoded, Error> {
        let mut result = Decoded::default();
        let mut found = false;
        let mut sized = false;
        let mut pos = 0;
        while pos + 6 <= data.len() {
            if !is_sync(data, pos) {
                pos += 1;
                continue;
            }
            let Some(s) = header::sync(&data[pos..]) else {
                pos += 1;
                continue;
            };
            let end = pos + s.bytes;
            if end > data.len() {
                // cut off: a frame of ours that does not fit is silence
                if s.main {
                    found = true;
                    let layout = header::parse(&data[pos..]).ok();
                    self.silence(s.blocks, layout.as_ref(), out, &mut sized);
                    result.damaged += 1;
                    result.samples += s.blocks * 256;
                }
                break;
            }
            let frame = &data[pos..end];
            if !crc_ok(frame, &s) {
                // A damaged frame is followed by the next frame (or the
                // end of the data); a sync word that is not is taken for
                // a chance match inside other data.
                if !(end == data.len() || is_sync(data, end)) {
                    pos += 1;
                    continue;
                }
                if s.main {
                    found = true;
                    let layout = header::parse(frame).ok();
                    self.silence(s.blocks, layout.as_ref(), out, &mut sized);
                    result.damaged += 1;
                    result.samples += s.blocks * 256;
                }
                pos = end;
                continue;
            }
            if !s.main {
                self.skipped += 1;
                pos = end;
                continue;
            }
            found = true;
            result.samples += s.blocks * 256;
            match header::parse(frame) {
                Ok(h) if h.bsid <= 8 || (11..=16).contains(&h.bsid) => {
                    if self.decode_frame(frame, &h, out, &mut sized).is_err() {
                        self.silence(s.blocks, Some(&h), out, &mut sized);
                        result.damaged += 1;
                    }
                }
                Ok(h) => {
                    // bsid 9 and 10: a decoder of this standard mutes
                    self.silence(s.blocks, Some(&h), out, &mut sized);
                    result.damaged += 1;
                }
                Err(_) => {
                    self.silence(s.blocks, None, out, &mut sized);
                    result.damaged += 1;
                }
            }
            pos = end;
        }
        if !found {
            return Err(Error::NoSync);
        }
        Ok(result)
    }

    /// The stream as its last decoded frame described it.
    pub fn info(&self) -> Option<StreamInfo> {
        self.info
    }

    /// Dither and the SPX noise blending on (the default) or off: off,
    /// zero-bit mantissas are zero and no noise is blended (for tests).
    pub fn set_noise(&mut self, on: bool) {
        self.frame.noise = on;
    }

    /// The coding tools the stream has used so far, and how many frames
    /// of other substreams were skipped (for diagnostics and tests).
    pub fn features(&self) -> (Features, u64) {
        (self.frame.features, self.skipped)
    }

    /// Record, for every block decoded from now on, which transform
    /// coefficients of each output channel carry noise and which blocks
    /// were switched (for tests; `Output::Native` only).
    #[doc(hidden)]
    pub fn set_trace(&mut self, on: bool) {
        self.frame.trace = on;
    }

    /// The blocks recorded since `set_trace(true)` or the last call, in
    /// output order, their channels in output order. Blocks of damaged
    /// frames have every coefficient marked noisy.
    #[doc(hidden)]
    pub fn take_trace(&mut self) -> Vec<BlockTrace> {
        std::mem::take(&mut self.frame.traces)
    }

    fn silence(&mut self, blocks: usize, h: Option<&Header>, out: &mut Vec<Vec<f32>>, sized: &mut bool) {
        // the layout: what the stream had, else what this frame claims,
        // else stereo
        let info = self.info.or(h.map(info_of)).unwrap_or(StreamInfo { sample_rate: 48000, channels: 2, eac3: false, acmod: 2, lfe: false });
        fit(self.output, info.channels, out, sized);
        for c in out.iter_mut() {
            c.resize(c.len() + blocks * 256, 0.0);
        }
        if self.frame.trace {
            for _ in 0..blocks {
                let all = ChannelTrace { blksw: false, noisy: [!0; 4], hebap4: [0; 4], spx: [0; 4], aht_dither: [0; 4] };
                self.frame.traces.push(BlockTrace { channels: vec![all; out.len()] });
            }
        }
        self.frame.reset_overlap();
    }

    fn decode_frame(&mut self, frame: &[u8], h: &Header, out: &mut Vec<Vec<f32>>, sized: &mut bool) -> Result<(), FrameError> {
        let coded = h.nfchans + h.lfeon as usize;
        let samples = h.blocks * 256;
        self.pcm.resize(6, Vec::new());
        for c in self.pcm.iter_mut() {
            c.clear();
            c.resize(samples, 0.0);
        }
        // a layout change: the old overlap belongs to other channels
        if let Some(old) = self.info {
            if old.acmod != h.acmod || old.lfe != h.lfeon || old.sample_rate != h.sample_rate {
                self.frame.reset_overlap();
            }
        }
        let traced = self.frame.traces.len();
        if let Err(e) = self.frame.decode(frame, h, &mut self.pcm[..coded]) {
            self.frame.traces.truncate(traced);
            return Err(e);
        }
        let info = info_of(h);
        self.info = Some(info);
        fit(self.output, info.channels, out, sized);
        match self.output {
            Output::Native => {
                let order = wave_order(h.acmod, h.lfeon);
                for (o, &c) in order.iter().enumerate() {
                    out[o].extend_from_slice(&self.pcm[c]);
                }
                for t in self.frame.traces[traced..].iter_mut() {
                    t.channels = order.iter().map(|&c| t.channels[c]).collect();
                }
            }
            Output::Stereo => {
                // (the full bandwidth channels: the LFE channel stays out)
                mix(&downmix_gains(h.acmod, h.clev, h.slev), &self.pcm[..h.nfchans], samples, out);
            }
        }
        Ok(())
    }
}

/// Where each audio block of `frame` ends, in bits from the start of the
/// frame, as far as the frame parses (its CRC is not checked), and the
/// grouped mantissa codes that are out of range (bit position, width,
/// largest valid code): for tests that build frames block by block.
#[doc(hidden)]
pub fn block_ends(frame: &[u8]) -> (Vec<usize>, Vec<(usize, u32, u32)>) {
    let Ok(h) = header::parse(frame) else { return (Vec::new(), Vec::new()) };
    let mut f = FrameDecoder::new();
    f.parse_only = true;
    let mut pcm = vec![vec![0f32; h.blocks * 256]; 6];
    let _ = f.decode(frame, &h, &mut pcm);
    (f.block_ends, f.bad_codes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wave_orders() {
        assert_eq!(wave_order(7, true), [0, 2, 1, 5, 3, 4]);
        assert_eq!(wave_order(7, false), [0, 2, 1, 3, 4]);
        assert_eq!(wave_order(2, true), [0, 1, 2]);
        assert_eq!(wave_order(1, true), [0, 1]);
        assert_eq!(wave_order(4, true), [0, 1, 3, 2]);
        assert_eq!(wave_order(5, false), [0, 2, 1, 3]);
        assert_eq!(wave_order(6, true), [0, 1, 4, 2, 3]);
        assert_eq!(wave_order(0, false), [0, 1]);
        assert_eq!(wave_order(3, true), [0, 2, 1, 3]);
    }

    #[test]
    fn downmix_gains_sum_to_one() {
        let g = downmix_gains(7, 0.707, 0.707);
        let sum: f32 = g[0].iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        assert!((g[0][0] - 1.0 / 2.414).abs() < 1e-3);
        assert_eq!(g[0][4], 0.0);
        assert_eq!(g[1][3], 0.0);
        assert_eq!(downmix_gains(2, 0.707, 0.707)[0][..2], [1.0, 0.0]);
        assert_eq!(downmix_gains(1, 0.707, 0.707)[1][0], 1.0);
        let g = downmix_gains(4, 0.707, 0.5);
        assert!((g[0][2] - 0.5 * 0.70710677 / (1.0 + 0.5 * 0.70710677)).abs() < 1e-6);
    }
}
