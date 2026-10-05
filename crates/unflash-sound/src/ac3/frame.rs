//! Decoding one sync frame of independent substream 0 into PCM: the audio
//! frame (E-AC-3) and audio blocks (A/52 §5.3.3, Annex E §2.2.3 and
//! §2.2.4), exponents and bit allocation, mantissas (§7.3, and Annex E
//! §3.4's adaptive hybrid transform), decoupling (§7.4), rematrixing
//! (§7.5), spectral extension (Annex E §3.6), dynamic range control
//! (§7.7.1) and the inverse transform with its overlap-add (§7.9).
//!
//! The state that the syntax lets a block reuse from the block before
//! (exponents, coupling coordinates, bit allocation parameters, ...) is
//! kept in `FrameDecoder` from frame to frame as well: a stream that
//! reuses something in its first block, which the standard does not
//! allow, gets what the last block had rather than an error.

use crate::ac3::bitalloc::{self, Dba, Params};
use crate::ac3::header::Header;
use crate::ac3::imdct::Imdct;
use crate::ac3::tables::{self, BAPTAB, HEBAPTAB, HEBAP_BITS, QNTZTAB};
use crate::ac3::vq;
use crate::bits::Bits;

/// Index of the coupling channel and of the LFE channel in the per
/// channel arrays; full bandwidth channels are 0 to 4 in coded order.
const CPL: usize = 5;
const LFE: usize = 6;

/// 2^-e for exponents 0 to 24.
fn exp_scale(e: u8) -> f32 {
    f32::from_bits((127u32.saturating_sub(e as u32)) << 23)
}

/// A frame that cannot be decoded: malformed, or a feature this decoder
/// does not implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Bitstream(&'static str),
    Unsupported(&'static str),
}

type Result<T> = std::result::Result<T, FrameError>;

fn bad(s: &'static str) -> FrameError {
    FrameError::Bitstream(s)
}

/// A small uniform generator (xorshift64*) for the dither and the
/// spectral extension noise; any uniform source will do (§7.3.4).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    /// Uniform in [-1, 1).
    #[inline]
    pub fn uniform(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        ((v >> 40) as i32 - (1 << 23)) as f32 * (1.0 / (1 << 23) as f32)
    }
}

/// Dither for a zero-bit mantissa: uniform between -0.707 and 0.707, the
/// scaling §7.3.4 calls optimum.
const DITHER_SCALE: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// Spectral extension noise: zero-mean and unit variance (Annex E
/// §3.6.4.2.4), uniform between -sqrt(3) and sqrt(3).
const SPX_NOISE_SCALE: f32 = 1.732_050_8;

/// What the audio frame (`audfrm`, Annex E Table E1.3) of an E-AC-3 frame
/// says; for AC-3 frames the fields that the blocks carry themselves are
/// read there instead.
#[derive(Clone, Default)]
struct AudFrm {
    ahte: bool,
    snroffststr: u8,
    blkswe: bool,
    dithflage: bool,
    bamode: bool,
    frmfgaincode: bool,
    dbaflde: bool,
    skipflde: bool,
    cplstre: [bool; 6],
    cplinu: [bool; 6],
    cplexpstr: [u8; 6],
    chexpstr: [[u8; 5]; 6],
    lfeexpstr: [u8; 6],
    cplahtinu: bool,
    chahtinu: [bool; 5],
    lfeahtinu: bool,
    frmcsnroffst: i32,
    frmfsnroffst: i32,
    spxattencod: [Option<u8>; 5],
}

/// Exponents and bit allocation of one channel.
#[derive(Clone)]
struct Alloc {
    exp: [u8; 256],
    /// `bap` values, or `hebap` values when the channel uses the adaptive
    /// hybrid transform.
    bap: [u8; 256],
    /// Bins start..end carry mantissas.
    start: usize,
    end: usize,
    /// This block's exponent strategy (0 reuse, 1 D15, 2 D25, 3 D45).
    expstr: u8,
    fsnroffst: i32,
    fgaincod: usize,
    dba: Dba,
    /// The adaptive hybrid transform codes this channel in this frame.
    aht: bool,
}

impl Default for Alloc {
    fn default() -> Self {
        Alloc { exp: [24; 256], bap: [0; 256], start: 0, end: 0, expstr: 0, fsnroffst: 0, fgaincod: 4, dba: Dba::default(), aht: false }
    }
}

/// Per full bandwidth channel state.
#[derive(Clone)]
struct Fbw {
    blksw: bool,
    dithflag: bool,
    chbwcod: u8,
    incpl: bool,
    /// Coupling coordinate of each coupling band, 8 times the §7.4.3 value
    /// (the factor 8 of the decoupling equation).
    cplco: [f32; 18],
    /// Coupling coordinates were sent in this block.
    cplcoe: bool,
    inspx: bool,
    /// Spectral extension coordinates of each band, 32 times the Annex E
    /// §3.6.3 value (the factor 32 of §3.6.4.3), and the blending factors.
    spxco: [f32; 17],
    nblend: [f32; 17],
    sblend: [f32; 17],
    firstspxcos: bool,
    firstcplcos: bool,
}

impl Default for Fbw {
    fn default() -> Self {
        Fbw {
            blksw: false,
            dithflag: false,
            chbwcod: 0,
            incpl: false,
            cplco: [0.0; 18],
            cplcoe: false,
            inspx: false,
            spxco: [0.0; 17],
            nblend: [0.0; 17],
            sblend: [0.0; 17],
            firstspxcos: true,
            firstcplcos: true,
        }
    }
}

/// Which syntax features a stream has used, for reports and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    pub block_switching: bool,
    pub coupling: bool,
    pub phase_flags: bool,
    pub rematrixing: bool,
    pub delta_bit_allocation: bool,
    pub dither: bool,
    pub spectral_extension: bool,
    pub spx_attenuation: bool,
    pub aht: bool,
    pub aht_gaq: bool,
    pub aht_vq: bool,
    pub transient_pre_noise: bool,
    pub skip_fields: bool,
    pub dynrng: bool,
    pub lfe: bool,
}

/// The decoder of independent substream 0's frames, with everything it
/// keeps from one block (and frame) to the next.
pub struct FrameDecoder {
    imdct: Imdct,
    rng: Rng,
    /// Dither and spectral extension noise on.
    pub noise: bool,
    /// Coding tools seen so far.
    pub features: Features,
    /// Record a `BlockTrace` for every block (tests).
    pub trace: bool,
    /// Where each block of the last frame ended, in bits (tests).
    pub block_ends: Vec<usize>,
    /// Parse only: no coefficients or samples, and grouped mantissa codes
    /// out of range are listed in `bad_codes` (bit position, width,
    /// largest valid code) instead of failing the frame (tests that build
    /// frames).
    pub parse_only: bool,
    pub bad_codes: Vec<(usize, u32, u32)>,
    pub traces: Vec<BlockTrace>,
    /// The bins of each channel's coefficients (0-4, 5 coupling, 6 LFE)
    /// that carry noise in the block being decoded, and those decoded
    /// with the 32-entry vector quantizer (hebap 4).
    noisy: [[u64; 4]; 7],
    hebap4: [[u64; 4]; 7],
    spx_bins: [[u64; 4]; 7],
    aht_dithered: [[u64; 4]; 7],
    /// The same for the whole frame of a channel the transform codes.
    aht_noisy: [[u64; 4]; 7],
    aht_hebap4: [[u64; 4]; 7],

    // per channel (0-4 full bandwidth, 5 coupling, 6 LFE)
    alloc: Vec<Alloc>,
    fbw: [Fbw; 5],
    /// Transform coefficients of the block being decoded.
    coef: [[f32; 256]; 7],
    /// For the coupling channel: bins whose mantissa had no bits.
    cpl_zero: [bool; 256],
    /// Adaptive hybrid transform: every block's mantissas (before the
    /// exponents), for channels coded with it.
    aht_mant: Vec<[[f32; 256]; 6]>,
    /// Overlap of each output channel (coded order, the LFE channel last
    /// at index 5).
    delay: [[f32; 256]; 6],

    // block state kept from block to block
    dynrng: [f32; 2],
    cplinu: bool,
    phsflginu: bool,
    cplbegf: usize,
    cplendf: i32,
    cplbndstrc: [bool; 18],
    phsflg: [bool; 18],
    rematflg: [bool; 4],
    params: Params,
    csnroffst: i32,
    cplleak: (i32, i32),
    firstcplleak: bool,
    spxinu: bool,
    spxstrtf: usize,
    spxbegf: usize,
    spx_begin: usize,
    spx_end: usize,
    spxbndstrc: [bool; 17],
    /// The frame's spectral extension attenuation code of each channel.
    spx_atten: [Option<u8>; 5],
}

