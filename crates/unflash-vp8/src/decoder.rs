//! The decoder: VP8 frames in, pictures out.

use crate::bool_decoder::BoolDecoder;
use crate::header::{self, FrameHeader, Parsed, State, MAX_PARTITIONS};
use crate::inter::{self, Filter};
use crate::intra;
use crate::loopfilter::{self, MbFilter};
use crate::modes::{self, MbInfo, ModeParams, Mv, INTRA, LAST};
use crate::picture::{try_alloc, Picture};
use crate::tables::*;
use crate::tokens::{self, Coeffs, NonZero, Y2_BLOCK};
use crate::transform;
use crate::{Error, Result};

/// A decoded picture, cropped to the display size.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// 8-bit 4:2:0 planes, tightly packed; chroma is (width + 1) / 2 by
    /// (height + 1) / 2.
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub pts: f64,
    /// Part of the frame's data was missing: the rest is concealed.
    pub damaged: bool,
    /// VP8 is BT.601 in limited range: both false. (ffmpeg reports the
    /// `clamping_type` bit as full range, but that bit is about clamping,
    /// and libvpx always writes 0.)
    pub bt709: bool,
    pub full_range: bool,
}

pub struct Decoder {
    /// Probabilities, segmentation and filter deltas carried between frames.
    state: State,
    /// Display size, and the size in macroblocks.
    width: u32,
    height: u32,
    mb_w: usize,
    mb_h: usize,
    /// Picture buffers: the three references and spares to decode into.
    pics: Vec<Picture>,
    last: Option<usize>,
    golden: Option<usize>,
    altref: Option<usize>,
    /// The segment of each macroblock in the frame decoded last, which a
    /// frame with segmentation but no map update keeps.
    segments: Vec<u8>,
    /// This frame's macroblock records and loop filter parameters.
    mbs: Vec<MbInfo>,
    filters: Vec<MbFilter>,
    /// Coefficient contexts along the bottom of the macroblock row above.
    above_nz: Vec<NonZero>,
    coeffs: Box<Coeffs>,
}

/// The references of a frame by `MbInfo::ref_frame` (none for intra).
type Refs<'a> = [Option<&'a Picture>; 4];

impl Decoder {
    /// `config`: codec configuration from the container. VP8 has none, so
    /// whatever the container gives (nothing in WebM, a `vpcC` in MP4) is
    /// not needed and not read.
    pub fn new(_config: &[u8]) -> Result<Decoder> {
        Ok(Decoder {
            state: State::default(),
            width: 0,
            height: 0,
            mb_w: 0,
            mb_h: 0,
            pics: Vec::new(),
            last: None,
            golden: None,
            altref: None,
            segments: Vec::new(),
            mbs: Vec::new(),
            filters: Vec::new(),
            above_nz: Vec::new(),
            coeffs: Box::default(),
        })
    }

    /// Decode one container sample (one VP8 frame). Returns the frame when
    /// it is shown, carrying the sample's `pts`; an invisible frame (such
    /// as an alt-ref) and an empty sample yield nothing.
    pub fn decode(&mut self, sample: &[u8], pts: f64) -> Result<Vec<Frame>> {
        if sample.is_empty() {
            return Ok(Vec::new());
        }
        let tag = header::parse_tag(sample)?;
        if !tag.key_frame && self.last.is_none() {
            return Err(Error::Bitstream("inter frame before the first key frame"));
        }
        let mut state = self.state.clone();
        let mut parsed = header::parse_frame(sample, &mut state)?;
        let h = &parsed.header;
        if tag.key_frame && (h.width != self.width || h.height != self.height) {
            self.resize(h.width, h.height)?;
        }
        let cur = self.free_picture()?;
        // taken out while the references are read
        let mut pic = std::mem::take(&mut self.pics[cur]);
        let damaged = self.decode_macroblocks(&mut parsed, &state, &mut pic);
        self.pics[cur] = pic;

        // the probabilities this frame changed for itself alone go back
        self.state = state;
        if let Some(saved) = parsed.saved_probs.take() {
            self.state.probs = saved;
        }
        // 9.7: the copies read the references as they were before this
        // frame, as in ffmpeg (libvpx copies to the alt-ref first, so it
        // cannot swap golden and alt-ref; encoders never ask for that)
        let h = &parsed.header;
        let (last, golden, altref) = (self.last, self.golden, self.altref);
        self.golden = match (h.refresh_golden, h.copy_to_golden) {
            (true, _) => Some(cur),
            (false, 1) => last,
            (false, 2) => altref,
            _ => golden,
        };
        self.altref = match (h.refresh_altref, h.copy_to_altref) {
            (true, _) => Some(cur),
            (false, 1) => last,
            (false, 2) => golden,
            _ => altref,
        };
        if h.refresh_last {
            self.last = Some(cur);
        }
        if !h.tag.show_frame {
            return Ok(Vec::new());
        }
        Ok(vec![self.output(cur, pts, damaged)?])
    }

