//! The decoder: NAL units in, pictures out.

use std::rc::Rc;

use crate::bitreader::{unescape, BitReader};
use crate::cabac::Cabac;
use crate::deblock::{self, MbDeblockInfo};
use crate::mb::{Entropy, MbInfo, SliceDecoder};
use crate::picture::{Dpb, Picture, PocState, BOTTOM, FRAME, TOP};
use crate::ps::{parse_pps, parse_sps, Pps, ScalingTables, Sps};
use crate::slice::{parse_slice_header, SliceHeader};
use crate::{Error, Result};

/// A decoded picture, in decoding order; the caller orders by `pts`.
pub struct DecodedFrame {
    pub pic: Rc<Picture>,
    /// The part of it to show (its sequence's frame cropping): left, top,
    /// width and height in luma samples, as [`crate::yuv::to_i420`] takes
    /// them. (The decoder's own sequence may already be the next one's.)
    pub crop: (usize, usize, usize, usize),
    /// Some slice of it could not be decoded (parts are concealed).
    pub damaged: bool,
}

/// The picture (frame or field) being decoded.
struct Current {
    pic: Picture,
    hdr: SliceHeader,
    poc: PocState,
    /// TOP, BOTTOM or FRAME.
    structure: u8,
    /// The second field of a frame whose first field is decoded.
    second_field: bool,
    mbs: Vec<MbInfo>,
    deblock: Vec<MbDeblockInfo>,
    slices: u32,
    damaged: bool,
    decoded_mbs: usize,
}

/// A decoded first field waiting for the second field of its frame.
struct Pending {
    pic: Picture,
    frame_num: u32,
    structure: u8,
    is_ref: bool,
    damaged: bool,
    mbs: Vec<MbInfo>,
    deblock: Vec<MbDeblockInfo>,
}

