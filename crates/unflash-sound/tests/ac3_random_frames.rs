//! Random AC-3 and E-AC-3 frames against ffmpeg. ffmpeg's encoders use
//! few of the tools the syntax offers; frames built here use them at
//! random.
//!
//! AC-3: every coding mode (1+1 included) with and without LFE, coupling
//! with random bands, coordinates and phase flags, rematrixing, every
//! exponent strategy and their reuse from block to block, random bit
//! allocation parameters, SNR offsets and fast gains, delta bit
//! allocation, dynamic range words, skip fields, dither flags and the odd
//! switched block.
//!
//! E-AC-3 (independent substream 0, 1, 2, 3 and 6 blocks, 32, 44.1 and
//! 48 kHz): all of the above as E-AC-3 codes it, plus frame and block
//! exponent strategies, the adaptive hybrid transform (vector quantized,
//! gain adaptive quantized), spectral extension with random bands,
//! blending, coordinates and attenuation, coupling and extension
//! strategies changing from block to block, the bit allocation mode, fast
//! gain codes, transient pre-noise processing data, converter fields,
//! block start information, mixing, informational and additional bit
//! stream information.
//!
//! The mantissas are random bits (a grouped code out of range is replaced
//! by a valid one); a block ends where this decoder's bit allocation says,
//! and the next is written from there, so if ffmpeg's allocation differed
//! its parse would go astray and the comparison would fail.
//!
//! Where ffmpeg 6.1 does not do what the standard says, the frames keep
//! to what both agree on (see the `ac3` module): a channel's delta
//! bit allocation is "reused" only once segments were sent in the frame,
//! the coupling channel's segments are empty; in E-AC-3 the SNR offsets
//! are the frame's, the mixing data never has the flexible layout
//! (mixdef 3), fast gain codes are sent again until back to the default,
//! coupling starts in block 0 only, the extension's strategy changes only
//! with coupling out of use, a band structure is sent when the extension
//! starts after block 0, and frames with the transform dither everywhere.

mod common;

use common::ac3::{compare_with, describe, fix_crcs};
use common::{ffmpeg_available, ffmpeg_decode_bytes, BitWriter, Lcg};
use unflash_sound::ac3::testing::{DEFAULT_CPL_BNDSTRC, DEFAULT_SPX_BNDSTRC, FRMEXPSTR, NFCHANS};

/// Frames built at random reach coefficient values that no encoder would
/// send; there ffmpeg 6.1's arithmetic is off by up to about 1e-5 of full
/// scale (the streams of `ac3_streams.rs` agree to 1e-5 and better).
const TOLERANCE: f64 = 3e-5;
use unflash_sound::ac3::Output;

/// A frame's fixed choices.
struct Plan {
    acmod: u8,
    lfeon: bool,
    nfch: usize,
    cplinu: bool,
    chincpl: [bool; 5],
    phsflginu: bool,
    cplbegf: u32,
    cplendf: u32,
    /// Coupling bands: the sub-band count of each.
    cplbands: Vec<u32>,
    chbwcod: [u32; 5],
}

impl Plan {
    fn random(rng: &mut Lcg, acmod: u8, lfeon: bool) -> Plan {
        let nfch = NFCHANS[acmod as usize];
        let cplinu = acmod >= 2 && rng.chance(70);
        let mut chincpl = [false; 5];
        if cplinu {
            while chincpl.iter().filter(|&&c| c).count() < 2 {
                for c in chincpl.iter_mut().take(nfch) {
                    *c = rng.chance(70);
                }
            }
        }
        let cplbegf = rng.below(12);
        let cplendf = (cplbegf + rng.below(16 - cplbegf)).max(cplbegf.saturating_sub(2)).min(15);
        let ncplsubnd = 3 + cplendf - cplbegf;
        let mut cplbands = Vec::new();
        for sb in 0..ncplsubnd {
            if sb == 0 || rng.chance(60) {
                cplbands.push(1);
            } else {
                *cplbands.last_mut().unwrap() += 1;
            }
        }
        let mut chbwcod = [0; 5];
        for c in chbwcod.iter_mut() {
            *c = 20 + rng.below(41);
        }
        Plan { acmod, lfeon, nfch, cplinu, chincpl, phsflginu: acmod == 2 && rng.chance(60), cplbegf, cplendf, cplbands, chbwcod }
    }

    fn endmant(&self, ch: usize) -> u32 {
        if self.cplinu && self.chincpl[ch] {
            37 + 12 * self.cplbegf
        } else {
            (self.chbwcod[ch] + 12) * 3 + 37
        }
    }
}

/// Coordinates (coupling, 4-bit mantissas, or spectral extension, 2-bit):
/// a master exponent, then an exponent and a mantissa per band, the total
/// shift at most 15 (below that ffmpeg 6.1 loses precision).
fn coordinates(w: &mut BitWriter, rng: &mut Lcg, bands: usize, mant_bits: u32) {
    let master = rng.below(4);
    w.put(2, master);
    for _ in 0..bands {
        w.put(4, rng.below(16 - 3 * master));
        w.put(mant_bits, rng.below(1 << mant_bits));
    }
}

/// `groups` exponent groups of three random differentials (a walk that
/// stays in `lo..=hi`) after the absolute exponent `start`.
fn exponent_groups(w: &mut BitWriter, rng: &mut Lcg, start: i32, groups: u32, lo: i32, hi: i32) {
    let mut e = start;
    for _ in 0..groups {
        let mut code = 0;
        for _ in 0..3 {
            let mut d;
            loop {
                d = rng.below(5) as i32 - 2;
                if (lo..=hi).contains(&(e + d)) {
                    break;
                }
            }
            e += d;
            code = code * 5 + (d + 2) as u32;
        }
        w.put(7, code);
    }
}