    /// VP8 frames come out in decoding order, so there is nothing to flush.
    pub fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }

    /// The macroblock records of the last frame decoded, in raster order
    /// (for looking at which coding tools a stream uses).
    pub fn macroblocks(&self) -> &[MbInfo] {
        &self.mbs
    }

    /// A key frame of a new size: start afresh with buffers of that size.
    fn resize(&mut self, width: u32, height: u32) -> Result<()> {
        self.pics.clear();
        self.last = None;
        self.golden = None;
        self.altref = None;
        self.width = 0;
        self.height = 0;
        let (mb_w, mb_h) = ((width as usize).div_ceil(16), (height as usize).div_ceil(16));
        let n = mb_w * mb_h;
        self.segments = try_alloc(n)?;
        self.mbs = try_alloc(n)?;
        self.filters = try_alloc(n)?;
        self.above_nz = try_alloc(mb_w)?;
        self.width = width;
        self.height = height;
        self.mb_w = mb_w;
        self.mb_h = mb_h;
        Ok(())
    }

    /// A picture buffer no reference holds, allocating one if need be.
    fn free_picture(&mut self) -> Result<usize> {
        let held = [self.last, self.golden, self.altref];
        if let Some(i) = (0..self.pics.len()).find(|&i| !held.contains(&Some(i))) {
            return Ok(i);
        }
        self.pics.push(Picture::new(self.mb_w, self.mb_h)?);
        Ok(self.pics.len() - 1)
    }

    /// Decode every macroblock of a frame into `pic`, loop filtering one
    /// row behind. Returns whether the frame ran out of data (the rest of
    /// it is then copied from the last frame).
    fn decode_macroblocks(&mut self, p: &mut Parsed, state: &State, pic: &mut Picture) -> bool {
        let h = &p.header;
        let (mb_w, mb_h) = (self.mb_w, self.mb_h);
        let dq = header::dequant_factors(h, &state.segmentation);
        let params = ModeParams {
            key_frame: h.tag.key_frame,
            segmentation: &state.segmentation,
            skip_prob: h.skip_prob,
            prob_intra: h.prob_intra,
            prob_last: h.prob_last,
            prob_golden: h.prob_golden,
            probs: &state.probs,
            sign_bias: [false, false, h.sign_bias_golden, h.sign_bias_altref],
            mb_w,
            mb_h,
        };
        // version 0 is the six-tap filter; ffmpeg treats every other
        // version (reserved ones too) as bilinear, only 3 with whole-sample
        // chroma vectors
        let filter = if h.tag.version == 0 { Filter::SixTap } else { Filter::Bilinear };
        let full_pixel = h.tag.version == 3;
        let loop_filter = h.filter_level > 0;
        let refs: Refs = [None, self.last.map(|i| &self.pics[i]), self.golden.map(|i| &self.pics[i]), self.altref.map(|i| &self.pics[i])];
        let mut parts: [BoolDecoder; MAX_PARTITIONS] = std::array::from_fn(|i| BoolDecoder::new(p.partitions[i]));
        self.above_nz.fill([0; 9]);
        let mut concealing = false;
        for mb_y in 0..mb_h {
            let part = &mut parts[mb_y % p.num_partitions];
            let mut left_nz: NonZero = [0; 9];
            for mb_x in 0..mb_w {
                let i = mb_y * mb_w + mb_x;
                if !concealing {
                    let above = if mb_y > 0 { &self.mbs[i - mb_w] } else { &MbInfo::OUTSIDE };
                    let left = if mb_x > 0 { &self.mbs[i - 1] } else { &MbInfo::OUTSIDE };
                    let above_left = if mb_x > 0 && mb_y > 0 { &self.mbs[i - mb_w - 1] } else { &MbInfo::OUTSIDE };
                    let mb = modes::read_mb(&mut p.first, &params, mb_x, mb_y, above, left, above_left, &mut self.segments[i]);
                    let y2 = mb.y_mode != B_PRED && mb.y_mode != SPLITMV;
                    let coded = if mb.skip {
                        tokens::skip_mb(y2, &mut self.above_nz[mb_x], &mut left_nz);
                        self.coeffs.eob = [0; 25];
                        false
                    } else {
                        tokens::read_mb(part, &state.probs.coeff, y2, &dq[mb.segment as usize], &mut self.above_nz[mb_x], &mut left_nz, &mut self.coeffs)
                    };
                    if p.first.exhausted() || part.exhausted() {
                        concealing = true;
                        *self.coeffs = Coeffs::default();
                    } else {
                        self.filters[i] = mb_filter(h, state, &mb, coded);
                        if mb.ref_frame == INTRA {
                            predict_intra(pic, &mb, mb_x, mb_y, mb_w, &mut self.coeffs);
                        } else if let Some(r) = refs[mb.ref_frame as usize] {
                            // (inter frames come after a key frame, which fills
                            // every reference)
                            predict_inter(pic, r, &mb, mb_x, mb_y, filter, full_pixel);
                        }
                        if coded {
                            add_residual(pic, &mb, mb_x, mb_y, &mut self.coeffs);
                        }
                        self.mbs[i] = mb;
                    }
                }
                if concealing {
                    conceal(pic, refs[LAST as usize], mb_x, mb_y);
                    self.mbs[i] = MbInfo::OUTSIDE;
                    self.filters[i] = MbFilter::default();
                }
            }
            if loop_filter && mb_y > 0 {
                loopfilter::filter_row(pic, mb_y - 1, &self.filters[(mb_y - 1) * mb_w..mb_y * mb_w], h.simple_filter);
            }
        }
        if loop_filter {
            loopfilter::filter_row(pic, mb_h - 1, &self.filters[(mb_h - 1) * mb_w..], h.simple_filter);
        }
        concealing
    }

    /// The picture `cur` cropped to the display size.
    fn output(&self, cur: usize, pts: f64, damaged: bool) -> Result<Frame> {
        let pic = &self.pics[cur];
        let (w, h) = (self.width as usize, self.height as usize);
        let crop = |plane: &[u8], stride: usize, w: usize, h: usize| -> Result<Vec<u8>> {
            let mut out = Vec::new();
            out.try_reserve_exact(w * h).map_err(|_| Error::Unsupported("picture too large for the memory available"))?;
            for row in plane.chunks(stride).take(h) {
                out.extend_from_slice(&row[..w]);
            }
            Ok(out)
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Ok(Frame {
            width: self.width,
            height: self.height,
            y: crop(&pic.y, pic.width, w, h)?,
            u: crop(&pic.u, pic.width / 2, cw, ch)?,
            v: crop(&pic.v, pic.width / 2, cw, ch)?,
            pts,
            damaged,
            bt709: false,
            full_range: false,
        })
    }
}

