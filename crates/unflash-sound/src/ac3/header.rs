//! Sync frame headers: `syncinfo` and `bsi` of AC-3 (A/52 §5.3.1, §5.3.2,
//! Annex D's alternate `bsi` for bsid 6) and of E-AC-3 (Annex E §2.2.1,
//! §2.2.2).

use crate::ac3::tables;
use crate::bits::Bits;

/// What the first bytes of a sync frame say about its length, enough to
/// find the next frame and to know whether this one is to be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sync {
    /// E-AC-3 syntax (bsid 11 to 16) rather than AC-3 (bsid 0 to 10).
    pub eac3: bool,
    pub bsid: u8,
    /// Length of the frame in bytes.
    pub bytes: usize,
    /// Audio blocks (256 samples each) in the frame.
    pub blocks: usize,
    /// Independent substream 0: the one this decoder plays. AC-3 frames
    /// count as independent substream 0 (Annex E §2.3.1.2).
    pub main: bool,
}

/// Read the first 6 bytes of a would-be sync frame (the sync word
/// included). `None` if they cannot start one: no sync word, a reserved
/// sample rate or frame size, or a bsid above 16 whose syntax is unknown.
pub fn sync(data: &[u8]) -> Option<Sync> {
    if data.len() < 6 || data[0] != 0x0b || data[1] != 0x77 {
        return None;
    }
    // bsid is in the same place in both syntaxes
    let bsid = data[5] >> 3;
    if bsid <= 10 {
        let fscod = data[4] >> 6;
        let frmsizecod = data[4] & 0x3f;
        let words = tables::ac3_frame_words(fscod, frmsizecod)?;
        Some(Sync { eac3: false, bsid, bytes: words * 2, blocks: 6, main: true })
    } else if bsid <= 16 {
        let strmtyp = data[2] >> 6;
        let substreamid = (data[2] >> 3) & 7;
        let frmsiz = ((data[2] as usize & 7) << 8) | data[3] as usize;
        let fscod = data[4] >> 6;
        let code2 = (data[4] >> 4) & 3;
        if fscod == 3 && code2 == 3 {
            return None;
        }
        let blocks = if fscod == 3 { 6 } else { tables::BLOCKS_PER_FRAME[code2 as usize] };
        if strmtyp == 3 {
            return None;
        }
        Some(Sync { eac3: true, bsid, bytes: (frmsiz + 1) * 2, blocks, main: strmtyp != 1 && substreamid == 0 })
    } else {
        None
    }
}

/// The bit stream information of a sync frame, as far as decoding needs
/// it (everything else is read past).
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub eac3: bool,
    pub bsid: u8,
    /// E-AC-3 stream type (0 independent, 1 dependent, 2 converted from
    /// AC-3); 0 for AC-3.
    pub strmtyp: u8,
    pub bytes: usize,
    pub sample_rate: u32,
    /// The sample rate code that chooses the hearing threshold table:
    /// `fscod`, or for E-AC-3's reduced rates `fscod2` (the standard does
    /// not say; the reduced rates halve the rate of the code's table).
    pub fscod: u8,
    pub numblkscod: u8,
    pub blocks: usize,
    pub acmod: u8,
    pub lfeon: bool,
    pub nfchans: usize,
    /// Lo/Ro downmix levels of the center and surround channels (§7.8.2,
    /// Annex D §3.1.2, Annex E's mixing metadata).
    pub clev: f32,
    pub slev: f32,
    /// Where the audio frame (E-AC-3) or the first audio block (AC-3)
    /// starts, in bits from the start of the frame.
    pub bsi_end: usize,
}