/// One block's side information (§5.3.3), up to the mantissas. `sent`
/// tells which channels (the coupling channel last) have had delta bit
/// allocation segments sent in this frame.
fn side_info(w: &mut BitWriter, rng: &mut Lcg, p: &Plan, blk: usize, sent: &mut [bool; 6]) {
    let nfch = p.nfch;
    for _ in 0..nfch {
        // blksw: the comparison leaves switched blocks out (ffmpeg 6.1
        // overlaps them with another channel's block)
        w.put(1, rng.chance(2) as u32);
    }
    for _ in 0..nfch {
        w.put(1, rng.chance(50) as u32); // dithflag
    }
    for _ in 0..if p.acmod == 0 { 2 } else { 1 } {
        if rng.chance(50) {
            w.put(1, 1);
            w.put(8, rng.below(256));
        } else {
            w.put(1, 0);
        }
    }
    // coupling strategy: the frame's, sent in block 0
    if blk == 0 {
        w.put(1, 1);
        w.put(1, p.cplinu as u32);
        if p.cplinu {
            for ch in 0..nfch {
                w.put(1, p.chincpl[ch] as u32);
            }
            if p.acmod == 2 {
                w.put(1, p.phsflginu as u32);
            }
            w.put(4, p.cplbegf);
            w.put(4, p.cplendf);
            // cplbndstrc for sub-bands 1 on: 1 continues the band before
            let bits: Vec<u32> = p.cplbands.iter().flat_map(|&n| (0..n).map(|k| (k > 0) as u32)).collect();
            for &b in &bits[1..] {
                w.put(1, b);
            }
        }
    } else {
        w.put(1, 0);
    }
    if p.cplinu {
        let mut any = [false; 5];
        for (a, _) in any.iter_mut().zip(&p.chincpl).take(nfch).filter(|(_, &incpl)| incpl) {
            let cplcoe = blk == 0 || rng.chance(50);
            w.put(1, cplcoe as u32);
            if cplcoe {
                *a = true;
                coordinates(w, rng, p.cplbands.len(), 4);
            }
        }
        if p.acmod == 2 && p.phsflginu && (any[0] || any[1]) {
            for _ in 0..p.cplbands.len() {
                w.put(1, rng.below(2));
            }
        }
    }
    if p.acmod == 2 {
        let rematstr = blk == 0 || rng.chance(50);
        w.put(1, rematstr as u32);
        if rematstr {
            let n = if !p.cplinu || p.cplbegf > 2 {
                4
            } else if p.cplbegf > 0 {
                3
            } else {
                2
            };
            w.put(n, rng.below(1 << n));
        }
    }
    // exponent strategies: new in block 0, then new or reused
    let pick = |rng: &mut Lcg| if blk == 0 || rng.chance(40) { 1 + rng.below(3) } else { 0 };
    let cplexpstr = if p.cplinu { pick(rng) } else { 0 };
    if p.cplinu {
        w.put(2, cplexpstr);
    }
    let chexpstr: Vec<u32> = (0..nfch).map(|_| pick(rng)).collect();
    for &s in &chexpstr {
        w.put(2, s);
    }
    let lfeexpstr = if p.lfeon { (blk == 0 || rng.chance(40)) as u32 } else { 0 };
    if p.lfeon {
        w.put(1, lfeexpstr);
    }
    for (ch, &s) in chexpstr.iter().enumerate() {
        if s != 0 && !(p.cplinu && p.chincpl[ch]) {
            w.put(6, p.chbwcod[ch]);
        }
    }
    if cplexpstr != 0 {
        let start = 3 + rng.below(8); // cplabsexp: 6 to 20
        w.put(4, start);
        let grpsize = 1 << (cplexpstr - 1);
        let groups = 12 * (3 + p.cplendf - p.cplbegf) / (3 * grpsize);
        exponent_groups(w, rng, 2 * start as i32, groups, 2, 22);
    }
    for (ch, &s) in chexpstr.iter().enumerate() {
        if s != 0 {
            let start = 2 + rng.below(12);
            w.put(4, start);
            let end = p.endmant(ch);
            let groups = match s {
                1 => (end - 1) / 3,
                2 => (end - 1 + 3) / 6,
                _ => (end - 1 + 9) / 12,
            };
            exponent_groups(w, rng, start as i32, groups, 2, 22);
            w.put(2, rng.below(4)); // gainrng
        }
    }
    if lfeexpstr != 0 {
        let start = 2 + rng.below(12);
        w.put(4, start);
        exponent_groups(w, rng, start as i32, 2, 2, 22);
    }
    // bit allocation parameters
    let baie = blk == 0 || rng.chance(30);
    w.put(1, baie as u32);
    if baie {
        w.put(2, rng.below(4)); // sdcycod
        w.put(2, rng.below(4)); // fdcycod
        w.put(2, rng.below(4)); // sgaincod
        w.put(2, rng.below(4)); // dbpbcod
        w.put(3, rng.below(8)); // floorcod
    }
    let snroffste = blk == 0 || rng.chance(30);
    w.put(1, snroffste as u32);
    if snroffste {
        w.put(6, 12 + rng.below(20)); // csnroffst
        let mut fine = |w: &mut BitWriter| {
            w.put(4, rng.below(16));
            w.put(3, rng.below(8));
        };
        if p.cplinu {
            fine(w);
        }
        for _ in 0..nfch {
            fine(w);
        }
        if p.lfeon {
            fine(w);
        }
    }
    if p.cplinu {
        let cplleake = blk == 0 || rng.chance(30);
        w.put(1, cplleake as u32);
        if cplleake {
            w.put(6, rng.below(64));
        }
    }
    // delta bit allocation
    let deltbaie = rng.chance(60);
    w.put(1, deltbaie as u32);
    if deltbaie {
        // new segments, none, or (once some were sent in this frame) the
        // last ones again: "reuse" before any were sent is the one case
        // where ffmpeg 6.1 does not do what §7.2.2.6 describes
        let pick = |rng: &mut Lcg, sent: &mut bool| {
            let v = if *sent { rng.below(3) } else { 1 + rng.below(2) };
            if v == 1 {
                *sent = true;
            }
            v
        };
        let cpl = if p.cplinu { pick(rng, &mut sent[5]) } else { 2 };
        if p.cplinu {
            w.put(2, cpl);
        }
        let chans: Vec<u32> = (0..nfch).map(|c| pick(rng, &mut sent[c])).collect();
        for &c in &chans {
            w.put(2, c);
        }
        // the coupling channel's segments are empty: ffmpeg 6.1 counts
        // their bands from the coupling channel's first band, where
        // §7.2.2.6 counts from band 0 (as this decoder does)
        let cpl = if p.cplinu { Some(cpl) } else { None };
        for (strategy, coupling) in cpl.map(|c| (c, true)).into_iter().chain(chans.into_iter().map(|c| (c, false))) {
            if strategy == 1 {
                let nseg = rng.below(4);
                w.put(3, nseg);
                for _ in 0..=nseg {
                    if coupling {
                        w.put(5, 0);
                        w.put(4, 0);
                    } else {
                        w.put(5, rng.below(10)); // offset
                        w.put(4, rng.below(4)); // length
                    }
                    w.put(3, rng.below(8)); // delta
                }
            }
        }
    }
    // skip field
    if rng.chance(20) {
        w.put(1, 1);
        let n = rng.below(8);
        w.put(9, n);
        for _ in 0..n {
            w.put(8, rng.below(256));
        }
    } else {
        w.put(1, 0);
    }
}

