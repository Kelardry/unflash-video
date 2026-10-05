//! The decoder: container samples in, shown frames out. A sample holds one
//! frame or a superframe (Annex B); frames are decoded as the
//! specification's frame() (6.1), which ends with the probability
//! adaptation, the reference updates (8.10) and the output (8.9).

use std::rc::Rc;

use crate::booldec::BoolDecoder;
use crate::frame::{FrameBuf, Pixel};
use crate::header::*;
use crate::loopfilter::{filter_frame, Levels};
use crate::probs::{adapt_coef_probs, adapt_noncoef_probs, Counts};
use crate::tables::{AC_QLOOKUP, DC_QLOOKUP};
use crate::tile::{AboveCtx, LeftCtx, MiInfo, PrevMv, RefFrame, Scratch, TileDecoder};
use crate::{Error, Result};

/// A decoded frame, as shown.
pub struct Frame {
    /// The frame size (the render size is informational only).
    pub width: u32,
    pub height: u32,
    /// 8-bit 4:2:0 planes, tightly packed; chroma is (w + 1) / 2 x (h + 1) / 2.
    /// Deeper samples are rounded: (v + (1 << (bd - 9))) >> (bd - 8).
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub bit_depth: u8,
    /// The planes at full precision, when `bit_depth` is above 8 and the
    /// decoder was asked for them ([`Decoder::set_keep_deep`]).
    pub y16: Option<Vec<u16>>,
    pub u16: Option<Vec<u16>>,
    pub v16: Option<Vec<u16>>,
    /// The timestamp of the sample the frame was shown by.
    pub pts: f64,
    /// Some of it could not be decoded, or it was predicted from such a frame.
    pub damaged: bool,
    /// The colour matrix is BT.709 (color_space; streams that leave it
    /// unknown are taken as BT.709 above 576 lines, as players do).
    pub bt709: bool,
    /// Full-range samples (color_range).
    pub full_range: bool,
}

/// A reference slot's frame, at its bit depth.
#[derive(Clone)]
enum Stored {
    Low(Rc<FrameBuf<u8>>),
    High(Rc<FrameBuf<u16>>),
}

impl Stored {
    fn size(&self) -> (u32, u32) {
        match self {
            Stored::Low(f) => (f.width, f.height),
            Stored::High(f) => (f.width, f.height),
        }
    }

    fn to_frame(&self, pts: f64, keep_deep: bool) -> Frame {
        match self {
            Stored::Low(f) => frame_from_low(f, pts),
            Stored::High(f) => frame_from_high(f, pts, keep_deep),
        }
    }
}

/// What differs between the 8-bit and the 10/12-bit decoding paths.
trait Depth: Pixel {
    fn wrap(f: Rc<FrameBuf<Self>>) -> Stored;
    fn unwrap(s: &Stored) -> Option<&Rc<FrameBuf<Self>>>;
    fn pool(d: &mut Decoder) -> &mut Vec<FrameBuf<Self>>;
    fn scratch(d: &mut Decoder) -> &mut Option<Box<Scratch<Self>>>;
}

impl Depth for u8 {
    fn wrap(f: Rc<FrameBuf<u8>>) -> Stored {
        Stored::Low(f)
    }
    fn unwrap(s: &Stored) -> Option<&Rc<FrameBuf<u8>>> {
        match s {
            Stored::Low(f) => Some(f),
            Stored::High(_) => None,
        }
    }
    fn pool(d: &mut Decoder) -> &mut Vec<FrameBuf<u8>> {
        &mut d.pool8
    }
    fn scratch(d: &mut Decoder) -> &mut Option<Box<Scratch<u8>>> {
        &mut d.scratch8
    }
}

impl Depth for u16 {
    fn wrap(f: Rc<FrameBuf<u16>>) -> Stored {
        Stored::High(f)
    }
    fn unwrap(s: &Stored) -> Option<&Rc<FrameBuf<u16>>> {
        match s {
            Stored::High(f) => Some(f),
            Stored::Low(_) => None,
        }
    }
    fn pool(d: &mut Decoder) -> &mut Vec<FrameBuf<u16>> {
        &mut d.pool16
    }
    fn scratch(d: &mut Decoder) -> &mut Option<Box<Scratch<u16>>> {
        &mut d.scratch16
    }
}

