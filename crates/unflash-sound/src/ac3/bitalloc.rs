//! Exponent decoding (A/52 §7.1.3) and the parametric bit allocation
//! (§7.2.2, with Annex E §3.4.3's high efficiency pointers for the
//! adaptive hybrid transform), in the standard's integer arithmetic.

use crate::ac3::tables::{BNDSZ, BNDTAB, DBPBTAB, FASTDEC, FASTGAIN, FLOORTAB, HTH, LATAB, MASKTAB, SLOWDEC, SLOWGAIN};

/// Decode one channel's exponents: `absexp` then `groups` 7-bit words of
/// three mapped differentials each, each differential shared by
/// `grpsize` (1, 2 or 4) bins. Writes `exp[first..]`, the absolute
/// exponent first for full bandwidth and LFE channels (`skip_abs` false)
/// or not at all for the coupling channel (`skip_abs` true, §7.1.3:
/// "cplexp[n + cplstrtmant] = exp[n + 1]"). Fails if a group code is
/// above 124 or an exponent leaves 0 to 24 (§7.10.2).
pub fn decode_exponents(absexp: u8, groups: &[u8], grpsize: usize, skip_abs: bool, exp: &mut [u8; 256], first: usize) -> Result<(), &'static str> {
    let mut prev = absexp as i32;
    let mut pos = first;
    if !skip_abs {
        exp[pos] = absexp;
        pos += 1;
    }
    for &g in groups {
        if g > 124 {
            return Err("exponent group code above 124");
        }
        let m = [g / 25, (g % 25) / 5, g % 5];
        for d in m {
            let e = prev + d as i32 - 2;
            if !(0..=24).contains(&e) {
                return Err("exponent out of range");
            }
            prev = e;
            for _ in 0..grpsize {
                if pos < 256 {
                    exp[pos] = e as u8;
                }
                pos += 1;
            }
        }
    }
    Ok(())
}

/// The bit allocation parameters common to the channels of a block.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub sdcycod: usize,
    pub fdcycod: usize,
    pub sgaincod: usize,
    pub dbpbcod: usize,
    pub floorcod: usize,
    /// Which hearing threshold table (`fscod`, 0 to 2).
    pub fscod: usize,
}

impl Default for Params {
    fn default() -> Self {
        // the values E-AC-3 uses when bamode is 0 (Annex E Table E1.4)
        Params { sdcycod: 2, fdcycod: 1, sgaincod: 1, dbpbcod: 2, floorcod: 7, fscod: 0 }
    }
}

/// Delta bit allocation of one channel (§7.2.2.6).
#[derive(Clone, Copy, Debug, Default)]
pub struct Dba {
    pub active: bool,
    pub nseg: usize,
    pub offst: [u8; 8],
    pub len: [u8; 8],
    pub ba: [u8; 8],
}

/// One channel's allocation inputs.
pub struct Channel<'a> {
    pub exp: &'a [u8; 256],
    pub start: usize,
    pub end: usize,
    /// fastgain code (Table 7.11).
    pub fgaincod: usize,
    /// The SNR offset: (((csnroffst - 15) << 4) + fsnroffst) << 2.
    pub snroffset: i32,
    /// The coupling channel's leak initialisation (cplfleak, cplsleak).
    pub leak: Option<(i32, i32)>,
    pub dba: &'a Dba,
    /// Look pointers up in `hebaptab` (the adaptive hybrid transform)
    /// rather than `baptab`.
    pub table: &'a [u8; 64],
}

fn logadd(a: i32, b: i32) -> i32 {
    let c = a - b;
    let address = ((c.abs() >> 1) as usize).min(255);
    if c >= 0 {
        a + LATAB[address] as i32
    } else {
        b + LATAB[address] as i32
    }
}

fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        if b0 + 256 == b1 {
            384
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else if bin < 20 {
        if b0 + 256 == b1 {
            320
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else {
        (a - 128).max(0)
    }
}

/// Compute the bit allocation pointers of bins `start..end` (§7.2.2.2 to
/// §7.2.2.7).
pub fn allocate(p: &Params, ch: &Channel, bap: &mut [u8; 256]) {
    let (start, end) = (ch.start, ch.end.min(253));
    if start >= end {
        return;
    }
    let sdecay = SLOWDEC[p.sdcycod];
    let fdecay = FASTDEC[p.fdcycod];
    let sgain = SLOWGAIN[p.sgaincod];
    let dbknee = DBPBTAB[p.dbpbcod];
    let floor = FLOORTAB[p.floorcod];
    let fgain = FASTGAIN[ch.fgaincod];

    // exponents to power spectral density
    let mut psd = [0i32; 256];
    for (p, &e) in psd[start..end].iter_mut().zip(&ch.exp[start..end]) {
        *p = 3072 - ((e as i32) << 7);
    }

    // integrate into bands (one more slot so that bndpsd[bin + 1] is
    // always readable)
    let mut bndpsd = [0i32; 51];
    let mut j = start;
    let mut k = MASKTAB[start] as usize;
    loop {
        let lastbin = (BNDTAB[k] as usize + BNDSZ[k] as usize).min(end);
        bndpsd[k] = psd[j];
        j += 1;
        while j < lastbin {
            bndpsd[k] = logadd(bndpsd[k], psd[j]);
            j += 1;
        }
        k += 1;
        if end <= lastbin {
            break;
        }
    }

    // excitation
    let bndstrt = MASKTAB[start] as usize;
    let bndend = MASKTAB[end - 1] as usize + 1;
    let mut excite = [0i32; 50];
    let (mut fastleak, mut slowleak) = (0i32, 0i32);
    let begin;
    if bndstrt == 0 {
        // full bandwidth and LFE channels; bndend 7 is the LFE channel,
        // whose last band has no neighbour above
        let lfe = bndend == 7;
        let mut lowcomp = calc_lowcomp(0, bndpsd[0], bndpsd[1], 0);
        excite[0] = bndpsd[0] - fgain - lowcomp;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[1], bndpsd[2], 1);
        excite[1] = bndpsd[1] - fgain - lowcomp;
        let mut b = 7;
        for bin in 2..7usize {
            if bin >= bndend {
                break;
            }
            if !lfe || bin != 6 {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = bndpsd[bin] - fgain;
            slowleak = bndpsd[bin] - sgain;
            excite[bin] = fastleak - lowcomp;
            if (!lfe || bin != 6) && bndpsd[bin] <= bndpsd[bin + 1] {
                b = bin + 1;
                break;
            }
        }
        for bin in b..bndend.min(22) {
            if !lfe || bin != 6 {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak -= fdecay;
            fastleak = fastleak.max(bndpsd[bin] - fgain);
            slowleak -= sdecay;
            slowleak = slowleak.max(bndpsd[bin] - sgain);
            excite[bin] = (fastleak - lowcomp).max(slowleak);
        }
        begin = 22;
    } else {
        // the coupling channel
        let (fl, sl) = ch.leak.unwrap_or((0, 0));
        fastleak = (fl << 8) + 768;
        slowleak = (sl << 8) + 768;
        begin = bndstrt;
    }
    for bin in begin..bndend {
        fastleak -= fdecay;
        fastleak = fastleak.max(bndpsd[bin] - fgain);
        slowleak -= sdecay;
        slowleak = slowleak.max(bndpsd[bin] - sgain);
        excite[bin] = fastleak.max(slowleak);
    }

    // masking curve
    let mut mask = [0i32; 50];
    for bin in bndstrt..bndend {
        if bndpsd[bin] < dbknee {
            excite[bin] += (dbknee - bndpsd[bin]) >> 2;
        }
        mask[bin] = excite[bin].max(HTH[p.fscod][bin] as i32);
    }

    // delta bit allocation
    if ch.dba.active {
        let mut band = 0usize;
        for seg in 0..ch.dba.nseg {
            band += ch.dba.offst[seg] as usize;
            let ba = ch.dba.ba[seg] as i32;
            let delta = if ba >= 4 { (ba - 3) << 7 } else { (ba - 4) << 7 };
            for _ in 0..ch.dba.len[seg] {
                if band < 50 {
                    mask[band] += delta;
                }
                band += 1;
            }
        }
    }

    // bit allocation pointers
    let mut i = start;
    let mut j = MASKTAB[start] as usize;
    loop {
        let lastbin = (BNDTAB[j] as usize + BNDSZ[j] as usize).min(end);
        let mut m = mask[j] - ch.snroffset - floor;
        if m < 0 {
            m = 0;
        }
        m &= 0x1fe0;
        m += floor;
        while i < lastbin {
            let address = ((psd[i] - m) >> 5).clamp(0, 63) as usize;
            bap[i] = ch.table[address];
            i += 1;
        }
        j += 1;
        if end <= lastbin {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ac3::tables::BAPTAB;

    #[test]
    fn exponents_decode_differentially() {
        let mut exp = [99u8; 256];
        // D15: absolute 10, then mapped (3, 1, 2) = +1 -1 0 → 11 10 10
        decode_exponents(10, &[3 * 25 + 5 + 2], 1, false, &mut exp, 0).unwrap();
        assert_eq!(&exp[..5], &[10, 11, 10, 10, 99]);
        // D25 coupling channel from bin 37: absolute 2 * 5 = 10, (4, 2, 0)
        // = +2 0 -2, each exponent for two bins, the absolute not stored
        decode_exponents(10, &[4 * 25 + 2 * 5], 2, true, &mut exp, 37).unwrap();
        assert_eq!(&exp[37..44], &[12, 12, 12, 12, 10, 10, 99]);
        assert!(decode_exponents(0, &[0], 1, false, &mut exp, 0).is_err());
        assert!(decode_exponents(24, &[125], 1, false, &mut exp, 0).is_err());
    }

    /// With every exponent 0 (full scale) and a very generous SNR offset
    /// every bin gets the finest quantizer; with the offset all the way
    /// down, none gets any bits.
    #[test]
    fn allocation_follows_the_snr_offset() {
        let exp = [0u8; 256];
        let dba = Dba::default();
        let p = Params { fscod: 0, ..Params::default() };
        let mut bap = [0u8; 256];
        let ch = |snroffset| Channel { exp: &exp, start: 0, end: 253, fgaincod: 4, snroffset, leak: None, dba: &dba, table: &BAPTAB };
        allocate(&p, &ch(((63 - 15) << 4 | 15) << 2), &mut bap);
        assert!(bap[..253].iter().all(|&b| b == 15));
        allocate(&p, &ch(-15 << 6), &mut bap);
        assert!(bap[..253].iter().all(|&b| b == 0));
    }
}
