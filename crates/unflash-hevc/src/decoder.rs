//! The decoder: NAL units in, pictures out.

use std::rc::Rc;

use crate::bitreader::{unescape, BitReader};
use crate::ctu::{Carry, RefPic, SliceDecoder};
use crate::deblock::deblock;
use crate::dpb::{apply_rps, picture_order_count, ref_lists, DpbEntry, RefSet};
use crate::hash::{self, PictureHash};
use crate::meta::{Meta, RefKey, SliceInfo, INTRA, NO_SLICE};
use crate::picture::{ColMv, Picture, Plane, Sample, COL_L0, COL_L1, COL_LT0, COL_LT1};
use crate::ps::{parse_pps, parse_sps, Layout, Pps, Sps};
use crate::sao::sao;
use crate::slice::{nal, parse_slice_header, SliceHeader};
use crate::{Error, Result};

/// A decoded picture, cropped to the conformance window.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// 8-bit 4:2:0 planes, tightly packed; chroma is (w+1)/2 x (h+1)/2.
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// The stream's bit depth.
    pub bit_depth: u8,
    /// The planes at full precision when the bit depth is above 8 and the
    /// decoder was asked for them ([`Decoder::set_keep_deep`]).
    pub y16: Option<Vec<u16>>,
    pub u16: Option<Vec<u16>>,
    pub v16: Option<Vec<u16>>,
    /// The timestamp of the sample the picture came from.
    pub pts: f64,
    /// Part of the picture could not be decoded (it is concealed).
    pub damaged: bool,
    /// BT.709 colour (from the VUI, else by picture height).
    pub bt709: bool,
    pub full_range: bool,
}

/// A finished picture to hand out, with what its frame needs.
struct Finished<P> {
    pic: Rc<Picture<P>>,
    sps: Rc<Sps>,
    pts: f64,
    damaged: bool,
}

/// The picture being decoded.
struct Current<P> {
    pic: Picture<P>,
    sps: Rc<Sps>,
    pps: Rc<Pps>,
    layout: Rc<Layout>,
    refset: RefSet<P>,
    pts: f64,
    output: bool,
    damaged: bool,
    /// The encoder's hash of the picture, from a suffix SEI message.
    hash: Option<PictureHash>,
}

/// The decoding state for one sample type.
struct Core<P: Sample> {
    dpb: Vec<DpbEntry<P>>,
    /// Pictures that left the buffer, reused once nothing holds them.
    graveyard: Vec<Rc<Picture<P>>>,
    pool: Vec<Picture<P>>,
    cur: Option<Current<P>>,
    meta: Meta,
    carry: Carry,
    sao_scratch: Vec<P>,
    next_id: u32,
}

impl<P: Sample> Core<P> {
    fn new() -> Core<P> {
        Core { dpb: Vec::new(), graveyard: Vec::new(), pool: Vec::new(), cur: None, meta: Meta::new(), carry: Carry::default(), sao_scratch: Vec::new(), next_id: 1 }
    }

    /// A picture buffer of the given format: a reused one, or a new one.
    fn take_picture(&mut self, width: usize, height: usize, chroma: bool) -> Picture<P> {
        for rc in self.graveyard.drain(..) {
            if let Ok(p) = Rc::try_unwrap(rc) {
                if p.fits(width, height, chroma) {
                    self.pool.push(p);
                }
            }
        }
        self.pool.retain(|p| p.fits(width, height, chroma));
        // (a few: when references are missing, their stand-ins leave the
        // buffer too, and the spares would grow by one a picture)
        self.pool.truncate(4);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        match self.pool.pop() {
            Some(mut p) => {
                p.id = id;
                p.generated = false;
                p
            }
            None => Picture::new(id, width, height, chroma, P::default()),
        }
    }

    fn clear_dpb(&mut self) {
        self.graveyard.extend(self.dpb.drain(..).map(|e| e.pic));
    }