pub struct Decoder {
    state: StreamState,
    slots: [Option<Stored>; 8],
    /// The mode info of the frame being decoded.
    mi: Vec<MiInfo>,
    /// The motion of the previous decoded frame (for UsePrevFrameMvs).
    prev_mvs: Vec<PrevMv>,
    /// The segmentation map of earlier frames (PrevSegmentIds).
    prev_segment_ids: Vec<u8>,
    above: AboveCtx,
    counts: Box<Counts>,
    scratch8: Option<Box<Scratch<u8>>>,
    scratch16: Option<Box<Scratch<u16>>>,
    /// Frame buffers no reference slot holds any more, for reuse.
    pool8: Vec<FrameBuf<u8>>,
    pool16: Vec<FrameBuf<u16>>,
    /// The size and show_frame of the last decoded frame (compute_image_size).
    last_size: Option<(u32, u32)>,
    last_show_frame: bool,
    /// Deeper frames come out at full precision too (`set_keep_deep`).
    keep_deep: bool,
}

impl Decoder {
    /// `config`: the vpcC record from an MP4 track or the Matroska
    /// CodecPrivate (may be empty: VP9 needs no configuration). Only used to
    /// turn away the profiles this decoder does not implement early.
    pub fn new(config: &[u8]) -> Result<Decoder> {
        check_config(config)?;
        Ok(Decoder {
            state: StreamState::default(),
            slots: Default::default(),
            mi: Vec::new(),
            prev_mvs: Vec::new(),
            prev_segment_ids: Vec::new(),
            above: AboveCtx::default(),
            counts: Box::default(),
            scratch8: None,
            scratch16: None,
            pool8: Vec::new(),
            pool16: Vec::new(),
            last_size: None,
            last_show_frame: false,
            keep_deep: false,
        })
    }

    /// Give 10 and 12-bit frames at full precision too ([`Frame::y16`] and
    /// the others) besides the rounded 8-bit planes every frame has: for
    /// comparing with another decoder at that depth, as the tests do. Off
    /// by default (they stay None), since the app reads only the 8-bit
    /// planes, and the copy is about 6 MB a frame at 10-bit 1080p.
    pub fn set_keep_deep(&mut self, keep: bool) {
        self.keep_deep = keep;
    }