/// 9.6, 10 and 15.1: a macroblock's filter level and limits. `coded`: it
/// has coefficients; without them only subblock-predicted macroblocks
/// filter their inner edges. As in ffmpeg the level is clamped once, after
/// the segment and the deltas.
fn mb_filter(h: &FrameHeader, st: &State, mb: &MbInfo, coded: bool) -> MbFilter {
    let seg = &st.segmentation;
    let mut level = h.filter_level;
    if seg.enabled {
        level = seg.filter_level[mb.segment as usize] + if seg.absolute { 0 } else { h.filter_level };
    }
    let d = &st.deltas;
    if d.enabled {
        level += d.ref_frame[mb.ref_frame as usize];
        level += match mb.y_mode {
            B_PRED => d.mode[0],
            ZEROMV => d.mode[1],
            NEARESTMV | NEARMV | NEWMV => d.mode[2],
            SPLITMV => d.mode[3],
            _ => 0,
        };
    }
    let inner = coded || mb.y_mode == B_PRED || mb.y_mode == SPLITMV;
    MbFilter::new(level, h.sharpness, h.tag.key_frame, inner)
}

/// The edges of a macroblock's 16x16 luma block: above-left, the row above
/// and the four samples beyond it (the last sample repeated at the right
/// edge of the picture), and the column to the left.
fn luma_edges(pic: &Picture, mb_x: usize, mb_y: usize, mb_w: usize) -> ([u8; 21], [u8; 16]) {
    let stride = pic.width;
    let (x0, y0) = (mb_x * 16, mb_y * 16);
    let mut above = [127u8; 21];
    let mut left = [129u8; 16];
    if mb_y > 0 {
        let row = &pic.y[(y0 - 1) * stride..y0 * stride];
        above[0] = if mb_x > 0 { row[x0 - 1] } else { 129 };
        above[1..17].copy_from_slice(&row[x0..x0 + 16]);
        if mb_x + 1 < mb_w {
            above[17..21].copy_from_slice(&row[x0 + 16..x0 + 20]);
        } else {
            above[17..21].fill(row[x0 + 15]);
        }
    }
    if mb_x > 0 {
        for (l, &v) in left.iter_mut().zip(pic.y[y0 * stride + x0 - 1..].iter().step_by(stride)) {
            *l = v;
        }
    }
    (above, left)
}