    /// Start decoding a picture: its reference picture set and buffers.
    #[allow(clippy::too_many_arguments)]
    fn start_picture(&mut self, sps: Rc<Sps>, pps: Rc<Pps>, layout: Rc<Layout>, hdr: &SliceHeader, poc: i32, pts: f64, output: bool, flush_refs: bool) {
        if flush_refs {
            self.clear_dpb();
        }
        let (w, h) = (sps.width as usize, sps.height as usize);
        let chroma = sps.chroma_format_idc != 0;
        let mut damaged = false;
        let grey = P::new(1 << (sps.bit_depth - 1));
        let mut next_id = self.next_id;
        let mut missing = |p: i32| {
            let mut pic = Picture::new(next_id, w, h, chroma, grey);
            next_id = next_id.wrapping_add(1);
            pic.poc = p;
            pic.generated = true;
            pic
        };
        let refset = apply_rps(&mut self.dpb, &mut self.graveyard, &sps, hdr, poc, &mut missing, &mut damaged);
        self.next_id = next_id;
        let mut pic = self.take_picture(w, h, chroma);
        pic.poc = poc;
        self.meta.reset(w, h, layout.ts_to_rs.len());
        self.carry = Carry::default();
        self.cur = Some(Current { pic, sps, pps, layout, refset, pts, output, damaged, hash: None });
    }

    /// Decode one slice segment of the current picture.
    fn decode_slice(&mut self, hdr: &SliceHeader, rbsp: &[u8]) -> Result<()> {
        let Some(cur) = self.cur.as_mut() else { return Err(Error::Bitstream("slice without a picture")) };
        let lists = if hdr.is_intra() {
            [Vec::new(), Vec::new()]
        } else {
            match ref_lists(&cur.refset, hdr) {
                Ok(l) => l,
                Err(e) => {
                    cur.damaged = true;
                    return Err(e);
                }
            }
        };
        if self.meta.slices.len() >= NO_SLICE as usize {
            cur.damaged = true;
            return Err(Error::Bitstream("too many slices in a picture"));
        }
        let key = |r: &RefPic<P>| RefKey { poc: r.poc, long_term: r.long_term, id: r.pic.id };
        self.meta.slices.push(SliceInfo {
            addr: hdr.slice_address,
            deblocking_disabled: hdr.deblocking_disabled,
            beta_offset_div2: hdr.beta_offset_div2,
            tc_offset_div2: hdr.tc_offset_div2,
            loop_filter_across_slices: hdr.loop_filter_across_slices,
            refs: [lists[0].iter().map(key).collect(), lists[1].iter().map(key).collect()],
        });
        let slice_idx = (self.meta.slices.len() - 1) as u16;
        let poc = cur.pic.poc;
        let result = SliceDecoder::new(&cur.sps, &cur.pps, &cur.layout, hdr, slice_idx, rbsp, &mut cur.pic, &mut self.meta, &lists, &mut self.carry, poc).and_then(|mut sd| sd.decode());
        if result.is_err() {
            cur.damaged = true;
        }
        Ok(())
    }

    /// Keep the decoded picture hash a suffix SEI message carries for the
    /// current picture.
    fn picture_hash(&mut self, rbsp: &[u8]) {
        if let Some(cur) = self.cur.as_mut() {
            let components = if cur.sps.chroma_format_idc == 0 { 1 } else { 3 };
            if let Some(h) = hash::parse_sei(rbsp, components) {
                cur.hash = Some(h);
            }
        }
    }

    /// Finish the current picture: conceal what no slice covered, run the
    /// in-loop filters, check it against its hash when asked to, keep it
    /// as a reference, and return it for output.
    fn finish_picture(&mut self, check_hash: bool) -> Option<Finished<P>> {
        let mut cur = self.cur.take()?;
        let layout = cur.layout.clone();
        let sps = cur.sps.clone();
        if self.meta.ctb_slice.contains(&NO_SLICE) {
            cur.damaged = true;
            self.conceal(&mut cur);
        }
        if self.meta.slices.iter().any(|s| !s.deblocking_disabled) {
            deblock(&mut cur.pic, &self.meta, &sps, &cur.pps, &layout);
        }
        if sps.sao_enabled {
            sao(&mut cur.pic, &self.meta, &sps, &cur.pps, &layout, &mut self.sao_scratch);
        }
        if let Some(h) = cur.hash.as_ref().filter(|_| check_hash) {
            cur.damaged |= !hash::matches(&cur.pic, sps.bit_depth, h);
        }
        self.compress_motion(&mut cur.pic, sps.log2_ctb as usize, layout.width_ctbs as usize);
        let pic = Rc::new(cur.pic);
        self.dpb.push(DpbEntry { pic: pic.clone(), long_term: false });
        cur.output.then_some(Finished { pic, sps, pts: cur.pts, damaged: cur.damaged })
    }