    /// Decode one container sample (a frame, or a superframe holding several
    /// frames with its index). Returns the frames it shows (show_frame, or
    /// show_existing_frame), each carrying the sample's pts; hidden frames
    /// (alt-ref) yield nothing by themselves.
    pub fn decode(&mut self, sample: &[u8], pts: f64) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        for frame in split_superframe(sample) {
            let mut data = frame;
            // a frame that only shows another one may be followed by more
            // frames without a superframe index (libvpx decodes those too);
            // a sample shows no more frames than a superframe can hold (each
            // a copy of a whole frame, from as little as one byte)
            while !data.is_empty() && out.len() < 8 {
                let used = self.decode_frame(data, pts, &mut out)?;
                data = &data[used.min(data.len())..];
                while let Some((&0, rest)) = data.split_first() {
                    data = rest;
                }
            }
        }
        Ok(out)
    }

    /// Nothing is held back (VP9 frames come out in decoding order).
    pub fn flush(&mut self) -> Result<Vec<Frame>> {
        Ok(Vec::new())
    }

    /// Decode one frame; returns the bytes it took.
    fn decode_frame(&mut self, data: &[u8], pts: f64, out: &mut Vec<Frame>) -> Result<usize> {
        let ref_sizes: [Option<(u32, u32)>; 8] = std::array::from_fn(|i| self.slots[i].as_ref().map(|s| s.size()));
        let fh = parse_uncompressed_header(data, &mut self.state, &ref_sizes)?;
        if fh.show_existing_frame {
            let slot = self.slots[fh.frame_to_show].as_ref().ok_or(Error::Bitstream("frame to show is missing"))?;
            out.push(slot.to_frame(pts, self.keep_deep));
            return Ok(fh.uncompressed_size);
        }
        let end = fh.uncompressed_size + fh.compressed_size;
        if end > data.len() {
            return Err(Error::Bitstream("frame shorter than its headers"));
        }
        if fh.bit_depth == 8 {
            self.decode_frame_t::<u8>(fh, data, pts, out)?;
        } else {
            self.decode_frame_t::<u16>(fh, data, pts, out)?;
        }
        Ok(data.len())
    }

    fn decode_frame_t<T: Depth>(&mut self, mut fh: FrameHeader, data: &[u8], pts: f64, out: &mut Vec<Frame>) -> Result<()> {
        let (w, h) = (fh.width, fh.height);
        let (mi_cols, mi_rows) = (fh.mi_cols(), fh.mi_rows());
        let bd = fh.bit_depth as u32;

        // the references, with their scale to this frame (8.5.2.3)
        let mut refs: [Option<Rc<FrameBuf<T>>>; 3] = [None, None, None];
        if !fh.is_intra() {
            for (i, r) in refs.iter_mut().enumerate() {
                let slot = self.slots[fh.ref_frame_idx[i]].as_ref().ok_or(Error::Bitstream("reference frame missing"))?;
                let f = T::unwrap(slot).ok_or(Error::Bitstream("reference frame of another bit depth"))?;
                *r = Some(f.clone());
            }
        }

        // compute_image_size (7.2.6)
        let size_changed = self.last_size != Some((w, h));
        let use_prev_mvs = !size_changed && self.last_show_frame && !fh.error_resilient_mode && !fh.is_intra();
        let n_mi = mi_cols * mi_rows;
        // (the map can have another size even when the last decoded frame
        // had this one: a frame of another size that then failed in its
        // compressed header resized it)
        if size_changed || fh.reset_past || self.prev_segment_ids.len() != n_mi {
            self.prev_segment_ids.clear();
            self.prev_segment_ids.resize(n_mi, 0);
        }
        // every block writes all the cells it covers before any are read
        // (except in a tile that could not be decoded, which keeps the last
        // frame's records), so the array is not cleared between frames
        self.mi.resize(n_mi, MiInfo::default());

        let mut fc = self.state.contexts[fh.frame_context_idx].clone();
        let header = &data[fh.uncompressed_size..fh.uncompressed_size + fh.compressed_size];
        parse_compressed_header(header, &mut fh, &mut fc)?;

        let mut cur = match T::pool(self).iter().position(|f| f.fits(w, h, fh.bit_depth)) {
            Some(i) => T::pool(self).swap_remove(i),
            None => FrameBuf::new(w, h, fh.bit_depth),
        };
        cur.color_space = fh.color_space;
        cur.color_range = fh.color_range;
        cur.damaged = refs.iter().flatten().any(|r| r.damaged);
        let mut scratch = T::scratch(self).take().unwrap_or_default();

        let adapt = !fh.error_resilient_mode && !fh.frame_parallel_decoding_mode;
        if adapt {
            self.counts.clear();
        }
        let dequant = dequant_tables(&fh);
        self.above.reset(fh.sb64_cols());

        let ref_frames: [Option<RefFrame<T>>; 3] = std::array::from_fn(|i| {
            refs[i].as_deref().map(|buf| {
                let x_scale = ((buf.width as i64) << 14) / w as i64;
                let y_scale = ((buf.height as i64) << 14) / h as i64;
                RefFrame {
                    buf,
                    x_scale: x_scale as i32,
                    y_scale: y_scale as i32,
                    x_step: ((16 * x_scale) >> 14) as i32,
                    y_step: ((16 * y_scale) >> 14) as i32,
                    scaled: buf.width != w || buf.height != h,
                    valid: 2 * w >= buf.width && 2 * h >= buf.height && w <= 16 * buf.width && h <= 16 * buf.height,
                }
            })
        });

        // decode_tiles (6.4)
        let tile_cols = 1usize << fh.tile_cols_log2;
        let tile_rows = 1usize << fh.tile_rows_log2;
        let mut rest = &data[fh.uncompressed_size + fh.compressed_size..];
        let mut damaged = false;
        for tile_row in 0..tile_rows {
            for tile_col in 0..tile_cols {
                let last = tile_row == tile_rows - 1 && tile_col == tile_cols - 1;
                let tile = if last {
                    std::mem::take(&mut rest)
                } else if rest.len() >= 4 {
                    let size = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
                    let size = size.min(rest.len() - 4);
                    let t = &rest[4..4 + size];
                    rest = &rest[4 + size..];
                    t
                } else {
                    std::mem::take(&mut rest)
                };
                let bounds = (tile_offset(tile_row, mi_rows, fh.tile_rows_log2), tile_offset(tile_row + 1, mi_rows, fh.tile_rows_log2), tile_offset(tile_col, mi_cols, fh.tile_cols_log2), tile_offset(tile_col + 1, mi_cols, fh.tile_cols_log2));
                let Ok(bd_tile) = BoolDecoder::new(tile) else {
                    damaged = true;
                    conceal(&mut cur, refs[0].as_deref(), bounds, bd);
                    continue;
                };
                let mut td = TileDecoder {
                    fh: &fh,
                    fc: &fc,
                    counts: if adapt { Some(&mut *self.counts) } else { None },
                    bd: bd_tile,
                    cur: &mut cur,
                    refs: [None, None, None],
                    mi: &mut self.mi,
                    mi_cols,
                    mi_rows,
                    prev_mvs: if use_prev_mvs && self.prev_mvs.len() == n_mi { Some(&self.prev_mvs) } else { None },
                    prev_segment_ids: &self.prev_segment_ids,
                    above: &mut self.above,
                    left: LeftCtx::default(),
                    mi_row_start: bounds.0,
                    mi_row_end: bounds.1,
                    mi_col_start: bounds.2,
                    mi_col_end: bounds.3,
                    scratch: &mut scratch,
                    dequant,
                    bit_depth: bd,
                    damaged: false,
                };
                for (i, r) in ref_frames.iter().enumerate() {
                    td.refs[i] = r.as_ref().map(|r| RefFrame { ..*r });
                }
                td.decode();
                damaged |= td.damaged;
            }
        }
        cur.damaged |= damaged;

        if fh.lf.level != 0 {
            filter_frame(&mut cur, &self.mi, mi_rows, mi_cols, &Levels::new(&fh));
        }

        // refresh_probs (6.1.2)
        if adapt {
            let pre = &self.state.contexts[fh.frame_context_idx];
            let update_factor = if fh.is_intra() || !fh.after_key_frame { 112 } else { 128 };
            adapt_coef_probs(&mut fc, pre, &self.counts, update_factor);
            if !fh.is_intra() {
                adapt_noncoef_probs(&mut fc, pre, &self.counts, fh.interp_filter == SWITCHABLE, fh.tx_mode == TX_MODE_SELECT, fh.allow_high_precision_mv);
            }
        }
        if fh.refresh_frame_context {
            self.state.contexts[fh.frame_context_idx] = fc;
        }

        // what the next frame predicts from
        if fh.seg.enabled && fh.seg.update_map {
            for (s, m) in self.prev_segment_ids.iter_mut().zip(self.mi.iter()) {
                *s = m.segment_id;
            }
        }
        self.prev_mvs.clear();
        self.prev_mvs.extend(self.mi.iter().map(|m| PrevMv { ref_frame: m.ref_frame, mv: [m.mv[0][3], m.mv[1][3]] }));
        self.last_size = Some((w, h));
        self.last_show_frame = fh.show_frame;
        *T::scratch(self) = Some(scratch);
        drop(refs);

        // the reference update process (8.10)
        let cur = Rc::new(cur);
        for i in 0..8 {
            if fh.refresh_frame_flags & (1 << i) != 0 {
                let old = self.slots[i].replace(T::wrap(cur.clone()));
                self.recycle(old);
            }
        }
        if fh.show_frame {
            out.push(T::wrap(cur.clone()).to_frame(pts, self.keep_deep));
        }
        if let Ok(f) = Rc::try_unwrap(cur) {
            T::pool(self).push(f);
        }
        Ok(())
    }

    /// Keep the buffer of a frame no slot refers to any more.
    fn recycle(&mut self, old: Option<Stored>) {
        match old {
            Some(Stored::Low(f)) => {
                if let Ok(f) = Rc::try_unwrap(f) {
                    self.pool8.push(f);
                }
            }
            Some(Stored::High(f)) => {
                if let Ok(f) = Rc::try_unwrap(f) {
                    self.pool16.push(f);
                }
            }
            None => {}
        }
        self.pool8.truncate(4);
        self.pool16.truncate(4);
    }
}