/// The edges of a macroblock's 8x8 block in one chroma plane.
fn chroma_edges(plane: &[u8], stride: usize, mb_x: usize, mb_y: usize) -> ([u8; 9], [u8; 8]) {
    let (x0, y0) = (mb_x * 8, mb_y * 8);
    let mut above = [127u8; 9];
    let mut left = [129u8; 8];
    if mb_y > 0 {
        let row = &plane[(y0 - 1) * stride..y0 * stride];
        above[0] = if mb_x > 0 { row[x0 - 1] } else { 129 };
        above[1..9].copy_from_slice(&row[x0..x0 + 8]);
    }
    if mb_x > 0 {
        for (l, &v) in left.iter_mut().zip(plane[y0 * stride + x0 - 1..].iter().step_by(stride)) {
            *l = v;
        }
    }
    (above, left)
}

/// Intra prediction of a macroblock; `B_PRED` subblocks are reconstructed
/// one by one (each predicts from the ones before), so their residual is
/// added here.
fn predict_intra(pic: &mut Picture, mb: &MbInfo, mb_x: usize, mb_y: usize, mb_w: usize, coeffs: &mut Coeffs) {
    let stride = pic.width;
    let y0 = mb_y * 16 * stride + mb_x * 16;
    let (above, left) = luma_edges(pic, mb_x, mb_y, mb_w);
    if mb.y_mode == B_PRED {
        for b in 0..16 {
            let (bx, by) = (b & 3, b >> 2);
            let at = y0 + by * 4 * stride + bx * 4;
            let mut e = [0u8; 9];
            if by == 0 {
                e.copy_from_slice(&above[4 * bx..4 * bx + 9]);
            } else {
                let row = at - stride;
                e[0] = if bx == 0 { left[4 * by - 1] } else { pic.y[row - 1] };
                e[1..5].copy_from_slice(&pic.y[row..row + 4]);
                // the rightmost column takes the row above the macroblock,
                // whatever the subblock row
                if bx == 3 {
                    e[5..9].copy_from_slice(&above[17..21]);
                } else {
                    e[5..9].copy_from_slice(&pic.y[row + 4..row + 8]);
                }
            }
            let mut l = [0u8; 4];
            for (j, v) in l.iter_mut().enumerate() {
                *v = if bx == 0 { left[4 * by + j] } else { pic.y[at + j * stride - 1] };
            }
            intra::predict_subblock(mb.bmodes[b], &e, &l, &mut pic.y[at..], stride);
            add_block(&mut coeffs.blocks[b], coeffs.eob[b], &mut pic.y, at, stride);
        }
    } else {
        intra::predict_block::<16>(mb.y_mode, &above, &left, mb_y > 0, mb_x > 0, &mut pic.y[y0..], stride);
    }
    let uv_stride = stride / 2;
    let c0 = mb_y * 8 * uv_stride + mb_x * 8;
    for plane in [&mut pic.u, &mut pic.v] {
        let (above, left) = chroma_edges(plane, uv_stride, mb_x, mb_y);
        intra::predict_block::<8>(mb.uv_mode, &above, &left, mb_y > 0, mb_x > 0, &mut plane[c0..], uv_stride);
    }
}