/// Random mantissas for block `blk` of the frame being built in `w`, up
/// to `bytes`: where the block ends (with room left for the CRC), or
/// `None` if it does not fit. Grouped codes out of range are replaced by
/// valid ones (a gain triplet of the transform moves what follows, so
/// the frame is parsed again until none is left).
fn settle(w: &mut BitWriter, rng: &mut Lcg, bytes: usize, blk: usize) -> Option<usize> {
    while w.bytes.len() < bytes {
        w.put(8, rng.below(256));
    }
    for _ in 0..30 {
        let mut frame = w.bytes.clone();
        frame.truncate(bytes);
        let (ends, bad_codes) = unflash_sound::ac3::block_ends(&frame);
        let end = *ends.get(blk)?;
        if end + 17 >= bytes * 8 {
            return None;
        }
        let bad: Vec<_> = bad_codes.into_iter().filter(|c| c.0 < end).collect();
        if bad.is_empty() {
            return Some(end);
        }
        for (pos, bits, max) in bad {
            w.set(pos, bits, rng.below(max + 1));
        }
    }
    None
}

/// A random frame at 32 kHz, 640 kbit/s (3840 bytes).
fn random_frame(rng: &mut Lcg, acmod: u8, lfeon: bool) -> Vec<u8> {
    const BYTES: usize = 3840;
    let mut attempts = 0;
    'frame: loop {
        attempts += 1;
        assert!(attempts < 1000, "acmod {acmod} lfe {lfeon}: cannot build a frame");
        let p = Plan::random(rng, acmod, lfeon);
        let mut w = BitWriter::default();
        w.put(16, 0x0b77);
        w.put(16, 0); // crc1
        w.put(2, 2); // fscod: 32 kHz
        w.put(6, 36); // frmsizecod: 640 kbit/s
        w.put(5, 8); // bsid
        w.put(3, 0); // bsmod
        w.put(3, acmod as u32);
        if (acmod & 1) != 0 && acmod != 1 {
            w.put(2, rng.below(3)); // cmixlev
        }
        if acmod & 4 != 0 {
            w.put(2, rng.below(3)); // surmixlev
        }
        if acmod == 2 {
            w.put(2, 0); // dsurmod
        }
        w.put(1, lfeon as u32);
        w.put(5, 27); // dialnorm
        w.put(3, 0); // compre, langcode, audprodie
        if acmod == 0 {
            w.put(5, 27);
            w.put(3, 0);
        }
        w.put(2, 0); // copyrightb, origbs
        w.put(3, 0); // timecod1e, timecod2e, addbsie
        let mut sent = [false; 6];
        for blk in 0..6 {
            let mut tries = 0;
            loop {
                tries += 1;
                if tries > 50 {
                    // too many mantissas or unlucky group codes: start over
                    continue 'frame;
                }
                let mut trial = w.clone();
                let mut trial_sent = sent;
                side_info(&mut trial, rng, &p, blk, &mut trial_sent);
                if let Some(end) = settle(&mut trial, rng, BYTES, blk) {
                    trial.truncate(end);
                    w = trial;
                    sent = trial_sent;
                    break;
                }
            }
        }
        let mut frame = w.bytes.clone();
        frame.resize(BYTES, 0);
        fix_crcs(&mut frame);
        return frame;
    }
}

#[test]
fn random_ac3_frames_decode_as_ffmpeg_decodes_them() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED random_ac3_frames_decode_as_ffmpeg_decodes_them: ffmpeg and ffprobe are not installed");
        return;
    }
    let mut rng = Lcg(11);
    let mut seen = unflash_sound::ac3::Features::default();
    for acmod in 0..8u8 {
        for lfeon in [false, true] {
            let mut data = Vec::new();
            for _ in 0..30 {
                data.extend(random_frame(&mut rng, acmod, lfeon));
            }
            let noisy = common::ac3::decode(&data, Output::Native, true);
            let quiet = common::ac3::decode(&data, Output::Native, false);
            assert_eq!(noisy.decoded.damaged, 0, "acmod {acmod} lfe {lfeon}");
            let reference = ffmpeg_decode_bytes(&data, &format!("random-{acmod}-{lfeon}.ac3"), &[], &[]).expect("ffmpeg decodes the frames");
            assert_eq!(reference.len(), noisy.out.len(), "acmod {acmod} lfe {lfeon}: channels");
            assert_eq!(reference[0].len(), noisy.out[0].len(), "acmod {acmod} lfe {lfeon}: samples");
            let map: Vec<Option<usize>> = (0..reference.len()).map(Some).collect();
            let f = noisy.features;
            eprintln!(
                "acmod {acmod} lfe {lfeon}: block switching {} coupling {} phase flags {} rematrixing {} delta bit allocation {} skip fields {} dynrng {}",
                f.block_switching, f.coupling, f.phase_flags, f.rematrixing, f.delta_bit_allocation, f.skip_fields, f.dynrng
            );
            seen.block_switching |= f.block_switching;
            seen.coupling |= f.coupling;
            seen.phase_flags |= f.phase_flags;
            seen.rematrixing |= f.rematrixing;
            seen.delta_bit_allocation |= f.delta_bit_allocation;
            seen.skip_fields |= f.skip_fields;
            seen.dynrng |= f.dynrng;
            for (c, s) in compare_with(&quiet.out, &noisy.out, &noisy.trace, &reference, &map, TOLERANCE).iter().enumerate() {
                eprintln!("  channel {c}: {}", describe(s));
                assert!(s.bins > 1000, "acmod {acmod} lfe {lfeon} channel {c}: too little to compare");
                assert_eq!(s.over, 0, "acmod {acmod} lfe {lfeon} channel {c}: differs from ffmpeg by up to {:e}", s.max_diff);
            }
        }
    }
    assert!(seen.block_switching && seen.coupling && seen.phase_flags && seen.rematrixing, "{seen:?}");
    assert!(seen.delta_bit_allocation && seen.skip_fields && seen.dynrng, "{seen:?}");
}

