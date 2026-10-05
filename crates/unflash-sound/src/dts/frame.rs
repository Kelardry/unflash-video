//! Decoding one core frame: the primary audio coding header (clause
//! 5.4.3), each subframe's side information (5.5) and audio data (5.6),
//! their reconstruction (inverse ADPCM, joint intensity, sum/difference,
//! the synthesis filter bank and the LFE interpolation, clause C.3), and
//! the optional information after them (5.7) with its embedded downmix
//! coefficients (5.8.2).

use crate::bits::Bits;
use crate::dts::header::{self, Header};
use crate::dts::huffman::Books;
use crate::dts::qmf::{Lfe, Modulation, Qmf};
use crate::dts::{fir, tables, vq};

/// Primary channels the decoder takes: the arrangements up to six
/// channels (AMODE 0 to 12).
pub const MAX_CHANNELS: usize = 6;

/// The gain from the synthesis filter bank of C.3.6 to output samples at
/// full scale ±1.0: the standard leaves it out (C.3.6's `rScale`); 128
/// sqrt 2 over 2^23 is what both ffmpeg's decoder and the reference
/// outputs of FATE's dcadec suite give.
const OUTPUT_SCALE: f64 = 128.0 * std::f64::consts::SQRT_2 / 8388608.0;
/// The LFE channel's samples are 24-bit samples: over 2^23.
const LFE_SCALE: f64 = 1.0 / 8388608.0;

/// Subband samples and decimated LFE samples are held to 24 bits, as in
/// ffmpeg's decoder: full scale is 2^23. The loudest streams of DTS's own
/// encoder go a little past it (FATE's `xll_51_24_48_768.dtshd` has a
/// decimated LFE sample of -1.004), and damaged data far past it, where
/// it would otherwise run away in the predictors from frame to frame.
fn clip24(v: f64) -> f64 {
    v.clamp(-8388608.0, 8388607.0)
}

/// Levels of the quantizers ABITS 1 to 10 (Table 5-26).
const LEVELS: [u32; 10] = [3, 5, 7, 9, 13, 17, 25, 33, 65, 129];
/// Bits of a block code (four levels) for ABITS 1 to 7 (clause D.6).
const BLOCK_BITS: [u32; 7] = [7, 10, 12, 13, 15, 17, 19];

/// Why a frame was not decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// It does not parse (or its DSYNC words do not check).
    Invalid(&'static str),
    /// It uses what this decoder does not implement.
    Unsupported(&'static str),
}

use FrameError::{Invalid, Unsupported};

/// The coding tools a stream has used so far (for diagnostics and tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// Frames of more than one subframe, and subframes of more than one
    /// subsubframe.
    pub subframes: bool,
    pub subsubframes: bool,
    /// Subbands with ADPCM prediction (PMODE 1).
    pub adpcm: bool,
    /// Frames that start without the previous frame's predictor history
    /// (HFLAG 0).
    pub history_reset: bool,
    /// High frequency subbands coded as vectors.
    pub high_frequency_vq: bool,
    /// Joint intensity coded subbands.
    pub joint_intensity: bool,
    /// Subbands with a transient (two scale factors).
    pub transients: bool,
    /// Subband samples in Huffman codes, block codes and plain codes.
    pub huffman_samples: bool,
    pub block_codes: bool,
    pub plain_samples: bool,
    /// Scale factors and bit allocation indexes in Huffman codes.
    pub huffman_scales: bool,
    pub huffman_bit_allocation: bool,
    /// Scale factors from the 7-bit table.
    pub scales_7bit: bool,
    /// Scale factor adjustments other than 1.
    pub adjustments: bool,
    /// The two filter banks.
    pub perfect_filter: bool,
    pub nonperfect_filter: bool,
    /// The LFE channel interpolated 64 and 128 times.
    pub lfe_64: bool,
    pub lfe_128: bool,
    /// Front or surround pairs in sum/difference.
    pub sum_difference: bool,
    /// Dynamic range coefficients.
    pub dynamic_range: bool,
    /// DSYNC words after every subsubframe (ASPF).
    pub dsync_every_subsubframe: bool,
    /// CRC words (CPF).
    pub crc_words: bool,
    /// The lossless quantizer steps (RATE 31).
    pub lossless_steps: bool,
    /// Embedded downmix coefficients in the auxiliary data.
    pub embedded_downmix: bool,
    /// Frames with a core extension (XCh, X96, XXCh), which is skipped.
    pub core_extension: bool,
}