/// The chroma vector of a 4x4 split: the rounded average of its four luma
/// vectors (summed in 16 bits as ffmpeg does).
fn chroma_average(mvs: &[Mv; 16], b: usize) -> Mv {
    let avg = |v: [i16; 4]| {
        let s = v.iter().fold(0i16, |a, &x| a.wrapping_add(x)) as i32;
        ((s + 2 + (s >> 15)) >> 2) as i16
    };
    let (a, c, d, e) = (mvs[b], mvs[b + 1], mvs[b + 4], mvs[b + 5]);
    Mv { x: avg([a.x, c.x, d.x, e.x]), y: avg([a.y, c.y, d.y, e.y]) }
}

/// A block of a picture: position and size in samples of its plane.
type Block = (usize, usize, usize, usize);

/// 18: predict a luma block from `r` displaced by `mv` (quarter samples).
fn predict_luma(pic: &mut Picture, r: &Picture, block: Block, mv: Mv, filter: Filter) {
    let (x, y, w, h) = block;
    let at = y * pic.width + x;
    let (sx, sy) = (x as i32 + (mv.x as i32 >> 2), y as i32 + (mv.y as i32 >> 2));
    inter::predict(&r.y, r.width, r.width, r.height, sx, sy, (mv.x as usize & 3) * 2, (mv.y as usize & 3) * 2, w, h, filter, &mut pic.y[at..], pic.width);
}

/// 18: predict a block of both chroma planes from `r` displaced by `mv`
/// (eighth samples; whole samples in version 3).
fn predict_chroma(pic: &mut Picture, r: &Picture, block: Block, mv: Mv, filter: Filter, full_pixel: bool) {
    let (x, y, w, h) = block;
    let mv = if full_pixel { Mv { x: mv.x & !7, y: mv.y & !7 } } else { mv };
    let (pw, ph) = (r.width / 2, r.height / 2);
    let at = y * pw + x;
    let (sx, sy) = (x as i32 + (mv.x as i32 >> 3), y as i32 + (mv.y as i32 >> 3));
    let (fx, fy) = (mv.x as usize & 7, mv.y as usize & 7);
    inter::predict(&r.u, pw, pw, ph, sx, sy, fx, fy, w, h, filter, &mut pic.u[at..], pw);
    inter::predict(&r.v, pw, pw, ph, sx, sy, fx, fy, w, h, filter, &mut pic.v[at..], pw);
}