impl FrameDecoder {
    pub fn new() -> FrameDecoder {
        FrameDecoder {
            imdct: Imdct::new(),
            rng: Rng::new(0x9e37_79b9_7f4a_7c15),
            noise: true,
            features: Features::default(),
            trace: false,
            block_ends: Vec::new(),
            parse_only: false,
            bad_codes: Vec::new(),
            traces: Vec::new(),
            noisy: [[0; 4]; 7],
            hebap4: [[0; 4]; 7],
            spx_bins: [[0; 4]; 7],
            aht_dithered: [[0; 4]; 7],
            aht_noisy: [[0; 4]; 7],
            aht_hebap4: [[0; 4]; 7],
            alloc: vec![Alloc::default(); 7],
            fbw: Default::default(),
            coef: [[0.0; 256]; 7],
            cpl_zero: [false; 256],
            aht_mant: vec![[[0.0; 256]; 6]; 7],
            delay: [[0.0; 256]; 6],
            dynrng: [1.0; 2],
            cplinu: false,
            phsflginu: false,
            cplbegf: 0,
            cplendf: 0,
            cplbndstrc: [false; 18],
            phsflg: [false; 18],
            rematflg: [false; 4],
            params: Params::default(),
            csnroffst: 0,
            cplleak: (0, 0),
            firstcplleak: true,
            spxinu: false,
            spxstrtf: 0,
            spxbegf: 0,
            spx_begin: 0,
            spx_end: 0,
            spxbndstrc: [false; 17],
            spx_atten: [None; 5],
        }
    }

    /// Forget the overlap: the next block starts from silence.
    pub fn reset_overlap(&mut self) {
        self.delay = [[0.0; 256]; 6];
    }

    /// Decode a whole frame (its CRC already checked) into `pcm`, one
    /// slice per coded channel (the full bandwidth channels in coded
    /// order, then the LFE channel), each `h.blocks * 256` long.
    pub fn decode(&mut self, frame: &[u8], h: &Header, pcm: &mut [Vec<f32>]) -> Result<()> {
        let mut b = Bits::new(frame);
        // the frame ends with its CRC word (and E-AC-3's encinfo bit is
        // before it); nothing a block reads may go past it
        let limit = frame.len().saturating_sub(2) * 8;
        b.seek(h.bsi_end);
        let nfch = h.nfchans;
        let mut af = AudFrm::default();
        if h.eac3 {
            self.parse_audfrm(&mut b, h, &mut af)?;
        }
        // frame initialisation (Annex E: the syntax state flags, the
        // default band structures)
        for f in self.fbw.iter_mut() {
            f.firstspxcos = true;
            f.firstcplcos = true;
        }
        self.firstcplleak = true;
        self.spxbndstrc = tables::DEFAULT_SPX_BNDSTRC;
        if h.eac3 {
            self.cplbndstrc = tables::DEFAULT_CPL_BNDSTRC;
        }
        for a in self.alloc.iter_mut() {
            a.aht = false;
            // no delta bit allocation until the frame sends some (§7.2.2.6)
            a.dba = Dba::default();
        }
        self.spx_atten = af.spxattencod;
        self.features.lfe |= h.lfeon;
        if h.eac3 && af.ahte {
            for ch in 0..nfch {
                self.alloc[ch].aht = af.chahtinu[ch];
            }
            self.alloc[CPL].aht = af.cplahtinu;
            self.alloc[LFE].aht = af.lfeahtinu;
        }
        let mut aht_read = [false; 7];
        self.block_ends.clear();
        self.bad_codes.clear();
        for blk in 0..h.blocks {
            self.parse_block(&mut b, h, &af, blk, &mut aht_read)?;
            if b.overrun() || b.position() > limit {
                return Err(bad("audio block runs past the end of the frame"));
            }
            self.block_ends.push(b.position());
            if !self.parse_only {
                self.reconstruct(h, blk, pcm);
            }
        }
        Ok(())
    }

    fn parse_audfrm(&mut self, b: &mut Bits, h: &Header, af: &mut AudFrm) -> Result<()> {
        let nfch = h.nfchans;
        let blocks = h.blocks;
        let expstre;
        if h.numblkscod == 3 {
            expstre = b.flag();
            af.ahte = b.flag();
        } else {
            expstre = true;
            af.ahte = false;
        }
        af.snroffststr = b.read(2) as u8;
        let transproce = b.flag();
        af.blkswe = b.flag();
        af.dithflage = b.flag();
        af.bamode = b.flag();
        af.frmfgaincode = b.flag();
        af.dbaflde = b.flag();
        af.skipflde = b.flag();
        let spxattene = b.flag();
        if af.snroffststr == 3 {
            return Err(bad("reserved SNR offset strategy"));
        }
        if h.acmod > 1 {
            af.cplstre[0] = true;
            af.cplinu[0] = b.flag();
            for blk in 1..blocks {
                af.cplstre[blk] = b.flag();
                af.cplinu[blk] = if af.cplstre[blk] { b.flag() } else { af.cplinu[blk - 1] };
            }
        }
        let ncplblks = af.cplinu[..blocks].iter().filter(|&&c| c).count();
        if expstre {
            for blk in 0..blocks {
                if af.cplinu[blk] {
                    af.cplexpstr[blk] = b.read(2) as u8;
                }
                for ch in 0..nfch {
                    af.chexpstr[blk][ch] = b.read(2) as u8;
                }
            }
        } else {
            if h.acmod > 1 && ncplblks > 0 {
                let code = b.read(5) as usize;
                for blk in 0..6 {
                    af.cplexpstr[blk] = tables::FRMEXPSTR[code][blk];
                }
            }
            for ch in 0..nfch {
                let code = b.read(5) as usize;
                for blk in 0..6 {
                    af.chexpstr[blk][ch] = tables::FRMEXPSTR[code][blk];
                }
            }
        }
        if h.lfeon {
            for blk in 0..blocks {
                af.lfeexpstr[blk] = b.read(1) as u8;
            }
        }
        if h.strmtyp == 0 {
            let convexpstre = if h.numblkscod != 3 { b.flag() } else { true };
            if convexpstre {
                b.skip(5 * nfch); // convexpstr
            }
        }
        if af.ahte {
            // the number of times each channel's exponents are sent in
            // the frame (§3.4.2); only a channel whose exponents are sent
            // once can use the transform (six blocks: numblkscod 3)
            let ncplregs = (0..6).filter(|&blk| af.cplstre[blk] || af.cplexpstr[blk] != 0).count();
            af.cplahtinu = if ncplblks == 6 && ncplregs == 1 { b.flag() } else { false };
            for ch in 0..nfch {
                let nchregs = (0..6).filter(|&blk| af.chexpstr[blk][ch] != 0).count();
                af.chahtinu[ch] = if nchregs == 1 { b.flag() } else { false };
            }
            if h.lfeon {
                let nlferegs = (0..6).filter(|&blk| af.lfeexpstr[blk] != 0).count();
                af.lfeahtinu = if nlferegs == 1 { b.flag() } else { false };
            }
        }
        if af.snroffststr == 0 {
            af.frmcsnroffst = b.read(6) as i32;
            af.frmfsnroffst = b.read(4) as i32;
        }
        if transproce {
            // transient pre-noise processing: optional for a decoder
            self.features.transient_pre_noise = true;
            for _ in 0..nfch {
                if b.flag() {
                    b.skip(10 + 8); // transprocloc, transproclen
                }
            }
        }
        if spxattene {
            for ch in 0..nfch {
                af.spxattencod[ch] = if b.flag() { Some(b.read(5) as u8) } else { None };
            }
        }
        let blkstrtinfoe = if h.numblkscod != 0 { b.flag() } else { false };
        if blkstrtinfoe {
            let words = h.bytes / 2;
            let log2 = if words <= 1 { 0 } else { 32 - (words as u32 - 1).leading_zeros() as usize };
            b.skip((blocks - 1) * (4 + log2));
        }
        if b.overrun() {
            return Err(bad("audio frame runs past the end of the frame"));
        }
        Ok(())
    }