/// An embedded downmix (5.8.2): its type (Table 5-32) and its
/// coefficients, one row per downmix channel, one column per coded
/// channel (coded order, LFE last).
pub type Downmix = (u8, Vec<Vec<f32>>);

/// The primary audio coding header (Table 5-21).
#[derive(Clone, Copy)]
struct Coding {
    subframes: usize,
    channels: usize,
    /// nSUBS, nVQSUB, JOINX (source channel + 1), THUFF, SHUFF, BHUFF.
    subs: [usize; MAX_CHANNELS],
    vqsub: [usize; MAX_CHANNELS],
    joinx: [usize; MAX_CHANNELS],
    thuff: [usize; MAX_CHANNELS],
    shuff: [usize; MAX_CHANNELS],
    bhuff: [usize; MAX_CHANNELS],
    /// SEL and the scale factor adjustment for ABITS 1 to 10.
    sel: [[usize; 10]; MAX_CHANNELS],
    adj: [[f64; 10]; MAX_CHANNELS],
}

/// A subframe's side information (Table 5-28), per channel and subband.
struct Side {
    pmode: [[bool; 32]; MAX_CHANNELS],
    pvq: [[u16; 32]; MAX_CHANNELS],
    abits: [[u8; 32]; MAX_CHANNELS],
    tmode: [[u8; 32]; MAX_CHANNELS],
    scales: [[[f64; 2]; 32]; MAX_CHANNELS],
    joint: [[f64; 32]; MAX_CHANNELS],
    range: Option<u8>,
}

impl Default for Side {
    fn default() -> Self {
        Side {
            pmode: [[false; 32]; MAX_CHANNELS],
            pvq: [[0; 32]; MAX_CHANNELS],
            abits: [[0; 32]; MAX_CHANNELS],
            tmode: [[0; 32]; MAX_CHANNELS],
            scales: [[[0.0; 2]; 32]; MAX_CHANNELS],
            joint: [[0.0; 32]; MAX_CHANNELS],
            range: None,
        }
    }
}