// E-AC-3 ------------------------------------------------------------------

/// An E-AC-3 frame's choices made in its audio frame element (Annex E
/// §2.3.2): the strategies of every block.
struct EPlan {
    acmod: u8,
    lfeon: bool,
    nfch: usize,
    expstre: bool,
    ahte: bool,
    snroffststr: u32,
    transproce: bool,
    blkswe: bool,
    dithflage: bool,
    bamode: bool,
    frmfgaincode: bool,
    dbaflde: bool,
    skipflde: bool,
    spxattene: bool,
    cplstre: [bool; 6],
    cplinu: [bool; 6],
    cplexpstr: [u32; 6],
    chexpstr: [[u32; 5]; 6],
    lfeexpstr: [u32; 6],
    frmcplexpstr: u32,
    frmchexpstr: [u32; 5],
    /// Blocks with a new spectral extension strategy, and the extension
    /// in use.
    spxstre: [bool; 6],
    spxinu: [bool; 6],
}

impl EPlan {
    fn random(rng: &mut Lcg, acmod: u8, lfeon: bool, blocks: usize) -> EPlan {
        let nfch = NFCHANS[acmod as usize];
        let six = blocks == 6;
        let expstre = !six || rng.chance(50);
        let ahte = six && rng.chance(60);
        let mut p = EPlan {
            acmod,
            lfeon,
            nfch,
            expstre,
            ahte,
            // (SNR offsets per block, strategies 1 and 2, ffmpeg 6.1 does
            // not decode as the standard says)
            snroffststr: 0,
            transproce: rng.chance(30),
            blkswe: rng.chance(40),
            // (with the transform on, dither everywhere: ffmpeg 6.1 dithers
            // its zero-bit coefficients whatever the dither flags say)
            dithflage: rng.chance(50) && !ahte,
            bamode: rng.chance(50),
            frmfgaincode: rng.chance(50),
            dbaflde: rng.chance(50),
            skipflde: rng.chance(50),
            spxattene: rng.chance(50),
            cplstre: [false; 6],
            cplinu: [false; 6],
            cplexpstr: [0; 6],
            chexpstr: [[0; 5]; 6],
            lfeexpstr: [0; 6],
            frmcplexpstr: 0,
            frmchexpstr: [0; 5],
            spxstre: [false; 6],
            spxinu: [false; 6],
        };
        // exponent strategies; with the transform on, some channels send
        // exponents once, as it needs
        for ch in 0..nfch {
            let once = ahte && rng.chance(60);
            if expstre {
                for blk in 0..blocks {
                    p.chexpstr[blk][ch] = if blk == 0 || (!once && rng.chance(40)) { 1 + rng.below(3) } else { 0 };
                }
            } else {
                let code = if once { 0 } else { rng.below(32) };
                p.frmchexpstr[ch] = code;
                p.chexpstr.iter_mut().enumerate().for_each(|(blk, s)| s[ch] = u32::from(FRMEXPSTR[code as usize][blk]));
            }
        }
        if lfeon {
            let once = ahte && rng.chance(60);
            for blk in 0..blocks {
                p.lfeexpstr[blk] = (blk == 0 || (!once && rng.chance(40))) as u32;
            }
        }
        // coupling and spectral extension strategies change only where
        // every full bandwidth channel sends exponents (their bandwidths
        // change)
        let open = |p: &EPlan, blk: usize| blk == 0 || (0..nfch).all(|ch| p.chexpstr[blk][ch] != 0);
        p.cplstre[0] = true;
        p.spxstre[0] = true;
        if acmod > 1 {
            p.cplinu[0] = rng.chance(60);
            for blk in 1..blocks {
                if open(&p, blk) && rng.chance(50) {
                    // coupling goes on, changes or stops, but does not
                    // start after block 0 (ffmpeg 6.1 decodes such blocks
                    // otherwise)
                    p.cplstre[blk] = true;
                    p.cplinu[blk] = p.cplinu[blk - 1] && rng.chance(70);
                } else {
                    p.cplinu[blk] = p.cplinu[blk - 1];
                }
            }
        }
        // the extension: new strategies only where coupling is not in use
        // around (ffmpeg 6.1 decodes such blocks otherwise; the coupling's
        // end moves with the extension's beginning)
        p.spxinu[0] = rng.chance(60);
        for blk in 1..blocks {
            p.spxstre[blk] = open(&p, blk) && rng.chance(40) && !p.cplinu[blk] && !p.cplinu[blk - 1];
            p.spxinu[blk] = if p.spxstre[blk] { rng.chance(60) } else { p.spxinu[blk - 1] };
        }
        // coupling exponents: new wherever coupling starts or changes
        let must = |p: &EPlan, blk: usize| p.cplinu[blk] && (blk == 0 || p.cplstre[blk] || !p.cplinu[blk - 1]);
        if expstre {
            for blk in 0..blocks {
                if p.cplinu[blk] {
                    p.cplexpstr[blk] = if must(&p, blk) || rng.chance(40) { 1 + rng.below(3) } else { 0 };
                }
            }
        } else if p.cplinu.iter().any(|&c| c) {
            let codes: Vec<usize> = (0..32).filter(|&c| (0..6).all(|blk| !must(&p, blk) || FRMEXPSTR[c][blk] != 0)).collect();
            let code = codes[rng.below(codes.len() as u32) as usize];
            p.frmcplexpstr = code as u32;
            p.cplexpstr = FRMEXPSTR[code].map(u32::from);
        }
        p
    }
}

/// What an E-AC-3 frame's blocks carry over from one block to the next.
#[derive(Clone)]
struct EState {
    spxinu: bool,
    chinspx: [bool; 5],
    spxbegf: u32,
    spx_begin: u32,
    spx_end: u32,
    spxbndstrc: [bool; 17],
    firstspxcos: [bool; 5],
    chincpl: [bool; 5],
    phsflginu: bool,
    cplbegf: u32,
    /// The coupling channel's last sub-band plus one.
    cpl_end: u32,
    cplbndstrc: [bool; 18],
    firstcplcos: [bool; 5],
    firstcplleak: bool,
    /// An extension band structure was set in this frame (the default one
    /// counts only in block 0: ffmpeg 6.1 does not take it when the
    /// extension starts in a later block)
    spxbndstrc_set: bool,
    chbwcod: [u32; 5],
    /// Each full bandwidth channel's end bin in the block before.
    ends: [u32; 5],
    /// The fast gain codes in use are the default ones.
    fgain_default: bool,
    /// Delta bit allocation segments sent in this frame (as in `side_info`).
    sent: [bool; 6],
}

