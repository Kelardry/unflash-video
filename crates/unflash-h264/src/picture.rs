//! Decoded pictures, the decoded picture buffer, picture order counts,
//! reference picture lists and reference marking (8.2.1, 8.2.4, 8.2.5),
//! for frames and for field pictures.

use std::rc::Rc;

use crate::ps::Sps;
use crate::slice::{Mmco, RefListMod, SliceHeader, SliceType};
use crate::{Error, Result};

/// Picture structures: a top field, a bottom field, or a frame (both).
pub const TOP: u8 = 1;
pub const BOTTOM: u8 = 2;
pub const FRAME: u8 = 3;

/// A decoded frame with the motion data later pictures may refer to. A
/// frame coded as two field pictures shares this buffer between them.
#[derive(Clone)]
pub struct Picture {
    /// Unique within a decoder instance.
    pub id: u32,
    /// Luma size in samples (multiples of 16).
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// TopFieldOrderCnt / BottomFieldOrderCnt (`i32::MAX` while that field
    /// is not decoded) and PicOrderCnt (their minimum).
    pub poc_top: i32,
    pub poc_bot: i32,
    pub poc: i32,
    pub frame_num: u32,
    pub is_idr: bool,
    pub is_ref: bool,
    /// The fields holding decoded samples (TOP | BOTTOM).
    pub decoded: u8,
    /// Coded as field pictures / as an MBAFF frame.
    pub coded_fields: bool,
    pub mbaff: bool,
    /// Per 4x4 block, per list: motion vector, reference index (-1 = none)
    /// and the referenced picture as `4 * id + structure`. Blocks are in
    /// macroblock-row raster order; the macroblocks of a field picture
    /// occupy the frame's macroblock rows of their parity (row 2r + parity
    /// for field row r), so a frame coded either way has the same layout.
    pub mv: [Vec<[i16; 2]>; 2],
    pub ref_idx: [Vec<i8>; 2],
    pub ref_id: [Vec<i32>; 2],
    /// Per macroblock (the same raster): intra, and a field macroblock.
    pub mb_intra: Vec<bool>,
    pub mb_field: Vec<bool>,
    /// The caller's timestamp for this picture.
    pub pts: f64,
}

impl Picture {
    pub fn new(id: u32, width_mbs: usize, height_mbs: usize) -> Picture {
        let (w, h) = (width_mbs * 16, height_mbs * 16);
        let n4 = (w / 4) * (h / 4);
        Picture {
            id,
            width: w,
            height: h,
            y: vec![0; w * h],
            u: vec![128; w * h / 4],
            v: vec![128; w * h / 4],
            poc_top: i32::MAX,
            poc_bot: i32::MAX,
            poc: 0,
            frame_num: 0,
            is_idr: false,
            is_ref: false,
            decoded: 0,
            coded_fields: false,
            mbaff: false,
            mv: [vec![[0; 2]; n4], vec![[0; 2]; n4]],
            ref_idx: [vec![-1; n4], vec![-1; n4]],
            ref_id: [vec![-1; n4], vec![-1; n4]],
            mb_intra: vec![false; width_mbs * height_mbs],
            mb_field: vec![false; width_mbs * height_mbs],
            pts: 0.0,
        }
    }

    /// Make a reused picture buffer a fresh picture (its samples and motion
    /// are left as they were; every decoded macroblock overwrites its own).
    pub fn reset(&mut self, id: u32) {
        self.id = id;
        self.poc_top = i32::MAX;
        self.poc_bot = i32::MAX;
        self.poc = 0;
        self.frame_num = 0;
        self.is_idr = false;
        self.is_ref = false;
        self.decoded = 0;
        self.coded_fields = false;
        self.mbaff = false;
        self.pts = 0.0;
    }

    /// PicOrderCnt of one field, or of the frame.
    pub fn field_poc(&self, structure: u8) -> i32 {
        match structure {
            TOP => self.poc_top,
            BOTTOM => self.poc_bot,
            _ => self.poc,
        }
    }

    /// Record the order count of a decoded field / frame.
    pub fn set_poc(&mut self, structure: u8, top: i32, bottom: i32) {
        if structure & TOP != 0 {
            self.poc_top = top;
        }
        if structure & BOTTOM != 0 {
            self.poc_bot = bottom;
        }
        self.decoded |= structure;
        self.poc = self.poc_top.min(self.poc_bot);
    }
}