/// The core frame decoder and the state it carries from frame to frame:
/// the filter banks' history, the LFE interpolation's and the ADPCM
/// predictors' (the last four samples of each subband).
pub struct FrameDecoder {
    books: Books,
    modulation: Modulation,
    qmf: Vec<Qmf>,
    lfe: Lfe,
    /// Each channel's subband samples in the subframe being decoded,
    /// after the last four of the one before: [channel][band][4 + m].
    samples: Box<[[[f64; 36]; 32]; MAX_CHANNELS]>,
    /// The filter banks' input: [channel][band][m].
    synth: Box<[[[f64; 32]; 32]; MAX_CHANNELS]>,
    side: Box<Side>,
    pub features: Features,
    /// Apply the dynamic range coefficients (off by default).
    pub dynamic_range: bool,
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder {
            books: Books::new(),
            modulation: Modulation::new(),
            qmf: vec![Qmf::default(); MAX_CHANNELS],
            lfe: Lfe::default(),
            samples: Box::new([[[0.0; 36]; 32]; MAX_CHANNELS]),
            synth: Box::new([[[0.0; 32]; 32]; MAX_CHANNELS]),
            side: Box::default(),
            features: Features::default(),
            dynamic_range: false,
        }
    }

    /// Forget the filter banks', the LFE's and the predictors' history.
    pub fn reset(&mut self) {
        for q in self.qmf.iter_mut() {
            q.reset();
        }
        self.lfe.reset();
        self.reset_prediction();
    }

    fn reset_prediction(&mut self) {
        for ch in self.samples.iter_mut() {
            for band in ch.iter_mut() {
                band[..4].fill(0.0);
            }
        }
    }

    /// Decode `frame` (the 16-bit big endian form, its header `h`) into
    /// `pcm`: one buffer per coded channel, the LFE channel last, each
    /// `h.samples()` long. Gives the frame's embedded downmix, if it
    /// carries one.
    pub fn decode(&mut self, frame: &[u8], h: &Header, pcm: &mut [Vec<f32>]) -> Result<Option<Downmix>, FrameError> {
        if h.vernum > 7 {
            // Table 5-16: a decoder not made for such a revision mutes
            return Err(Unsupported("encoder revision (VERNUM) above 7"));
        }
        if h.amode as usize >= 13 {
            return Err(Unsupported("audio channel arrangement of more than six channels or user defined (AMODE above 12)"));
        }
        let mut b = Bits::new(frame);
        b.seek(h.end);
        let c = self.coding(&mut b, h)?;
        let f = &mut self.features;
        f.subframes |= c.subframes > 1;
        f.perfect_filter |= h.perfect;
        f.nonperfect_filter |= !h.perfect;
        f.lfe_64 |= h.lff == 2;
        f.lfe_128 |= h.lff == 1;
        f.dsync_every_subsubframe |= h.dsync_every_subsubframe;
        f.crc_words |= h.crc_present;
        f.lossless_steps |= h.rate == 31;
        f.sum_difference |= h.sum_front || h.sum_surround || h.amode == 3;
        f.dynamic_range |= h.dynamic_range;
        f.core_extension |= h.ext_audio;
        if !h.predictor_history {
            self.features.history_reset = true;
            self.reset_prediction();
        }
        let mut slot = 0;
        for _ in 0..c.subframes {
            slot += self.subframe(&mut b, h, &c, slot, pcm)?;
        }
        if slot != h.blocks {
            return Err(Invalid("the subframes do not fill the frame"));
        }
        // optional information (Table 5-30)
        if h.time_stamp {
            b.skip(32);
        }
        let mut downmix = None;
        if h.aux {
            let count = b.read(6) as usize;
            b.align(32);
            let start = b.position() / 8;
            if b.overrun() || start + count > h.bytes.min(frame.len()) {
                return Err(Invalid("auxiliary data past the end of the frame"));
            }
            downmix = aux_downmix(&frame[start..start + count], c.channels + (h.lff > 0) as usize);
            self.features.embedded_downmix |= downmix.is_some();
        }
        if b.overrun() || b.position() > h.bytes * 8 {
            return Err(Invalid("the audio data runs past the end of the frame"));
        }
        Ok(downmix)
    }

    /// The primary audio coding header (Table 5-21).
    fn coding(&mut self, b: &mut Bits, h: &Header) -> Result<Coding, FrameError> {
        let subframes = b.read(4) as usize + 1;
        let channels = b.read(3) as usize + 1;
        if channels != tables::AMODE_CHANNELS[h.amode as usize] {
            return Err(Unsupported("primary channels (PCHS) other than the arrangement's (AMODE)"));
        }
        let mut c = Coding {
            subframes,
            channels,
            subs: [0; MAX_CHANNELS],
            vqsub: [0; MAX_CHANNELS],
            joinx: [0; MAX_CHANNELS],
            thuff: [0; MAX_CHANNELS],
            shuff: [0; MAX_CHANNELS],
            bhuff: [0; MAX_CHANNELS],
            sel: [[0; 10]; MAX_CHANNELS],
            adj: [[1.0; 10]; MAX_CHANNELS],
        };
        for ch in 0..channels {
            c.subs[ch] = b.read(5) as usize + 2;
            if c.subs[ch] > 32 {
                return Err(Invalid("more than 32 active subbands"));
            }
        }
        for ch in 0..channels {
            c.vqsub[ch] = b.read(5) as usize + 1;
        }
        for ch in 0..channels {
            c.joinx[ch] = b.read(3) as usize;
            if c.joinx[ch] > 0 && (c.joinx[ch] > channels || c.joinx[ch] - 1 == ch) {
                return Err(Invalid("joint intensity source channel"));
            }
        }
        for ch in 0..channels {
            c.thuff[ch] = b.read(2) as usize;
        }
        for ch in 0..channels {
            c.shuff[ch] = b.read(3) as usize;
            if c.shuff[ch] == 7 {
                return Err(Invalid("scale factor code book 7"));
            }
        }
        for ch in 0..channels {
            c.bhuff[ch] = b.read(3) as usize;
            if c.bhuff[ch] == 7 {
                return Err(Invalid("bit allocation code book 7"));
            }
        }
        // SEL: 1 bit for ABITS 1, 2 bits for 2 to 5, 3 bits for 6 to 10
        for n in 0..10 {
            let bits = match n {
                0 => 1,
                1..=4 => 2,
                _ => 3,
            };
            for ch in 0..channels {
                c.sel[ch][n] = b.read(bits) as usize;
            }
        }
        // ADJ, where SEL chooses a Huffman book
        for n in 0..10 {
            for ch in 0..channels {
                if c.sel[ch][n] < block_or_plain_sel(n + 1) {
                    c.adj[ch][n] = tables::ADJ[b.read(2) as usize];
                    self.features.adjustments |= c.adj[ch][n] != 1.0;
                }
            }
        }
        if h.crc_present {
            b.skip(16); // AHCRC
        }
        if b.overrun() {
            return Err(Invalid("coding header cut off"));
        }
        Ok(c)
    }

    /// Decode one subframe starting at subband sample `slot` of the frame
    /// into `pcm`: the side information, the audio data, and their
    /// reconstruction. Returns its subband samples per subband.
    fn subframe(&mut self, b: &mut Bits, h: &Header, c: &Coding, slot: usize, pcm: &mut [Vec<f32>]) -> Result<usize, FrameError> {
        let FrameDecoder { books, modulation, qmf, lfe: lfe_filter, samples, synth, side, features, dynamic_range } = self;
        let nch = c.channels;
        let nssc = b.read(2) as usize + 1;
        b.skip(3); // PSC: the partial subsubframe of termination frames
        let len = 8 * nssc;
        if slot + len > h.blocks {
            return Err(Invalid("the subframes overrun the frame"));
        }
        features.subsubframes |= nssc > 1;
        **side = Side::default();
        // prediction modes and coefficients
        for ch in 0..nch {
            for n in 0..c.subs[ch] {
                side.pmode[ch][n] = b.flag();
            }
        }
        for ch in 0..nch {
            for n in 0..c.subs[ch] {
                if side.pmode[ch][n] {
                    side.pvq[ch][n] = b.read(12) as u16;
                }
            }
        }
        // bit allocation
        for ch in 0..nch {
            for n in 0..c.vqsub[ch] {
                let abits = match c.bhuff[ch] {
                    5 => b.read(4),
                    6 => b.read(5),
                    book => {
                        features.huffman_bit_allocation = true;
                        books.bit_alloc[book].decode(b) as u32
                    }
                };
                if abits > 26 {
                    return Err(Invalid("bit allocation index above 26"));
                }
                side.abits[ch][n] = abits as u8;
            }
        }
        // transient modes
        if nssc > 1 {
            for ch in 0..nch {
                for n in 0..c.vqsub[ch] {
                    if side.abits[ch][n] > 0 {
                        side.tmode[ch][n] = books.tmode[c.thuff[ch]].decode(b) as u8;
                    }
                }
            }
        }
        // scale factors
        for ch in 0..nch {
            let shuff = c.shuff[ch];
            features.huffman_scales |= shuff < 5;
            features.scales_7bit |= shuff == 6;
            let mut sum = 0i32;
            for n in 0..c.vqsub[ch] {
                if side.abits[ch][n] > 0 {
                    side.scales[ch][n][0] = scale(books, b, shuff, &mut sum)?;
                    if side.tmode[ch][n] > 0 {
                        features.transients = true;
                        side.scales[ch][n][1] = scale(books, b, shuff, &mut sum)?;
                    }
                }
            }
            for n in c.vqsub[ch]..c.subs[ch] {
                side.scales[ch][n][0] = scale(books, b, shuff, &mut sum)?;
            }
        }
        // joint intensity
        let mut join_shuff = [0; MAX_CHANNELS];
        for (book, &joinx) in join_shuff.iter_mut().zip(&c.joinx).take(nch) {
            if joinx > 0 {
                *book = b.read(3) as usize;
                if *book == 7 {
                    return Err(Invalid("joint scale code book 7"));
                }
            }
        }
        for (ch, &book) in join_shuff.iter().enumerate().take(nch) {
            if c.joinx[ch] > 0 {
                let src = c.joinx[ch] - 1;
                for n in c.subs[ch]..c.subs[src] {
                    let v = match book {
                        5 => b.read(6) as i32,
                        6 => b.read(7) as i32,
                        book => books.scales[book].decode(b),
                    } + 64;
                    if !(0..=128).contains(&v) {
                        return Err(Invalid("joint scale factor index out of range"));
                    }
                    side.joint[ch][n] = tables::JOINT_SCALES[v as usize] as f64;
                }
            }
        }
        if h.fixed_bit && nch > 2 {
            // V1.2.1's embedded downmix indexes (DOWN), two per channel,
            // in streams from before the bit was reserved
            b.skip(14 * nch);
        }
        if h.dynamic_range {
            side.range = Some(b.read(8) as u8);
        }
        if h.crc_present {
            b.skip(16); // SICRC
        }
        if b.overrun() {
            return Err(Invalid("side information cut off"));
        }

        // audio data: first the high frequency vectors
        for ch in samples.iter_mut().take(nch) {
            for band in ch.iter_mut() {
                band[4..].fill(0.0);
            }
        }
        for ch in 0..nch {
            for n in c.vqsub[ch]..c.subs[ch] {
                features.high_frequency_vq = true;
                let vector = &vq::HIGH_FREQUENCY[b.read(10) as usize];
                let s = side.scales[ch][n][0] / 16.0;
                for m in 0..len {
                    samples[ch][n][4 + m] = clip24(s * vector[m] as f64);
                }
                if side.pmode[ch][n] {
                    // (the vector is the prediction residual, as ffmpeg
                    // has it: PMODE and PVQ come for every active subband)
                    features.adpcm = true;
                    predict(&mut samples[ch][n], vq::ADPCM[side.pvq[ch][n] as usize], 4, 4 + len);
                }
            }
        }
        // the LFE channel's decimated samples
        let mut lfe = [0.0f64; 16];
        let nlfe = 2 * h.lff as usize * nssc;
        if h.lff > 0 {
            let mut codes = [0i32; 16];
            for code in codes.iter_mut().take(nlfe) {
                *code = b.read_signed(8);
            }
            let index = b.read(8) as usize;
            if index >= tables::RMS7.len() {
                return Err(Invalid("LFE scale factor index above 124"));
            }
            let s = tables::RMS7[index] as f64 * tables::LFE_STEP;
            for (v, &code) in lfe.iter_mut().zip(&codes).take(nlfe) {
                *v = clip24(code as f64 * s);
            }
        }
        // the subsubframes
        let steps = if h.rate == 31 { &tables::STEP_LOSSLESS } else { &tables::STEP_LOSSY };
        for sub in 0..nssc {
            for ch in 0..nch {
                for n in 0..c.vqsub[ch] {
                    let abits = side.abits[ch][n] as usize;
                    let sel = if (1..=10).contains(&abits) { c.sel[ch][abits - 1] } else { 0 };
                    let mut levels = [0i32; 8];
                    read_levels(books, features, b, abits, sel, &mut levels)?;
                    let tmode = side.tmode[ch][n] as usize;
                    let which = if tmode > 0 && sub >= tmode { 1 } else { 0 };
                    let adj = if (1..=10).contains(&abits) { c.adj[ch][abits - 1] } else { 1.0 };
                    let step = steps[abits] as f64 / (1 << 22) as f64;
                    let q = step * side.scales[ch][n][which] * adj;
                    let x = &mut samples[ch][n];
                    let at = 4 + 8 * sub;
                    for m in 0..8 {
                        x[at + m] = clip24(levels[m] as f64 * q);
                    }
                    if side.pmode[ch][n] {
                        features.adpcm = true;
                        predict(x, vq::ADPCM[side.pvq[ch][n] as usize], at, at + 8);
                    }
                }
            }
            if (sub == nssc - 1 || h.dsync_every_subsubframe) && b.read(16) != 0xffff {
                return Err(Invalid("DSYNC check failed"));
            }
        }
        if b.overrun() {
            return Err(Invalid("audio data cut off"));
        }

        // reconstruction: joint intensity subbands, copied into the
        // channel's own subband samples
        for ch in 0..nch {
            if c.joinx[ch] > 0 {
                features.joint_intensity = true;
                let src = c.joinx[ch] - 1;
                for n in c.subs[ch]..c.subs[src] {
                    let g = side.joint[ch][n];
                    for m in 4..4 + len {
                        samples[ch][n][m] = clip24(g * samples[src][n][m]);
                    }
                }
            }
        }
        // subbands above the active ones are silent (also those decoded
        // up to nVQSUB above nSUBS), and the filter banks' input
        for ch in 0..nch {
            let active = if c.joinx[ch] > 0 { c.subs[ch].max(c.subs[c.joinx[ch] - 1]) } else { c.subs[ch] };
            for n in 0..32 {
                if n < active {
                    synth[ch][n][..len].copy_from_slice(&samples[ch][n][4..4 + len]);
                } else {
                    samples[ch][n][4..4 + len].fill(0.0);
                    synth[ch][n][..len].fill(0.0);
                }
            }
        }
        // the predictors' history: the subbands as decoded, before
        // sum/difference
        for ch in samples.iter_mut().take(nch) {
            for band in ch.iter_mut() {
                band.copy_within(len..len + 4, 0);
            }
        }
        if h.sum_front || h.amode == 3 {
            if let Some((l, r)) = front_pair(h.amode) {
                sum_difference(synth, l, r, len);
            }
        }
        if h.sum_surround {
            if let Some((l, r)) = surround_pair(h.amode) {
                sum_difference(synth, l, r, len);
            }
        }
        let coeff = if h.perfect { &fir::PERFECT } else { &fir::NONPERFECT };
        let gain = match side.range {
            Some(r) if *dynamic_range => tables::range_gain(r),
            _ => 1.0,
        } * OUTPUT_SCALE;
        let mut out = [0.0f64; 32];
        for ch in 0..nch {
            let dst = &mut pcm[ch][slot * 32..(slot + len) * 32];
            for m in 0..len {
                let mut input = [0.0f64; 32];
                for (n, v) in input.iter_mut().enumerate() {
                    *v = synth[ch][n][m];
                }
                qmf[ch].synthesize(modulation, &input, coeff, &mut out);
                for (d, &v) in dst[m * 32..m * 32 + 32].iter_mut().zip(&out) {
                    *d = (v * gain) as f32;
                }
            }
        }
        if h.lff > 0 {
            let factor = if h.lff == 1 { 128 } else { 64 };
            let mut out = [0.0f64; 1024];
            lfe_filter.interpolate(&lfe[..nlfe], factor, &mut out[..nlfe * factor]);
            let dst = &mut pcm[nch][slot * 32..(slot + len) * 32];
            for (d, &v) in dst.iter_mut().zip(&out) {
                *d = (v * LFE_SCALE) as f32;
            }
        }
        Ok(len)
    }
}