    /// Read one audio block's side information and mantissas into the
    /// decoder state and `coef`.
    fn parse_block(&mut self, b: &mut Bits, h: &Header, af: &AudFrm, blk: usize, aht_read: &mut [bool; 7]) -> Result<()> {
        let nfch = h.nfchans;
        self.noisy = [[0; 4]; 7];
        self.hebap4 = [[0; 4]; 7];
        self.spx_bins = [[0; 4]; 7];
        self.aht_dithered = [[0; 4]; 7];
        let acmod = h.acmod;
        let eac3 = h.eac3;

        // block switch and dither flags
        for ch in 0..nfch {
            self.fbw[ch].blksw = if !eac3 || af.blkswe { b.flag() } else { false };
            self.features.block_switching |= self.fbw[ch].blksw;
        }
        for ch in 0..nfch {
            self.fbw[ch].dithflag = if !eac3 || af.dithflage { b.flag() } else { true };
        }

        // dynamic range control
        for i in 0..if acmod == 0 { 2 } else { 1 } {
            if b.flag() {
                let code = b.read(8) as u8;
                self.dynrng[i] = tables::dynrng_gain(code);
                self.features.dynrng |= code != 0;
            } else if blk == 0 {
                self.dynrng[i] = 1.0;
            }
        }

        // spectral extension strategy and coordinates (E-AC-3)
        if eac3 {
            self.parse_spx(b, h, blk)?;
        } else {
            self.spxinu = false;
            for f in self.fbw.iter_mut() {
                f.inspx = false;
            }
        }

        // coupling strategy
        let (cplstre, cplinu) = if eac3 {
            (af.cplstre[blk], af.cplinu[blk])
        } else {
            let e = b.flag();
            let u = if e { b.flag() } else { self.cplinu };
            (e, u)
        };
        if cplstre {
            if cplinu {
                if acmod < 2 {
                    return Err(bad("coupling in a mono or dual mono frame"));
                }
                if eac3 && b.flag() {
                    return Err(FrameError::Unsupported("enhanced coupling"));
                }
                if eac3 && acmod == 2 {
                    self.fbw[0].incpl = true;
                    self.fbw[1].incpl = true;
                } else {
                    for ch in 0..nfch {
                        self.fbw[ch].incpl = b.flag();
                    }
                }
                self.phsflginu = if acmod == 2 { b.flag() } else { false };
                // (kept only once checked: when this frame fails, the next
                // AC-3 frame's first block may still reuse coupling, and
                // this range with it)
                let cplbegf = b.read(4) as usize;
                let cplendf = if !eac3 || !self.spxinu {
                    b.read(4) as i32
                } else if self.spxbegf < 6 {
                    self.spxbegf as i32 - 2
                } else {
                    self.spxbegf as i32 * 2 - 7
                };
                let ncplsubnd = 3 + cplendf - cplbegf as i32;
                if ncplsubnd < 1 {
                    return Err(bad("coupling ends before it begins"));
                }
                (self.cplbegf, self.cplendf) = (cplbegf, cplendf);
                if !eac3 || b.flag() {
                    for sb in 1..ncplsubnd as usize {
                        self.cplbndstrc[self.cplbegf + sb] = b.flag();
                    }
                }
                if (0..nfch).filter(|&ch| self.fbw[ch].incpl).count() < 2 {
                    return Err(bad("fewer than two channels in coupling"));
                }
            } else {
                for f in self.fbw.iter_mut() {
                    f.incpl = false;
                    f.firstcplcos = true;
                }
                self.firstcplleak = true;
                self.phsflginu = false;
            }
        }
        self.cplinu = cplinu;
        if cplinu {
            self.features.coupling = true;
        }

        // coupling coordinates and phase flags
        let cpl_end_sb = (3 + self.cplendf).max(0) as usize;
        if cplinu {
            let (_, ncplbnd) = self.cpl_bands();
            let mut any = false;
            for ch in 0..nfch {
                let f = &mut self.fbw[ch];
                f.cplcoe = false;
                if f.incpl {
                    let cplcoe = if eac3 && f.firstcplcos {
                        f.firstcplcos = false;
                        true
                    } else {
                        b.flag()
                    };
                    if cplcoe {
                        f.cplcoe = true;
                        any = true;
                        let mstrcplco = b.read(2) as i32;
                        for bnd in 0..ncplbnd {
                            let cplcoexp = b.read(4) as i32;
                            let cplcomant = b.read(4) as f32;
                            let mant = if cplcoexp == 15 { cplcomant / 16.0 } else { (cplcomant + 16.0) / 32.0 };
                            f.cplco[bnd] = mant * 2f32.powi(-(cplcoexp + 3 * mstrcplco)) * 8.0;
                        }
                    }
                } else if eac3 {
                    f.firstcplcos = true;
                }
            }
            if any {
                self.features.coupling = true;
            }
            if acmod == 2 && self.phsflginu && (self.fbw[0].cplcoe || self.fbw[1].cplcoe) {
                for bnd in 0..ncplbnd {
                    self.phsflg[bnd] = b.flag();
                    self.features.phase_flags |= self.phsflg[bnd];
                }
            }
            if !self.phsflginu {
                self.phsflg = [false; 18];
            }
        }

        // rematrixing
        if acmod == 2 {
            let nrematbd = if cplinu {
                if self.cplbegf == 0 {
                    2
                } else if self.cplbegf < 3 {
                    3
                } else {
                    4
                }
            } else if self.spxinu {
                if self.spxbegf < 2 {
                    3
                } else {
                    4
                }
            } else {
                4
            };
            let rematstr = if eac3 && blk == 0 { true } else { b.flag() };
            if rematstr {
                for bnd in 0..nrematbd {
                    self.rematflg[bnd] = b.flag();
                    self.features.rematrixing |= self.rematflg[bnd];
                }
            }
        }

        // exponent strategies
        let cplexpstr;
        let lfeexpstr;
        if eac3 {
            cplexpstr = if cplinu { af.cplexpstr[blk] } else { 0 };
            for ch in 0..nfch {
                self.alloc[ch].expstr = af.chexpstr[blk][ch];
            }
            lfeexpstr = if h.lfeon { af.lfeexpstr[blk] } else { 0 };
        } else {
            cplexpstr = if cplinu { b.read(2) as u8 } else { 0 };
            for ch in 0..nfch {
                self.alloc[ch].expstr = b.read(2) as u8;
            }
            lfeexpstr = if h.lfeon { b.read(1) as u8 } else { 0 };
        }
        for ch in 0..nfch {
            if self.alloc[ch].expstr != 0 && !self.fbw[ch].incpl && !self.fbw[ch].inspx {
                let c = b.read(6) as u8;
                if c > 60 {
                    return Err(bad("channel bandwidth code above 60"));
                }
                self.fbw[ch].chbwcod = c;
            }
        }

        // where each channel's mantissas start and end
        let cplstrtmant = 37 + 12 * self.cplbegf;
        let cplendmant = 37 + 12 * cpl_end_sb;
        for ch in 0..nfch {
            let f = &self.fbw[ch];
            self.alloc[ch].start = 0;
            self.alloc[ch].end = if cplinu && f.incpl {
                cplstrtmant
            } else if self.spxinu && f.inspx {
                tables::spx_band_start(self.spx_begin)
            } else {
                (f.chbwcod as usize + 12) * 3 + 37
            };
        }
        if cplinu {
            self.alloc[CPL].start = cplstrtmant;
            self.alloc[CPL].end = cplendmant;
        }
        self.alloc[LFE].start = 0;
        self.alloc[LFE].end = 7;

        // exponents
        let mut groups = [0u8; 84];
        if cplinu && cplexpstr != 0 {
            let cplabsexp = b.read(4) as u8;
            let grpsize = 1usize << (cplexpstr - 1);
            let ngrps = (cplendmant - cplstrtmant) / (3 * grpsize);
            for g in groups.iter_mut().take(ngrps) {
                *g = b.read(7) as u8;
            }
            bitalloc::decode_exponents(cplabsexp << 1, &groups[..ngrps], grpsize, true, &mut self.alloc[CPL].exp, cplstrtmant).map_err(bad)?;
        }
        for ch in 0..nfch {
            let expstr = self.alloc[ch].expstr;
            if expstr != 0 {
                let absexp = b.read(4) as u8;
                let grpsize = 1usize << (expstr - 1);
                let end = self.alloc[ch].end;
                let ngrps = match expstr {
                    1 => (end - 1) / 3,
                    2 => (end - 1 + 3) / 6,
                    _ => (end - 1 + 9) / 12,
                };
                for g in groups.iter_mut().take(ngrps) {
                    *g = b.read(7) as u8;
                }
                bitalloc::decode_exponents(absexp, &groups[..ngrps], grpsize, false, &mut self.alloc[ch].exp, 0).map_err(bad)?;
                b.skip(2); // gainrng
            }
        }
        if h.lfeon && lfeexpstr != 0 {
            let absexp = b.read(4) as u8;
            groups[0] = b.read(7) as u8;
            groups[1] = b.read(7) as u8;
            bitalloc::decode_exponents(absexp, &groups[..2], 1, false, &mut self.alloc[LFE].exp, 0).map_err(bad)?;
        }

        // bit allocation parameters
        if !eac3 || af.bamode {
            if b.flag() {
                self.params.sdcycod = b.read(2) as usize;
                self.params.fdcycod = b.read(2) as usize;
                self.params.sgaincod = b.read(2) as usize;
                self.params.dbpbcod = b.read(2) as usize;
                self.params.floorcod = b.read(3) as usize;
            }
        } else {
            self.params = Params::default();
        }
        self.params.fscod = (h.fscod as usize).min(2);
        if !eac3 {
            if b.flag() {
                self.csnroffst = b.read(6) as i32;
                if cplinu {
                    self.alloc[CPL].fsnroffst = b.read(4) as i32;
                    self.alloc[CPL].fgaincod = b.read(3) as usize;
                }
                for ch in 0..nfch {
                    self.alloc[ch].fsnroffst = b.read(4) as i32;
                    self.alloc[ch].fgaincod = b.read(3) as usize;
                }
                if h.lfeon {
                    self.alloc[LFE].fsnroffst = b.read(4) as i32;
                    self.alloc[LFE].fgaincod = b.read(3) as usize;
                }
            }
        } else {
            if af.snroffststr == 0 {
                self.csnroffst = af.frmcsnroffst;
                for a in self.alloc.iter_mut() {
                    a.fsnroffst = af.frmfsnroffst;
                }
            } else {
                let snroffste = blk == 0 || b.flag();
                if snroffste {
                    self.csnroffst = b.read(6) as i32;
                    if af.snroffststr == 1 {
                        let v = b.read(4) as i32;
                        for a in self.alloc.iter_mut() {
                            a.fsnroffst = v;
                        }
                    } else {
                        if cplinu {
                            self.alloc[CPL].fsnroffst = b.read(4) as i32;
                        }
                        for ch in 0..nfch {
                            self.alloc[ch].fsnroffst = b.read(4) as i32;
                        }
                        if h.lfeon {
                            self.alloc[LFE].fsnroffst = b.read(4) as i32;
                        }
                    }
                }
            }
            let fgaincode = af.frmfgaincode && b.flag();
            if fgaincode {
                if cplinu {
                    self.alloc[CPL].fgaincod = b.read(3) as usize;
                }
                for ch in 0..nfch {
                    self.alloc[ch].fgaincod = b.read(3) as usize;
                }
                if h.lfeon {
                    self.alloc[LFE].fgaincod = b.read(3) as usize;
                }
            } else {
                for a in self.alloc.iter_mut() {
                    a.fgaincod = 4;
                }
            }
            if h.strmtyp == 0 && b.flag() {
                b.skip(10); // convsnroffst
            }
        }
        if cplinu {
            let cplleake = if eac3 && self.firstcplleak {
                self.firstcplleak = false;
                true
            } else {
                b.flag()
            };
            if cplleake {
                let f = b.read(3) as i32;
                let s = b.read(3) as i32;
                self.cplleak = (f, s);
            }
        }

        // delta bit allocation
        // (deltbaie 0 keeps every channel's state, which starts inactive
        // in each frame, §5.4.3.47)
        if (!eac3 || af.dbaflde) && b.flag() {
            let cpldeltbae = if cplinu { b.read(2) } else { 2 };
            let mut deltbae = [2u32; 5];
            for d in deltbae.iter_mut().take(nfch) {
                *d = b.read(2);
            }
            if cplinu {
                Self::parse_dba(b, cpldeltbae, &mut self.alloc[CPL].dba)?;
            }
            for (ch, &d) in deltbae.iter().enumerate().take(nfch) {
                Self::parse_dba(b, d, &mut self.alloc[ch].dba)?;
            }
        }
        if self.alloc.iter().any(|a| a.dba.active) {
            self.features.delta_bit_allocation = true;
        }

        // skip field
        if (!eac3 || af.skipflde) && b.flag() {
            let skipl = b.read(9) as usize;
            b.skip(skipl * 8);
            self.features.skip_fields = true;
        }

        // bit allocation of every channel that has mantissas in this block
        self.allocate_all(nfch, cplinu, h.lfeon, aht_read);

        // mantissas
        self.read_mantissas(b, h, blk, cplinu, aht_read)?;
        Ok(())
    }