/// A reference picture as seen from the current slice: a frame, or one
/// field of a frame.
#[derive(Clone)]
pub struct RefPic {
    pub pic: Rc<Picture>,
    /// TOP, BOTTOM or FRAME.
    pub structure: u8,
    pub long_term: bool,
    /// PicOrderCnt of the field / frame.
    pub poc: i32,
    /// PicNum (short-term) or LongTermPicNum.
    pub pic_num: i32,
}

impl RefPic {
    /// Identifies the referenced field / frame in the motion field.
    pub fn key(&self) -> i32 {
        self.pic.id as i32 * 4 + self.structure as i32
    }
    fn same(&self, other: &RefPic) -> bool {
        self.pic.id == other.pic.id && self.structure == other.structure && self.long_term == other.long_term
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    Short,
    Long,
}

pub struct DpbEntry {
    pub pic: Rc<Picture>,
    pub kind: RefKind,
    pub long_term_frame_idx: u32,
    /// The fields marked as used for reference (TOP | BOTTOM).
    pub reference: u8,
    /// The frame_num and POC the picture is referenced by (a memory
    /// management control operation 5 renumbers a picture after the fact).
    pub frame_num: u32,
    pub poc: i32,
}

/// The reference pictures and the state that carries between pictures.
pub struct Dpb {
    pub entries: Vec<DpbEntry>,
    /// Pictures that left the buffer since the decoder last reclaimed them
    /// (their sample buffers are reused once nobody else holds them).
    pub graveyard: Vec<Rc<Picture>>,
    /// None: "no long-term frame indices".
    pub max_long_term_frame_idx: Option<u32>,
    prev_ref_frame_num: u32,
    prev_poc_msb: i32,
    prev_poc_lsb: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: u32,
    prev_had_mmco5: bool,
    next_id: u32,
    /// A reference picture has been marked (gaps in frame_num are relative
    /// to it; a stream that starts on a non-IDR picture has none to fill).
    started: bool,
}

impl Default for Dpb {
    fn default() -> Self {
        Self::new()
    }
}

/// The picture order counts of a picture and the state to carry forward.
#[derive(Clone, Copy, Debug)]
pub struct PocState {
    /// PicOrderCnt of the picture (the field's, or the frame's minimum).
    pub poc: i32,
    pub top: i32,
    pub bottom: i32,
    poc_msb: i32,
    poc_lsb: i32,
    frame_num_offset: i32,
}

impl Dpb {
    pub fn new() -> Dpb {
        Dpb {
            entries: Vec::new(),
            graveyard: Vec::new(),
            max_long_term_frame_idx: None,
            prev_ref_frame_num: 0,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            prev_frame_num_offset: 0,
            prev_frame_num: 0,
            prev_had_mmco5: false,
            next_id: 1,
            started: false,
        }
    }

    pub fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn clear(&mut self) {
        self.graveyard.extend(self.entries.drain(..).map(|e| e.pic));
        self.max_long_term_frame_idx = None;
    }

    /// Drop the entries matching `pred`, keeping their pictures for reuse.
    fn remove_where(&mut self, pred: impl Fn(&DpbEntry) -> bool) {
        let mut i = 0;
        while i < self.entries.len() {
            if pred(&self.entries[i]) {
                let e = self.entries.remove(i);
                self.graveyard.push(e.pic);
            } else {
                i += 1;
            }
        }
    }

    /// Unmark the fields in `mask` of the entries matching `pred`; entries
    /// with no field left are removed.
    fn unmark_where(&mut self, mask: u8, pred: impl Fn(&DpbEntry) -> bool) {
        for e in self.entries.iter_mut() {
            if pred(e) {
                e.reference &= !mask;
            }
        }
        self.remove_where(|e| e.reference == 0);
    }