    /// Fill the coding tree blocks no slice decoded with the co-located
    /// samples of the latest reference picture (or mid-grey).
    fn conceal(&mut self, cur: &mut Current<P>) {
        let log2 = cur.sps.log2_ctb as usize;
        let wc = cur.layout.width_ctbs as usize;
        let chroma = cur.sps.chroma_format_idc != 0;
        let (w, h) = (cur.pic.width(), cur.pic.height());
        let src = self.dpb.iter().rev().map(|e| &e.pic).find(|p| p.fits(w, h, chroma) && !p.generated);
        let grey = P::new(1 << (cur.sps.bit_depth - 1));
        for (rs, &s) in self.meta.ctb_slice.iter().enumerate() {
            if s != NO_SLICE {
                continue;
            }
            let (x0, y0) = ((rs % wc) << log2, (rs / wc) << log2);
            for c in 0..if chroma { 3 } else { 1 } {
                let sub = (c > 0) as usize;
                let plane = &mut cur.pic.planes[c];
                let (xs, ys) = (x0 >> sub, y0 >> sub);
                let size = (1 << log2) >> sub;
                let (cw, chh) = (size.min(plane.width - xs), size.min(plane.height - ys));
                for j in 0..chh {
                    let at = (ys + j) * plane.stride + xs;
                    match src {
                        Some(p) => plane.data[at..at + cw].copy_from_slice(&p.planes[c].data[at..at + cw]),
                        None => plane.data[at..at + cw].fill(grey),
                    }
                }
            }
            let n = 1 << log2;
            let (bw, bh) = (n.min(w - x0), n.min(h - y0));
            let w4 = self.meta.w4;
            Meta::fill(&mut self.meta.flags, w4, x0, y0, bw, bh, INTRA);
        }
    }

    /// Keep the motion of the top-left 4x4 block of every 16x16 block, with
    /// the order counts it refers to, for temporal prediction (8.5.3.2.8).
    fn compress_motion(&self, pic: &mut Picture<P>, log2: usize, wc: usize) {
        let meta = &self.meta;
        let (w16, h16) = (pic.width().div_ceil(16), pic.height().div_ceil(16));
        for by in 0..h16 {
            for bx in 0..w16 {
                let (x, y) = (bx * 16, by * 16);
                let i = meta.at(x, y);
                let slice = meta.ctb_slice[(y >> log2) * wc + (x >> log2)];
                let m = meta.motion[i];
                let mut c = ColMv::default();
                if slice != NO_SLICE && meta.flags[i] & INTRA == 0 {
                    for (l, refs) in meta.slices[slice as usize].refs.iter().enumerate() {
                        if let Some(k) = m.uses(l).then(|| refs.get(m.ref_idx[l] as usize)).flatten() {
                            c.mv[l] = m.mv[l];
                            c.poc[l] = k.poc;
                            c.flags |= if l == 0 { COL_L0 } else { COL_L1 };
                            if k.long_term {
                                c.flags |= if l == 0 { COL_LT0 } else { COL_LT1 };
                            }
                        }
                    }
                }
                pic.col[by * w16 + bx] = c;
            }
        }
    }
}

/// The `w`×`h` area of `plane` at (`x0`, `y0`), each sample through `f`.
fn crop<P: Sample, T>(plane: &Plane<P>, (x0, y0, w, h): (usize, usize, usize, usize), f: impl Fn(P) -> T) -> Vec<T> {
    let mut out = Vec::with_capacity(w * h);
    for y in y0..y0 + h {
        out.extend(plane.data[y * plane.stride + x0..][..w].iter().map(|&s| f(s)));
    }
    out
}