/// Reject profiles 1 and 3 from a vpcC record (full box: version and flags
/// first) or a Matroska CodecPrivate (ID, length, value triples); anything
/// else is ignored, as VP9 frames carry their own setup.
fn check_config(config: &[u8]) -> Result<()> {
    let unsupported = |profile: u8, chroma: u8| profile == 1 || profile == 3 || chroma >= 2;
    if config.len() >= 8 && config[0] <= 1 && config[1..4] == [0, 0, 0] {
        if unsupported(config[4], (config[6] >> 1) & 7) {
            return Err(Error::Unsupported("profiles 1 and 3 (4:4:4, 4:2:2 and 4:4:0 chroma)"));
        }
        return Ok(());
    }
    let (mut profile, mut chroma) = (0, 0);
    let mut p = config;
    while let [id, len, rest @ ..] = p {
        let len = *len as usize;
        if len != 1 || rest.is_empty() {
            return Ok(());
        }
        match id {
            1 => profile = rest[0],
            4 => chroma = rest[0],
            2 | 3 => {}
            _ => return Ok(()),
        }
        p = &rest[1..];
    }
    if unsupported(profile, chroma) {
        return Err(Error::Unsupported("profiles 1 and 3 (4:4:4, 4:2:2 and 4:4:0 chroma)"));
    }
    Ok(())
}