    /// One channel's delta bit allocation strategy (Table 5.16). "Reuse
    /// previous state" applies the segments last sent in this frame, even
    /// after a block that did without them: §7.2.2.6 applies the stored
    /// segments whenever deltbae is 0 or 1.
    fn parse_dba(b: &mut Bits, deltbae: u32, dba: &mut Dba) -> Result<()> {
        match deltbae {
            0 => dba.active = true,
            1 => {
                dba.active = true;
                dba.nseg = b.read(3) as usize + 1;
                for seg in 0..dba.nseg {
                    dba.offst[seg] = b.read(5) as u8;
                    dba.len[seg] = b.read(4) as u8;
                    dba.ba[seg] = b.read(3) as u8;
                }
            }
            2 => dba.active = false,
            _ => return Err(bad("reserved delta bit allocation code")),
        }
        Ok(())
    }

    fn parse_spx(&mut self, b: &mut Bits, h: &Header, blk: usize) -> Result<()> {
        let nfch = h.nfchans;
        let spxstre = if blk == 0 { true } else { b.flag() };
        if spxstre {
            self.spxinu = b.flag();
            if self.spxinu {
                if h.acmod == 1 {
                    self.fbw[0].inspx = true;
                } else {
                    for ch in 0..nfch {
                        self.fbw[ch].inspx = b.flag();
                    }
                }
                self.spxstrtf = b.read(2) as usize;
                self.spxbegf = b.read(3) as usize;
                let spxendf = b.read(3) as usize;
                self.spx_begin = if self.spxbegf < 6 { self.spxbegf + 2 } else { self.spxbegf * 2 - 3 };
                self.spx_end = if spxendf < 3 { spxendf + 5 } else { spxendf * 2 + 3 };
                if b.flag() {
                    for bnd in self.spx_begin + 1..self.spx_end {
                        self.spxbndstrc[bnd] = b.flag();
                    }
                }
                if self.spx_begin >= self.spx_end || self.spxstrtf >= self.spx_begin {
                    return Err(bad("spectral extension bands out of order"));
                }
            } else {
                for f in self.fbw.iter_mut() {
                    f.inspx = false;
                    f.firstspxcos = true;
                }
            }
        }
        if self.spxinu {
            self.features.spectral_extension = true;
            let (nbnds, sizes) = self.spx_bands();
            for ch in 0..nfch {
                let f = &mut self.fbw[ch];
                if !f.inspx {
                    f.firstspxcos = true;
                    continue;
                }
                let spxcoe = if f.firstspxcos {
                    f.firstspxcos = false;
                    true
                } else {
                    b.flag()
                };
                if spxcoe {
                    let spxblnd = b.read(5) as f32;
                    let mstrspxco = b.read(2) as i32;
                    for bnd in 0..nbnds {
                        let spxcoexp = b.read(4) as i32;
                        let spxcomant = b.read(2) as f32;
                        let mant = if spxcoexp == 15 { spxcomant / 4.0 } else { (spxcomant + 4.0) / 8.0 };
                        f.spxco[bnd] = mant * 2f32.powi(-(spxcoexp + 3 * mstrspxco)) * 32.0;
                    }
                    // blending factors (§3.6.4.2.1)
                    let noffset = spxblnd / 32.0;
                    let mut spxmant = tables::spx_band_start(self.spx_begin) as f32;
                    let top = tables::spx_band_start(self.spx_end) as f32;
                    for (bnd, &size) in sizes.iter().enumerate().take(nbnds) {
                        let size = size as f32;
                        let nratio = ((spxmant + 0.5 * size) / top - noffset).clamp(0.0, 1.0);
                        f.nblend[bnd] = nratio.sqrt();
                        f.sblend[bnd] = (1.0 - nratio).sqrt();
                        spxmant += size;
                    }
                }
            }
        }
        Ok(())
    }

    /// The coupling band of each coupling sub-band in use, and how many
    /// bands there are (§5.4.3.13).
    fn cpl_bands(&self) -> ([usize; 18], usize) {
        let (first, last) = (self.cplbegf, (3 + self.cplendf).max(0) as usize);
        let mut band_of = [0usize; 18];
        let mut band = 0;
        for (sb, b) in band_of.iter_mut().enumerate().take(last).skip(first) {
            if sb > first && !self.cplbndstrc[sb] {
                band += 1;
            }
            *b = band;
        }
        (band_of, band + 1)
    }