    /// 8.2.1: the picture order counts of the picture a slice header starts.
    pub fn compute_poc(&self, sps: &Sps, hdr: &SliceHeader) -> PocState {
        let max_frame_num = sps.max_frame_num() as i32;
        let structure = hdr.structure();
        // FrameNumOffset (order count types 1 and 2)
        let prev_offset = if self.prev_had_mmco5 { 0 } else { self.prev_frame_num_offset };
        let frame_num_offset = if hdr.is_idr() {
            0
        } else if self.prev_frame_num > hdr.frame_num {
            prev_offset + max_frame_num
        } else {
            prev_offset
        };
        let (top, bottom, poc_msb, poc_lsb, frame_num_offset) = match sps.poc_type {
            0 => {
                let max_lsb = 1i32 << sps.log2_max_poc_lsb;
                let (prev_msb, prev_lsb) = if hdr.is_idr() { (0, 0) } else { (self.prev_poc_msb, self.prev_poc_lsb) };
                let lsb = hdr.poc_lsb as i32;
                let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                    prev_msb + max_lsb
                } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                    prev_msb - max_lsb
                } else {
                    prev_msb
                };
                let top = msb + lsb;
                let bottom = if structure == FRAME { top + hdr.delta_poc_bottom } else { top };
                (top, bottom, msb, lsb, 0)
            }
            1 => {
                let cycle = sps.offset_for_ref_frame.len() as i32;
                let mut abs_frame_num = if cycle != 0 { frame_num_offset + hdr.frame_num as i32 } else { 0 };
                if hdr.nal_ref_idc == 0 && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0i32;
                if abs_frame_num > 0 {
                    let cycle_cnt = (abs_frame_num - 1) / cycle;
                    let in_cycle = (abs_frame_num - 1) % cycle;
                    let delta_per_cycle: i32 = sps.offset_for_ref_frame.iter().sum();
                    expected = cycle_cnt * delta_per_cycle;
                    for k in 0..=in_cycle as usize {
                        expected += sps.offset_for_ref_frame[k];
                    }
                }
                if hdr.nal_ref_idc == 0 {
                    expected += sps.offset_for_non_ref_pic;
                }
                let top = expected + hdr.delta_poc[0];
                let bottom = top + sps.offset_for_top_to_bottom_field + if structure == FRAME { hdr.delta_poc[1] } else { 0 };
                (top, bottom, 0, 0, frame_num_offset)
            }
            _ => {
                let poc = if hdr.is_idr() {
                    0
                } else if hdr.nal_ref_idc == 0 {
                    2 * (frame_num_offset + hdr.frame_num as i32) - 1
                } else {
                    2 * (frame_num_offset + hdr.frame_num as i32)
                };
                (poc, poc, 0, 0, frame_num_offset)
            }
        };
        let poc = match structure {
            TOP => top,
            BOTTOM => bottom,
            _ => top.min(bottom),
        };
        PocState { poc, top, bottom, poc_msb, poc_lsb, frame_num_offset }
    }

    /// FrameNumWrap of a short-term entry relative to the current frame_num.
    fn frame_num_wrap(sps: &Sps, entry_frame_num: u32, cur_frame_num: u32) -> i32 {
        if entry_frame_num > cur_frame_num {
            entry_frame_num as i32 - sps.max_frame_num() as i32
        } else {
            entry_frame_num as i32
        }
    }

    /// The reference picture a DPB entry provides for the current picture
    /// structure: the whole frame, or one of its fields.
    fn ref_pic(sps: &Sps, e: &DpbEntry, structure: u8, cur_structure: u8, cur_frame_num: u32) -> RefPic {
        let long_term = e.kind == RefKind::Long;
        let (poc, pic_num) = if structure == FRAME {
            (e.poc, if long_term { e.long_term_frame_idx as i32 } else { Self::frame_num_wrap(sps, e.frame_num, cur_frame_num) })
        } else {
            let base = if long_term { e.long_term_frame_idx as i32 } else { Self::frame_num_wrap(sps, e.frame_num, cur_frame_num) };
            (e.pic.field_poc(structure), 2 * base + (structure == cur_structure) as i32)
        };
        RefPic { pic: e.pic.clone(), structure, long_term, poc, pic_num }
    }

    /// 8.2.4.2.5: the fields of an ordered list of frames, alternating in
    /// parity starting with the current field's.
    fn alternate_fields(sps: &Sps, frames: &[&DpbEntry], cur_structure: u8, cur_frame_num: u32) -> Vec<RefPic> {
        let mut out = Vec::new();
        let mut i = [0usize; 2];
        let parity = [cur_structure, cur_structure ^ 3];
        while i[0] < frames.len() || i[1] < frames.len() {
            for p in 0..2 {
                while i[p] < frames.len() && frames[i[p]].reference & parity[p] == 0 {
                    i[p] += 1;
                }
                if i[p] < frames.len() {
                    out.push(Self::ref_pic(sps, frames[i[p]], parity[p], cur_structure, cur_frame_num));
                    i[p] += 1;
                }
            }
        }
        out
    }

    /// 8.2.4.2 + 8.2.4.3: the reference picture lists of a slice.
    pub fn ref_lists(&self, sps: &Sps, hdr: &SliceHeader, cur_poc: i32) -> Result<[Vec<RefPic>; 2]> {
        let structure = hdr.structure();
        let field = structure != FRAME;
        let cur_frame_num = hdr.frame_num;
        let mut lists: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
        if hdr.slice_type == SliceType::I {
            return Ok(lists);
        }
        // the frames with a usable reference field / both fields
        let usable = |e: &&DpbEntry| if field { e.reference != 0 } else { e.reference == FRAME };
        let mut short: Vec<&DpbEntry> = self.entries.iter().filter(|e| e.kind == RefKind::Short).filter(usable).collect();
        let mut long: Vec<&DpbEntry> = self.entries.iter().filter(|e| e.kind == RefKind::Long).filter(usable).collect();
        long.sort_by_key(|e| e.long_term_frame_idx);
        let frames_to_refs = |frames: &[&DpbEntry]| -> Vec<RefPic> {
            if field {
                Self::alternate_fields(sps, frames, structure, cur_frame_num)
            } else {
                frames.iter().map(|e| Self::ref_pic(sps, e, FRAME, FRAME, cur_frame_num)).collect()
            }
        };
        match hdr.slice_type {
            SliceType::P => {
                short.sort_by(|a, b| Self::frame_num_wrap(sps, b.frame_num, cur_frame_num).cmp(&Self::frame_num_wrap(sps, a.frame_num, cur_frame_num)));
                lists[0] = frames_to_refs(&short);
                lists[0].extend(frames_to_refs(&long));
            }
            SliceType::B => {
                let mut before: Vec<&DpbEntry> = short.iter().copied().filter(|e| e.poc <= cur_poc).collect();
                before.sort_by(|a, b| b.poc.cmp(&a.poc));
                let mut after: Vec<&DpbEntry> = short.iter().copied().filter(|e| e.poc > cur_poc).collect();
                after.sort_by(|a, b| a.poc.cmp(&b.poc));
                let mut l0 = before.clone();
                l0.extend(after.iter().copied());
                let mut l1 = after;
                l1.extend(before);
                let long_refs = frames_to_refs(&long);
                lists[0] = frames_to_refs(&l0);
                lists[0].extend(long_refs.iter().cloned());
                lists[1] = frames_to_refs(&l1);
                lists[1].extend(long_refs);
                if lists[1].len() > 1 && lists[1].len() == lists[0].len() && lists[1].iter().zip(&lists[0]).all(|(a, b)| a.same(b)) {
                    lists[1].swap(0, 1);
                }
            }
            SliceType::I => unreachable!(),
        }
        let max_pic_num = if field { 2 * sps.max_frame_num() as i32 } else { sps.max_frame_num() as i32 };
        let cur_pic_num = if field { 2 * hdr.frame_num as i32 + 1 } else { hdr.frame_num as i32 };
        for l in 0..2 {
            let n = hdr.num_ref_idx_active[l] as usize;
            if n == 0 {
                lists[l].clear();
                continue;
            }
            lists[l].truncate(n);
            if !hdr.ref_list_mods[l].is_empty() {
                let mut pred = cur_pic_num;
                let mut list = lists[l].clone();
                for (idx, op) in hdr.ref_list_mods[l].iter().enumerate() {
                    if idx >= n {
                        return Err(Error::Bitstream("too many reference list modifications"));
                    }
                    let entry = match *op {
                        RefListMod::ShortTermSub(d) | RefListMod::ShortTermAdd(d) => {
                            let mut no_wrap = if matches!(op, RefListMod::ShortTermSub(_)) { pred - d as i32 } else { pred + d as i32 };
                            if no_wrap < 0 {
                                no_wrap += max_pic_num;
                            } else if no_wrap >= max_pic_num {
                                no_wrap -= max_pic_num;
                            }
                            pred = no_wrap;
                            // the field / frame with this picture number
                            let (frame_num, want) = if field { (no_wrap >> 1, if no_wrap & 1 != 0 { structure } else { structure ^ 3 }) } else { (no_wrap, FRAME) };
                            self.entries.iter().find(|e| e.kind == RefKind::Short && e.frame_num as i32 == frame_num && e.reference & want == want).map(|e| Self::ref_pic(sps, e, want, structure, cur_frame_num))
                        }
                        RefListMod::LongTerm(num) => {
                            let (idx_lt, want) = if field { (num >> 1, if num & 1 != 0 { structure } else { structure ^ 3 }) } else { (num, FRAME) };
                            self.entries.iter().find(|e| e.kind == RefKind::Long && e.long_term_frame_idx == idx_lt && e.reference & want == want).map(|e| Self::ref_pic(sps, e, want, structure, cur_frame_num))
                        }
                    };
                    let Some(entry) = entry else {
                        return Err(Error::Bitstream("reference list modification names a missing picture"));
                    };
                    // insert at idx, drop the later duplicate
                    list.insert(idx.min(list.len()), entry.clone());
                    let mut k = idx + 1;
                    while k < list.len() {
                        if list[k].same(&entry) {
                            list.remove(k);
                        } else {
                            k += 1;
                        }
                    }
                    list.truncate(n);
                }
                lists[l] = list;
            }
            if lists[l].is_empty() {
                return Err(Error::Bitstream("no reference pictures for an inter slice"));
            }
            // entries past the initial list are "no reference picture"; repeat
            // the last one so a broken stream is concealed rather than a panic
            while lists[l].len() < n {
                let last = lists[l].last().unwrap().clone();
                lists[l].push(last);
            }
        }
        Ok(lists)
    }

    /// 8.2.5.2: insert the frames a gap in frame_num skipped (or that were
    /// lost), so the reference picture numbering stays consistent. `fill`
    /// makes the picture (with the id given) used to stand in for them.
    pub fn fill_frame_num_gap(&mut self, sps: &Sps, hdr: &SliceHeader, fill: &dyn Fn(u32) -> Picture) {
        if hdr.is_idr() || !self.started {
            return;
        }
        let max = sps.max_frame_num();
        let prev = self.prev_ref_frame_num % max;
        if hdr.frame_num == prev {
            return;
        }
        let missing = (hdr.frame_num + max - prev - 1) % max;
        // only the last max_num_ref_frames of them: each one inserted into
        // a full buffer pushes out the oldest short-term frame, so the
        // earlier ones would be gone again by the end
        let keep = sps.max_num_ref_frames.max(1);
        for k in (1..=missing.min(keep)).rev() {
            let num = (hdr.frame_num + max - k) % max;
            let mut pic = fill(self.alloc_id());
            pic.frame_num = num;
            pic.is_ref = true;
            pic.decoded = FRAME;
            self.sliding_window(sps, num);
            self.entries.push(DpbEntry { frame_num: num, poc: pic.poc, pic: Rc::new(pic), kind: RefKind::Short, long_term_frame_idx: 0, reference: FRAME });
            self.prev_ref_frame_num = num;
        }
    }

    /// 8.2.5.3: make room for the current picture by dropping the oldest
    /// short-term frame when the buffer is full.
    fn sliding_window(&mut self, sps: &Sps, cur_frame_num: u32) {
        if self.entries.len() >= sps.max_num_ref_frames.max(1) as usize {
            let mut worst: Option<(usize, i32)> = None;
            for (i, e) in self.entries.iter().enumerate() {
                if e.kind != RefKind::Short {
                    continue;
                }
                let fnw = Self::frame_num_wrap(sps, e.frame_num, cur_frame_num);
                if worst.map_or(true, |(_, w)| fnw < w) {
                    worst = Some((i, fnw));
                }
            }
            if let Some((i, _)) = worst {
                let e = self.entries.remove(i);
                self.graveyard.push(e.pic);
            }
        }
    }

    /// The field / frame a picture number in a marking operation names:
    /// (frame_num, fields).
    fn pic_num_fields(no_wrap: i32, structure: u8) -> (u32, u8) {
        if structure == FRAME {
            (no_wrap as u32, FRAME)
        } else {
            (no_wrap as u32 >> 1, if no_wrap & 1 != 0 { structure } else { structure ^ 3 })
        }
    }

    /// 8.2.5: reference marking after decoding a picture (a frame, or one
    /// field), and the state updates for the next one. `pic` is the buffer
    /// later pictures reference; for the second field of a frame it
    /// replaces the first field's.
    pub fn mark(&mut self, sps: &Sps, hdr: &SliceHeader, pic: Rc<Picture>, poc: PocState, structure: u8) {
        let mut had_mmco5 = false;
        let field = structure != FRAME;
        let cur_pic_num = if field { 2 * hdr.frame_num as i32 + 1 } else { hdr.frame_num as i32 };
        let max_pic_num = if field { 2 * sps.max_frame_num() as i32 } else { sps.max_frame_num() as i32 };
        if hdr.is_ref() {
            if hdr.is_idr() {
                self.clear();
                let kind = if hdr.long_term_reference { RefKind::Long } else { RefKind::Short };
                self.max_long_term_frame_idx = if hdr.long_term_reference { Some(0) } else { None };
                self.entries.push(DpbEntry { pic: pic.clone(), kind, long_term_frame_idx: 0, reference: structure, frame_num: hdr.frame_num, poc: poc.poc });
            } else {
                // the second field of a frame whose first field is a reference joins its entry
                let existing = self.entries.iter().position(|e| e.pic.id == pic.id);
                let mut current_long: Option<u32> = None;
                match &hdr.mmco {
                    None => {
                        if existing.is_none() {
                            self.sliding_window(sps, hdr.frame_num);
                        }
                    }
                    Some(ops) => {
                        for op in ops {
                            match *op {
                                Mmco::UnmarkShortTerm(d) => {
                                    let no_wrap = (cur_pic_num - d as i32).rem_euclid(max_pic_num);
                                    let (frame_num, fields) = Self::pic_num_fields(no_wrap, structure);
                                    self.unmark_where(fields, |e| e.kind == RefKind::Short && e.frame_num == frame_num);
                                }
                                Mmco::UnmarkLongTerm(num) => {
                                    let (idx, fields) = Self::pic_num_fields(num as i32, structure);
                                    self.unmark_where(fields, |e| e.kind == RefKind::Long && e.long_term_frame_idx == idx);
                                }
                                Mmco::ShortToLong(d, idx) => {
                                    let no_wrap = (cur_pic_num - d as i32).rem_euclid(max_pic_num);
                                    let (frame_num, _) = Self::pic_num_fields(no_wrap, structure);
                                    // another frame holding this index is unmarked
                                    self.remove_where(|e| e.kind == RefKind::Long && e.long_term_frame_idx == idx && e.frame_num != frame_num);
                                    for e in self.entries.iter_mut() {
                                        if e.kind == RefKind::Short && e.frame_num == frame_num {
                                            e.kind = RefKind::Long;
                                            e.long_term_frame_idx = idx;
                                        }
                                    }
                                }
                                Mmco::MaxLongTermIdx(plus1) => {
                                    self.max_long_term_frame_idx = if plus1 == 0 { None } else { Some(plus1 - 1) };
                                    let max = self.max_long_term_frame_idx;
                                    self.remove_where(|e| e.kind == RefKind::Long && max.map_or(true, |m| e.long_term_frame_idx > m));
                                }
                                Mmco::UnmarkAll => {
                                    self.clear();
                                    had_mmco5 = true;
                                }
                                Mmco::CurrentToLong(idx) => {
                                    let id = pic.id;
                                    self.remove_where(|e| e.kind == RefKind::Long && e.long_term_frame_idx == idx && e.pic.id != id);
                                    current_long = Some(idx);
                                }
                            }
                        }
                        if current_long.is_none() && existing.is_none() && self.entries.len() >= sps.max_num_ref_frames.max(1) as usize {
                            // a stream that forgot to make room: behave like the sliding window
                            self.sliding_window(sps, hdr.frame_num);
                        }
                    }
                }
                // after a memory_management_control_operation 5 the picture counts as frame_num 0 with POC 0
                let (frame_num, poc_after) = if had_mmco5 { (0, 0) } else { (hdr.frame_num, poc.poc) };
                match self.entries.iter().position(|e| e.pic.id == pic.id) {
                    Some(i) => {
                        let e = &mut self.entries[i];
                        e.reference |= structure;
                        e.poc = e.poc.min(poc_after);
                        if let Some(idx) = current_long {
                            e.kind = RefKind::Long;
                            e.long_term_frame_idx = idx;
                        }
                        let old = std::mem::replace(&mut e.pic, pic.clone());
                        self.graveyard.push(old);
                    }
                    None => match current_long {
                        Some(idx) => self.entries.push(DpbEntry { pic: pic.clone(), kind: RefKind::Long, long_term_frame_idx: idx, reference: structure, frame_num, poc: poc_after }),
                        None => self.entries.push(DpbEntry { pic: pic.clone(), kind: RefKind::Short, long_term_frame_idx: 0, reference: structure, frame_num, poc: poc_after }),
                    },
                }
            }
        } else if let Some(i) = self.entries.iter().position(|e| e.pic.id == pic.id) {
            // a non-reference second field: the frame buffer still gains its samples
            let old = std::mem::replace(&mut self.entries[i].pic, pic.clone());
            self.graveyard.push(old);
        }
        // state for the next picture
        let frame_num_after = if had_mmco5 { 0 } else { hdr.frame_num };
        if hdr.is_ref() {
            self.started = true;
            self.prev_ref_frame_num = frame_num_after;
            if had_mmco5 {
                // 8.2.1: after mmco 5 the picture's order counts restart from zero
                self.prev_poc_msb = 0;
                self.prev_poc_lsb = if structure == BOTTOM { 0 } else { poc.top - poc.poc };
            } else {
                self.prev_poc_msb = poc.poc_msb;
                self.prev_poc_lsb = poc.poc_lsb;
            }
        }
        self.prev_frame_num = frame_num_after;
        self.prev_frame_num_offset = if had_mmco5 { 0 } else { poc.frame_num_offset };
        self.prev_had_mmco5 = had_mmco5;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::unescape;
    use crate::decoder::tests::{header, sps_nal, Seq, Slice};
    use crate::ps::parse_sps;

    /// A full buffer of four frames, frame_num `last - 3` to `last`, the
    /// oldest of them long-term when `long_term`.
    fn buffer(sps: &Sps, last: u32, long_term: bool) -> Dpb {
        let mut dpb = Dpb::new();
        let max = sps.max_frame_num();
        for k in (0..4).rev() {
            let frame_num = (last + max - k) % max;
            let kind = if long_term && k == 3 { RefKind::Long } else { RefKind::Short };
            let pic = Rc::new(Picture::new(dpb.alloc_id(), 1, 1));
            dpb.entries.push(DpbEntry { pic, kind, long_term_frame_idx: 0, reference: FRAME, frame_num, poc: 2 * frame_num as i32 });
        }
        dpb.prev_ref_frame_num = last;
        dpb.started = true;
        dpb
    }

    /// 8.2.5.2 as written: every missing frame, each through the sliding
    /// window.
    fn fill_every_frame(dpb: &mut Dpb, sps: &Sps, frame_num: u32) {
        let max = sps.max_frame_num();
        let mut num = (dpb.prev_ref_frame_num + 1) % max;
        while num != frame_num {
            dpb.sliding_window(sps, num);
            let pic = Rc::new(Picture::new(dpb.alloc_id(), 1, 1));
            dpb.entries.push(DpbEntry { pic, kind: RefKind::Short, long_term_frame_idx: 0, reference: FRAME, frame_num: num, poc: 0 });
            dpb.prev_ref_frame_num = num;
            num = (num + 1) % max;
        }
    }

    fn contents(dpb: &Dpb) -> Vec<(u32, i32, RefKind, u8)> {
        dpb.entries.iter().map(|e| (e.frame_num, e.poc, e.kind, e.reference)).collect()
    }

    #[test]
    fn a_frame_num_gap_fills_in_only_the_frames_that_stay() {
        let seq = Seq { max_refs: 4, log2_max_frame_num: 8, ..Seq::default() };
        let sps = parse_sps(&unescape(&sps_nal(&seq)[1..])).unwrap();
        for last in [10, 250] {
            for gap in [0, 1, 3, 6, 100, 200] {
                for long_term in [false, true] {
                    let frame_num = (last + 1 + gap) % 256;
                    let hdr = header(&seq, &Slice { frame_num, mbs: 4, ..Slice::default() });
                    let mut dpb = buffer(&sps, last, long_term);
                    dpb.fill_frame_num_gap(&sps, &hdr, &|id| Picture::new(id, 1, 1));
                    let mut every = buffer(&sps, last, long_term);
                    fill_every_frame(&mut every, &sps, frame_num);
                    let what = format!("a gap of {gap} after frame_num {last}, long-term {long_term}");
                    assert_eq!(contents(&dpb), contents(&every), "{what}");
                    assert_eq!(dpb.prev_ref_frame_num, every.prev_ref_frame_num, "{what}");
                    // (the four frames of the buffer, then at most four stand-ins)
                    assert!(dpb.next_id <= 1 + 4 + 4, "{what}: {} stand-ins", dpb.next_id - 5);
                }
            }
        }
    }
}