/// Inverse ADPCM (C.3.3) over `x[from..to]`: add to each sample the
/// prediction from the four before it, with coefficients `k` (D.10.1),
/// held to 24 bits (see `clip24`).
fn predict(x: &mut [f64; 36], k: [i16; 4], from: usize, to: usize) {
    let k = k.map(|v| v as f64 / 8192.0);
    for m in from..to {
        x[m] = clip24(x[m] + k[0] * x[m - 1] + k[1] * x[m - 2] + k[2] * x[m - 3] + k[3] * x[m - 4]);
    }
}

/// Read the quantization indexes of one subband in one subsubframe
/// (Table 5-29) for quantizer `abits` and code book `sel`.
fn read_levels(books: &Books, features: &mut Features, b: &mut Bits, abits: usize, sel: usize, out: &mut [i32; 8]) -> Result<(), FrameError> {
    match abits {
        0 => {}
        1..=10 if sel < block_or_plain_sel(abits) => {
            features.huffman_samples = true;
            let tree = &books.audio[abits - 1][sel];
            for v in out.iter_mut() {
                *v = tree.decode(b);
            }
        }
        1..=7 => {
            features.block_codes = true;
            let levels = LEVELS[abits - 1];
            let offset = (levels as i32 - 1) / 2;
            for half in 0..2 {
                let mut code = b.read(BLOCK_BITS[abits - 1]);
                for v in out[4 * half..4 * half + 4].iter_mut() {
                    *v = (code % levels) as i32 - offset;
                    code /= levels;
                }
                if code != 0 {
                    return Err(Invalid("block code out of range"));
                }
            }
        }
        _ => {
            // no further encoding: ABITS - 3 bits of two's complement
            features.plain_samples = true;
            let bits = abits as u32 - 3;
            for v in out.iter_mut() {
                *v = b.read_signed(bits);
            }
        }
    }
    Ok(())
}