/// Parse `syncinfo` and `bsi`. `data` is the whole frame.
pub fn parse(data: &[u8]) -> Result<Header, &'static str> {
    let s = sync(data).ok_or("no sync frame")?;
    let mut b = Bits::new(data);
    let mut h = Header { eac3: s.eac3, bsid: s.bsid, bytes: s.bytes, blocks: s.blocks, ..Default::default() };
    b.skip(16);
    if !s.eac3 {
        b.skip(16); // crc1
        h.fscod = b.read(2) as u8;
        b.skip(6); // frmsizecod
        h.sample_rate = tables::SAMPLE_RATES[h.fscod as usize];
        b.skip(5); // bsid
        b.skip(3); // bsmod
        h.acmod = b.read(3) as u8;
        // the defaults when a level is not coded (§7.8.2: the reserved
        // codes' values)
        let (mut cmixlev, mut surmixlev) = (1, 1);
        if (h.acmod & 1) != 0 && h.acmod != 1 {
            cmixlev = b.read(2) as usize;
        }
        if h.acmod & 4 != 0 {
            surmixlev = b.read(2) as usize;
        }
        if h.acmod == 2 {
            b.skip(2); // dsurmod
        }
        h.lfeon = b.flag();
        b.skip(5); // dialnorm
        if b.flag() {
            b.skip(8); // compr
        }
        if b.flag() {
            b.skip(8); // langcod
        }
        if b.flag() {
            b.skip(7); // mixlevel, roomtyp
        }
        if h.acmod == 0 {
            b.skip(5); // dialnorm2
            if b.flag() {
                b.skip(8);
            }
            if b.flag() {
                b.skip(8);
            }
            if b.flag() {
                b.skip(7);
            }
        }
        b.skip(2); // copyrightb, origbs
        h.clev = tables::CMIXLEV[cmixlev];
        h.slev = tables::SURMIXLEV[surmixlev];
        if h.bsid == 6 {
            // Annex D: the time code fields carry the extra bsi
            if b.flag() {
                b.skip(2); // dmixmod
                b.skip(6); // ltrtcmixlev, ltrtsurmixlev
                let lorocmixlev = b.read(3) as usize;
                let lorosurmixlev = b.read(3) as usize;
                // "compliant decoders should use the lorocmixlev and
                // lorosurmixlev parameters" (Annex D §3.1.2)
                if (h.acmod & 1) != 0 && h.acmod != 1 {
                    h.clev = tables::LOROCMIXLEV[lorocmixlev];
                }
                if h.acmod & 4 != 0 {
                    h.slev = tables::LOROSURMIXLEV[lorosurmixlev];
                }
            }
            if b.flag() {
                b.skip(14); // dsurexmod, dheadphonmod, adconvtyp, xbsi2, encinfo
            }
        } else {
            if b.flag() {
                b.skip(14); // timecod1
            }
            if b.flag() {
                b.skip(14); // timecod2
            }
        }
        if b.flag() {
            let addbsil = b.read(6) as usize;
            b.skip((addbsil + 1) * 8);
        }
    } else {
        h.strmtyp = b.read(2) as u8;
        b.skip(3); // substreamid
        b.skip(11); // frmsiz
        let fscod = b.read(2) as u8;
        if fscod == 3 {
            let fscod2 = b.read(2) as u8;
            h.fscod = fscod2;
            h.sample_rate = tables::REDUCED_SAMPLE_RATES[fscod2 as usize];
            h.numblkscod = 3;
        } else {
            h.fscod = fscod;
            h.sample_rate = tables::SAMPLE_RATES[fscod as usize];
            h.numblkscod = b.read(2) as u8;
        }
        h.acmod = b.read(3) as u8;
        h.lfeon = b.flag();
        b.skip(5); // bsid
        b.skip(5); // dialnorm
        if b.flag() {
            b.skip(8); // compr
        }
        if h.acmod == 0 {
            b.skip(5); // dialnorm2
            if b.flag() {
                b.skip(8); // compr2
            }
        }
        if h.strmtyp == 1 && b.flag() {
            b.skip(16); // chanmap
        }
        // Without mixing metadata the standard gives no levels; these are
        // the "intermediate" -4.5 dB center and -6 dB surround levels that
        // §5.4.2.4 and §5.4.2.5 say to use when AC-3's codes are unknown
        // (reserved). ffmpeg 6.1 uses the same.
        h.clev = tables::CMIXLEV[1];
        h.slev = tables::SURMIXLEV[1];
        if b.flag() {
            // mixmdate
            if h.acmod > 2 {
                b.skip(2); // dmixmod
            }
            if (h.acmod & 1) != 0 && h.acmod > 2 {
                b.skip(3); // ltrtcmixlev
                h.clev = tables::LOROCMIXLEV[b.read(3) as usize];
            }
            if h.acmod & 4 != 0 {
                b.skip(3); // ltrtsurmixlev
                h.slev = tables::LOROSURMIXLEV[b.read(3) as usize];
            }
            if h.lfeon && b.flag() {
                b.skip(5); // lfemixlevcod
            }
            if h.strmtyp == 0 {
                if b.flag() {
                    b.skip(6); // pgmscl
                }
                if h.acmod == 0 && b.flag() {
                    b.skip(6); // pgmscl2
                }
                if b.flag() {
                    b.skip(6); // extpgmscl
                }
                match b.read(2) {
                    // mixdef
                    1 => b.skip(5), // premixcmpsel, drcsrc, premixcmpscl
                    2 => b.skip(12),
                    3 => {
                        // mixdeflen gives the length of the whole mixdata
                        // field in bytes, less 2, and the field starts
                        // with mixdeflen itself (ETSI TS 102 366 §E.2.10.5:
                        // "the mixdata field is required, at a minimum, to
                        // contain the mixdeflen, mixdata2e and mixdata3e
                        // parameters"); what it holds is only for mixing
                        // with another program.
                        let start = b.position();
                        let mixdeflen = b.read(5) as usize;
                        b.seek(start + 8 * (mixdeflen + 2));
                    }
                    _ => {}
                }
                if h.acmod < 2 {
                    if b.flag() {
                        b.skip(14); // panmean, paninfo
                    }
                    if h.acmod == 0 && b.flag() {
                        b.skip(14); // panmean2, paninfo2
                    }
                }
                if b.flag() {
                    // frmmixcfginfoe
                    if h.numblkscod == 0 {
                        b.skip(5);
                    } else {
                        for _ in 0..h.blocks {
                            if b.flag() {
                                b.skip(5);
                            }
                        }
                    }
                }
            }
        }
        if b.flag() {
            // infomdate
            b.skip(3); // bsmod
            b.skip(2); // copyrightb, origbs
            if h.acmod == 2 {
                b.skip(4); // dsurmod, dheadphonmod
            }
            if h.acmod >= 6 {
                b.skip(2); // dsurexmod
            }
            if b.flag() {
                b.skip(8); // mixlevel, roomtyp, adconvtyp
            }
            if h.acmod == 0 && b.flag() {
                b.skip(8); // mixlevel2, roomtyp2, adconvtyp2
            }
            if fscod < 3 {
                b.skip(1); // sourcefscod
            }
        }
        if h.strmtyp == 0 && h.numblkscod != 3 {
            b.skip(1); // convsync
        }
        if h.strmtyp == 2 {
            let blkid = if h.numblkscod == 3 { true } else { b.flag() };
            if blkid {
                b.skip(6); // frmsizecod
            }
        }
        if b.flag() {
            let addbsil = b.read(6) as usize;
            b.skip((addbsil + 1) * 8);
        }
    }
    h.nfchans = tables::NFCHANS[h.acmod as usize];
    h.bsi_end = b.position();
    if b.overrun() || h.bsi_end > h.bytes * 8 {
        return Err("bit stream information runs past the frame");
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_reads_sizes() {
        // AC-3, 48 kHz, 448 kbit/s (frmsizecod 30), bsid 8
        let f = [0x0b, 0x77, 0, 0, 0x1e, 8 << 3, 0];
        assert_eq!(sync(&f), Some(Sync { eac3: false, bsid: 8, bytes: 1792, blocks: 6, main: true }));
        // E-AC-3 dependent substream, frmsiz 383, 48 kHz, 6 blocks
        let f = [0x0b, 0x77, 0x40 | 0x01, 0x7f, 0x30, 16 << 3];
        assert_eq!(sync(&f), Some(Sync { eac3: true, bsid: 16, bytes: 768, blocks: 6, main: false }));
        // reduced rate: fscod 3, fscod2 1 → 6 blocks
        let f = [0x0b, 0x77, 0x00, 0xff, 0xd0, 16 << 3];
        assert_eq!(sync(&f).unwrap().blocks, 6);
        // reserved codes
        assert_eq!(sync(&[0x0b, 0x77, 0, 0, 0xc0, 8 << 3]), None);
        assert_eq!(sync(&[0x0b, 0x77, 0, 0, 0x26, 8 << 3]), None);
        assert_eq!(sync(&[0x0b, 0x77, 0, 0, 0xf0, 16 << 3]), None);
        assert_eq!(sync(&[0x0b, 0x77, 0, 0, 0x00, 17 << 3]), None);
        assert_eq!(sync(&[0x0b, 0x78, 0, 0, 0x00, 8 << 3]), None);
    }
}