    /// The spectral extension bands: how many, and each one's width in
    /// bins (§3.6.2).
    fn spx_bands(&self) -> (usize, [usize; 17]) {
        let mut sizes = [0usize; 17];
        sizes[0] = 12;
        let mut n = 1;
        for bnd in self.spx_begin + 1..self.spx_end {
            if self.spxbndstrc[bnd] {
                sizes[n - 1] += 12;
            } else {
                sizes[n] = 12;
                n += 1;
            }
        }
        (n, sizes)
    }

    fn allocate_all(&mut self, nfch: usize, cplinu: bool, lfeon: bool, aht_read: &[bool; 7]) {
        // all SNR offsets zero: no bits for anything (§7.2.2.1.1)
        let mut all_zero = self.csnroffst == 0;
        for ch in 0..nfch {
            all_zero &= self.alloc[ch].fsnroffst == 0;
        }
        if cplinu {
            all_zero &= self.alloc[CPL].fsnroffst == 0;
        }
        if lfeon {
            all_zero &= self.alloc[LFE].fsnroffst == 0;
        }
        let mut chans: Vec<usize> = (0..nfch).collect();
        if cplinu {
            chans.push(CPL);
        }
        if lfeon {
            chans.push(LFE);
        }
        for ch in chans {
            // the transform's mantissas are all read with the first block's
            // allocation
            if self.alloc[ch].aht && aht_read[ch] {
                continue;
            }
            let a = &mut self.alloc[ch];
            if all_zero {
                a.bap = [0; 256];
                continue;
            }
            let snroffset = (((self.csnroffst - 15) << 4) + a.fsnroffst) << 2;
            let leak = if ch == CPL { Some(self.cplleak) } else { None };
            let table = if a.aht { &HEBAPTAB } else { &BAPTAB };
            let input = bitalloc::Channel { exp: &a.exp, start: a.start, end: a.end, fgaincod: a.fgaincod, snroffset, leak, dba: &a.dba, table };
            let mut bap = [0u8; 256];
            bitalloc::allocate(&self.params, &input, &mut bap);
            a.bap = bap;
        }
    }

    fn read_mantissas(&mut self, b: &mut Bits, h: &Header, blk: usize, cplinu: bool, aht_read: &mut [bool; 7]) -> Result<()> {
        let nfch = h.nfchans;
        let mut groups = Groups::default();
        let mut got_cpl = false;
        for ch in 0..nfch {
            self.read_channel(b, ch, blk, &mut groups, aht_read)?;
            if cplinu && self.fbw[ch].incpl && !got_cpl {
                self.read_channel(b, CPL, blk, &mut groups, aht_read)?;
                got_cpl = true;
            }
        }
        if h.lfeon {
            self.read_channel(b, LFE, blk, &mut groups, aht_read)?;
        }
        Ok(())
    }

    /// One channel's mantissas into `coef[ch]` (the coupling channel's
    /// zero-bit bins are left at zero and marked in `cpl_zero`; the
    /// decoupling dithers them per channel).
    fn read_channel(&mut self, b: &mut Bits, ch: usize, blk: usize, groups: &mut Groups, aht_read: &mut [bool; 7]) -> Result<()> {
        let (start, end) = (self.alloc[ch].start, self.alloc[ch].end.min(256));
        // only full bandwidth channels have a dither flag; the coupling
        // channel's zero-bit bins are dithered after decoupling (§7.3.4)
        let dither = ch < 5 && self.fbw[ch].dithflag && self.noise;
        if self.alloc[ch].aht {
            if !aht_read[ch] {
                self.read_aht(b, ch)?;
                aht_read[ch] = true;
            }
            // this block's coefficients from the transform's output (zero
            // bit coefficients were dithered, if at all, before the
            // inverse DCT)
            let coef = &mut self.coef[ch];
            coef.fill(0.0);
            let a = &self.alloc[ch];
            for (bin, c) in coef.iter_mut().enumerate().take(end).skip(start) {
                *c = self.aht_mant[ch][blk][bin] * exp_scale(a.exp[bin]);
            }
            if ch == CPL {
                self.cpl_zero[start..end].fill(false);
            }
            for w in 0..4 {
                self.noisy[ch][w] |= self.aht_noisy[ch][w];
                self.aht_dithered[ch][w] |= self.aht_noisy[ch][w];
                self.hebap4[ch][w] |= self.aht_hebap4[ch][w];
            }
            return Ok(());
        }
        let coef = &mut self.coef[ch];
        coef.fill(0.0);
        let a = &self.alloc[ch];
        for (bin, c) in coef.iter_mut().enumerate().take(end).skip(start) {
            let bap = a.bap[bin];
            let s = exp_scale(a.exp[bin]);
            let m = match bap {
                0 => {
                    if ch == CPL {
                        self.cpl_zero[bin] = true;
                        0.0
                    } else if dither {
                        self.features.dither = true;
                        mark(&mut self.noisy[ch], bin);
                        self.rng.uniform() * DITHER_SCALE
                    } else {
                        0.0
                    }
                }
                1 => {
                    if groups.n1 == 0 {
                        let code = group_code(b, 5, 26, self.parse_only, &mut self.bad_codes).ok_or(bad("3-level mantissa group above 26"))?;
                        groups.g1 = [code / 9, (code % 9) / 3, code % 3].map(|c| (c as f32 - 1.0) * (2.0 / 3.0));
                        groups.n1 = 3;
                    }
                    groups.n1 -= 1;
                    groups.g1[2 - groups.n1]
                }
                2 => {
                    if groups.n2 == 0 {
                        let code = group_code(b, 7, 124, self.parse_only, &mut self.bad_codes).ok_or(bad("5-level mantissa group above 124"))?;
                        groups.g2 = [code / 25, (code % 25) / 5, code % 5].map(|c| (c as f32 - 2.0) * (2.0 / 5.0));
                        groups.n2 = 3;
                    }
                    groups.n2 -= 1;
                    groups.g2[2 - groups.n2]
                }
                3 => {
                    let code = b.read(3);
                    // code 7 is not a level of the 7-level quantizer
                    if code == 7 {
                        0.0
                    } else {
                        (code as f32 - 3.0) * (2.0 / 7.0)
                    }
                }
                4 => {
                    if groups.n4 == 0 {
                        let code = group_code(b, 7, 120, self.parse_only, &mut self.bad_codes).ok_or(bad("11-level mantissa group above 120"))?;
                        groups.g4 = [code / 11, code % 11].map(|c| (c as f32 - 5.0) * (2.0 / 11.0));
                        groups.n4 = 2;
                    }
                    groups.n4 -= 1;
                    groups.g4[1 - groups.n4]
                }
                5 => {
                    let code = b.read(4);
                    if code == 15 {
                        0.0
                    } else {
                        (code as f32 - 7.0) * (2.0 / 15.0)
                    }
                }
                _ => {
                    let bits = QNTZTAB[bap as usize];
                    b.read_signed(bits) as f32 * (1.0 / (1u32 << (bits - 1)) as f32)
                }
            };
            if ch == CPL && bap != 0 {
                self.cpl_zero[bin] = false;
            }
            *c = m * s;
        }
        Ok(())
    }

