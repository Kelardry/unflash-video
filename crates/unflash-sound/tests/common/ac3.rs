//! AC-3's part of `common`, for its tests and `examples/fate/ac3.rs`: a
//! comparison with ffmpeg's decode that separates what two correct
//! decoders must agree on from the noise they are free to differ in, and
//! the editing and building of frames.
//!
//! Dither (zero-bit mantissas) and spectral extension noise make two
//! correct decoders differ, but only in known transform coefficients:
//! the decoder's trace says which ones, block by block. The transform
//! with its window reconstructs perfectly, so the forward transform
//! (A/52 §8.2.3.2) of either decoder's output gives back that decoder's
//! coefficients, block by block. Every other coefficient must then agree
//! to float precision, and the noisy ones must differ by noise of the
//! same loudness as ours.

use unflash_sound::ac3::testing::{crc16, frame_bytes};
use unflash_sound::ac3::{BlockTrace, Decoded, Decoder, Features, Output, StreamInfo};

use super::BitWriter;

/// How far apart two float decoders' coefficients may be.
pub const TOLERANCE: f64 = 1e-5;
/// For coefficients the spectral extension made: ffmpeg 6.1's differ from
/// what Annex E's arithmetic gives by up to about 2e-4 of their value
/// when a dynamic range gain is applied (as if they went through a fixed
/// point format), a few times 1e-5 in all.
pub const SPX_TOLERANCE: f64 = 1e-4;

/// One run of our decoder over a whole stream.
pub struct Ours {
    pub out: Vec<Vec<f32>>,
    pub trace: Vec<BlockTrace>,
    pub decoded: Decoded,
    pub features: Features,
    pub skipped_substream_frames: u64,
    pub info: Option<StreamInfo>,
}

/// Decode `data` all at once, with a trace.
pub fn decode(data: &[u8], output: Output, noise: bool) -> Ours {
    let mut dec = Decoder::new(output);
    dec.set_noise(noise);
    dec.set_trace(true);
    let mut out = Vec::new();
    let decoded = dec.decode(data, &mut out).expect("a sync frame");
    let (features, skipped) = dec.features();
    Ours { out, trace: dec.take_trace(), decoded, features, skipped_substream_frames: skipped, info: dec.info() }
}

// ---------------------------------------------------------------------
// The forward transform, fast enough for unoptimised test builds.

struct Fft {
    n: usize,
    twiddle: Vec<(f64, f64)>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        let twiddle = (0..n / 2).map(|k| {
            let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
            (a.cos(), a.sin())
        });
        Fft { n, twiddle: twiddle.collect() }
    }

    /// In-place forward complex FFT (e^-i).
    fn run(&self, re: &mut [f64], im: &mut [f64]) {
        let n = self.n;
        let bits = n.trailing_zeros();
        for i in 0..n {
            let j = i.reverse_bits() >> (usize::BITS - bits);
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut m = 2;
        while m <= n {
            let half = m / 2;
            let step = n / m;
            for s in (0..n).step_by(m) {
                for k in 0..half {
                    let (wr, wi) = self.twiddle[k * step];
                    let (br, bi) = (re[s + k + half], im[s + k + half]);
                    let (tr, ti) = (br * wr - bi * wi, br * wi + bi * wr);
                    re[s + k + half] = re[s + k] - tr;
                    im[s + k + half] = im[s + k] - ti;
                    re[s + k] += tr;
                    im[s + k] += ti;
                }
            }
            m *= 2;
        }
    }
}

/// The forward transform of §8.2.3.2 through the transform window.
pub struct Analyzer {
    window: Vec<f64>,
    fft128: Fft,
    fft64: Fft,
}

impl Default for Analyzer {
    fn default() -> Self {
        Analyzer::new()
    }
}