impl EState {
    fn new() -> EState {
        EState {
            spxinu: false,
            chinspx: [false; 5],
            spxbegf: 0,
            spx_begin: 0,
            spx_end: 0,
            spxbndstrc: DEFAULT_SPX_BNDSTRC,
            firstspxcos: [true; 5],
            chincpl: [false; 5],
            phsflginu: false,
            cplbegf: 0,
            cpl_end: 0,
            cplbndstrc: DEFAULT_CPL_BNDSTRC,
            firstcplcos: [true; 5],
            firstcplleak: true,
            spxbndstrc_set: false,
            chbwcod: [0; 5],
            ends: [0; 5],
            fgain_default: true,
            sent: [false; 6],
        }
    }

    fn endmant(&self, ch: usize, cplinu: bool) -> u32 {
        if cplinu && self.chincpl[ch] {
            37 + 12 * self.cplbegf
        } else if self.spxinu && self.chinspx[ch] {
            25 + 12 * self.spx_begin
        } else {
            (self.chbwcod[ch] + 12) * 3 + 37
        }
    }
}

/// One E-AC-3 block's side information (Annex E §2.3.3), up to the
/// mantissas.
fn eac3_side_info(w: &mut BitWriter, rng: &mut Lcg, p: &EPlan, st: &mut EState, blk: usize) {
    let (acmod, nfch) = (p.acmod, p.nfch);
    let cplinu = p.cplinu[blk];
    if p.blkswe {
        for _ in 0..nfch {
            w.put(1, rng.chance(3) as u32); // blksw (left out of the comparison)
        }
    }
    if p.dithflage {
        for _ in 0..nfch {
            w.put(1, rng.chance(50) as u32);
        }
    }
    for _ in 0..if acmod == 0 { 2 } else { 1 } {
        if rng.chance(40) {
            w.put(1, 1);
            w.put(8, rng.below(256));
        } else {
            w.put(1, 0);
        }
    }
    // spectral extension strategy and coordinates
    let spxstre = p.spxstre[blk];
    if blk > 0 {
        w.put(1, spxstre as u32);
    }
    let (old_begin, old_end, was_spx) = (st.spx_begin, st.spx_end, st.spxinu);
    if spxstre {
        st.spxinu = p.spxinu[blk];
        w.put(1, st.spxinu as u32);
        if st.spxinu {
            for ch in 0..nfch {
                st.chinspx[ch] = acmod == 1 || rng.chance(70);
                if acmod != 1 {
                    w.put(1, st.chinspx[ch] as u32);
                }
            }
            const BEGIN: [u32; 8] = [2, 3, 4, 5, 6, 7, 9, 11];
            const END: [u32; 8] = [5, 6, 7, 9, 11, 13, 15, 17];
            st.spxbegf = rng.below(8);
            st.spx_begin = BEGIN[st.spxbegf as usize];
            let ends: Vec<u32> = (0..8).filter(|&e| END[e as usize] > st.spx_begin).collect();
            let spxendf = ends[rng.below(ends.len() as u32) as usize];
            st.spx_end = END[spxendf as usize];
            let spxstrtf = rng.below(st.spx_begin.min(4));
            w.put(2, spxstrtf);
            w.put(3, st.spxbegf);
            w.put(3, spxendf);
            // the band structure of the block before is reused only for
            // the same range
            let spxbndstrce = rng.chance(50) || (was_spx && (old_begin, old_end) != (st.spx_begin, st.spx_end)) || (blk > 0 && !st.spxbndstrc_set);
            w.put(1, spxbndstrce as u32);
            st.spxbndstrc_set = true;
            if spxbndstrce {
                for bnd in st.spx_begin + 1..st.spx_end {
                    st.spxbndstrc[bnd as usize] = rng.chance(40);
                    w.put(1, st.spxbndstrc[bnd as usize] as u32);
                }
            }
        } else {
            st.chinspx = [false; 5];
            st.firstspxcos = [true; 5];
        }
    }
    if st.spxinu {
        let nbnds = 1 + (st.spx_begin + 1..st.spx_end).filter(|&b| !st.spxbndstrc[b as usize]).count();
        for ch in 0..nfch {
            if !st.chinspx[ch] {
                st.firstspxcos[ch] = true;
                continue;
            }
            // (new coordinates with every new strategy)
            let spxcoe = if st.firstspxcos[ch] {
                st.firstspxcos[ch] = false;
                true
            } else {
                let e = spxstre || rng.chance(50);
                w.put(1, e as u32);
                e
            };
            if spxcoe {
                // no noise at all half the time, so that the translation
                // is compared exactly
                w.put(5, if rng.chance(50) { 31 } else { rng.below(32) }); // spxblnd
                coordinates(w, rng, nbnds, 2);
            }
        }
    }
    // coupling strategy
    let (old_cplbegf, old_cpl_end, was_cpl) = (st.cplbegf, st.cpl_end, blk > 0 && p.cplinu[blk - 1]);
    if p.cplstre[blk] && acmod > 1 {
        if cplinu {
            w.put(1, 0); // ecplinu
            if acmod == 2 {
                st.chincpl = [true, true, false, false, false];
            } else {
                loop {
                    for ch in 0..nfch {
                        st.chincpl[ch] = rng.chance(70);
                    }
                    if st.chincpl.iter().filter(|&&c| c).count() >= 2 {
                        break;
                    }
                }
                for ch in 0..nfch {
                    w.put(1, st.chincpl[ch] as u32);
                }
            }
            if acmod == 2 {
                st.phsflginu = rng.chance(60);
                w.put(1, st.phsflginu as u32);
            }
            if st.spxinu {
                st.cpl_end = if st.spxbegf < 6 { st.spxbegf + 1 } else { st.spxbegf * 2 - 4 };
                st.cplbegf = rng.below(st.cpl_end.min(16));
                w.put(4, st.cplbegf);
            } else {
                st.cplbegf = rng.below(16);
                let lo = st.cplbegf.saturating_sub(2);
                let cplendf = lo + rng.below(16 - lo);
                st.cpl_end = cplendf + 3;
                w.put(4, st.cplbegf);
                w.put(4, cplendf);
            }
            let cplbndstrce = rng.chance(50) || (was_cpl && (old_cplbegf, old_cpl_end) != (st.cplbegf, st.cpl_end));
            w.put(1, cplbndstrce as u32);
            if cplbndstrce {
                for sb in st.cplbegf + 1..st.cpl_end {
                    st.cplbndstrc[sb as usize] = rng.chance(40);
                    w.put(1, st.cplbndstrc[sb as usize] as u32);
                }
            }
        } else {
            st.chincpl = [false; 5];
            st.firstcplcos = [true; 5];
            st.firstcplleak = true;
            st.phsflginu = false;
        }
    }
    // coupling coordinates and phase flags
    if cplinu {
        let ncplbnd = 1 + (st.cplbegf + 1..st.cpl_end).filter(|&sb| !st.cplbndstrc[sb as usize]).count();
        let mut cplcoe = [false; 5];
        for (ch, coe) in cplcoe.iter_mut().enumerate().take(nfch) {
            if !st.chincpl[ch] {
                st.firstcplcos[ch] = true;
                continue;
            }
            // new coordinates with every new strategy (A/52 §7.14's
            // error conditions 23 and 24)
            *coe = if st.firstcplcos[ch] {
                st.firstcplcos[ch] = false;
                true
            } else {
                let e = p.cplstre[blk] || rng.chance(50);
                w.put(1, e as u32);
                e
            };
            if *coe {
                coordinates(w, rng, ncplbnd, 4);
            }
        }
        if acmod == 2 && st.phsflginu && (cplcoe[0] || cplcoe[1]) {
            for _ in 0..ncplbnd {
                w.put(1, rng.below(2));
            }
        }
    }
    // rematrixing: new flags whenever the bands may have changed
    if acmod == 2 {
        let n = if cplinu {
            match st.cplbegf {
                0 => 2,
                1 | 2 => 3,
                _ => 4,
            }
        } else if st.spxinu && st.spxbegf < 2 {
            3
        } else {
            4
        };
        let rematstr = blk == 0 || p.cplstre[blk] || spxstre || rng.chance(50);
        if blk > 0 {
            w.put(1, rematstr as u32);
        }
        if rematstr {
            w.put(n, rng.below(1 << n));
        }
    }
    // bandwidths, exponents
    for ch in 0..nfch {
        if p.chexpstr[blk][ch] != 0 && !(cplinu && st.chincpl[ch]) && !(st.spxinu && st.chinspx[ch]) {
            st.chbwcod[ch] = rng.below(61);
            w.put(6, st.chbwcod[ch]);
        }
    }
    if cplinu && p.cplexpstr[blk] != 0 {
        let start = 3 + rng.below(8);
        w.put(4, start);
        let grpsize = 1 << (p.cplexpstr[blk] - 1);
        let groups = 12 * (st.cpl_end - st.cplbegf) / (3 * grpsize);
        exponent_groups(w, rng, 2 * start as i32, groups, 2, 22);
    }
    for ch in 0..nfch {
        let strategy = p.chexpstr[blk][ch];
        if strategy != 0 {
            let start = 2 + rng.below(12);
            w.put(4, start);
            let end = st.endmant(ch, cplinu);
            let groups = match strategy {
                1 => (end - 1) / 3,
                2 => (end - 1 + 3) / 6,
                _ => (end - 1 + 9) / 12,
            };
            exponent_groups(w, rng, start as i32, groups, 2, 22);
            w.put(2, rng.below(4)); // gainrng
        }
    }
    if p.lfeon && p.lfeexpstr[blk] != 0 {
        let start = 2 + rng.below(12);
        w.put(4, start);
        exponent_groups(w, rng, start as i32, 2, 2, 22);
    }
    let mut ends = [0; 5];
    for (ch, e) in ends.iter_mut().enumerate().take(nfch) {
        *e = st.endmant(ch, cplinu);
    }
    let changed = ends != st.ends;
    st.ends = ends;
    // bit allocation parameters, SNR offsets, fast gains
    if p.bamode {
        let baie = blk == 0 || rng.chance(30);
        w.put(1, baie as u32);
        if baie {
            w.put(11, rng.below(1 << 11));
        }
    }
    if p.snroffststr != 0 {
        let snroffste = blk == 0 || rng.chance(30);
        if blk > 0 {
            w.put(1, snroffste as u32);
        }
        if snroffste {
            w.put(6, 8 + rng.below(20)); // csnroffst
            let n = if p.snroffststr == 1 { 1 } else { cplinu as usize + nfch + p.lfeon as usize };
            for _ in 0..n {
                w.put(4, rng.below(16));
            }
        }
    }
    if p.frmfgaincode {
        // no fast gain codes means the default ones (Table E1.4); ffmpeg
        // 6.1 keeps those of the block before, so codes are sent again
        // until they are back to the default
        let fgaincode = !st.fgain_default || rng.chance(50);
        w.put(1, fgaincode as u32);
        st.fgain_default = true;
        if fgaincode {
            for _ in 0..cplinu as usize + nfch + p.lfeon as usize {
                let code = if rng.chance(30) { 4 } else { rng.below(8) };
                st.fgain_default &= code == 4;
                w.put(3, code);
            }
        }
    }
    if rng.chance(20) {
        w.put(1, 1); // convsnroffste
        w.put(10, rng.below(1024));
    } else {
        w.put(1, 0);
    }
    if cplinu {
        let cplleake = if st.firstcplleak {
            st.firstcplleak = false;
            true
        } else {
            let e = rng.chance(30);
            w.put(1, e as u32);
            e
        };
        if cplleake {
            w.put(6, rng.below(64));
        }
    }
    // delta bit allocation (as for AC-3); where a bandwidth or the
    // coupling set-up changes, every channel's strategy is sent anew
    // (A/52 §7.14's error conditions 26 to 28)
    if p.dbaflde {
        let fresh = blk > 0 && (changed || p.cplstre[blk]);
        let deltbaie = fresh || rng.chance(60);
        w.put(1, deltbaie as u32);
        if deltbaie {
            let mut pick = |rng: &mut Lcg, c: usize| {
                let v = if st.sent[c] && !fresh { rng.below(3) } else { 1 + rng.below(2) };
                if v == 1 {
                    st.sent[c] = true;
                }
                v
            };
            let cpl = if cplinu { Some(pick(rng, 5)) } else { None };
            let chans: Vec<u32> = (0..nfch).map(|c| pick(rng, c)).collect();
            if let Some(c) = cpl {
                w.put(2, c);
            }
            for &c in &chans {
                w.put(2, c);
            }
            for (strategy, coupling) in cpl.map(|c| (c, true)).into_iter().chain(chans.into_iter().map(|c| (c, false))) {
                if strategy == 1 {
                    let nseg = rng.below(4);
                    w.put(3, nseg);
                    for _ in 0..=nseg {
                        if coupling {
                            w.put(9, 0);
                        } else {
                            w.put(5, rng.below(10));
                            w.put(4, rng.below(4));
                        }
                        w.put(3, rng.below(8));
                    }
                }
            }
        }
    }
    if p.skipflde {
        if rng.chance(20) {
            w.put(1, 1);
            let n = rng.below(8);
            w.put(9, n);
            for _ in 0..n {
                w.put(8, rng.below(256));
            }
        } else {
            w.put(1, 0);
        }
    }
}