    /// The adaptive hybrid transform: read all six blocks' mantissas of a
    /// channel (vector quantized, gain-adaptive quantized or scalar,
    /// Annex E §3.4.4) and invert the DCT across the blocks (§3.4.5)
    /// into `aht_mant[ch]`.
    fn read_aht(&mut self, b: &mut Bits, ch: usize) -> Result<()> {
        self.features.aht = true;
        let (start, end) = (self.alloc[ch].start, self.alloc[ch].end.min(256));
        let hebap = self.alloc[ch].bap;
        let gaqmod = b.read(2);
        let endbap = if gaqmod < 2 { 12 } else { 17 };
        let active = (start..end).filter(|&bin| hebap[bin] > 7 && hebap[bin] < endbap).count();
        // one gain per active bin (in frequency order)
        let mut gains = vec![1u8; active];
        match gaqmod {
            1 | 2 => {
                for g in gains.iter_mut() {
                    if b.flag() {
                        *g = if gaqmod == 1 { 2 } else { 4 };
                    }
                }
            }
            3 => {
                for n in 0..active.div_ceil(3) {
                    let grp = group_code(b, 5, 26, self.parse_only, &mut self.bad_codes).ok_or(bad("gain triplet above 26"))?;
                    for (k, m) in [grp / 9, (grp % 9) / 3, grp % 3].into_iter().enumerate() {
                        if let Some(g) = gains.get_mut(3 * n + k) {
                            *g = [1, 2, 4][m as usize];
                        }
                    }
                }
            }
            _ => {}
        }
        if gaqmod != 0 && active > 0 {
            self.features.aht_gaq = true;
        }
        // Zero-bit coefficients: the standard says nothing of dither for
        // the transform; its zero-bit "mantissas" (§7.3.4) are these
        // DCT-domain values, so they are dithered, in every channel the
        // transform codes (with the channel's dither flag of this block
        // where it has one). ffmpeg 6.1 places its dither the same way.
        let dither = self.noise && (ch >= 5 || self.fbw[ch].dithflag);
        self.aht_noisy[ch] = [0; 4];
        self.aht_hebap4[ch] = [0; 4];
        let mant = &mut self.aht_mant[ch];
        let mut gi = 0;
        for bin in start..end {
            let h = hebap[bin];
            let mut x = [0f32; 6];
            if h == 0 {
                if dither {
                    for v in x.iter_mut() {
                        *v = self.rng.uniform() * DITHER_SCALE;
                    }
                    mark(&mut self.aht_noisy[ch], bin);
                    self.features.dither = true;
                }
            } else if h <= 7 {
                self.features.aht_vq = true;
                if h == 4 {
                    mark(&mut self.aht_hebap4[ch], bin);
                }
                let index = b.read(HEBAP_BITS[h as usize]) as usize;
                let row = vq::table(h)[index];
                for j in 0..6 {
                    x[j] = row[j] as i16 as f32 / 32768.0;
                }
            } else {
                let gain = if h < endbap {
                    gi += 1;
                    gains[gi - 1]
                } else {
                    1
                };
                for v in x.iter_mut() {
                    *v = read_gaq(b, h, gain);
                }
            }
            // C(k, m) = sqrt(2) sum_j R_j X(k, j) cos(j (2m + 1) pi / 12)
            for m in 0..6 {
                let mut c = x[0];
                for j in 1..6 {
                    c += std::f32::consts::SQRT_2 * x[j] * AHT_COS[m][j];
                }
                mant[m][bin] = c;
            }
        }
        if b.overrun() {
            return Err(bad("transform mantissas run past the end of the frame"));
        }
        Ok(())
    }

    /// Decoupling, rematrixing, spectral extension, dynamic range control
    /// and the inverse transform of the block just parsed, into
    /// `pcm[..][blk * 256..]`.
    fn reconstruct(&mut self, h: &Header, blk: usize, pcm: &mut [Vec<f32>]) {
        let nfch = h.nfchans;
        // decoupling (§7.4), dithering zero-bit coupled bins per channel
        if self.cplinu {
            let (first, last) = (self.cplbegf, (3 + self.cplendf).max(0) as usize);
            let (band_of, _) = self.cpl_bands();
            for ch in 0..nfch {
                if !self.fbw[ch].incpl {
                    continue;
                }
                let dither = self.fbw[ch].dithflag && self.noise;
                for (sb, &bnd) in band_of.iter().enumerate().take(last).skip(first) {
                    let mut co = self.fbw[ch].cplco[bnd];
                    if ch == 1 && h.acmod == 2 && self.phsflg[bnd] {
                        co = -co;
                    }
                    for bin in 37 + 12 * sb..49 + 12 * sb {
                        if is_marked(&self.noisy[CPL], bin) {
                            mark(&mut self.noisy[ch], bin);
                        }
                        if is_marked(&self.hebap4[CPL], bin) {
                            mark(&mut self.hebap4[ch], bin);
                        }
                        if is_marked(&self.aht_dithered[CPL], bin) {
                            mark(&mut self.aht_dithered[ch], bin);
                        }
                        self.coef[ch][bin] = if self.cpl_zero[bin] {
                            if dither {
                                self.features.dither = true;
                                mark(&mut self.noisy[ch], bin);
                                self.rng.uniform() * DITHER_SCALE * exp_scale(self.alloc[CPL].exp[bin]) * co
                            } else {
                                0.0
                            }
                        } else {
                            self.coef[CPL][bin] * co
                        };
                    }
                }
            }
        }

        // rematrixing (§7.5.4), up to the lower of the two bandwidths
        if h.acmod == 2 {
            let nrematbd = if self.cplinu {
                match self.cplbegf {
                    0 => 2,
                    1 | 2 => 3,
                    _ => 4,
                }
            } else if self.spxinu && self.spxbegf < 2 {
                3
            } else {
                4
            };
            let top = self.alloc[0].end.min(self.alloc[1].end);
            const EDGES: [usize; 5] = [13, 25, 37, 61, 253];
            for bnd in 0..nrematbd {
                if !self.rematflg[bnd] {
                    continue;
                }
                let (lo, hi) = (EDGES[bnd], EDGES[bnd + 1].min(top));
                let (l, r) = self.coef.split_at_mut(1);
                for bin in lo..hi.max(lo) {
                    let (a, c) = (l[0][bin], r[0][bin]);
                    l[0][bin] = a + c;
                    r[0][bin] = a - c;
                    if is_marked(&self.noisy[0], bin) || is_marked(&self.noisy[1], bin) {
                        mark(&mut self.noisy[0], bin);
                        mark(&mut self.noisy[1], bin);
                    }
                    if is_marked(&self.hebap4[0], bin) || is_marked(&self.hebap4[1], bin) {
                        mark(&mut self.hebap4[0], bin);
                        mark(&mut self.hebap4[1], bin);
                    }
                    if is_marked(&self.aht_dithered[0], bin) || is_marked(&self.aht_dithered[1], bin) {
                        mark(&mut self.aht_dithered[0], bin);
                        mark(&mut self.aht_dithered[1], bin);
                    }
                }
            }
        }

        // spectral extension (Annex E §3.6.4)
        if self.spxinu {
            let (nbnds, sizes) = self.spx_bands();
            let copystart = tables::spx_band_start(self.spxstrtf);
            let copyend = tables::spx_band_start(self.spx_begin);
            for ch in 0..nfch {
                if !self.fbw[ch].inspx {
                    continue;
                }
                let tc = &mut self.coef[ch];
                // translation, remembering where the copy wrapped
                let mut wrap = [false; 17];
                let mut copy = copystart;
                let mut insert = copyend;
                for bnd in 0..nbnds {
                    let size = sizes[bnd];
                    if copy + size > copyend {
                        copy = copystart;
                        wrap[bnd] = true;
                    }
                    for _ in 0..size {
                        if copy == copyend {
                            copy = copystart;
                        }
                        tc[insert] = tc[copy];
                        if is_marked(&self.noisy[ch], copy) {
                            mark(&mut self.noisy[ch], insert);
                        }
                        if is_marked(&self.hebap4[ch], copy) {
                            mark(&mut self.hebap4[ch], insert);
                        }
                        insert += 1;
                        copy += 1;
                    }
                }
                // banded RMS energy of the translated coefficients
                let mut rms = [0f32; 17];
                let mut bin = copyend;
                for bnd in 0..nbnds {
                    let mut acc = 0f32;
                    for _ in 0..sizes[bnd] {
                        acc += tc[bin] * tc[bin];
                        bin += 1;
                    }
                    rms[bnd] = (acc / sizes[bnd] as f32).sqrt();
                }
                // notch filter at the baseband border and at each wrap
                // (§3.6.4.2.3)
                if let Some(code) = self.spx_atten[ch] {
                    self.features.spx_attenuation = true;
                    let att = [tables::spx_atten(code, 0), tables::spx_atten(code, 1), tables::spx_atten(code, 2)];
                    let notch = |tc: &mut [f32; 256], center: usize| {
                        for (k, &a) in [att[0], att[1], att[2], att[1], att[0]].iter().enumerate() {
                            if let Some(v) = tc.get_mut(center + k - 2) {
                                *v *= a;
                            }
                        }
                    };
                    notch(tc, copyend);
                    let mut start = copyend + sizes[0];
                    for bnd in 1..nbnds {
                        if wrap[bnd] {
                            notch(tc, start);
                        }
                        start += sizes[bnd];
                    }
                }
                // blend with noise and scale (§3.6.4.2.4, §3.6.4.3)
                let f = &self.fbw[ch];
                let mut bin = copyend;
                for bnd in 0..nbnds {
                    let nscale = rms[bnd] * f.nblend[bnd] * SPX_NOISE_SCALE;
                    let sscale = f.sblend[bnd];
                    let co = f.spxco[bnd];
                    let blended = self.noise && nscale != 0.0;
                    for _ in 0..sizes[bnd] {
                        let noise = if blended { self.rng.uniform() * nscale } else { 0.0 };
                        tc[bin] = (tc[bin] * sscale + noise) * co;
                        mark(&mut self.spx_bins[ch], bin);
                        if blended {
                            mark(&mut self.noisy[ch], bin);
                        }
                        bin += 1;
                    }
                }
                // nothing above the extension region
                for v in tc.iter_mut().skip(bin) {
                    *v = 0.0;
                }
            }
        }
        // dynamic range control, inverse transform, overlap-add
        let mut x = [0f32; 512];
        let outputs = nfch + h.lfeon as usize;
        for (out, pcm) in pcm.iter_mut().enumerate().take(outputs) {
            let (ch, blksw) = if out < nfch { (out, self.fbw[out].blksw) } else { (LFE, false) };
            // in 1+1 mode dynrng2 scales Ch2 (§7.7.1.2); the standard does
            // not say which word scales the LFE channel there: dynrng2, as
            // ffmpeg does
            let gain = if h.acmod == 0 && out >= 1 { self.dynrng[1] } else { self.dynrng[0] };
            let coef = &mut self.coef[ch];
            if gain != 1.0 {
                for v in coef.iter_mut() {
                    *v *= gain;
                }
            }
            if blksw {
                self.imdct.short(coef, &mut x);
            } else {
                self.imdct.long(coef, &mut x);
            }
            let delay = &mut self.delay[if out < nfch { out } else { 5 }];
            let dst = &mut pcm[blk * 256..blk * 256 + 256];
            for n in 0..256 {
                dst[n] = 2.0 * (x[n] + delay[n]);
                delay[n] = x[256 + n];
            }
        }
        if self.trace {
            let mut t = BlockTrace::default();
            for out in 0..outputs {
                let ch = if out < nfch { out } else { LFE };
                t.channels.push(ChannelTrace { blksw: out < nfch && self.fbw[out].blksw, noisy: self.noisy[ch], hebap4: self.hebap4[ch], spx: self.spx_bins[ch], aht_dither: self.aht_dithered[ch] });
            }
            self.traces.push(t);
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        FrameDecoder::new()
    }
}

/// cos(j (2m + 1) pi / 12) for the AHT's inverse DCT, [m][j].
static AHT_COS: [[f32; 6]; 6] = {
    // computed below by a const-free table: the values are exact cosines
    // of multiples of pi/12
    const C: [f32; 24] = [
        1.0,
        0.965_925_8,
        0.866_025_4,
        0.707_106_77,
        0.5,
        0.258_819_04,
        0.0,
        -0.258_819_04,
        -0.5,
        -0.707_106_77,
        -0.866_025_4,
        -0.965_925_8,
        -1.0,
        -0.965_925_8,
        -0.866_025_4,
        -0.707_106_77,
        -0.5,
        -0.258_819_04,
        0.0,
        0.258_819_04,
        0.5,
        0.707_106_77,
        0.866_025_4,
        0.965_925_8,
    ];
    let mut t = [[0f32; 6]; 6];
    let mut m = 0;
    while m < 6 {
        let mut j = 0;
        while j < 6 {
            t[m][j] = C[(j * (2 * m + 1)) % 24];
            j += 1;
        }
        m += 1;
    }
    t
};

fn mark(set: &mut [u64; 4], bin: usize) {
    set[bin >> 6] |= 1 << (bin & 63);
}

fn is_marked(set: &[u64; 4], bin: usize) -> bool {
    set[bin >> 6] & (1 << (bin & 63)) != 0
}

/// What a test needs to know about one block of one output channel: the
/// transform coefficients that carry noise (dither, spectral extension
/// noise) when noise is on, and whether the block was switched.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelTrace {
    pub blksw: bool,
    /// Bit k of word k / 64: coefficient k carries noise.
    pub noisy: [u64; 4],
    /// Coefficient k was decoded with the adaptive hybrid transform's
    /// 32-entry vector quantizer (hebap 4), whose table ffmpeg 6.1 reads
    /// one row off.
    pub hebap4: [u64; 4],
    /// Coefficient k was made by spectral extension (it is also noisy).
    pub spx: [u64; 4],
    /// Coefficient k was coded by the adaptive hybrid transform with no
    /// bits, and dithered before its inverse DCT (it is also noisy).
    pub aht_dither: [u64; 4],
}