/// The frames of a sample: the frames of its superframe index (B.4), or the
/// whole sample.
fn split_superframe(data: &[u8]) -> Vec<&[u8]> {
    if let Some(&marker) = data.last() {
        if marker & 0xe0 == 0xc0 {
            let frames = (marker & 7) as usize + 1;
            let mag = ((marker >> 3) & 3) as usize + 1;
            let index = 2 + mag * frames;
            if data.len() >= index && data[data.len() - index] == marker {
                let mut sizes = &data[data.len() - index + 1..];
                let mut out = Vec::with_capacity(frames);
                let mut start = 0usize;
                let limit = data.len() - index;
                for _ in 0..frames {
                    let mut size = 0usize;
                    for (j, &b) in sizes[..mag].iter().enumerate() {
                        size |= (b as usize) << (8 * j);
                    }
                    sizes = &sizes[mag..];
                    // (saturating: on 32-bit targets the sum can wrap)
                    let end = start.saturating_add(size).min(limit);
                    if end > start {
                        out.push(&data[start..end]);
                    }
                    start = end;
                }
                return out;
            }
        }
    }
    vec![data]
}

/// get_tile_offset (6.4.1).
fn tile_offset(n: usize, mis: usize, log2: u32) -> usize {
    let sbs = (mis + 7) >> 3;
    (((n * sbs) >> log2) << 3).min(mis)
}

/// The dequantisation factors of each segment (8.6.1): [plane > 0][dc, ac].
fn dequant_tables(fh: &FrameHeader) -> [[[i32; 2]; 2]; 8] {
    let depth = ((fh.bit_depth - 8) >> 1) as usize;
    let q = |base: i32, delta: i8, table: &[[i32; 256]; 3]| table[depth][(base + delta as i32).clamp(0, 255) as usize];
    std::array::from_fn(|seg| {
        let mut qindex = fh.base_q_idx as i32;
        if fh.seg.active(seg as u8, SEG_LVL_ALT_Q) {
            let data = fh.seg.data(seg as u8, SEG_LVL_ALT_Q);
            qindex = if fh.seg.abs_delta { data } else { qindex + data }.clamp(0, 255);
        }
        [[q(qindex, fh.delta_q_y_dc, &DC_QLOOKUP), q(qindex, 0, &AC_QLOOKUP)], [q(qindex, fh.delta_q_uv_dc, &DC_QLOOKUP), q(qindex, fh.delta_q_uv_ac, &AC_QLOOKUP)]]
    })
}

/// Fill the area of a tile that could not be decoded with the co-located
/// samples of the last frame (or mid grey).
fn conceal<T: Pixel>(cur: &mut FrameBuf<T>, last: Option<&FrameBuf<T>>, (r0, r1, c0, c1): (usize, usize, usize, usize), bd: u32) {
    let last = last.filter(|l| l.width == cur.width && l.height == cur.height);
    for (p, plane) in cur.planes.iter_mut().enumerate() {
        let s = (p > 0) as usize;
        let (x0, x1) = ((c0 * 8) >> s, (c1 * 8) >> s);
        for y in (r0 * 8) >> s..(r1 * 8) >> s {
            let row = &mut plane.data[y * plane.stride + x0..y * plane.stride + x1];
            match last {
                Some(l) => {
                    let lp = &l.planes[p];
                    row.copy_from_slice(&lp.data[y * lp.stride + x0..y * lp.stride + x1]);
                }
                None => row.fill(T::new(1 << (bd - 1))),
            }
        }
    }
}