impl Analyzer {
    pub fn new() -> Analyzer {
        // the Kaiser-Bessel-derived window, alpha 5 (Table 7.33)
        fn i0(x: f64) -> f64 {
            let (mut s, mut t, mut k) = (1.0, 1.0, 1.0);
            while t > 1e-20 * s {
                t *= (x / (2.0 * k)).powi(2);
                s += t;
                k += 1.0;
            }
            s
        }
        let kaiser: Vec<f64> = (0..=256).map(|n| i0(std::f64::consts::PI * 5.0 * (1.0 - (2.0 * n as f64 / 256.0 - 1.0).powi(2)).max(0.0).sqrt())).collect();
        let total: f64 = kaiser.iter().sum();
        let mut half = Vec::with_capacity(256);
        let mut acc = 0.0;
        for k in kaiser.iter().take(256) {
            acc += k;
            half.push((acc / total).sqrt());
        }
        let window = (0..512).map(|n| if n < 256 { half[n] } else { half[511 - n] }).collect();
        Analyzer { window, fft128: Fft::new(128), fft64: Fft::new(64) }
    }

    /// DCT-IV of `u` (length 256 or 128) through a half-length FFT.
    fn dct4(&self, u: &[f64], out: &mut [f64]) {
        let m = u.len();
        let h = m / 2;
        let fft = if h == 128 { &self.fft128 } else { &self.fft64 };
        let (mut re, mut im) = (vec![0.0; h], vec![0.0; h]);
        for j in 0..h {
            let (a, b) = (u[2 * j], u[m - 1 - 2 * j]);
            let t = -std::f64::consts::PI * (4 * j + 1) as f64 / (4 * m) as f64;
            let (c, s) = (t.cos(), t.sin());
            re[j] = a * c - b * s;
            im[j] = a * s + b * c;
        }
        fft.run(&mut re, &mut im);
        for j in 0..h {
            let t = -std::f64::consts::PI * j as f64 / m as f64;
            let (c, s) = (t.cos(), t.sin());
            out[2 * j] = re[j] * c - im[j] * s;
            out[m - 1 - 2 * j] = -(re[j] * s + im[j] * c);
        }
    }

    /// X[k] = sum z[n] cos(2 pi / 4N (2n + 1)(2k + 1) + pi / 4 (2k + 1)(1 + alpha)),
    /// by folding z into a DCT-IV.
    fn mdct(&self, z: &[f64], alpha: i32, out: &mut [f64]) {
        let n = z.len();
        let m = n / 2;
        let shift = (n as i32 / 4) * (1 + alpha);
        let mut u = vec![0.0; m];
        for (t, &v) in z.iter().enumerate() {
            let mut k = t as i32 + shift;
            let mut sign = 1.0;
            while k >= 2 * m as i32 {
                k -= 2 * m as i32;
                sign = -sign;
            }
            while k < 0 {
                k += 2 * m as i32;
                sign = -sign;
            }
            if k >= m as i32 {
                k = 2 * m as i32 - 1 - k;
                sign = -sign;
            }
            u[k as usize] += sign * v;
        }
        self.dct4(&u, out);
    }