impl ChannelTrace {
    pub fn is_noisy(&self, bin: usize) -> bool {
        is_marked(&self.noisy, bin)
    }

    pub fn is_hebap4(&self, bin: usize) -> bool {
        is_marked(&self.hebap4, bin)
    }

    pub fn is_spx(&self, bin: usize) -> bool {
        is_marked(&self.spx, bin)
    }

    pub fn is_aht_dither(&self, bin: usize) -> bool {
        is_marked(&self.aht_dither, bin)
    }
}

/// One block's `ChannelTrace`s, by output channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BlockTrace {
    pub channels: Vec<ChannelTrace>,
}

/// Grouped mantissas left over from the last group read (§7.3.5); groups
/// run on across the channels of a block.
#[derive(Default)]
struct Groups {
    g1: [f32; 3],
    n1: usize,
    g2: [f32; 3],
    n2: usize,
    g4: [f32; 2],
    n4: usize,
}

/// One mantissa of a gain-adaptive or scalar quantized bin, `hebap` 8 to
/// 19, with the bin's gain 1, 2 or 4 (Annex E §3.4.4.2, Tables E3.5 and
/// E3.6).
fn read_gaq(b: &mut Bits, hebap: u8, gain: u8) -> f32 {
    let m = HEBAP_BITS[hebap as usize];
    let remap = |x: f32, g: usize| -> f32 {
        let c = tables::GAQ_REMAP[hebap as usize - 8][g];
        let a = c[0] as i16 as f32 / 32768.0;
        let bb = if x >= 0.0 { c[1] } else { c[2] } as i16 as f32 / 32768.0;
        x + a * x + bb
    };
    let frac = |v: i32, bits: u32| v as f32 / (1u32 << (bits - 1)) as f32;
    match gain {
        2 => {
            let v = b.read_signed(m - 1);
            if v == -(1 << (m - 2)) {
                let large = b.read_signed(m - 1);
                remap(frac(large, m - 1), 1)
            } else {
                frac(v, m - 1) / 2.0
            }
        }
        4 => {
            let v = b.read_signed(m - 2);
            if v == -(1 << (m - 3)) {
                let large = b.read_signed(m);
                remap(frac(large, m), 2)
            } else {
                frac(v, m - 2) / 4.0
            }
        }
        _ => remap(frac(b.read_signed(m), m), 0),
    }
}