fn is_bt709(color_space: u8, height: u32) -> bool {
    match color_space {
        CS_BT_709 | CS_SMPTE_240 | CS_BT_2020 => true,
        CS_UNKNOWN => height > 576,
        _ => false,
    }
}

/// The visible part of a plane, tightly packed.
fn visible<T: Copy>(p: &crate::frame::Plane<T>) -> Vec<T> {
    let mut out = Vec::with_capacity(p.width * p.height);
    for row in p.data.chunks(p.stride).take(p.height) {
        out.extend_from_slice(&row[..p.width]);
    }
    out
}

fn frame_from_low(f: &FrameBuf<u8>, pts: f64) -> Frame {
    Frame {
        width: f.width,
        height: f.height,
        y: visible(&f.planes[0]),
        u: visible(&f.planes[1]),
        v: visible(&f.planes[2]),
        bit_depth: 8,
        y16: None,
        u16: None,
        v16: None,
        pts,
        damaged: f.damaged,
        bt709: is_bt709(f.color_space, f.height),
        full_range: f.color_range,
    }
}

/// The visible part of a deeper plane, tightly packed and rounded to 8 bits.
fn visible_rounded(p: &crate::frame::Plane<u16>, bit_depth: u8) -> Vec<u8> {
    let shift = bit_depth as u32 - 8;
    let mut out = Vec::with_capacity(p.width * p.height);
    for row in p.data.chunks(p.stride).take(p.height) {
        out.extend(row[..p.width].iter().map(|&s| ((s as u32 + (1 << (shift - 1))) >> shift).min(255) as u8));
    }
    out
}

/// A deeper frame: its planes rounded straight from the frame's rows, and
/// copied out at full precision as well only when `keep_deep` asks.
fn frame_from_high(f: &FrameBuf<u16>, pts: f64, keep_deep: bool) -> Frame {
    let deep = |p| keep_deep.then(|| visible(p));
    Frame {
        width: f.width,
        height: f.height,
        y: visible_rounded(&f.planes[0], f.bit_depth),
        u: visible_rounded(&f.planes[1], f.bit_depth),
        v: visible_rounded(&f.planes[2], f.bit_depth),
        bit_depth: f.bit_depth,
        y16: deep(&f.planes[0]),
        u16: deep(&f.planes[1]),
        v16: deep(&f.planes[2]),
        pts,
        damaged: f.damaged,
        bt709: is_bt709(f.color_space, f.height),
        full_range: f.color_range,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superframe_index() {
        // two frames of 3 and 2 bytes, one-byte sizes: marker 0b110_00_001
        let data = [1, 2, 3, 4, 5, 0xc1, 3, 2, 0xc1];
        let frames = split_superframe(&data);
        assert_eq!(frames, vec![&[1u8, 2, 3][..], &[4, 5][..]]);
        // a marker without its twin at the start of the index: one frame
        let data = [1, 2, 3, 0xc1];
        assert_eq!(split_superframe(&data), vec![&data[..]]);
        // four-byte sizes (marker 0b110_11_001), the second 2^32 - 1: cut
        // to the data (on 32-bit targets the end wrapped around and the
        // frame was lost, or a debug build panicked; not so on 64-bit ones)
        let data = [1, 2, 3, 0xd9, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xd9];
        assert_eq!(split_superframe(&data), vec![&[1u8][..], &[2, 3][..]]);
    }

    #[test]
    fn configs() {
        assert!(Decoder::new(&[]).is_ok());
        // vpcC: version 1, profile 0, level 10, 8-bit 4:2:0
        assert!(Decoder::new(&[1, 0, 0, 0, 0, 10, 0x82, 2, 2, 2, 0, 0]).is_ok());
        assert!(Decoder::new(&[1, 0, 0, 0, 1, 10, 0x86, 2, 2, 2, 0, 0]).is_err());
        // Matroska CodecPrivate: profile 2, bit depth 10
        assert!(Decoder::new(&[1, 1, 2, 3, 1, 10]).is_ok());
        assert!(Decoder::new(&[1, 1, 3]).is_err());
    }
}