/// 18: inter prediction of a macroblock from reference `r`. A luma vector
/// in quarter samples is the chroma vector in eighth samples; a 4x4 split
/// gives each 4x4 chroma block the average of its four luma vectors.
fn predict_inter(pic: &mut Picture, r: &Picture, mb: &MbInfo, mb_x: usize, mb_y: usize, filter: Filter, full_pixel: bool) {
    let (x0, y0) = (mb_x * 16, mb_y * 16);
    // the parts (in the macroblock) with the subblock whose vector they use
    let parts: &[(Block, usize)] = match (mb.y_mode, mb.split) {
        (SPLITMV, SPLIT_16X8) => &[((0, 0, 16, 8), 0), ((0, 8, 16, 8), 8)],
        (SPLITMV, SPLIT_8X16) => &[((0, 0, 8, 16), 0), ((8, 0, 8, 16), 2)],
        (SPLITMV, SPLIT_8X8) => &[((0, 0, 8, 8), 0), ((8, 0, 8, 8), 2), ((0, 8, 8, 8), 8), ((8, 8, 8, 8), 10)],
        (SPLITMV, _) => {
            for b in 0..16 {
                predict_luma(pic, r, (x0 + (b & 3) * 4, y0 + (b >> 2) * 4, 4, 4), mb.mvs[b], filter);
            }
            for b in [0, 2, 8, 10] {
                predict_chroma(pic, r, (x0 / 2 + (b & 3) * 2, y0 / 2 + (b >> 2) * 2, 4, 4), chroma_average(&mb.mvs, b), filter, full_pixel);
            }
            return;
        }
        _ => &[((0, 0, 16, 16), 0)],
    };
    for &((x, y, w, h), b) in parts {
        predict_luma(pic, r, (x0 + x, y0 + y, w, h), mb.mvs[b], filter);
        predict_chroma(pic, r, ((x0 + x) / 2, (y0 + y) / 2, w / 2, h / 2), mb.mvs[b], filter, full_pixel);
    }
}

/// Add one block's residual to the prediction at `plane[at]` and clear the
/// block.
#[inline]
fn add_block(block: &mut [i16; 16], eob: u8, plane: &mut [u8], at: usize, stride: usize) {
    if eob > 1 {
        transform::idct_add(block, &mut plane[at..], stride);
        *block = [0; 16];
    } else if block[0] != 0 {
        transform::idct_dc_add(block[0], &mut plane[at..], stride);
        block[0] = 0;
    }
}

/// Add a macroblock's residual (for `B_PRED` only the chroma: the luma went
/// in with the prediction), leaving `coeffs` cleared.
fn add_residual(pic: &mut Picture, mb: &MbInfo, mb_x: usize, mb_y: usize, coeffs: &mut Coeffs) {
    let stride = pic.width;
    if mb.y_mode != B_PRED {
        if mb.y_mode != SPLITMV && coeffs.eob[Y2_BLOCK] > 0 {
            let mut dc = [0i16; 16];
            transform::iwht(&coeffs.blocks[Y2_BLOCK], &mut dc);
            coeffs.blocks[Y2_BLOCK] = [0; 16];
            for (block, &v) in coeffs.blocks.iter_mut().zip(&dc) {
                block[0] = v;
            }
        }
        let y0 = mb_y * 16 * stride + mb_x * 16;
        for b in 0..16 {
            add_block(&mut coeffs.blocks[b], coeffs.eob[b], &mut pic.y, y0 + (b >> 2) * 4 * stride + (b & 3) * 4, stride);
        }
    }
    let uv_stride = stride / 2;
    let c0 = mb_y * 8 * uv_stride + mb_x * 8;
    for (plane, first) in [(&mut pic.u, 16), (&mut pic.v, 20)] {
        for k in 0..4 {
            add_block(&mut coeffs.blocks[first + k], coeffs.eob[first + k], plane, c0 + (k >> 1) * 4 * uv_stride + (k & 1) * 4, uv_stride);
        }
    }
}

/// Fill a macroblock the frame's data did not reach with the co-located
/// samples of the last frame (mid grey when there is none).
fn conceal(pic: &mut Picture, last: Option<&Picture>, mb_x: usize, mb_y: usize) {
    let stride = pic.width;
    for (plane, src, size, s) in [(&mut pic.y, last.map(|l| &l.y), 16, stride), (&mut pic.u, last.map(|l| &l.u), 8, stride / 2), (&mut pic.v, last.map(|l| &l.v), 8, stride / 2)] {
        for j in 0..size {
            let at = (mb_y * size + j) * s + mb_x * size;
            match src {
                Some(src) => plane[at..at + size].copy_from_slice(&src[at..at + size]),
                None => plane[at..at + size].fill(128),
            }
        }
    }
}