pub struct Decoder {
    spss: Vec<Option<Sps>>,
    ppss: Vec<Option<Pps>>,
    dpb: Dpb,
    cur: Option<Current>,
    pending: Option<Pending>,
    /// Frames finished but not yet handed out.
    out: Vec<DecodedFrame>,
    nal_length_size: usize,
    /// The active sequence (for size and colour information).
    active_sps: Option<Sps>,
    last_output: Option<Rc<Picture>>,
    /// macroblock kinds of the last finished picture (debugging aid)
    last_kinds: Vec<crate::mb::MbKind>,
    /// Picture buffers free for reuse, and per-picture macroblock tables.
    pool: Vec<Picture>,
    retired: Vec<Rc<Picture>>,
    mbs_buf: Vec<MbInfo>,
    deblock_buf: Vec<MbDeblockInfo>,
    /// leave the deblocking filter out (see `set_skip_deblock`)
    skip_deblock: bool,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder {
            spss: vec![None; 32],
            ppss: vec![None; 256],
            dpb: Dpb::new(),
            cur: None,
            pending: None,
            out: Vec::new(),
            nal_length_size: 4,
            active_sps: None,
            last_output: None,
            last_kinds: Vec::new(),
            pool: Vec::new(),
            retired: Vec::new(),
            mbs_buf: Vec::new(),
            deblock_buf: Vec::new(),
            skip_deblock: false,
        }
    }

    /// Skip the in-loop deblocking filter: about a fifth of the decoding
    /// time. The pictures are no longer bit-exact (block edges keep their
    /// coding artefacts, and later pictures predicted from them drift very
    /// slightly), which is fine for statistics such as flash detection but
    /// not for pictures that are shown or re-encoded.
    pub fn set_skip_deblock(&mut self, skip: bool) {
        self.skip_deblock = skip;
    }

    /// Feed an `AVCDecoderConfigurationRecord` (the `avcC` box payload):
    /// the parameter sets and the NAL length size of the samples.
    /// (Records some encoders damage are repaired first, see
    /// [`crate::rewrite::parse_avcc`].)
    pub fn configure_avcc(&mut self, avcc: &[u8]) -> Result<()> {
        let rec = crate::rewrite::parse_avcc(avcc)?;
        self.nal_length_size = rec.len_size;
        for nal in rec.sps.iter().chain(rec.pps.iter()) {
            self.decode_nal(nal, 0.0)?;
        }
        Ok(())
    }

    /// The sequence parameter set in use, once a slice has been seen.
    pub fn sps(&self) -> Option<&Sps> {
        self.active_sps.as_ref()
    }

    /// The sequence parameter set with the lowest id (normally the only one,
    /// from `configure_avcc`).
    pub fn first_sps(&self) -> Option<&Sps> {
        self.spss.iter().flatten().next()
    }

    pub fn pps(&self, id: usize) -> Option<&Pps> {
        self.ppss.get(id).and_then(|p| p.as_ref())
    }

    /// The macroblock kinds of the last finished picture, in raster order.
    pub fn last_mb_kinds(&self) -> &[crate::mb::MbKind] {
        &self.last_kinds
    }

    /// Decode one MP4 sample (length-prefixed NAL units of one access
    /// unit) and return its picture. A sample holding the first field of a
    /// frame alone yields nothing; the frame comes with its second field.
    pub fn decode_sample(&mut self, data: &[u8], pts: f64) -> Result<Option<DecodedFrame>> {
        for nal in sample_nal_units(data, self.nal_length_size) {
            self.decode_nal(nal?, pts)?;
        }
        self.finish_picture();
        let mut frames = std::mem::take(&mut self.out);
        Ok(frames.pop())
    }

    /// Decode an Annex B byte stream chunk (start-code delimited NAL
    /// units); pictures complete when the next picture starts, so call
    /// `flush` at the end.
    pub fn decode_annexb(&mut self, data: &[u8], pts: f64) -> Result<Vec<DecodedFrame>> {
        for nal in annexb_nal_units(data) {
            self.decode_nal(nal, pts)?;
        }
        Ok(std::mem::take(&mut self.out))
    }

    /// Finish the picture in progress, if any (and an unpaired field).
    pub fn flush(&mut self) -> Result<Option<DecodedFrame>> {
        self.finish_picture();
        if let Some(p) = self.pending.take() {
            self.output_unpaired(p);
        }
        let mut frames = std::mem::take(&mut self.out);
        Ok(frames.pop())
    }

    /// Decode one NAL unit (with its header byte, emulation prevention
    /// still in place). Completed pictures are queued for the caller.
    pub fn decode_nal(&mut self, nal: &[u8], pts: f64) -> Result<()> {
        if nal.is_empty() {
            return Ok(());
        }
        let nal_type = nal[0] & 0x1f;
        let nal_ref_idc = (nal[0] >> 5) & 3;
        match nal_type {
            7 => {
                let sps = parse_sps(&unescape(&nal[1..]))?;
                let id = sps.id as usize;
                self.spss[id] = Some(sps);
            }
            8 => {
                let pps = parse_pps(&unescape(&nal[1..]))?;
                let id = pps.id as usize;
                self.ppss[id] = Some(pps);
            }
            1 | 5 => self.decode_slice(nal_type, nal_ref_idc, &unescape(&nal[1..]), pts)?,
            2..=4 => return Err(Error::Unsupported("slice data partitioning")),
            _ => {}
        }
        Ok(())
    }

    fn new_picture_starts(&self, hdr: &SliceHeader, sps: &Sps) -> bool {
        let Some(cur) = &self.cur else { return true };
        // a parameter set re-sent changed (another size, or the same size
        // with other content): the picture was started with the old one,
        // whose size its tables have, so the slice belongs to the next
        // picture (it never decodes with a mix of the two)
        if self.active_sps.as_ref() != Some(sps) {
            return true;
        }
        let prev = &cur.hdr;
        if hdr.first_mb == 0 || hdr.frame_num != prev.frame_num || hdr.pps_id != prev.pps_id || (hdr.nal_ref_idc == 0) != (prev.nal_ref_idc == 0) || hdr.is_idr() != prev.is_idr() || hdr.field_pic != prev.field_pic || hdr.bottom_field != prev.bottom_field {
            return true;
        }
        if hdr.is_idr() && hdr.idr_pic_id != prev.idr_pic_id {
            return true;
        }
        match sps.poc_type {
            0 => hdr.poc_lsb != prev.poc_lsb || hdr.delta_poc_bottom != prev.delta_poc_bottom,
            1 => hdr.delta_poc != prev.delta_poc,
            _ => false,
        }
    }

    fn decode_slice(&mut self, nal_type: u8, nal_ref_idc: u8, rbsp: &[u8], pts: f64) -> Result<()> {
        let mut r = BitReader::new(rbsp);
        let hdr = parse_slice_header(&mut r, nal_type, nal_ref_idc, &self.spss, &self.ppss)?;
        if hdr.redundant_pic_cnt > 0 {
            return Ok(());
        }
        let pps = self.ppss[hdr.pps_id as usize].clone().unwrap();
        let sps = self.spss[pps.sps_id as usize].clone().unwrap();
        if self.new_picture_starts(&hdr, &sps) {
            self.finish_picture();
            self.start_picture(&sps, &hdr, pts);
        }
        let scaling = ScalingTables::new(&sps, &pps);
        let cur = self.cur.as_mut().unwrap();
        cur.slices += 1;
        let slice_id = cur.slices;
        let lists = match self.dpb.ref_lists(&sps, &hdr, cur.poc.poc) {
            Ok(l) => l,
            Err(e) => {
                cur.damaged = true;
                return Err(e);
            }
        };
        let entropy = if pps.entropy_coding_mode {
            r.byte_align();
            Entropy::Cabac(Cabac::new(rbsp, r.byte_pos(), hdr.slice_type == crate::slice::SliceType::I, hdr.cabac_init_idc, hdr.slice_qp)?)
        } else {
            Entropy::Cavlc(r)
        };
        let poc = cur.poc.poc;
        let structure = cur.structure;
        let mut sd = SliceDecoder::new(&sps, &pps, &scaling, &hdr, &lists, slice_id, entropy, &mut cur.mbs, &mut cur.deblock, &mut cur.pic, poc, structure);
        match sd.decode() {
            Ok(n) => cur.decoded_mbs += n,
            Err(_) => cur.damaged = true,
        }
        Ok(())
    }

    /// A picture buffer of the right size: one nobody uses any more, or a
    /// new one.
    fn take_picture(&mut self, id: u32, wm: usize, hm: usize) -> Picture {
        for rc in self.dpb.graveyard.drain(..).chain(self.retired.drain(..)) {
            if let Ok(p) = Rc::try_unwrap(rc) {
                // a few spares are enough (each picture takes one and hands
                // back the reference it pushes out; a frame coded as two
                // fields also hands back its first field's copy, which would
                // make the spares pile up a picture per frame)
                if p.width == wm * 16 && p.height == hm * 16 && self.pool.len() < 4 {
                    self.pool.push(p);
                }
            }
        }
        match self.pool.pop() {
            Some(mut p) => {
                p.reset(id);
                p
            }
            None => Picture::new(id, wm, hm),
        }
    }

    fn reset_tables(mbs: &mut Vec<MbInfo>, deblock: &mut Vec<MbDeblockInfo>, n: usize) {
        mbs.resize(n, MbInfo::default());
        for m in mbs.iter_mut() {
            m.slice = 0;
        }
        deblock.clear();
        deblock.resize(n, MbDeblockInfo::default());
    }

    fn start_picture(&mut self, sps: &Sps, hdr: &SliceHeader, pts: f64) {
        // another sequence parameter set, or the active one's id sent again
        // with other content (cropping, order counts, reference counts and
        // the rest: the whole set is compared): it takes effect here, so
        // the picture's order count, reference marking and cropping all
        // come from it. A set re-sent unchanged, as encoders do before
        // every IDR picture, changes nothing.
        if self.active_sps.as_ref() != Some(sps) {
            // (a first field still waiting for its second goes out with its
            // own sequence's cropping)
            if let Some(p) = self.pending.take() {
                self.output_unpaired(p);
            }
            // References and spare buffers of another size are useless. A
            // change that keeps the size keeps the references: a conforming
            // stream activates a new set only at an IDR picture, which
            // empties the buffer below anyway, so this decides only for
            // streams that change it elsewhere. Their references still fit
            // (the same size is the same format here), so at worst the
            // pictures predict from them wrongly, where dropping them would
            // conceal every picture up to the next IDR; ffmpeg's decoder
            // keeps them too (it drops them only when the size, format,
            // aspect ratio or colour matrix changes).
            if self.active_sps.as_ref().is_none_or(|a| (a.width_mbs, a.height_mbs) != (sps.width_mbs, sps.height_mbs)) {
                self.dpb.clear();
                self.pool.clear();
            }
            self.active_sps = Some(sps.clone());
        }
        let (wm, hm) = (sps.width_mbs as usize, sps.height_mbs as usize);
        let structure = hdr.structure();
        let n = wm * hm;
        // the second field of the frame whose first field is waiting?
        if let Some(p) = self.pending.take() {
            if hdr.field_pic && structure != p.structure && hdr.frame_num == p.frame_num && hdr.is_ref() == p.is_ref && !hdr.is_idr() {
                let poc = self.dpb.compute_poc(sps, hdr);
                let mut pic = p.pic;
                pic.set_poc(structure, poc.top, poc.bottom);
                let mut mbs = p.mbs;
                let mut deblock = p.deblock;
                Self::reset_tables(&mut mbs, &mut deblock, n);
                self.cur = Some(Current { pic, hdr: hdr.clone(), poc, structure, second_field: true, mbs, deblock, slices: 0, damaged: p.damaged, decoded_mbs: 0 });
                return;
            }
            self.output_unpaired(p);
        }
        if hdr.is_idr() {
            self.dpb.clear();
        } else {
            let last = self.last_output.clone();
            self.dpb.fill_frame_num_gap(sps, hdr, &|id| {
                let mut p = Picture::new(id, wm, hm);
                if let Some(l) = &last {
                    if l.width == p.width && l.height == p.height {
                        p.y.copy_from_slice(&l.y);
                        p.u.copy_from_slice(&l.u);
                        p.v.copy_from_slice(&l.v);
                    }
                }
                p
            });
        }
        let poc = self.dpb.compute_poc(sps, hdr);
        let id = self.dpb.alloc_id();
        let mut pic = self.take_picture(id, wm, hm);
        pic.set_poc(structure, poc.top, poc.bottom);
        pic.frame_num = hdr.frame_num;
        pic.is_idr = hdr.is_idr();
        pic.is_ref = hdr.is_ref();
        pic.coded_fields = hdr.field_pic;
        pic.mbaff = sps.mbaff && !hdr.field_pic;
        pic.pts = pts;
        let mut mbs = std::mem::take(&mut self.mbs_buf);
        let mut deblock = std::mem::take(&mut self.deblock_buf);
        Self::reset_tables(&mut mbs, &mut deblock, n);
        self.cur = Some(Current { pic, hdr: hdr.clone(), poc, structure, second_field: false, mbs, deblock, slices: 0, damaged: false, decoded_mbs: 0 });
    }

    /// Finish the field or frame being decoded: deblock it, mark the
    /// references, and queue the frame for output once it is complete.
    fn finish_picture(&mut self) {
        let Some(mut cur) = self.cur.take() else { return };
        let sps = self.active_sps.clone().unwrap();
        let (wm, hm) = (sps.width_mbs as usize, sps.height_mbs as usize);
        let total = if cur.structure == FRAME { wm * hm } else { wm * hm / 2 };
        if cur.decoded_mbs < total {
            cur.damaged = true;
            self.conceal(&mut cur, wm, hm);
        }
        if !self.skip_deblock {
            deblock::filter_picture(&mut cur.pic, &cur.deblock, wm, hm, cur.structure);
        }
        self.last_kinds = cur.mbs.iter().map(|m| if m.slice != 0 { m.kind } else { crate::mb::MbKind::None }).collect();
        if cur.structure != FRAME && !cur.second_field {
            // the first field: later pictures (its own second field included)
            // reference a copy while the frame buffer waits for the other field
            let snapshot = Rc::new(cur.pic.clone());
            self.dpb.mark(&sps, &cur.hdr, snapshot, cur.poc, cur.structure);
            self.pending = Some(Pending { pic: cur.pic, frame_num: cur.hdr.frame_num, structure: cur.structure, is_ref: cur.hdr.is_ref(), damaged: cur.damaged, mbs: cur.mbs, deblock: cur.deblock });
            return;
        }
        let pic = Rc::new(cur.pic);
        self.dpb.mark(&sps, &cur.hdr, pic.clone(), cur.poc, cur.structure);
        self.emit(pic, cur.damaged);
        self.mbs_buf = cur.mbs;
        self.deblock_buf = cur.deblock;
    }

    fn emit(&mut self, pic: Rc<Picture>, damaged: bool) {
        // the picture's own sequence: a new one only becomes active once
        // the pictures before it are out
        let sps = self.active_sps.as_ref().unwrap();
        let (w, h) = sps.cropped_size();
        let crop = (sps.crop.0 as usize, sps.crop.2 as usize, w as usize, h as usize);
        if let Some(prev) = self.last_output.replace(pic.clone()) {
            self.retired.push(prev);
        }
        self.out.push(DecodedFrame { pic, crop, damaged });
    }

    /// A first field whose second field never came: show it with its lines
    /// doubled into the missing field.
    fn output_unpaired(&mut self, p: Pending) {
        let mut pic = p.pic;
        let w = pic.width;
        let cw = w / 2;
        let (from, to) = if p.structure == TOP { (0, 1) } else { (1, 0) };
        for r in (0..pic.height).step_by(2) {
            let (src, dst) = ((r + from) * w, (r + to) * w);
            pic.y.copy_within(src..src + w, dst);
        }
        for r in (0..pic.height / 2).step_by(2) {
            let (src, dst) = ((r + from) * cw, (r + to) * cw);
            pic.u.copy_within(src..src + cw, dst);
            pic.v.copy_within(src..src + cw, dst);
        }
        self.mbs_buf = p.mbs;
        self.deblock_buf = p.deblock;
        self.emit(Rc::new(pic), true);
    }

    /// Fill the macroblocks no slice covered with the co-located samples of
    /// the previous output picture (or mid grey when there is none).
    fn conceal(&self, cur: &mut Current, wm: usize, hm: usize) {
        let w = cur.pic.width;
        let cw = w / 2;
        let last = self.last_output.as_ref().filter(|l| l.width == cur.pic.width && l.height == cur.pic.height);
        let field = cur.structure != FRAME;
        let parity = (cur.structure == BOTTOM) as usize;
        let rows = if field { hm / 2 } else { hm };
        for row in 0..rows {
            let my = if field { 2 * row + parity } else { row };
            for mx in 0..wm {
                if cur.deblock[my * wm + mx].decoded {
                    continue;
                }
                for j in 0..16 {
                    let line = if field { 32 * row + parity + 2 * j } else { 16 * my + j };
                    let o = line * w + mx * 16;
                    match last {
                        Some(l) => cur.pic.y[o..o + 16].copy_from_slice(&l.y[o..o + 16]),
                        None => cur.pic.y[o..o + 16].fill(128),
                    }
                }
                for j in 0..8 {
                    let line = if field { 16 * row + parity + 2 * j } else { 8 * my + j };
                    let o = line * cw + mx * 8;
                    match last {
                        Some(l) => {
                            cur.pic.u[o..o + 8].copy_from_slice(&l.u[o..o + 8]);
                            cur.pic.v[o..o + 8].copy_from_slice(&l.v[o..o + 8]);
                        }
                        None => {
                            cur.pic.u[o..o + 8].fill(128);
                            cur.pic.v[o..o + 8].fill(128);
                        }
                    }
                }
                // a concealed macroblock predicts like an intra one with no motion
                cur.pic.mb_intra[my * wm + mx] = true;
                cur.pic.mb_field[my * wm + mx] = field;
                let w4 = w / 4;
                for by in 0..4 {
                    let b = (my * 4 + by) * w4 + mx * 4;
                    for l in 0..2 {
                        cur.pic.mv[l][b..b + 4].fill([0, 0]);
                        cur.pic.ref_idx[l][b..b + 4].fill(-1);
                        cur.pic.ref_id[l][b..b + 4].fill(-1);
                    }
                }
            }
        }
    }
}