/// A grouped mantissa code of `bits` bits, `None` above `max` (listed in
/// `bad_codes` instead when only parsing).
fn group_code(b: &mut Bits, bits: u32, max: u32, parse_only: bool, bad_codes: &mut Vec<(usize, u32, u32)>) -> Option<u32> {
    let pos = b.position();
    let code = b.read(bits);
    if code <= max {
        Some(code)
    } else if parse_only {
        bad_codes.push((pos, bits, max));
        Some(0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one GAQ mantissa from a bit pattern.
    fn gaq(bits: &[(u32, u32)], hebap: u8, gain: u8) -> f32 {
        let mut bytes = vec![0u8; 8];
        let mut pos = 0;
        for &(n, v) in bits {
            for k in (0..n).rev() {
                if (v >> k) & 1 != 0 {
                    bytes[pos / 8] |= 0x80 >> (pos % 8);
                }
                pos += 1;
            }
        }
        let mut b = Bits::new(&bytes);
        let v = read_gaq(&mut b, hebap, gain);
        assert_eq!(b.position(), pos, "hebap {hebap} gain {gain}: bits read");
        v
    }

    fn levels(mut v: Vec<f32>) -> Vec<f32> {
        v.sort_by(f32::total_cmp);
        v.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        v
    }

    fn check_steps(v: &[f32], step: f32, what: &str) {
        for w in v.windows(2) {
            assert!((w[1] - w[0] - step).abs() < 2e-4, "{what}: step {} not {step}", w[1] - w[0]);
        }
    }

    /// A dead-zone quantizer: symmetric, `step` apart within each sign.
    fn check_dead_zone(v: &[f32], step: f32, what: &str) {
        let n = v.len();
        for i in 0..n {
            assert!((v[i] + v[n - 1 - i]).abs() < 2e-4, "{what}: not symmetric");
        }
        check_steps(&v[..n / 2], step, what);
        check_steps(&v[n / 2..], step, what);
    }

    /// The quantizers of Table E3.5: with m mantissa bits, gain 1 has
    /// 2^m - 1 levels 2 / (2^m - 1) apart; gain 2 has 2^(m-1) - 1 small
    /// levels 1 / 2^(m-1) apart and 2^(m-1) large ones 1 / (2^(m-1) - 1)
    /// apart; gain 4 has 2^(m-2) - 1 small levels 1 / 2^(m-1) apart and
    /// 2^m large ones 3 / (2^(m+1) - 2) apart. Large mantissas follow a
    /// tag, the small quantizer's most negative code.
    #[test]
    fn gain_adaptive_quantizers_match_table_e3_5() {
        for hebap in 8u8..=16 {
            let m = HEBAP_BITS[hebap as usize];
            let p = |e: u32| (1u32 << e) as f32;
            // gain 1: every code but the most negative
            let g1: Vec<f32> = (0..1u32 << m).filter(|&c| c != 1 << (m - 1)).map(|code| gaq(&[(m, code)], hebap, 1)).collect();
            let g1 = levels(g1);
            assert_eq!(g1.len() as f32, p(m) - 1.0, "hebap {hebap} gain 1 levels");
            check_steps(&g1, 2.0 / (p(m) - 1.0), &format!("hebap {hebap} gain 1"));
            // gain 2
            let tag2 = 1u32 << (m - 2);
            let small: Vec<f32> = (0..1u32 << (m - 1)).filter(|&c| c != tag2).map(|c| gaq(&[(m - 1, c)], hebap, 2)).collect();
            let small = levels(small);
            assert_eq!(small.len() as f32, p(m - 1) - 1.0, "hebap {hebap} gain 2 small levels");
            check_steps(&small, 1.0 / p(m - 1), &format!("hebap {hebap} gain 2 small"));
            let large = levels((0..1u32 << (m - 1)).map(|c| gaq(&[(m - 1, tag2), (m - 1, c)], hebap, 2)).collect());
            assert_eq!(large.len() as f32, p(m - 1), "hebap {hebap} gain 2 large levels");
            check_dead_zone(&large, 1.0 / (p(m - 1) - 1.0), &format!("hebap {hebap} gain 2 large"));
            assert!(large[0] < small[0] && large[large.len() - 1] > small[small.len() - 1]);
            // gain 4
            let tag4 = 1u32 << (m - 3);
            let small: Vec<f32> = (0..1u32 << (m - 2)).filter(|&c| c != tag4).map(|c| gaq(&[(m - 2, c)], hebap, 4)).collect();
            let small = levels(small);
            assert_eq!(small.len() as f32, p(m - 2) - 1.0, "hebap {hebap} gain 4 small levels");
            if small.len() > 1 {
                check_steps(&small, 1.0 / p(m - 1), &format!("hebap {hebap} gain 4 small"));
            }
            let large = levels((0..1u32 << m).map(|c| gaq(&[(m - 2, tag4), (m, c)], hebap, 4)).collect());
            assert_eq!(large.len() as f32, p(m), "hebap {hebap} gain 4 large levels");
            check_dead_zone(&large, 3.0 / (2.0 * p(m) - 2.0), &format!("hebap {hebap} gain 4 large"));
            // the large quantizers leave out the small ones' range
            assert!(large.iter().all(|v| v.abs() > small.iter().fold(0f32, |a, s| a.max(s.abs()))));
        }
        // above hebap 16 there is only the plain quantizer
        for hebap in 17u8..=19 {
            let m = HEBAP_BITS[hebap as usize];
            assert!((gaq(&[(m, 1)], hebap, 1) - 1.0 / (1u32 << (m - 1)) as f32 * (1.0 + GAQ_REMAP_A1[hebap as usize - 8])).abs() < 1e-9);
        }
    }

    const GAQ_REMAP_A1: [f32; 12] = [
        0x1249 as f32 / 32768.0,
        0x0889 as f32 / 32768.0,
        0x0421 as f32 / 32768.0,
        0x0208 as f32 / 32768.0,
        0x0102 as f32 / 32768.0,
        0x0081 as f32 / 32768.0,
        0x0040 as f32 / 32768.0,
        0x0020 as f32 / 32768.0,
        0x0010 as f32 / 32768.0,
        0x0008 as f32 / 32768.0,
        0x0002 as f32 / 32768.0,
        0.0,
    ];

    /// C(k, m) = sqrt(2) sum_j R_j X(k, j) cos(j (2m + 1) pi / 12), R_0 =
    /// 1 / sqrt(2) (Annex E §3.4.5): the table used matches the formula,
    /// and the transform is sqrt(6) times an orthonormal one (so it
    /// inverts the DCT-II scaled by 1 / sqrt(6)).
    #[test]
    fn aht_inverse_dct() {
        let idct = |x: &[f64; 6]| -> [f64; 6] {
            let mut c = [0f64; 6];
            for (m, cm) in c.iter_mut().enumerate() {
                *cm = x[0] + (1..6).map(|j| 2f64.sqrt() * x[j] * AHT_COS[m][j] as f64).sum::<f64>();
            }
            c
        };
        for (m, row) in AHT_COS.iter().enumerate() {
            for (j, &v) in row.iter().enumerate() {
                let exact = (j as f64 * (2 * m + 1) as f64 * std::f64::consts::PI / 12.0).cos();
                assert!((v as f64 - exact).abs() < 1e-7, "{m} {j}");
            }
        }
        for j in 0..6 {
            let mut x = [0f64; 6];
            x[j] = 1.0;
            let c = idct(&x);
            let energy: f64 = c.iter().map(|v| v * v).sum();
            assert!((energy - 6.0).abs() < 1e-5, "basis {j}: energy {energy}");
            // the forward DCT-II / sqrt(6) gives it back
            for (k, &xk) in x.iter().enumerate() {
                let r = if k == 0 { 1.0 / 2f64.sqrt() } else { 1.0 };
                let back: f64 = (0..6).map(|m| c[m] * (k as f64 * (2 * m + 1) as f64 * std::f64::consts::PI / 12.0).cos()).sum::<f64>() * 2f64.sqrt() * r / 6.0;
                assert!((back - xk).abs() < 1e-6, "basis {j} coefficient {k}: {back}");
            }
        }
    }

    #[test]
    fn dither_is_uniform_with_the_standard_scale() {
        let mut rng = Rng::new(1);
        let n = 200_000;
        let v: Vec<f32> = (0..n).map(|_| rng.uniform() * DITHER_SCALE).collect();
        let mean = v.iter().map(|&x| x as f64).sum::<f64>() / n as f64;
        let var = v.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.005);
        // uniform on [-0.707, 0.707]: variance 0.707^2 / 3
        assert!((var - 0.5 / 3.0).abs() < 0.003, "{var}");
        assert!(v.iter().all(|x| x.abs() <= DITHER_SCALE));
        // the spectral extension noise: unit variance
        let var: f64 = (0..n).map(|_| ((rng.uniform() * SPX_NOISE_SCALE) as f64).powi(2)).sum::<f64>() / n as f64;
        assert!((var - 1.0).abs() < 0.02, "{var}");
    }

    /// A coupling range that ends before it begins fails the frame and is
    /// not kept: an AC-3 frame's first block may reuse coupling, and its
    /// range, from the frame before (with the reversed range, the count of
    /// coupling exponent groups went below zero, wrapped, and indexed past
    /// the groups: a panic, which aborts the WebAssembly instance).
    #[test]
    fn a_reversed_coupling_range_is_not_kept() {
        let h = Header { acmod: 2, nfchans: 2, blocks: 6, bytes: 64, bsi_end: 0, ..Default::default() };
        let frame = |start: [u8; 2]| {
            let mut f = [0u8; 64];
            f[..2].copy_from_slice(&start);
            f
        };
        let mut pcm = vec![vec![0f32; 6 * 256]; 2];
        let mut dec = FrameDecoder::new();
        // coupling in use from the frame before, sub-bands 2 to 7
        (dec.cplinu, dec.cplbegf, dec.cplendf) = (true, 2, 5);
        // block 0: cplstre and cplinu, both channels coupled, no phase
        // flags, cplbegf 15, cplendf 0
        assert_eq!(dec.decode(&frame([0x07, 0xbc]), &h, &mut pcm), Err(bad("coupling ends before it begins")));
        assert_eq!((dec.cplbegf, dec.cplendf), (2, 5));
        // the next frame's block 0 reuses coupling (cplstre 0) with new
        // exponents over the range kept (cplexpstr 1, then zeros: they
        // fall below 0, which fails the frame)
        assert_eq!(dec.decode(&frame([0x00, 0x20]), &h, &mut pcm), Err(bad("exponent out of range")));
    }
}