/// Crop and convert a picture for output (and keep it at full precision
/// too when it is deeper than 8 bits and `keep_deep` asks for that).
fn to_frame<P: Sample>(pic: &Picture<P>, sps: &Sps, pts: f64, damaged: bool, keep_deep: bool) -> Frame {
    let (l, _, t, _) = sps.conf_win;
    let (w, h) = sps.cropped_size();
    let (w, h, l, t) = (w as usize, h as usize, l as usize, t as usize);
    let bd = sps.bit_depth;
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let areas = [(l, t, w, h), (l / 2, t / 2, cw, ch), (l / 2, t / 2, cw, ch)];
    let chroma = sps.chroma_format_idc != 0;
    let deep = bd > 8;
    let (round, shift) = if deep { (1 << (bd - 9), bd - 8) } else { (0, 0) };
    let planes8: [Vec<u8>; 3] = std::array::from_fn(|c| match (c, chroma) {
        (1 | 2, false) => vec![128; cw * ch],
        _ => crop(&pic.planes[c], areas[c], |s| ((s.get() as u32 + round) >> shift).min(255) as u8),
    });
    let planes16: Option<[Vec<u16>; 3]> = (deep && keep_deep).then(|| {
        std::array::from_fn(|c| match (c, chroma) {
            (1 | 2, false) => vec![1 << (bd - 1); cw * ch],
            _ => crop(&pic.planes[c], areas[c], |s| s.get() as u16),
        })
    });
    let vui = sps.vui.as_ref();
    let bt709 = is_bt709(vui.map(|v| v.matrix_coeffs), h);
    let [y, u, v] = planes8;
    let (y16, u16, v16) = match planes16 {
        Some([y, u, v]) => (Some(y), Some(u), Some(v)),
        None => (None, None, None),
    };
    Frame {
        width: w as u32,
        height: h as u32,
        y,
        u,
        v,
        bit_depth: bd as u8,
        y16,
        u16,
        v16,
        pts,
        damaged,
        bt709,
        full_range: vui.is_some_and(|v| v.video_full_range),
    }
}

/// Whether a picture's colour matrix is BT.709: from the VUI's
/// matrix_coeffs (BT.2020's taken as BT.709), else above 576 lines, as
/// players guess (the rule of the other decoders and of web/media.js).
fn is_bt709(matrix_coeffs: Option<u8>, height: usize) -> bool {
    match matrix_coeffs {
        Some(1) | Some(9) => true,
        Some(5) | Some(6) => false,
        _ => height > 576,
    }
}

/// The decoding state of the sample type in use.
enum Cores {
    None,
    Eight(Box<Core<u8>>),
    Deep(Box<Core<u16>>),
}

/// Run an expression on the core whatever its sample type.
macro_rules! with_core {
    ($core:expr, $c:ident => $e:expr, $none:expr) => {
        match $core {
            Cores::None => $none,
            Cores::Eight($c) => $e,
            Cores::Deep($c) => $e,
        }
    };
}

/// An HEVC decoder: parameter sets, the reference pictures and the
/// picture in progress. Pictures come out in decoding order as soon as
/// they are complete (the caller orders them by timestamp), so the
/// decoder keeps only the pictures later ones may predict from.
pub struct Decoder {
    nal_length_size: usize,
    spss: Vec<Option<Rc<Sps>>>,
    ppss: Vec<Option<Rc<Pps>>>,
    active_sps: Option<Rc<Sps>>,
    layout: Option<(Rc<Sps>, Rc<Pps>, Rc<Layout>)>,
    core: Cores,
    check_hashes: bool,
    /// Deeper pictures come out at full precision too (`set_keep_deep`).
    keep_deep: bool,
    out: Vec<Frame>,
    rbsp: Vec<u8>,
    /// The header of the last independent slice segment of the picture.
    prev_hdr: Option<SliceHeader>,
    /// Slices of a picture that is skipped (RASL after a random access).
    skipping: bool,
    /// The next IRAP picture starts a coded video sequence (the first
    /// picture, or one after an end of sequence).
    new_sequence: bool,
    /// PicOrderCntVal of the previous TemporalId 0 picture (8.3.1).
    prev_tid0_poc: i32,
    /// ffmpeg's rule for dropping RASL pictures: those with an order count
    /// up to that of the CRA or BLA picture decoding started at.
    max_ra: i32,
}

impl Decoder {
    /// `config`: the hvcC record from the MP4 / Matroska track (may be
    /// empty; parameter sets can also arrive in-band).
    pub fn new(config: &[u8]) -> Result<Decoder> {
        let mut d = Decoder {
            nal_length_size: 4,
            spss: vec![None; 16],
            ppss: vec![None; 64],
            active_sps: None,
            layout: None,
            core: Cores::None,
            check_hashes: false,
            keep_deep: false,
            out: Vec::new(),
            rbsp: Vec::new(),
            prev_hdr: None,
            skipping: false,
            new_sequence: true,
            prev_tid0_poc: 0,
            max_ra: i32::MAX,
        };
        if !config.is_empty() {
            d.configure(config)?;
        }
        Ok(d)
    }