/// The SEL value that means a block code (ABITS 1 to 7) or no further
/// encoding (8 to 10): the last of its quantizer's group (Table 5-26).
fn block_or_plain_sel(abits: usize) -> usize {
    match abits {
        1 => 1,
        2..=5 => 3,
        _ => 7,
    }
}

/// One scale factor (SCALES): a Huffman coded difference (SHUFF 0 to 4)
/// or a 6 or 7-bit index, looked up in the 6 or 7-bit table.
fn scale(books: &Books, b: &mut Bits, shuff: usize, sum: &mut i32) -> Result<f64, FrameError> {
    match shuff {
        5 => *sum = b.read(6) as i32,
        6 => *sum = b.read(7) as i32,
        book => *sum += books.scales[book].decode(b),
    }
    let table: &[u32] = if shuff == 6 { &tables::RMS7 } else { &tables::RMS6 };
    table.get(usize::try_from(*sum).map_err(|_| Invalid("negative scale factor index"))?).map(|&v| v as f64).ok_or(Invalid("scale factor index out of range"))
}

/// The front left and right channels of an arrangement, coded order.
fn front_pair(amode: u8) -> Option<(usize, usize)> {
    match amode {
        2..=4 | 6 | 8 => Some((0, 1)),
        5 | 7 | 9 => Some((1, 2)),
        _ => None,
    }
}

