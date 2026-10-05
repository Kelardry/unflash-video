//! Renumbering the parameter sets of an H.264 stream, so that two streams
//! can share one MP4 track.
//!
//! A decoder keeps its sequence parameter sets by id (32 of them) and its
//! picture parameter sets by id (256), and every slice names the PPS it
//! uses. An encoder numbers its sets from 0, as the source stream did, so
//! samples from the two cannot share one `avcC` record as they are. The
//! [`Rewriter`] gives the encoder's sets ids the source does not use: in
//! the SPS and PPS themselves and in the `pic_parameter_set_id` of every
//! slice header. Everything after that field moves by the difference in
//! code length; the slice data itself is untouched (CABAC data is aligned
//! to its byte boundary again, CAVLC data shifted bit by bit) and the
//! emulation prevention bytes are inserted afresh.
//!
//! The [`AvcRegistry`] does the bookkeeping for a track: it starts from the
//! source's record, takes an encoder's record and hands back the rewriter
//! for that encoder's samples, and writes the merged record.

use crate::bitreader::{unescape, BitReader};
use crate::decoder::sample_nal_units;
use crate::ps::{parse_pps, parse_sps, Pps, Sps};
use crate::slice::parse_slice_header;
use crate::{Error, Result};

/// Writes bits, most significant first.
#[derive(Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn bit(&mut self, b: bool) {
        if self.nbits % 8 == 0 {
            self.buf.push(0);
        }
        if b {
            let i = self.nbits >> 3;
            self.buf[i] |= 0x80 >> (self.nbits & 7);
        }
        self.nbits += 1;
    }

    /// u(n)
    pub fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 != 0);
        }
    }

    /// ue(v): unsigned Exp-Golomb.
    pub fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 != 0);
        }
    }

    /// Append bits `from..to` of `src`.
    pub fn copy(&mut self, src: &[u8], from: usize, to: usize) {
        debug_assert!(to <= src.len() * 8);
        let mut p = from;
        while p < to {
            if p & 7 == 0 && self.nbits & 7 == 0 && to - p >= 8 {
                self.buf.push(src[p >> 3]);
                self.nbits += 8;
                p += 8;
                continue;
            }
            self.bit((src[p >> 3] >> (7 - (p & 7))) & 1 != 0);
            p += 1;
        }
    }

    /// `cabac_alignment_one_bit`s up to the next byte boundary.
    pub fn align_ones(&mut self) {
        while self.nbits % 8 != 0 {
            self.bit(true);
        }
    }

    /// `rbsp_trailing_bits`: the stop bit and zeros to the byte boundary.
    pub fn trailing(&mut self) {
        self.bit(true);
        while self.nbits % 8 != 0 {
            self.bit(false);
        }
    }

    /// Whole bytes, at a byte boundary.
    pub fn bytes(&mut self, b: &[u8]) {
        debug_assert!(self.nbits % 8 == 0);
        self.buf.extend_from_slice(b);
        self.nbits += 8 * b.len();
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Insert the `emulation_prevention_three_byte`s an RBSP needs to become a
/// NAL unit payload (the inverse of [`unescape`]).
pub fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

/// Bit position of the `rbsp_stop_one_bit`: the last 1 bit of the RBSP.
fn stop_bit(rbsp: &[u8]) -> Result<usize> {
    let mut last = rbsp.len();
    while last > 0 && rbsp[last - 1] == 0 {
        last -= 1;
    }
    if last == 0 {
        return Err(Error::Bitstream("RBSP without a stop bit"));
    }
    let b = rbsp[last - 1];
    Ok((last - 1) * 8 + (7 - b.trailing_zeros() as usize))
}

fn nal_with(header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    out.push(header);
    out.extend(escape(rbsp));
    out
}

/// An SPS NAL unit with a new `seq_parameter_set_id`.
pub fn renumber_sps(nal: &[u8], new_id: u32) -> Result<Vec<u8>> {
    if nal.is_empty() || nal[0] & 0x1f != 7 {
        return Err(Error::Bitstream("not a sequence parameter set"));
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    r.u(24)?;
    let p0 = r.bit_pos();
    r.ue()?;
    let p1 = r.bit_pos();
    let stop = stop_bit(&rbsp)?;
    if stop < p1 {
        return Err(Error::Bitstream("truncated sequence parameter set"));
    }
    let mut w = BitWriter::new();
    w.copy(&rbsp, 0, p0);
    w.ue(new_id);
    w.copy(&rbsp, p1, stop);
    w.trailing();
    Ok(nal_with(nal[0], &w.into_bytes()))
}

/// A PPS NAL unit with a new `pic_parameter_set_id` and
/// `seq_parameter_set_id`.
pub fn renumber_pps(nal: &[u8], new_id: u32, new_sps_id: u32) -> Result<Vec<u8>> {
    if nal.is_empty() || nal[0] & 0x1f != 8 {
        return Err(Error::Bitstream("not a picture parameter set"));
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    r.ue()?;
    r.ue()?;
    let p1 = r.bit_pos();
    let stop = stop_bit(&rbsp)?;
    if stop < p1 {
        return Err(Error::Bitstream("truncated picture parameter set"));
    }
    let mut w = BitWriter::new();
    w.ue(new_id);
    w.ue(new_sps_id);
    w.copy(&rbsp, p1, stop);
    w.trailing();
    Ok(nal_with(nal[0], &w.into_bytes()))
}

/// An SEI NAL unit without its buffering period messages (they name an
/// SPS by id and describe hypothetical decoder buffering nobody needs from
/// a spliced file). None when nothing is left; the input when it has no
/// such message or cannot be parsed.
fn strip_buffering_period(nal: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(&nal[1..]);
    let mut p = 0;
    let mut kept: Vec<u8> = Vec::new();
    let mut changed = false;
    // messages up to the trailing bits (0x80 or the last non-zero byte)
    let end = {
        let mut e = rbsp.len();
        while e > 0 && rbsp[e - 1] == 0 {
            e -= 1;
        }
        e.saturating_sub(1)
    };
    while p < end {
        let start = p;
        let mut ty = 0usize;
        while p < end && rbsp[p] == 0xff {
            ty += 255;
            p += 1;
        }
        if p >= end {
            return Some(nal.to_vec());
        }
        ty += rbsp[p] as usize;
        p += 1;
        let mut size = 0usize;
        while p < end && rbsp[p] == 0xff {
            size += 255;
            p += 1;
        }
        if p >= end {
            return Some(nal.to_vec());
        }
        size += rbsp[p] as usize;
        p += 1;
        if p + size > end {
            return Some(nal.to_vec());
        }
        p += size;
        if ty == 0 {
            changed = true;
        } else {
            kept.extend_from_slice(&rbsp[start..p]);
        }
    }
    if !changed {
        return Some(nal.to_vec());
    }
    if kept.is_empty() {
        return None;
    }
    kept.push(0x80);
    Some(nal_with(nal[0], &kept))
}

/// Renumbers the parameter sets of one encoder's samples.
#[derive(Clone)]
pub struct Rewriter {
    /// The encoder's sets under their original ids (slice headers are
    /// parsed against these).
    spss: Vec<Option<Sps>>,
    ppss: Vec<Option<Pps>>,
    sps_map: Vec<Option<u32>>,
    pps_map: Vec<Option<u32>>,
    /// In-band parameter sets, by original bytes, with their rewritten form.
    inband: Vec<(Vec<u8>, Vec<u8>)>,
    identity: bool,
    in_len: usize,
    out_len: usize,
}

impl Rewriter {
    /// `in_len` / `out_len`: the NAL length size of the samples going in
    /// and coming out.
    pub fn new(in_len: usize, out_len: usize) -> Self {
        Rewriter { spss: vec![None; 32], ppss: vec![None; 256], sps_map: vec![None; 32], pps_map: vec![None; 256], inband: Vec::new(), identity: true, in_len, out_len }
    }

    /// Register one of the encoder's SPS NAL units under `new_id`; returns
    /// the NAL unit as the merged record should hold it.
    pub fn add_sps(&mut self, nal: &[u8], new_id: u32) -> Result<Vec<u8>> {
        if new_id > 31 {
            return Err(Error::Bitstream("SPS id out of range"));
        }
        let sps = parse_sps(&unescape(&nal[1..]))?;
        let old = sps.id;
        self.spss[old as usize] = Some(sps);
        self.sps_map[old as usize] = Some(new_id);
        let out = if new_id == old { nal.to_vec() } else { renumber_sps(nal, new_id)? };
        if new_id != old {
            self.identity = false;
        }
        self.inband.push((nal.to_vec(), out.clone()));
        Ok(out)
    }

    /// Register one of the encoder's PPS NAL units under `new_id` (its SPS
    /// must have been added first); returns the NAL unit as the merged
    /// record should hold it.
    pub fn add_pps(&mut self, nal: &[u8], new_id: u32) -> Result<Vec<u8>> {
        if new_id > 255 {
            return Err(Error::Bitstream("PPS id out of range"));
        }
        let pps = parse_pps(&unescape(&nal[1..]))?;
        let old = pps.id;
        let new_sps = self.sps_map.get(pps.sps_id as usize).copied().flatten().ok_or(Error::Bitstream("PPS refers to an SPS the record does not have"))?;
        let same = new_id == old && new_sps == pps.sps_id;
        self.ppss[old as usize] = Some(pps);
        self.pps_map[old as usize] = Some(new_id);
        let out = if same { nal.to_vec() } else { renumber_pps(nal, new_id, new_sps)? };
        if !same {
            self.identity = false;
        }
        self.inband.push((nal.to_vec(), out.clone()));
        Ok(out)
    }

    /// The new id of the encoder's SPS `old`.
    pub fn sps_id(&self, old: u32) -> Option<u32> {
        self.sps_map.get(old as usize).copied().flatten()
    }

    /// Whether the samples pass through unchanged.
    pub fn is_identity(&self) -> bool {
        self.identity && self.in_len == self.out_len
    }

    fn rewrite_slice(&self, nal: &[u8]) -> Result<Vec<u8>> {
        let header = nal[0];
        let nal_type = header & 0x1f;
        let ref_idc = (header >> 5) & 3;
        let rbsp = unescape(&nal[1..]);
        let mut r = BitReader::new(&rbsp);
        r.ue()?; // first_mb_in_slice
        r.ue()?; // slice_type
        let p0 = r.bit_pos();
        let pps_id = r.ue_max(255, "pic_parameter_set_id")?;
        let p1 = r.bit_pos();
        let new_pps = self.pps_map[pps_id as usize].ok_or(Error::Bitstream("slice refers to a PPS the record does not have"))?;
        let pps = self.ppss[pps_id as usize].as_ref().ok_or(Error::Bitstream("slice refers to a PPS the record does not have"))?;
        // the whole header, for where the slice data starts
        let mut r2 = BitReader::new(&rbsp);
        parse_slice_header(&mut r2, nal_type, ref_idc, &self.spss, &self.ppss)?;
        let hend = r2.bit_pos();
        let mut w = BitWriter::new();
        w.copy(&rbsp, 0, p0);
        w.ue(new_pps);
        w.copy(&rbsp, p1, hend);
        if pps.entropy_coding_mode {
            // cabac_alignment_one_bits, then the arithmetic-coded data as it
            // is; the cabac_zero_words after the stop bit are padding
            w.align_ones();
            let start = hend.div_ceil(8);
            let mut end = rbsp.len();
            while end > start && rbsp[end - 1] == 0 {
                end -= 1;
            }
            if end <= start {
                return Err(Error::Bitstream("slice without data"));
            }
            w.bytes(&rbsp[start..end]);
        } else {
            let stop = stop_bit(&rbsp)?;
            if stop < hend {
                return Err(Error::Bitstream("slice header runs past the NAL unit"));
            }
            w.copy(&rbsp, hend, stop);
            w.trailing();
        }
        Ok(nal_with(header, &w.into_bytes()))
    }

    /// One NAL unit (header byte included) as the merged track needs it;
    /// None drops it.
    pub fn rewrite_nal(&mut self, nal: &[u8]) -> Result<Option<Vec<u8>>> {
        if nal.is_empty() {
            return Ok(None);
        }
        match nal[0] & 0x1f {
            1 | 5 => Ok(Some(self.rewrite_slice(nal)?)),
            2..=4 => Err(Error::Unsupported("slice data partitioning")),
            6 => Ok(strip_buffering_period(nal)),
            7 | 8 => match self.inband.iter().find(|(orig, _)| orig.as_slice() == nal) {
                Some((_, out)) => Ok(Some(out.clone())),
                None => self.rewrite_inband(nal).map(Some),
            },
            _ => Ok(Some(nal.to_vec())),
        }
    }

    /// A parameter set sent in a sample that the record does not hold byte
    /// for byte (a hardware encoder repeating its sets before each IDR
    /// picture, with a different VUI say): it takes effect from here on,
    /// under the id the record gave the set it replaces.
    fn rewrite_inband(&mut self, nal: &[u8]) -> Result<Vec<u8>> {
        if nal[0] & 0x1f == 7 {
            let sps = parse_sps(&unescape(&nal[1..]))?;
            let old = sps.id;
            let new_id = self.sps_id(old).ok_or(Error::Bitstream("a sample sends an SPS the record has no id for"))?;
            self.spss[old as usize] = Some(sps);
            let out = if new_id == old { nal.to_vec() } else { renumber_sps(nal, new_id)? };
            self.inband.push((nal.to_vec(), out.clone()));
            Ok(out)
        } else {
            let pps = parse_pps(&unescape(&nal[1..]))?;
            let old = pps.id;
            let new_id = self.pps_map.get(old as usize).copied().flatten().ok_or(Error::Bitstream("a sample sends a PPS the record has no id for"))?;
            let new_sps = self.sps_id(pps.sps_id).ok_or(Error::Bitstream("a sample's PPS refers to an SPS the record does not have"))?;
            let same = new_id == old && new_sps == pps.sps_id;
            self.ppss[old as usize] = Some(pps);
            let out = if same { nal.to_vec() } else { renumber_pps(nal, new_id, new_sps)? };
            self.inband.push((nal.to_vec(), out.clone()));
            Ok(out)
        }
    }

    /// One MP4 sample (length-prefixed NAL units) as the merged track needs
    /// it.
    pub fn rewrite_sample(&mut self, sample: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(sample.len() + 16);
        for nal in sample_nal_units(sample, self.in_len) {
            let nal = nal?;
            let nal = if self.identity { Some(nal.to_vec()) } else { self.rewrite_nal(nal)? };
            if let Some(nal) = nal {
                let l = nal.len();
                for i in (0..self.out_len).rev() {
                    out.push((l >> (8 * i)) as u8);
                }
                out.extend_from_slice(&nal);
            }
        }
        Ok(out)
    }
}

/// The fields of an `AVCDecoderConfigurationRecord`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvcRecord {
    pub profile: u8,
    pub compat: u8,
    pub level: u8,
    pub len_size: usize,
    /// NAL units, header byte included.
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
    /// The bytes after the PPS list (the High profile extension), if any.
    pub tail: Vec<u8>,
}

/// Take off what some writers leave around a parameter set in a record: an
/// Annex B start code in front, zero bytes behind (a parameter set ends in
/// its stop bit, so it never ends in a zero byte).
fn trim_parameter_set(nal: &[u8]) -> &[u8] {
    let mut n = nal;
    for sc in [&[0u8, 0, 0, 1][..], &[0, 0, 1][..]] {
        if n.starts_with(sc) {
            n = &n[sc.len()..];
            break;
        }
    }
    let mut end = n.len();
    while end > 1 && n[end - 1] == 0 {
        end -= 1;
    }
    &n[..end]
}

/// Repair the parameter sets of a record some encoders write with every
/// NAL header byte twice: Firefox's Windows encoder hands a NAL unit that
/// already starts with its header to an avcC writer that puts one in front
/// of it (`67 67 64 00 1e ...`). An SPS is recognisable, because the byte
/// after its header is profile_idc and 0x67 is no profile; when every SPS
/// shows it, the PPSs whose first two bytes repeat their header get the same
/// repair.
fn repair_doubled_headers(sps: &mut [Vec<u8>], pps: &mut [Vec<u8>]) -> bool {
    let doubled = |n: &Vec<u8>, t: u8| n.len() > 2 && n[0] & 0x1f == t && n[1] == n[0];
    if sps.is_empty() || !sps.iter().all(|n| doubled(n, 7)) {
        return false;
    }
    for n in sps.iter_mut() {
        n.remove(0);
    }
    for n in pps.iter_mut() {
        if doubled(n, 8) {
            n.remove(0);
        }
    }
    true
}

/// Parse an `AVCDecoderConfigurationRecord` (the `avcC` payload), repairing
/// the damage some encoders do to its parameter sets (see
/// [`repair_doubled_headers`]).
pub fn parse_avcc(avcc: &[u8]) -> Result<AvcRecord> {
    if avcc.len() < 7 || avcc[0] != 1 {
        return Err(Error::Bitstream("bad avcC record"));
    }
    let get = |p: usize| avcc.get(p).copied().ok_or(Error::Bitstream("short avcC"));
    let mut p = 6;
    let nsps = (avcc[5] & 31) as usize;
    let mut sps = Vec::new();
    for _ in 0..nsps {
        let len = u16::from_be_bytes([get(p)?, get(p + 1)?]) as usize;
        p += 2;
        sps.push(trim_parameter_set(avcc.get(p..p + len).ok_or(Error::Bitstream("short avcC"))?).to_vec());
        p += len;
    }
    let npps = get(p)? as usize;
    p += 1;
    let mut pps = Vec::new();
    for _ in 0..npps {
        let len = u16::from_be_bytes([get(p)?, get(p + 1)?]) as usize;
        p += 2;
        pps.push(trim_parameter_set(avcc.get(p..p + len).ok_or(Error::Bitstream("short avcC"))?).to_vec());
        p += len;
    }
    repair_doubled_headers(&mut sps, &mut pps);
    Ok(AvcRecord { profile: avcc[1], compat: avcc[2], level: avcc[3], len_size: (avcc[4] & 3) as usize + 1, sps, pps, tail: avcc[p.min(avcc.len())..].to_vec() })
}

pub fn write_avcc(rec: &AvcRecord) -> Vec<u8> {
    let mut out = vec![1, rec.profile, rec.compat, rec.level, 0xfc | (rec.len_size as u8 - 1), 0xe0 | rec.sps.len() as u8];
    for s in &rec.sps {
        out.extend_from_slice(&(s.len() as u16).to_be_bytes());
        out.extend_from_slice(s);
    }
    out.push(rec.pps.len() as u8);
    for p in &rec.pps {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    out.extend_from_slice(&rec.tail);
    out
}

const HIGH_PROFILES: [u8; 4] = [100, 110, 122, 144];

/// The parameter sets of a track that splices several streams: the
/// source's, then every encoder's under ids the others do not use.
pub struct AvcRegistry {
    rec: AvcRecord,
    sps_used: Vec<bool>,
    pps_used: Vec<bool>,
    /// Encoder sets already registered, by their original bytes: (bytes,
    /// id). A PPS is keyed with the id its SPS was given.
    sps_seen: Vec<(Vec<u8>, u32)>,
    pps_seen: Vec<(Vec<u8>, u32, u32)>,
}

impl AvcRegistry {
    /// Start from the record of the stream whose samples are copied as
    /// they are (its ids stay).
    pub fn new(base: &[u8]) -> Result<Self> {
        let rec = parse_avcc(base)?;
        let mut sps_used = vec![false; 32];
        let mut pps_used = vec![false; 256];
        let mut sps_seen = Vec::new();
        let mut pps_seen = Vec::new();
        for s in &rec.sps {
            let id = parse_sps(&unescape(s.get(1..).unwrap_or(&[])))?.id;
            sps_used[id as usize] = true;
            sps_seen.push((s.clone(), id));
        }
        for p in &rec.pps {
            let pps = parse_pps(&unescape(p.get(1..).unwrap_or(&[])))?;
            pps_used[pps.id as usize] = true;
            pps_seen.push((p.clone(), pps.sps_id, pps.id));
        }
        Ok(AvcRegistry { rec, sps_used, pps_used, sps_seen, pps_seen })
    }

    /// The merged record so far.
    pub fn record(&self) -> Vec<u8> {
        write_avcc(&self.rec)
    }

    /// An id for a PPS the encoder numbered `old`: its own when free. A
    /// CAVLC slice's data is shifted by the change in the id's code length,
    /// and I_PCM samples in it are aligned to the NAL unit's bytes, so for
    /// a CAVLC PPS the id is one whose code is exactly a byte longer (a
    /// shift by whole bytes keeps every alignment); failing that, the
    /// lowest free id.
    fn free_pps_id(&self, old: u32, cavlc: bool) -> Result<u32> {
        if !self.pps_used[old as usize] {
            return Ok(old);
        }
        if cavlc {
            let len = |v: u32| 2 * (32 - (v + 1).leading_zeros()) - 1;
            let want = len(old) + 8;
            if let Some(id) = (0..256u32).find(|&id| !self.pps_used[id as usize] && len(id) == want) {
                return Ok(id);
            }
        }
        self.pps_used.iter().position(|u| !u).map(|i| i as u32).ok_or(Error::Unsupported("more than 256 picture parameter sets"))
    }

    /// Take an encoder's record in; the returned rewriter makes that
    /// encoder's samples fit the merged track. Sets seen before (same
    /// bytes) share their earlier ids.
    pub fn register(&mut self, avcc: &[u8]) -> Result<Rewriter> {
        let enc = parse_avcc(avcc)?;
        let mut rw = Rewriter::new(enc.len_size, self.rec.len_size);
        for nal in &enc.sps {
            let old = parse_sps(&unescape(nal.get(1..).unwrap_or(&[])))?.id;
            let id = match self.sps_seen.iter().find(|(b, _)| b == nal) {
                Some((_, id)) => *id,
                None => {
                    let id = if !self.sps_used[old as usize] { old } else { self.sps_used.iter().position(|u| !u).ok_or(Error::Unsupported("more than 32 sequence parameter sets"))? as u32 };
                    self.sps_used[id as usize] = true;
                    self.sps_seen.push((nal.clone(), id));
                    let out = rw.add_sps(nal, id)?;
                    self.rec.sps.push(out);
                    id
                }
            };
            // add_sps records the mapping; a set seen before still needs it
            if rw.sps_id(old).is_none() {
                rw.add_sps(nal, id)?;
            }
        }
        for nal in &enc.pps {
            let pps = parse_pps(&unescape(nal.get(1..).unwrap_or(&[])))?;
            let new_sps = rw.sps_id(pps.sps_id).ok_or(Error::Bitstream("PPS refers to an SPS the record does not have"))?;
            let id = match self.pps_seen.iter().find(|(b, s, _)| b == nal && *s == new_sps) {
                Some((_, _, id)) => *id,
                None => {
                    let id = self.free_pps_id(pps.id, !pps.entropy_coding_mode)?;
                    self.pps_used[id as usize] = true;
                    self.pps_seen.push((nal.clone(), new_sps, id));
                    let out = rw.add_pps(nal, id)?;
                    self.rec.pps.push(out);
                    id
                }
            };
            if rw.pps_map[pps.id as usize].is_none() {
                rw.add_pps(nal, id)?;
            }
        }
        // the record describes every stream in the track
        if enc.profile != self.rec.profile {
            let high = |p: u8| HIGH_PROFILES.contains(&p);
            if high(enc.profile) && !high(self.rec.profile) {
                self.rec.profile = enc.profile;
                if self.rec.tail.is_empty() {
                    // chroma 4:2:0, 8-bit, no SPS extensions
                    self.rec.tail = vec![0xfd, 0xf8, 0xf8, 0x00];
                }
            } else if !high(self.rec.profile) && enc.profile == 77 && self.rec.profile != 77 {
                self.rec.profile = 77;
            }
        }
        self.rec.compat &= enc.compat;
        self.rec.level = self.rec.level.max(enc.level);
        Ok(rw)
    }
}

/// The type of the first VCL NAL unit of a sample (1 or 5), 0 when the
/// data holds none (a partial read can stop short of it). An IDR picture
/// (5) starts a stream a decoder can pick up cold.
pub fn first_vcl_nal_type(sample: &[u8], len_size: usize) -> u8 {
    let mut p = 0;
    // (a NAL unit may run past the data read, so `p` may pass its end: it
    // saturates rather than wrapping on wasm32 for a length near 2^32, and
    // the loop compares without adding to it)
    while sample.len().saturating_sub(p) >= len_size {
        let mut len = 0usize;
        for i in 0..len_size {
            len = (len << 8) | sample[p + i] as usize;
        }
        p += len_size;
        if len == 0 {
            continue;
        }
        let Some(&h) = sample.get(p) else { return 0 };
        let t = h & 0x1f;
        if t == 1 || t == 5 {
            return t;
        }
        p = p.saturating_add(len);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_and_reader_agree() {
        let mut w = BitWriter::new();
        for v in [0u32, 1, 2, 3, 7, 8, 255, 256, 1000, 65535] {
            w.ue(v);
        }
        w.u(5, 0b10110);
        w.trailing();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for v in [0u32, 1, 2, 3, 7, 8, 255, 256, 1000, 65535] {
            assert_eq!(r.ue().unwrap(), v);
        }
        assert_eq!(r.u(5).unwrap(), 0b10110);
        assert!(!r.more_rbsp_data());
    }

    #[test]
    fn copy_moves_bit_ranges() {
        let src = [0b1011_0010u8, 0b0111_1100, 0b1000_0001];
        let mut w = BitWriter::new();
        w.u(3, 0b101);
        w.copy(&src, 2, 22);
        w.trailing();
        let out = w.into_bytes();
        let mut r = BitReader::new(&out);
        assert_eq!(r.u(3).unwrap(), 0b101);
        let mut r2 = BitReader::new(&src);
        r2.skip(2);
        for _ in 0..20 {
            assert_eq!(r.u(1).unwrap(), r2.u(1).unwrap());
        }
        assert!(!r.more_rbsp_data());
    }

    #[test]
    fn escaping_round_trips() {
        let rbsp = [0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 5, 0, 0];
        let nal = escape(&rbsp);
        assert_eq!(nal, vec![0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 5, 0, 0]);
        assert_eq!(unescape(&nal), rbsp.to_vec());
    }

    #[test]
    fn buffering_period_messages_go() {
        // an SEI with a buffering period (type 0, 2 bytes) and a user data
        // message (type 5, 3 bytes)
        let nal = [0x06, 0, 2, 0xaa, 0xbb, 5, 3, 1, 2, 3, 0x80];
        let out = strip_buffering_period(&nal).unwrap();
        assert_eq!(out, vec![0x06, 5, 3, 1, 2, 3, 0x80]);
        let only = [0x06, 0, 2, 0xaa, 0xbb, 0x80];
        assert!(strip_buffering_period(&only).is_none());
        let other = [0x06, 5, 3, 1, 2, 3, 0x80];
        assert_eq!(strip_buffering_period(&other).unwrap(), other.to_vec());
    }

    #[test]
    fn a_length_near_2_to_the_32_ends_the_search() {
        // (on wasm32 the position wrapped round to the start, for ever;
        // natively this passes either way)
        assert_eq!(first_vcl_nal_type(&[0xff, 0xff, 0xff, 0xfc, 0x06], 4), 0);
        assert_eq!(first_vcl_nal_type(&[0, 0, 0, 1, 0x06, 0, 0, 0, 9, 0x65], 4), 5);
        assert_eq!(first_vcl_nal_type(&[0, 0, 0, 9, 0x06, 0, 0], 4), 0);
    }

    #[test]
    fn record_round_trips() {
        let rec = AvcRecord { profile: 100, compat: 0, level: 31, len_size: 4, sps: vec![vec![0x67, 1, 2, 3]], pps: vec![vec![0x68, 4, 5]], tail: vec![0xfd, 0xf8, 0xf8, 0] };
        assert_eq!(parse_avcc(&write_avcc(&rec)).unwrap(), rec);
    }
}