    /// Read an HEVCDecoderConfigurationRecord (ISO/IEC 14496-15 8.3.3):
    /// the NAL unit length size and the parameter sets.
    fn configure(&mut self, c: &[u8]) -> Result<()> {
        if c.len() < 23 {
            return Err(Error::Bitstream("hvcC record too short"));
        }
        self.nal_length_size = (c[21] & 3) as usize + 1;
        let arrays = c[22] as usize;
        let mut p = 23;
        for _ in 0..arrays {
            if p + 3 > c.len() {
                return Err(Error::Bitstream("hvcC record truncated"));
            }
            let count = u16::from_be_bytes([c[p + 1], c[p + 2]]) as usize;
            p += 3;
            for _ in 0..count {
                if p + 2 > c.len() {
                    return Err(Error::Bitstream("hvcC record truncated"));
                }
                let len = u16::from_be_bytes([c[p], c[p + 1]]) as usize;
                p += 2;
                if p + len > c.len() {
                    return Err(Error::Bitstream("hvcC record truncated"));
                }
                self.decode_nal(&c[p..p + len], 0.0)?;
                p += len;
            }
        }
        Ok(())
    }

    /// How many pictures may come out before one that is shown earlier:
    /// sps_max_num_reorder_pics of the stream's sequence parameter sets
    /// (the most any of them allows), or the most the standard allows
    /// before one has been seen. A caller putting the pictures in
    /// presentation order holds this many back.
    pub fn reorder_depth(&self) -> u32 {
        let seen = self.spss.iter().flatten().map(|s| s.max_num_reorder_pics).max();
        seen.unwrap_or(15)
    }

    /// Check every picture against the decoded picture hash SEI message
    /// the encoder sent with it, when there is one, and mark those that
    /// differ as damaged: a self-test for conformance streams (hashing
    /// every picture costs time).
    pub fn set_check_hashes(&mut self, check: bool) {
        self.check_hashes = check;
    }

    /// Give pictures deeper than 8 bits at full precision too
    /// ([`Frame::y16`] and the others) besides the rounded 8-bit planes
    /// every picture has: for comparing with another decoder at that depth,
    /// as the tests do. Off by default (they stay None), since the app
    /// reads only the 8-bit planes, and the copy is about 6 MB a picture
    /// at 10-bit 1080p.
    pub fn set_keep_deep(&mut self, keep: bool) {
        self.keep_deep = keep;
    }

    /// Decode one container sample (one access unit of length-prefixed NAL
    /// units). Returns the pictures it completes, in decoding order, each
    /// carrying the pts of the sample it came from. Damage inside slice
    /// data is concealed (the picture is marked damaged); an error means a
    /// NAL unit could not be used at all (an unsupported or broken
    /// parameter set or slice header), and the pictures decoded anyway
    /// come with the next call's.
    pub fn decode(&mut self, sample: &[u8], pts: f64) -> Result<Vec<Frame>> {
        let n = self.nal_length_size;
        let mut p = 0;
        let mut result = Ok(());
        while p + n <= sample.len() {
            let len = sample[p..p + n].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
            p += n;
            if len == 0 {
                continue;
            }
            // (not p + len, which can wrap around on 32-bit targets)
            if len > sample.len() - p {
                result = Err(Error::Bitstream("NAL unit runs past the end of the sample"));
                break;
            }
            if let Err(e) = self.decode_nal(&sample[p..p + len], pts) {
                result = result.and(Err(e));
            }
            p += len;
        }
        self.finish_picture();
        result?;
        Ok(std::mem::take(&mut self.out))
    }

    /// Decode an Annex B byte stream chunk (start-code delimited NAL
    /// units); a picture completes when the next one starts, so call
    /// `flush` at the end.
    pub fn decode_annexb(&mut self, data: &[u8], pts: f64) -> Result<Vec<Frame>> {
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
        let mut result = Ok(());
        for (k, &s) in starts.iter().enumerate() {
            let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
            while e > s && data[e - 1] == 0 {
                e -= 1;
            }
            if e > s {
                if let Err(err) = self.decode_nal(&data[s..e], pts) {
                    result = result.and(Err(err));
                }
            }
        }
        result?;
        Ok(std::mem::take(&mut self.out))
    }

    /// Finish the picture in progress, if any.
    pub fn flush(&mut self) -> Result<Vec<Frame>> {
        self.finish_picture();
        Ok(std::mem::take(&mut self.out))
    }

    fn finish_picture(&mut self) {
        let (check, keep_deep) = (self.check_hashes, self.keep_deep);
        let frame = with_core!(&mut self.core, c => c.finish_picture(check).map(|f| to_frame(&f.pic, &f.sps, f.pts, f.damaged, keep_deep)), None);
        self.out.extend(frame);
    }