/// A random E-AC-3 frame (independent substream 0) of `blocks` blocks,
/// 4096 bytes.
fn random_eac3_frame(rng: &mut Lcg, acmod: u8, lfeon: bool, blocks: usize, fscod: u32) -> Vec<u8> {
    const BYTES: usize = 4096;
    let numblkscod = match blocks {
        1 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    };
    let nfch = NFCHANS[acmod as usize];
    let mut attempts = 0;
    'frame: loop {
        attempts += 1;
        assert!(attempts < 1000, "acmod {acmod} lfe {lfeon} blocks {blocks}: cannot build a frame");
        let p = EPlan::random(rng, acmod, lfeon, blocks);
        let mut w = BitWriter::default();
        let maybe = |w: &mut BitWriter, rng: &mut Lcg, bits: u32| {
            let e = rng.chance(50);
            w.put(1, e as u32);
            if e {
                w.put(bits, rng.below(1 << bits));
            }
        };
        // bit stream information (Annex E §2.3.1)
        w.put(16, 0x0b77);
        w.put(2, 0); // strmtyp: independent
        w.put(3, 0); // substreamid
        w.put(11, (BYTES / 2 - 1) as u32); // frmsiz
        w.put(2, fscod);
        w.put(2, numblkscod);
        w.put(3, acmod as u32);
        w.put(1, lfeon as u32);
        w.put(5, 16); // bsid
        w.put(5, 1 + rng.below(31)); // dialnorm
        maybe(&mut w, rng, 8); // compr
        if acmod == 0 {
            w.put(5, 1 + rng.below(31)); // dialnorm2
            maybe(&mut w, rng, 8); // compr2
        }
        let mixmdate = rng.chance(50);
        w.put(1, mixmdate as u32);
        if mixmdate {
            if acmod > 2 {
                w.put(2, rng.below(4)); // dmixmod
            }
            if (acmod & 1) != 0 && acmod > 2 {
                w.put(6, rng.below(64)); // ltrtcmixlev, lorocmixlev
            }
            if acmod & 4 != 0 {
                w.put(6, rng.below(64)); // ltrtsurmixlev, lorosurmixlev
            }
            if lfeon {
                maybe(&mut w, rng, 5); // lfemixlevcod
            }
            maybe(&mut w, rng, 6); // pgmscl
            if acmod == 0 {
                maybe(&mut w, rng, 6); // pgmscl2
            }
            maybe(&mut w, rng, 6); // extpgmscl
            // (not 3: ffmpeg 6.1 takes the mixing data to start after
            // mixdeflen, where Annex E §3.10.4 counts mixdeflen in)
            let mixdef = rng.below(3);
            w.put(2, mixdef);
            match mixdef {
                1 => w.put(5, rng.below(32)),
                2 => w.put(12, rng.below(4096)),
                3 => {
                    // mixdeflen, then mixing data to fill mixdeflen + 2 bytes
                    let len = rng.below(32);
                    w.put(5, len);
                    for _ in 0..8 * (len + 2) - 5 {
                        w.put(1, rng.below(2));
                    }
                }
                _ => {}
            }
            if acmod < 2 {
                maybe(&mut w, rng, 14); // panmean, paninfo
                if acmod == 0 {
                    maybe(&mut w, rng, 14); // panmean2, paninfo2
                }
            }
            let frmmixcfginfoe = rng.chance(50);
            w.put(1, frmmixcfginfoe as u32);
            if frmmixcfginfoe {
                if numblkscod == 0 {
                    w.put(5, rng.below(32));
                } else {
                    for _ in 0..blocks {
                        maybe(&mut w, rng, 5);
                    }
                }
            }
        }
        let infomdate = rng.chance(50);
        w.put(1, infomdate as u32);
        if infomdate {
            w.put(5, rng.below(32)); // bsmod, copyrightb, origbs
            if acmod == 2 {
                w.put(4, rng.below(16)); // dsurmod, dheadphonmod
            }
            if acmod >= 6 {
                w.put(2, rng.below(4)); // dsurexmod
            }
            maybe(&mut w, rng, 8); // mixlevel, roomtyp, adconvtyp
            if acmod == 0 {
                maybe(&mut w, rng, 8);
            }
            w.put(1, rng.below(2)); // sourcefscod
        }
        if numblkscod != 3 {
            w.put(1, rng.below(2)); // convsync
        }
        if rng.chance(20) {
            w.put(1, 1); // addbsie
            let n = rng.below(4);
            w.put(6, n);
            for _ in 0..=n {
                w.put(8, rng.below(256));
            }
        } else {
            w.put(1, 0);
        }
        // audio frame (Annex E §2.3.2)
        if numblkscod == 3 {
            w.put(1, p.expstre as u32);
            w.put(1, p.ahte as u32);
        }
        w.put(2, p.snroffststr);
        for flag in [p.transproce, p.blkswe, p.dithflage, p.bamode, p.frmfgaincode, p.dbaflde, p.skipflde, p.spxattene] {
            w.put(1, flag as u32);
        }
        if acmod > 1 {
            w.put(1, p.cplinu[0] as u32);
            for blk in 1..blocks {
                w.put(1, p.cplstre[blk] as u32);
                if p.cplstre[blk] {
                    w.put(1, p.cplinu[blk] as u32);
                }
            }
        }
        let ncplblks = p.cplinu[..blocks].iter().filter(|&&c| c).count();
        if p.expstre {
            for blk in 0..blocks {
                if p.cplinu[blk] {
                    w.put(2, p.cplexpstr[blk]);
                }
                for ch in 0..nfch {
                    w.put(2, p.chexpstr[blk][ch]);
                }
            }
        } else {
            if acmod > 1 && ncplblks > 0 {
                w.put(5, p.frmcplexpstr);
            }
            for ch in 0..nfch {
                w.put(5, p.frmchexpstr[ch]);
            }
        }
        if lfeon {
            for blk in 0..blocks {
                w.put(1, p.lfeexpstr[blk]);
            }
        }
        let convexpstre = numblkscod == 3 || rng.chance(50);
        if numblkscod != 3 {
            w.put(1, convexpstre as u32);
        }
        if convexpstre {
            for _ in 0..nfch {
                w.put(5, rng.below(32));
            }
        }
        // the transform, for channels whose exponents are sent once
        if p.ahte {
            let ncplregs = (0..6).filter(|&blk| p.cplstre[blk] || (p.cplinu[blk] && p.cplexpstr[blk] != 0)).count();
            if ncplblks == 6 && ncplregs == 1 {
                w.put(1, rng.chance(60) as u32); // cplahtinu
            }
            for ch in 0..nfch {
                if (0..6).filter(|&blk| p.chexpstr[blk][ch] != 0).count() == 1 {
                    w.put(1, rng.chance(70) as u32); // chahtinu
                }
            }
            if lfeon && (0..6).filter(|&blk| p.lfeexpstr[blk] != 0).count() == 1 {
                w.put(1, rng.chance(60) as u32); // lfeahtinu
            }
        }
        if p.snroffststr == 0 {
            w.put(6, 8 + rng.below(20)); // frmcsnroffst
            w.put(4, rng.below(16)); // frmfsnroffst
        }
        if p.transproce {
            for _ in 0..nfch {
                maybe(&mut w, rng, 18); // transprocloc, transproclen
            }
        }
        if p.spxattene {
            for _ in 0..nfch {
                maybe(&mut w, rng, 5); // spxattencod
            }
        }
        if numblkscod != 0 {
            let blkstrtinfoe = rng.chance(30);
            w.put(1, blkstrtinfoe as u32);
            if blkstrtinfoe {
                // a start position for blocks 1 on, 4 + log2(2048 words) bits each
                for _ in 0..(blocks - 1) * 15 {
                    w.put(1, rng.below(2));
                }
            }
        }
        let mut st = EState::new();
        for blk in 0..blocks {
            let mut tries = 0;
            loop {
                tries += 1;
                if tries > 50 {
                    continue 'frame;
                }
                let mut trial = w.clone();
                let mut trial_st = st.clone();
                eac3_side_info(&mut trial, rng, &p, &mut trial_st, blk);
                if let Some(end) = settle(&mut trial, rng, BYTES, blk) {
                    trial.truncate(end);
                    w = trial;
                    st = trial_st;
                    break;
                }
            }
        }
        let mut frame = w.bytes.clone();
        frame.resize(BYTES, 0);
        fix_crcs(&mut frame);
        return frame;
    }
}