    /// The coefficients of block `b` of one output channel: the forward
    /// transform of its samples 256 b to 256 b + 512 (a long block, or a
    /// switched block's two short transforms interleaved).
    pub fn block(&self, x: &[f32], b: usize, short: bool) -> [f64; 256] {
        let seg = &x[256 * b..256 * b + 512];
        let mut out = [0f64; 256];
        if !short {
            let z: Vec<f64> = (0..512).map(|n| -2.0 / 512.0 * self.window[n] * seg[n] as f64).collect();
            self.mdct(&z, 0, &mut out);
        } else {
            for (t, alpha) in [(0usize, -1i32), (1, 1)] {
                let z: Vec<f64> = (0..256).map(|n| -2.0 / 256.0 * self.window[256 * t + n] * seg[256 * t + n] as f64).collect();
                let mut half = [0f64; 128];
                self.mdct(&z, alpha, &mut half);
                for k in 0..128 {
                    out[2 * k + t] = half[k];
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------
// The comparison.

/// Noise energies in one kind of noisy coefficient.
#[derive(Clone, Copy, Debug, Default)]
pub struct Noise {
    pub bins: usize,
    /// Sum of (ffmpeg - ours without noise)^2: ffmpeg's noise.
    pub theirs: f64,
    /// Sum of (ours with noise - ours without)^2: our noise.
    pub ours: f64,
}

impl Noise {
    /// ffmpeg's noise over ours, in amplitude.
    pub fn ratio(&self) -> f64 {
        (self.theirs / self.ours).sqrt()
    }

    pub fn rms_ours(&self) -> f64 {
        (self.ours / self.bins.max(1) as f64).sqrt()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChannelStats {
    /// Blocks compared, and blocks left out: those next to a switched
    /// block (ffmpeg 6.1 takes the overlap of the wrong channel at a
    /// switched block) or to a frame output as silence.
    pub blocks: usize,
    pub skipped_blocks: usize,
    /// Noise-free coefficients compared, the largest difference among
    /// them, and how many differ by more than `TOLERANCE`. Differences
    /// are relative to the block's largest coefficient or sample where
    /// that is above 1 (floats: frames built at random can go far above
    /// full scale).
    pub bins: usize,
    pub max_diff: f64,
    pub over: usize,
    /// Coefficients of the AHT's 32-entry vector quantizer (hebap 4),
    /// which ffmpeg 6.1 decodes with the wrong table row: how many, how
    /// many differ, and by how much at most.
    pub hebap4_bins: usize,
    pub hebap4_over: usize,
    pub hebap4_max_diff: f64,
    pub dither: Noise,
    pub aht_dither: Noise,
    pub spx: Noise,
    /// Time domain: the largest difference from ffmpeg in blocks where
    /// our output has no noise, and how many such blocks there were.
    pub quiet_blocks: usize,
    pub quiet_max_diff: f64,
    /// Time domain, the whole stream (less the blocks left out):
    /// rms(ffmpeg - ours without noise) over rms(ours with noise - ours
    /// without); NaN without noise.
    pub time_ratio: f64,
    /// rms of our noise in the time domain.
    pub time_noise_rms: f64,
}

/// Compare our output (without and with noise, and the trace of the
/// run with noise) with ffmpeg's. `map[c]` is the reference channel for
/// our output channel `c`, or `None` to leave it out.
pub fn compare(quiet: &[Vec<f32>], noisy: &[Vec<f32>], trace: &[BlockTrace], reference: &[Vec<f32>], map: &[Option<usize>]) -> Vec<ChannelStats> {
    compare_with(quiet, noisy, trace, reference, map, TOLERANCE)
}

/// `compare` with another tolerance for the coefficients outside the
/// spectral extension.
pub fn compare_with(quiet: &[Vec<f32>], noisy: &[Vec<f32>], trace: &[BlockTrace], reference: &[Vec<f32>], map: &[Option<usize>], tolerance: f64) -> Vec<ChannelStats> {
    let analyzer = Analyzer::new();
    let mut stats = vec![ChannelStats::default(); quiet.len()];
    let len = quiet.iter().chain(noisy).map(|c| c.len()).min().unwrap_or(0);
    let len = map.iter().flatten().map(|&r| reference[r].len()).fold(len, usize::min);
    let blocks = (len / 256).min(trace.len());
    // blocks whose coefficients cannot be compared: a switched block (any
    // channel) or a damaged frame's block at b or b + 1
    let bad = |b: usize| -> bool {
        [b, b + 1].iter().any(|&x| x < trace.len() && trace[x].channels.iter().any(|t| t.blksw || t.noisy == [!0; 4]))
    };
    for (c, s) in stats.iter_mut().enumerate() {
        let Some(r) = map.get(c).copied().flatten() else { continue };
        let (q, n, f) = (&quiet[c], &noisy[c], &reference[r]);
        // time domain, over the output blocks whose samples come from
        // comparable blocks: not switched or damaged, nor the block before
        let comparable = |b: usize| -> bool {
            let clean = |x: usize| x >= trace.len() || !trace[x].channels.iter().any(|t| t.blksw || t.noisy == [!0; 4]);
            clean(b) && (b == 0 || clean(b - 1))
        };
        let (mut sq_theirs, mut sq_ours, mut samples) = (0f64, 0f64, 0usize);
        for b in 0..len / 256 {
            if !comparable(b) {
                continue;
            }
            let range = 256 * b..256 * b + 256;
            for i in range.clone() {
                sq_theirs += (f[i] as f64 - q[i] as f64).powi(2);
                sq_ours += (n[i] as f64 - q[i] as f64).powi(2);
            }
            samples += 256;
            if n[range.clone()] == q[range.clone()] {
                s.quiet_blocks += 1;
                for i in range {
                    s.quiet_max_diff = s.quiet_max_diff.max((q[i] - f[i]).abs() as f64);
                }
            }
        }
        s.time_ratio = if sq_ours > 0.0 { (sq_theirs / sq_ours).sqrt() } else { f64::NAN };
        s.time_noise_rms = (sq_ours / samples.max(1) as f64).sqrt();
        // coefficients
        for b in 0..blocks.saturating_sub(1) {
            if 256 * b + 512 > len {
                break;
            }
            if bad(b) {
                s.skipped_blocks += 1;
                continue;
            }
            s.blocks += 1;
            let t = &trace[b].channels[c];
            let (aq, an, af) = (analyzer.block(q, b, false), analyzer.block(n, b, false), analyzer.block(f, b, false));
            // (the samples the block is read from hold its neighbours too)
            let peak = f[256 * b..256 * b + 512].iter().fold(0f64, |m, &v| m.max((v as f64).abs()));
            let scale = af.iter().fold(peak.max(1.0), |m, v| m.max(v.abs()));
            for k in 0..256 {
                let d = (aq[k] - af[k]).abs() / scale;
                if t.is_hebap4(k) {
                    s.hebap4_bins += 1;
                    s.hebap4_max_diff = s.hebap4_max_diff.max(d);
                    if d > tolerance {
                        s.hebap4_over += 1;
                    }
                } else if t.is_noisy(k) {
                    let noise = if t.is_spx(k) {
                        &mut s.spx
                    } else if t.is_aht_dither(k) {
                        &mut s.aht_dither
                    } else {
                        &mut s.dither
                    };
                    noise.bins += 1;
                    noise.theirs += (af[k] - aq[k]).powi(2);
                    noise.ours += (an[k] - aq[k]).powi(2);
                } else {
                    s.bins += 1;
                    s.max_diff = s.max_diff.max(d);
                    if d > if t.is_spx(k) { SPX_TOLERANCE.max(tolerance) } else { tolerance } {
                        s.over += 1;
                    }
                }
            }
        }
    }
    stats
}

/// A short line about a channel's comparison.
pub fn describe(s: &ChannelStats) -> String {
    let mut line = format!("{} bins exact to {:.1e}", s.bins, s.max_diff);
    if s.over > 0 {
        line += &format!(" ({} OVER {TOLERANCE:.0e})", s.over);
    }
    for (name, n) in [("dither", s.dither), ("AHT dither", s.aht_dither), ("SPX noise", s.spx)] {
        if n.bins > 0 && n.ours > 0.0 {
            line += &format!(", {name} ratio {:.2} ({} bins)", n.ratio(), n.bins);
        }
    }
    if s.hebap4_bins > 0 {
        line += &format!(", hebap-4 bins {}/{} differ (max {:.1e})", s.hebap4_over, s.hebap4_bins, s.hebap4_max_diff);
    }
    if !s.time_ratio.is_nan() {
        line += &format!(", time ratio {:.2}", s.time_ratio);
    }
    if s.quiet_blocks > 0 {
        line += &format!(", {} noise-free blocks max diff {:.1e}", s.quiet_blocks, s.quiet_max_diff);
    }
    if s.skipped_blocks > 0 {
        line += &format!(", {} blocks left out", s.skipped_blocks);
    }
    line
}

// ---------------------------------------------------------------------
// Editing streams.

/// The sync frames of an elementary stream (whole frames only), found by
/// their sizes.
pub fn frames(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut v = Vec::new();
    let mut pos = 0;
    while pos + 6 <= data.len() {
        let size = frame_bytes(&data[pos..]).unwrap_or_else(|| panic!("no frame at byte {pos}"));
        if pos + size > data.len() {
            break;
        }
        v.push(pos..pos + size);
        pos += size;
    }
    v
}

/// Make a frame's CRC words check again after an edit: E-AC-3's crc2 over
/// the whole frame; AC-3's crc1 over its first 5/8 (crc1 comes first in
/// the span it covers, so it is solved for) and crc2 over the rest.
pub fn fix_crcs(f: &mut [u8]) {
    let n = f.len();
    if f[5] >> 3 > 10 {
        let c = crc16(&f[2..n - 2]);
        f[n - 2..].copy_from_slice(&c.to_be_bytes());
        return;
    }
    let words = n / 2;
    let split = ((words >> 1) + (words >> 3)) * 2;
    // the register is linear in crc1: find crc1 so that the span checks
    f[2] = 0;
    f[3] = 0;
    let target = crc16(&f[2..split]);
    let mut basis = Vec::new();
    for bit in 0..16 {
        let mut probe = vec![0u8; split - 2];
        probe[0] = ((1u16 << bit) >> 8) as u8;
        probe[1] = (1u16 << bit) as u8;
        basis.push((crc16(&probe), 1u16 << bit));
    }
    // Gaussian elimination over GF(2): find x with sum of basis[i].0 = target
    let mut rows = basis;
    let mut solution = 0u16;
    let mut t = target;
    for bit in (0..16).rev() {
        let Some(p) = (0..rows.len()).find(|&i| rows[i].0 >> bit & 1 == 1) else { continue };
        let pivot = rows.remove(p);
        for r in rows.iter_mut() {
            if r.0 >> bit & 1 == 1 {
                r.0 ^= pivot.0;
                r.1 ^= pivot.1;
            }
        }
        if t >> bit & 1 == 1 {
            t ^= pivot.0;
            solution ^= pivot.1;
        }
    }
    assert_eq!(t, 0, "crc1 has no solution");
    f[2..4].copy_from_slice(&solution.to_be_bytes());
    let c = crc16(&f[split..n - 2]);
    f[n - 2..].copy_from_slice(&c.to_be_bytes());
}

// ---------------------------------------------------------------------
// Building frames that ffmpeg's encoders do not make.

/// Exponents of a full bandwidth channel up to bin 253 (chbwcod 60), all
/// `exp`: an absolute exponent and 21 D45 groups of no change.
fn put_flat_exponents(w: &mut BitWriter, exp: u32) {
    w.put(4, exp);
    for _ in 0..21 {
        w.put(7, 2 * 25 + 2 * 5 + 2);
    }
    w.put(2, 0); // gainrng
}

/// `count` AC-3 frames (48 kHz, 128 kbit/s, bsid 8) in 1+1 dual mono
/// whose SNR offsets are all zero, so that no mantissa has bits (A/52
/// §7.2.2.1.1): Ch1 dithered with every exponent `exp1`, Ch2 not dithered
/// (silence). `dynrng2` is Ch2's dynamic range word in block 0.
pub fn dual_mono_frames(count: usize, exp1: u32, dynrng2: u8) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..count {
        let mut w = BitWriter::default();
        w.put(16, 0x0b77);
        w.put(16, 0); // crc1
        w.put(2, 0); // fscod: 48 kHz
        w.put(6, 16); // frmsizecod: 128 kbit/s, 256 words
        w.put(5, 8); // bsid
        w.put(3, 0); // bsmod
        w.put(3, 0); // acmod 1+1
        w.put(1, 0); // lfeon
        w.put(5, 27); // dialnorm
        w.put(3, 0); // compre, langcode, audprodie
        w.put(5, 27); // dialnorm2
        w.put(3, 0); // compr2e, langcod2e, audprodi2e
        w.put(2, 0); // copyrightb, origbs
        w.put(3, 0); // timecod1e, timecod2e, addbsie
        for blk in 0..6 {
            w.put(2, 0); // blksw
            w.put(2, 0b10); // dithflag: Ch1 on, Ch2 off
            w.put(1, 0); // dynrnge
            if blk == 0 {
                w.put(1, 1); // dynrng2e
                w.put(8, dynrng2 as u32);
            } else {
                w.put(1, 0);
            }
            if blk == 0 {
                w.put(2, 0b10); // cplstre, cplinu 0
            } else {
                w.put(1, 0);
            }
            let strategy = if blk == 0 { 3 } else { 0 };
            w.put(2, strategy);
            w.put(2, strategy);
            if blk == 0 {
                w.put(6, 60); // chbwcod
                w.put(6, 60);
                put_flat_exponents(&mut w, exp1);
                put_flat_exponents(&mut w, 12);
                w.put(1, 1); // baie
                w.put(2, 2); // sdcycod
                w.put(2, 1); // fdcycod
                w.put(2, 1); // sgaincod
                w.put(2, 2); // dbpbcod
                w.put(3, 4); // floorcod
                w.put(1, 1); // snroffste
                w.put(6, 0); // csnroffst
                w.put(7, 4); // fsnroffst 0, fgaincod 4
                w.put(7, 4);
            } else {
                w.put(2, 0); // baie, snroffste
            }
            w.put(2, 0); // deltbaie, skiple
        }
        w.bytes.resize(512, 0);
        fix_crcs(&mut w.bytes);
        out.extend_from_slice(&w.bytes);
    }
    out
}

/// `count` E-AC-3 frames (48 kHz, stereo) of `blocks` (1, 2, 3 or 6)
/// blocks with all SNR offsets zero (dither only; E-AC-3 without dither
/// flags dithers every channel), exponents `exp` in every block.
pub fn eac3_frames(count: usize, blocks: usize, exp: u32) -> Vec<u8> {
    let numblkscod = match blocks {
        1 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    };
    let words = 64 * blocks; // plenty
    let mut out = Vec::new();
    for _ in 0..count {
        let mut w = BitWriter::default();
        w.put(16, 0x0b77);
        w.put(2, 0); // strmtyp
        w.put(3, 0); // substreamid
        w.put(11, words as u32 - 1); // frmsiz
        w.put(2, 0); // fscod
        w.put(2, numblkscod);
        w.put(3, 2); // acmod 2/0
        w.put(1, 0); // lfeon
        w.put(5, 16); // bsid
        w.put(5, 27); // dialnorm
        w.put(1, 0); // compre
        w.put(2, 0); // mixmdate, infomdate
        if numblkscod != 3 {
            w.put(1, 0); // convsync
        }
        w.put(1, 0); // addbsie
        // audfrm
        if numblkscod == 3 {
            w.put(2, 0b10); // expstre 1, ahte 0
        }
        w.put(2, 0); // snroffststr: frame SNR offsets
        w.put(8, 0); // transproce, blkswe, dithflage, bamode, frmfgaincode, dbaflde, skipflde, spxattene
        w.put(1, 0); // cplinu[0]
        for _ in 1..blocks {
            w.put(1, 0); // cplstre
        }
        for blk in 0..blocks {
            let s = if blk == 0 { 3 } else { 0 };
            w.put(2, s);
            w.put(2, s);
        }
        if numblkscod != 3 {
            w.put(1, 0); // convexpstre
        } else {
            w.put(10, 0); // convexpstr (convexpstre is implied)
        }
        w.put(6, 0); // frmcsnroffst
        w.put(4, 0); // frmfsnroffst
        if numblkscod != 0 {
            w.put(1, 0); // blkstrtinfoe
        }
        for blk in 0..blocks {
            w.put(1, 0); // dynrnge
            if blk == 0 {
                w.put(1, 0); // spxinu
                w.put(4, 0); // rematflg
                w.put(6, 60); // chbwcod
                w.put(6, 60);
                put_flat_exponents(&mut w, exp);
                put_flat_exponents(&mut w, exp);
            } else {
                w.put(1, 0); // spxstre
                w.put(1, 0); // rematstr
            }
            w.put(1, 0); // convsnroffste
        }
        w.bytes.resize(words * 2, 0);
        fix_crcs(&mut w.bytes);
        out.extend_from_slice(&w.bytes);
    }
    out
}