    /// Decode one NAL unit (with its two-byte header, emulation prevention
    /// still in place).
    fn decode_nal(&mut self, data: &[u8], pts: f64) -> Result<()> {
        if data.len() < 2 {
            return Ok(());
        }
        let nal_type = (data[0] >> 1) & 0x3f;
        let layer = ((data[0] & 1) << 5) | (data[1] >> 3);
        let temporal_id = (data[1] & 7).saturating_sub(1);
        if layer > 0 {
            // other layers (multi-view, scalable) are not decoded
            return Ok(());
        }
        let mut rbsp = std::mem::take(&mut self.rbsp);
        unescape(&data[2..], &mut rbsp);
        let result = match nal_type {
            nal::SPS => parse_sps(&rbsp).map(|sps| {
                let id = sps.id as usize;
                self.spss[id] = Some(Rc::new(sps));
            }),
            nal::PPS => parse_pps(&rbsp, &self.spss).map(|pps| {
                let id = pps.id as usize;
                self.ppss[id] = Some(Rc::new(pps));
            }),
            nal::AUD => {
                self.finish_picture();
                Ok(())
            }
            nal::EOS | nal::EOB => {
                self.finish_picture();
                self.new_sequence = true;
                self.max_ra = i32::MAX;
                Ok(())
            }
            0..=9 | 16..=21 => self.decode_slice(nal_type, temporal_id, &rbsp, pts),
            nal::SEI_SUFFIX if self.check_hashes => {
                with_core!(&mut self.core, c => c.picture_hash(&rbsp), ());
                Ok(())
            }
            _ => Ok(()),
        };
        self.rbsp = rbsp;
        result
    }

    fn decode_slice(&mut self, nal_type: u8, temporal_id: u8, rbsp: &[u8], pts: f64) -> Result<()> {
        let mut r = BitReader::new(rbsp);
        // a dependent slice segment continues the last independent one
        let first = r.peek(1) == Some(1);
        let prev = if first { None } else { self.prev_hdr.as_ref() };
        let hdr = match parse_slice_header(&mut r, nal_type, &self.spss, &self.ppss, prev) {
            Ok(h) => h,
            Err(e) => {
                if first {
                    // the picture this slice starts cannot be decoded
                    self.finish_picture();
                    self.skipping = true;
                } else {
                    self.mark_damaged();
                }
                return Err(e);
            }
        };
        if hdr.first_slice_segment_in_pic {
            self.finish_picture();
            self.skipping = false;
            self.start_picture(&hdr, nal_type, temporal_id, pts)?;
        }
        if self.skipping {
            return Ok(());
        }
        // the slices of a picture are decoded with its parameter sets, so
        // one that refers to others (or to its ids, sent again changed) is
        // refused
        if !hdr.first_slice_segment_in_pic && !self.same_sets(&hdr) {
            self.mark_damaged();
            return Err(Error::Bitstream("parameter sets change within a picture"));
        }
        if !hdr.dependent {
            self.prev_hdr = Some(hdr.clone());
        }
        with_core!(&mut self.core, c => c.decode_slice(&hdr, rbsp), Err(Error::Bitstream("slice without a picture")))
    }

    fn mark_damaged(&mut self) {
        with_core!(&mut self.core, c => if let Some(cur) = c.cur.as_mut() { cur.damaged = true }, ());
    }

    /// Whether a slice refers to the parameter sets of the picture in
    /// progress (the same ones, or the same sent again).
    fn same_sets(&self, hdr: &SliceHeader) -> bool {
        let Some((sps, pps, _)) = &self.layout else { return false };
        let Some(p) = &self.ppss[hdr.pps_id as usize] else { return false };
        let s = &self.spss[p.sps_id as usize];
        (Rc::ptr_eq(p, pps) || **p == **pps) && s.as_ref().is_some_and(|s| Rc::ptr_eq(s, sps) || **s == **sps)
    }