#[test]
fn random_eac3_frames_decode_as_ffmpeg_decodes_them() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED random_eac3_frames_decode_as_ffmpeg_decodes_them: ffmpeg and ffprobe are not installed");
        return;
    }
    let mut rng = Lcg(23);
    let mut seen = unflash_sound::ac3::Features::default();
    let mut streams: Vec<(u8, bool, usize)> = Vec::new();
    for acmod in 0..8u8 {
        streams.push((acmod, false, 6));
        streams.push((acmod, true, 6));
    }
    streams.extend([(2, false, 1), (7, true, 2), (1, false, 3), (0, true, 3), (3, true, 1)]);
    for (acmod, lfeon, blocks) in streams {
        let fscod = rng.below(3);
        let mut data = Vec::new();
        for _ in 0..120 / blocks {
            data.extend(random_eac3_frame(&mut rng, acmod, lfeon, blocks, fscod));
        }
        let what = format!("acmod {acmod} lfe {lfeon} blocks {blocks} fscod {fscod}");
        let noisy = common::ac3::decode(&data, Output::Native, true);
        let quiet = common::ac3::decode(&data, Output::Native, false);
        assert_eq!(noisy.decoded.damaged, 0, "{what}");
        let reference = ffmpeg_decode_bytes(&data, &format!("random-{acmod}-{lfeon}-{blocks}.eac3"), &[], &[]).expect("ffmpeg decodes the frames");
        assert_eq!(reference.len(), noisy.out.len(), "{what}: channels");
        assert_eq!(reference[0].len(), noisy.out[0].len(), "{what}: samples");
        let map: Vec<Option<usize>> = (0..reference.len()).map(Some).collect();
        let f = noisy.features;
        eprintln!("{what}: {f:?}");
        seen.block_switching |= f.block_switching;
        seen.coupling |= f.coupling;
        seen.phase_flags |= f.phase_flags;
        seen.rematrixing |= f.rematrixing;
        seen.delta_bit_allocation |= f.delta_bit_allocation;
        seen.spectral_extension |= f.spectral_extension;
        seen.spx_attenuation |= f.spx_attenuation;
        seen.aht |= f.aht;
        seen.aht_gaq |= f.aht_gaq;
        seen.aht_vq |= f.aht_vq;
        seen.transient_pre_noise |= f.transient_pre_noise;
        seen.skip_fields |= f.skip_fields;
        seen.dynrng |= f.dynrng;
        for (c, s) in compare_with(&quiet.out, &noisy.out, &noisy.trace, &reference, &map, TOLERANCE).iter().enumerate() {
            eprintln!("  channel {c}: {}", describe(s));
            assert!(s.bins > 1000, "{what} channel {c}: too little to compare");
            assert_eq!(s.over, 0, "{what} channel {c}: differs from ffmpeg by up to {:e}", s.max_diff);
        }
    }
    eprintln!("{seen:?}");
    assert!(seen.block_switching && seen.coupling && seen.phase_flags && seen.rematrixing && seen.delta_bit_allocation, "{seen:?}");
    assert!(seen.spectral_extension && seen.spx_attenuation && seen.aht && seen.aht_gaq && seen.aht_vq, "{seen:?}");
    assert!(seen.transient_pre_noise && seen.skip_fields && seen.dynrng, "{seen:?}");
}