/// The surround pair of an arrangement with two surround channels.
fn surround_pair(amode: u8) -> Option<(usize, usize)> {
    match amode {
        8 => Some((2, 3)),
        9 => Some((3, 4)),
        _ => None,
    }
}

/// C.3.5: the pair (l, r) holds l + r and l - r.
fn sum_difference(synth: &mut [[[f64; 32]; 32]; MAX_CHANNELS], l: usize, r: usize, len: usize) {
    let (first, rest) = synth.split_at_mut(r);
    for (lb, rb) in first[l].iter_mut().zip(rest[0].iter_mut()) {
        for (a, d) in lb[..len].iter_mut().zip(rb[..len].iter_mut()) {
            (*a, *d) = (*a + *d, *a - *d);
        }
    }
}

/// The embedded downmix of the auxiliary data (5.8.2), if `aux` holds
/// one whose sync word and CRC check: its type and coefficients.
fn aux_downmix(aux: &[u8], channels: usize) -> Option<Downmix> {
    if aux.len() < 8 || aux[..4] != [0x9a, 0x11, 0x05, 0xa0] {
        return None;
    }
    let mut b = Bits::new(aux);
    b.skip(32);
    if b.flag() {
        // the decode time stamp, from the next multiple of 4 bits
        b.align(4);
        b.skip(8 + 4 + 28 + 4);
    }
    let mut downmix = None;
    if b.flag() {
        let kind = b.read(3) as u8;
        let rows = match kind {
            0 => 1,
            1 | 2 => 2,
            3 | 4 => 3,
            5 | 6 => 4,
            _ => return None,
        };
        let mut m = vec![vec![0f32; channels]; rows];
        for row in m.iter_mut() {
            for v in row.iter_mut() {
                let code = b.read(9);
                let index = (code & 0xff) as usize;
                if index > 0 {
                    let abs = *tables::DMIX_TABLE.get(index - 1)? as f32 / 32768.0;
                    *v = if code & 0x100 != 0 { abs } else { -abs };
                }
            }
        }
        downmix = Some((kind, m));
    }
    b.align(8);
    let end = b.position() / 8;
    if b.overrun() || end + 2 > aux.len() {
        return None;
    }
    let stored = u16::from_be_bytes([aux[end], aux[end + 1]]);
    if header::crc16(&aux[4..end]) != stored {
        return None;
    }
    downmix
}