    /// Begin a new picture with its first slice segment's header.
    fn start_picture(&mut self, hdr: &SliceHeader, nal_type: u8, temporal_id: u8, pts: f64) -> Result<()> {
        let pps = self.ppss[hdr.pps_id as usize].clone().ok_or(Error::Bitstream("slice refers to a missing PPS"))?;
        let sps = self.spss[pps.sps_id as usize].clone().ok_or(Error::Bitstream("PPS refers to a missing SPS"))?;
        let irap = nal::is_irap(nal_type);
        // a new sequence parameter set starts a new coded video sequence
        let sps_changed = self.active_sps.as_ref().is_none_or(|a| !Rc::ptr_eq(a, &sps) && **a != *sps);
        if sps_changed {
            self.max_ra = i32::MAX;
            self.new_sequence = true;
            let deep = sps.bit_depth > 8;
            let fits = matches!((&self.core, deep), (Cores::Eight(_), false) | (Cores::Deep(_), true));
            if !fits {
                self.core = if deep { Cores::Deep(Box::new(Core::new())) } else { Cores::Eight(Box::new(Core::new())) };
            }
        }
        self.active_sps = Some(sps.clone());
        let layout = match &self.layout {
            Some((s, p, l)) if Rc::ptr_eq(s, &sps) && Rc::ptr_eq(p, &pps) => l.clone(),
            _ => {
                let l = Rc::new(Layout::new(&sps, &pps)?);
                self.layout = Some((sps.clone(), pps.clone(), l.clone()));
                l
            }
        };
        // NoRaslOutputFlag: IDR and BLA pictures, and a CRA picture that
        // starts the stream or follows an end of sequence
        let no_rasl_output = irap && (nal::is_idr(nal_type) || nal::is_bla(nal_type) || self.new_sequence);
        if irap {
            self.new_sequence = false;
        }
        let poc = picture_order_count(&sps, hdr.poc_lsb, self.prev_tid0_poc, irap && no_rasl_output);
        if nal::is_idr(nal_type) || nal::is_bla(nal_type) {
            self.max_ra = i32::MAX;
        }
        if self.max_ra == i32::MAX {
            if nal_type == nal::CRA_NUT || nal::is_bla(nal_type) {
                self.max_ra = poc;
            } else if nal::is_idr(nal_type) {
                self.max_ra = i32::MIN;
            }
        }
        if nal::is_rasl(nal_type) && poc <= self.max_ra {
            // a leading picture whose references precede the random access point
            self.skipping = true;
            return Ok(());
        }
        if nal_type == nal::RASL_R && poc > self.max_ra {
            self.max_ra = i32::MIN;
        }
        if temporal_id == 0 && !nal::is_rasl(nal_type) && !nal::is_radl(nal_type) && !nal::is_sub_layer_non_ref(nal_type) {
            self.prev_tid0_poc = poc;
        }
        let output = hdr.pic_output;
        // (a stream changes its sequence parameter set only at an IRAP
        // picture that starts afresh; at any other picture the old
        // sequence's references, maybe of another size or format, are
        // dropped all the same)
        let flush_refs = sps_changed || (irap && no_rasl_output);
        with_core!(&mut self.core, c => c.start_picture(sps, pps, layout, hdr, poc, pts, output, flush_refs), ());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streams that leave the colour matrix unspecified are taken as
    /// BT.709 above 576 lines, as the other decoders and web/media.js take
    /// them, and BT.2020's matrix as BT.709.
    #[test]
    fn colour_matrix() {
        assert!(is_bt709(None, 640));
        assert!(is_bt709(Some(2), 577));
        assert!(!is_bt709(None, 576));
        assert!(is_bt709(Some(9), 480));
        assert!(!is_bt709(Some(6), 1080));
    }

    /// The spare picture buffers stay few when references are missing:
    /// p_frames' pictures each predict from the one before, so with every
    /// other one dropped each but the first names a missing picture, whose
    /// mid-grey stand-in joins the spares later (they grew by one for
    /// every picture).
    #[test]
    fn spare_pictures_stay_few() {
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/media/hevc/p_frames.mp4")).unwrap();
        let movie = unflash_mp4::demux::parse_bytes(&data).unwrap();
        let track = movie.video().unwrap();
        let mut dec = Decoder::new(track.description.as_deref().unwrap()).unwrap();
        for s in track.samples.iter().step_by(2) {
            let frames = dec.decode(&data[s.offset as usize..(s.offset + s.size as u64) as usize], s.pts as f64).unwrap();
            assert!(frames.iter().all(|f| f.damaged == (s.pts > 0)));
        }
        let Cores::Eight(core) = &dec.core else { panic!("an 8-bit stream") };
        assert!(core.pool.len() <= 4, "{} spare pictures", core.pool.len());
    }
}