/// The NAL units of an MP4 sample, each behind a `len_size`-byte length
/// (empty ones skipped). A length that runs past the sample is an error,
/// and the last item.
pub fn sample_nal_units(sample: &[u8], len_size: usize) -> impl Iterator<Item = Result<&[u8]>> {
    let mut p = 0;
    std::iter::from_fn(move || {
        while p + len_size <= sample.len() {
            let len = sample[p..p + len_size].iter().fold(0usize, |l, &b| (l << 8) | b as usize);
            p += len_size;
            if len == 0 {
                continue;
            }
            // (not `p + len > sample.len()`, which wraps on wasm32 for a
            // length near 2^32)
            if len > sample.len() - p {
                p = sample.len();
                return Some(Err(Error::Bitstream("NAL unit runs past the sample")));
            }
            p += len;
            return Some(Ok(&sample[p - len..p]));
        }
        None
    })
}

/// The NAL units of an Annex B byte stream, without their start codes and
/// the zero bytes before the next one.
pub fn annexb_nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::rewrite::{escape, BitWriter};
    use crate::slice::parse_slice_header;

    /// The fields of a test sequence that vary (Main profile, CAVLC,
    /// order counts of type 2).
    #[derive(Clone, Copy)]
    pub(crate) struct Seq {
        pub width_mbs: u32,
        pub height_map_units: u32,
        pub frame_mbs_only: bool,
        pub max_refs: u32,
        pub log2_max_frame_num: u32,
        /// left, right, top, bottom, in crop units
        pub crop: [u32; 4],
    }

    impl Default for Seq {
        fn default() -> Seq {
            Seq { width_mbs: 2, height_map_units: 2, frame_mbs_only: true, max_refs: 1, log2_max_frame_num: 4, crop: [0; 4] }
        }
    }

    /// A test slice: an I slice of macroblocks predicted flat (I_16x16, DC,
    /// no coefficients) or a P slice of skipped ones, every picture a
    /// reference.
    #[derive(Clone, Default)]
    pub(crate) struct Slice {
        pub idr: bool,
        pub idr_pic_id: u32,
        pub intra: bool,
        pub frame_num: u32,
        /// A field picture's slice: Some(bottom).
        pub field: Option<bool>,
        pub first_mb: u32,
        pub mbs: u32,
        /// The picture becomes the long-term frame of this index (memory
        /// management control operation 6).
        pub long_term: Option<u32>,
    }

    fn nal(header: u8, w: BitWriter) -> Vec<u8> {
        let mut nal = vec![header];
        nal.extend(escape(&w.into_bytes()));
        nal
    }

    pub(crate) fn sps_nal(s: &Seq) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.u(8, 77); // profile_idc: Main
        w.u(8, 0);
        w.u(8, 40); // level_idc
        w.ue(0); // seq_parameter_set_id
        w.ue(s.log2_max_frame_num - 4);
        w.ue(2); // pic_order_cnt_type
        w.ue(s.max_refs);
        w.u(1, 1); // gaps_in_frame_num_value_allowed_flag
        w.ue(s.width_mbs - 1);
        w.ue(s.height_map_units - 1);
        w.u(1, s.frame_mbs_only as u32);
        if !s.frame_mbs_only {
            w.u(1, 0); // mb_adaptive_frame_field_flag
        }
        w.u(1, 1); // direct_8x8_inference_flag
        w.u(1, (s.crop != [0; 4]) as u32);
        if s.crop != [0; 4] {
            for c in s.crop {
                w.ue(c);
            }
        }
        w.u(1, 0); // vui_parameters_present_flag
        w.trailing();
        nal(0x67, w)
    }

    pub(crate) fn pps_nal() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.u(2, 0); // CAVLC, bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.u(3, 0); // weighted_pred_flag, weighted_bipred_idc
        w.ue(0); // pic_init_qp_minus26 (se 0)
        w.ue(0); // pic_init_qs_minus26
        w.ue(0); // chroma_qp_index_offset
        w.u(3, 0); // deblocking_filter_control_present_flag, constrained_intra_pred_flag, redundant_pic_cnt_present_flag
        w.trailing();
        nal(0x68, w)
    }

    pub(crate) fn slice_nal(seq: &Seq, s: &Slice) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(s.first_mb);
        w.ue(if s.intra { 7 } else { 5 }); // slice_type: I or P, as all of the picture's
        w.ue(0); // pic_parameter_set_id
        w.u(seq.log2_max_frame_num, s.frame_num);
        if !seq.frame_mbs_only {
            w.u(1, s.field.is_some() as u32);
            if let Some(bottom) = s.field {
                w.u(1, bottom as u32);
            }
        }
        if s.idr {
            w.ue(s.idr_pic_id);
        }
        if !s.intra {
            w.u(2, 0); // num_ref_idx_active_override_flag, ref_pic_list_modification_flag_l0
        }
        if s.idr {
            w.u(2, 0); // no_output_of_prior_pics_flag, long_term_reference_flag
        } else if let Some(idx) = s.long_term {
            w.u(1, 1); // adaptive_ref_pic_marking_mode_flag
            w.ue(6);
            w.ue(idx);
            w.ue(0);
        } else {
            w.u(1, 0);
        }
        w.ue(0); // slice_qp_delta (se 0)
        if s.intra {
            for _ in 0..s.mbs {
                // mb_type I_16x16_2_0_0, intra_chroma_pred_mode, mb_qp_delta,
                // and the luma DC block's coeff_token for no coefficients
                w.ue(3);
                w.ue(0);
                w.ue(0);
                w.u(1, 1);
            }
        } else {
            w.ue(s.mbs); // mb_skip_run
        }
        w.trailing();
        nal(if s.idr { 0x65 } else { 0x61 }, w)
    }

    /// The parsed header of a test slice.
    pub(crate) fn header(seq: &Seq, s: &Slice) -> SliceHeader {
        let mut spss = vec![None; 32];
        spss[0] = Some(parse_sps(&unescape(&sps_nal(seq)[1..])).unwrap());
        let mut ppss = vec![None; 256];
        ppss[0] = Some(parse_pps(&unescape(&pps_nal()[1..])).unwrap());
        let nal = slice_nal(seq, s);
        parse_slice_header(&mut BitReader::new(&unescape(&nal[1..])), nal[0] & 0x1f, nal[0] >> 5, &spss, &ppss).unwrap()
    }

    /// NAL units as an MP4 sample (4-byte lengths).
    fn sample(nals: &[Vec<u8>]) -> Vec<u8> {
        nals.iter().flat_map(|n| (n.len() as u32).to_be_bytes().into_iter().chain(n.iter().copied())).collect()
    }

    /// NAL units as an Annex B byte stream.
    fn annexb(nals: &[Vec<u8>]) -> Vec<u8> {
        nals.iter().flat_map(|n| [0, 0, 0, 1].into_iter().chain(n.iter().copied())).collect()
    }

    #[test]
    fn nal_units_of_samples_and_byte_streams() {
        let nals: Vec<&[u8]> = sample_nal_units(&[0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 0, 0, 0, 0, 1, 0x06], 4).map(Result::unwrap).collect();
        assert_eq!(nals, [&[0x65, 0x88][..], &[0x06][..]]);
        let mut walk = sample_nal_units(&[0, 0, 0, 1, 0x06, 0, 0, 0, 9, 0x65], 4);
        assert_eq!(walk.next(), Some(Ok(&[0x06][..])));
        assert_eq!(walk.next(), Some(Err(Error::Bitstream("NAL unit runs past the sample"))));
        assert_eq!(walk.next(), None);
        assert_eq!(annexb_nal_units(&[0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 0, 0, 1, 0x65]), [&[0x67, 1][..], &[0x68, 2], &[0x65]]);
    }

    #[test]
    fn a_length_near_2_to_the_32_runs_past_the_sample() {
        // (the position wraps only on wasm32, where this panicked; natively
        // it passes without the fix too)
        assert!(Decoder::new().decode_sample(&[0xff, 0xff, 0xff, 0xff, 0x65], 0.0).is_err());
    }

    #[test]
    fn frame_cropping_must_fit_the_picture() {
        let crop = |crop| Decoder::new().decode_nal(&sps_nal(&Seq { crop, ..Seq::default() }), 0.0);
        // 32 x 32: two samples off the right, four off the bottom
        assert_eq!(crop([0, 1, 0, 2]), Ok(()));
        assert!(crop([0, 16, 0, 0]).is_err());
        // offsets whose sums in samples wrapped u32 to 0
        assert!(crop([0x7fff_ffff, 1, 0, 0]).is_err());
        assert!(crop([0, 0, 0x4000_0000, 0x4000_0000]).is_err());
    }

    #[test]
    fn pictures_larger_than_any_level_allows_are_refused() {
        let size = |width_mbs, height_map_units, frame_mbs_only| Decoder::new().decode_nal(&sps_nal(&Seq { width_mbs, height_map_units, frame_mbs_only, ..Seq::default() }), 0.0);
        // 8K (7680 x 4320), and as many macroblocks as level 6.2 allows
        assert_eq!(size(480, 270, true), Ok(()));
        assert_eq!(size(1024, 136, true), Ok(()));
        assert_eq!(size(1024, 137, true), Err(Error::Unsupported("picture size")));
        assert_eq!(size(1024, 1024, true), Err(Error::Unsupported("picture size")));
        // (only an interlaced sequence can be higher than 1024 macroblocks)
        assert_eq!(size(64, 527, false), Ok(()));
        assert_eq!(size(64, 528, false), Err(Error::Unsupported("picture size")));
    }

    #[test]
    fn long_term_frame_indices_are_bounded() {
        let seq = Seq { max_refs: 4, ..Seq::default() };
        let mut dec = Decoder::new();
        for n in [sps_nal(&seq), pps_nal(), slice_nal(&seq, &Slice { idr: true, intra: true, mbs: 4, ..Slice::default() })] {
            dec.decode_nal(&n, 0.0).unwrap();
        }
        let p = |frame_num, idx| slice_nal(&seq, &Slice { frame_num, mbs: 4, long_term: Some(idx), ..Slice::default() });
        assert_eq!(dec.decode_nal(&p(1, 15), 0.0), Ok(()));
        assert_eq!(dec.decode_nal(&p(2, 16), 0.0), Err(Error::Bitstream("long_term_frame_idx")));
    }

    #[test]
    fn field_pairs_keep_few_spare_pictures() {
        // frames of one macroblock column coded as two fields of one
        // macroblock each, every field a reference; each pair's first field
        // leaves a copy behind for the spares
        let seq = Seq { width_mbs: 1, height_map_units: 1, frame_mbs_only: false, ..Seq::default() };
        let mut dec = Decoder::new();
        dec.decode_nal(&sps_nal(&seq), 0.0).unwrap();
        dec.decode_nal(&pps_nal(), 0.0).unwrap();
        for k in 0..40 {
            let field = |bottom: bool| slice_nal(&seq, &Slice { idr: k == 0 && !bottom, intra: k == 0, frame_num: k % 16, field: Some(bottom), mbs: 1, ..Slice::default() });
            let frame = dec.decode_sample(&sample(&[field(false), field(true)]), k as f64).unwrap().expect("a frame for each pair");
            assert!(!frame.damaged && frame.pic.decoded == FRAME);
        }
        assert!(dec.pool.len() <= 4, "{} spare pictures", dec.pool.len());
    }

    #[test]
    fn a_new_size_between_two_slices_starts_a_new_picture() {
        // the same parameter set id re-sent with another size halfway
        // through a picture: the next slice belongs to a picture of that size
        let small = Seq { width_mbs: 2, height_map_units: 1, ..Seq::default() };
        let large = Seq { width_mbs: 4, height_map_units: 4, ..Seq::default() };
        let slice = |first_mb| Slice { idr: true, intra: true, first_mb, mbs: 1, ..Slice::default() };
        let stream = annexb(&[sps_nal(&small), pps_nal(), slice_nal(&small, &slice(0)), sps_nal(&large), slice_nal(&large, &slice(5))]);
        let mut dec = Decoder::new();
        let frames = dec.decode_annexb(&stream, 0.0).unwrap();
        assert_eq!(frames.iter().map(|f| (f.pic.width, f.pic.height, f.damaged)).collect::<Vec<_>>(), [(32, 16, true)]);
        let last = dec.flush().unwrap().unwrap();
        assert_eq!((last.pic.width, last.pic.height, last.damaged), (64, 64, true));
    }

    #[test]
    fn frames_carry_their_own_cropping() {
        // the second sequence is active by the time the first one's last
        // picture comes out of a byte stream
        let first = Seq { crop: [0, 1, 0, 2], ..Seq::default() };
        let second = Seq { width_mbs: 3, ..Seq::default() };
        let idr = |seq: &Seq, idr_pic_id| slice_nal(seq, &Slice { idr: true, idr_pic_id, intra: true, mbs: seq.width_mbs * seq.height_map_units, ..Slice::default() });
        let stream = annexb(&[sps_nal(&first), pps_nal(), idr(&first, 0), sps_nal(&second), idr(&second, 1)]);
        let mut dec = Decoder::new();
        let frames = dec.decode_annexb(&stream, 0.0).unwrap();
        assert_eq!(dec.sps().unwrap().cropped_size(), (48, 32));
        assert_eq!(frames.iter().map(|f| (f.crop, f.damaged)).collect::<Vec<_>>(), [((0, 0, 30, 28), false)]);
        assert_eq!(dec.flush().unwrap().map(|f| f.crop), Some((0, 0, 48, 32)));
    }

    #[test]
    fn a_sequence_sent_again_changed_takes_effect_at_its_idr() {
        // the second IDR picture comes with the set of the same id and size
        // cropped another way, and a later P picture with that set again,
        // unchanged: every frame from that IDR on is cropped the new way
        // (the old set stayed active until its id or size changed)
        let first = Seq { crop: [0, 1, 0, 2], ..Seq::default() };
        let second = Seq { crop: [1, 0, 2, 0], ..Seq::default() };
        let idr = |seq: &Seq, idr_pic_id| slice_nal(seq, &Slice { idr: true, idr_pic_id, intra: true, mbs: 4, ..Slice::default() });
        let p = |seq: &Seq, frame_num| slice_nal(seq, &Slice { frame_num, mbs: 4, ..Slice::default() });
        let samples = [vec![sps_nal(&first), pps_nal(), idr(&first, 0)], vec![p(&first, 1)], vec![sps_nal(&second), pps_nal(), idr(&second, 1)], vec![p(&second, 1)], vec![sps_nal(&second), p(&second, 2)]];
        let mut dec = Decoder::new();
        let frames: Vec<_> = samples.iter().map(|s| dec.decode_sample(&sample(s), 0.0).unwrap().expect("a frame for each sample")).collect();
        let (old, new) = ((0, 0, 30, 28), (2, 4, 30, 28));
        assert_eq!(frames.iter().map(|f| (f.crop, f.damaged)).collect::<Vec<_>>(), [(old, false), (old, false), (new, false), (new, false), (new, false)]);
    }

    #[test]
    fn a_sequence_changed_at_a_p_picture_keeps_the_references() {
        // a stream that sends its set again cropped another way before a P
        // picture (only an IDR picture may change it): the cropping takes
        // effect there, and the picture still predicts from the frame
        // before it, which has its size (without it, its slice has no
        // reference to predict from)
        let first = Seq { crop: [0, 1, 0, 2], ..Seq::default() };
        let second = Seq { crop: [1, 0, 2, 0], ..Seq::default() };
        let idr = slice_nal(&first, &Slice { idr: true, intra: true, mbs: 4, ..Slice::default() });
        let p = slice_nal(&second, &Slice { frame_num: 1, mbs: 4, ..Slice::default() });
        let mut dec = Decoder::new();
        let frames = [dec.decode_sample(&sample(&[sps_nal(&first), pps_nal(), idr]), 0.0), dec.decode_sample(&sample(&[sps_nal(&second), p]), 1.0)];
        assert_eq!(frames.map(|f| f.unwrap().map(|f| (f.crop, f.damaged))), [Some(((0, 0, 30, 28), false)), Some(((2, 4, 30, 28), false))]);
    }
}
