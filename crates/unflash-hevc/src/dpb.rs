//! The reference pictures: picture order counts (8.3.1), reference picture
//! sets and marking (8.3.2), and reference picture lists (8.3.4).
//!
//! Pictures are handed to the caller as soon as they are decoded, so the
//! buffer holds reference pictures only: whatever the current picture's
//! reference picture set leaves out is dropped.

use std::rc::Rc;

use crate::ctu::RefPic;
use crate::picture::{Picture, Sample};
use crate::ps::Sps;
use crate::slice::SliceHeader;
use crate::{Error, Result};

/// A reference picture in the buffer.
pub struct DpbEntry<P> {
    pub pic: Rc<Picture<P>>,
    pub long_term: bool,
}

/// The pictures the current picture's reference picture set names, with
/// how they may be used.
pub struct RefSet<P> {
    pub st_before: Vec<RefPic<P>>,
    pub st_after: Vec<RefPic<P>>,
    pub lt_curr: Vec<RefPic<P>>,
}

/// 8.3.1: PicOrderCntVal from slice_pic_order_cnt_lsb and the order count
/// of the previous TemporalId 0 reference picture (`prev_tid0`); `reset`
/// is an IRAP picture with NoRaslOutputFlag (its PicOrderCntMsb is 0).
pub fn picture_order_count(sps: &Sps, lsb: u32, prev_tid0: i32, reset: bool) -> i32 {
    let max = 1i32 << sps.log2_max_poc_lsb;
    let lsb = lsb as i32;
    if reset {
        return lsb;
    }
    let prev_lsb = prev_tid0.rem_euclid(max);
    let prev_msb = prev_tid0 - prev_lsb;
    let msb = if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
        prev_msb + max
    } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
        prev_msb - max
    } else {
        prev_msb
    };
    msb + lsb
}

/// 8.3.2: apply the current picture's reference picture set to the buffer
/// (dropping the pictures it does not name, marking long-term ones) and
/// return the sets it may predict from. Missing pictures are replaced by
/// `missing(poc)` (mid-grey stand-ins, as ffmpeg makes); `damaged` is set
/// when that happens for a picture the current one uses.
pub fn apply_rps<P: Sample>(dpb: &mut Vec<DpbEntry<P>>, graveyard: &mut Vec<Rc<Picture<P>>>, sps: &Sps, hdr: &SliceHeader, poc: i32, missing: &mut dyn FnMut(i32) -> Picture<P>, damaged: &mut bool) -> RefSet<P> {
    let max_lsb = 1i32 << sps.log2_max_poc_lsb;
    let rps = &hdr.st_rps;
    let mut st = [Vec::new(), Vec::new(), Vec::new()]; // before, after, foll
    for i in 0..rps.len() {
        let which = if !rps.used[i] {
            2
        } else if i < rps.num_negative {
            0
        } else {
            1
        };
        st[which].push(poc + rps.delta_poc[i]);
    }
    // long-term entries: (order count, whether it is complete, used)
    let mut lt = Vec::new();
    for e in &hdr.lt {
        let mut p = e.poc_lsb as i32;
        if e.msb_present {
            p += poc - (e.msb_cycle as i32).wrapping_mul(max_lsb) - (poc & (max_lsb - 1));
        }
        lt.push((p, e.msb_present, e.used));
    }
    let mut keep = vec![false; dpb.len()];
    let mut out = RefSet { st_before: Vec::new(), st_after: Vec::new(), lt_curr: Vec::new() };
    // long-term pictures first: any reference picture with that order count
    let mut lt_found: Vec<Option<usize>> = Vec::new();
    for &(p, full, _) in &lt {
        let found = dpb.iter().position(|e| if full { e.pic.poc == p } else { e.pic.poc & (max_lsb - 1) == p });
        lt_found.push(found);
    }
    for (k, &(p, _, used)) in lt.iter().enumerate() {
        let idx = match lt_found[k] {
            Some(i) => i,
            None => {
                if used {
                    *damaged = true;
                }
                let mut pic = missing(p);
                pic.poc = p;
                dpb.push(DpbEntry { pic: Rc::new(pic), long_term: true });
                keep.push(false);
                dpb.len() - 1
            }
        };
        dpb[idx].long_term = true;
        keep[idx] = true;
        if used {
            out.lt_curr.push(RefPic { pic: dpb[idx].pic.clone(), poc: dpb[idx].pic.poc, long_term: true });
        }
    }
    // short-term pictures
    for (which, pocs) in st.iter().enumerate() {
        for &p in pocs {
            let found = dpb.iter().position(|e| !e.long_term && e.pic.poc == p);
            let idx = match found {
                Some(i) => i,
                None => {
                    if which < 2 {
                        *damaged = true;
                    }
                    let mut pic = missing(p);
                    pic.poc = p;
                    dpb.push(DpbEntry { pic: Rc::new(pic), long_term: false });
                    keep.push(false);
                    dpb.len() - 1
                }
            };
            keep[idx] = true;
            let r = RefPic { pic: dpb[idx].pic.clone(), poc: p, long_term: false };
            match which {
                0 => out.st_before.push(r),
                1 => out.st_after.push(r),
                _ => {}
            }
        }
    }
    // everything else is no longer used for reference
    let mut k = 0;
    dpb.retain(|e| {
        let kept = keep[k];
        k += 1;
        if !kept {
            graveyard.push(e.pic.clone());
        }
        kept
    });
    out
}

/// 8.3.4: the reference picture lists of a P or B slice.
pub fn ref_lists<P: Sample>(set: &RefSet<P>, hdr: &SliceHeader) -> Result<[Vec<RefPic<P>>; 2]> {
    let total = set.st_before.len() + set.st_after.len() + set.lt_curr.len();
    if total == 0 {
        return Err(Error::Bitstream("inter slice without reference pictures"));
    }
    let clone = |r: &RefPic<P>| RefPic { pic: r.pic.clone(), poc: r.poc, long_term: r.long_term };
    let mut lists: [Vec<RefPic<P>>; 2] = [Vec::new(), Vec::new()];
    for (l, list) in lists.iter_mut().enumerate() {
        let n = hdr.num_ref_idx[l];
        if n == 0 {
            continue;
        }
        let order: [&Vec<RefPic<P>>; 3] = if l == 0 { [&set.st_before, &set.st_after, &set.lt_curr] } else { [&set.st_after, &set.st_before, &set.lt_curr] };
        // RefPicListTempX: the sets repeated up to Max(num_ref_idx, NumPicTotalCurr)
        let len = n.max(total);
        let mut temp = Vec::with_capacity(len);
        while temp.len() < len {
            for r in order.iter().flat_map(|s| s.iter()) {
                if temp.len() < len {
                    temp.push(r);
                }
            }
        }
        for i in 0..n {
            let pick = match &hdr.list_entry[l] {
                Some(entries) => entries[i] as usize,
                None => i,
            };
            let r = temp.get(pick).ok_or(Error::Bitstream("list_entry outside the reference picture set"))?;
            list.push(clone(r));
        }
    }
    Ok(lists)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sequence of which the order count process reads log2_max_poc_lsb.
    fn sps_with_lsb_bits(bits: u32) -> Sps {
        Sps {
            id: 0,
            general_profile_idc: 1,
            chroma_format_idc: 1,
            width: 64,
            height: 64,
            conf_win: (0, 0, 0, 0),
            bit_depth: 8,
            bit_depth_chroma: 8,
            log2_max_poc_lsb: bits,
            log2_min_cb: 3,
            log2_ctb: 4,
            log2_min_tb: 2,
            log2_max_tb: 4,
            max_transform_hierarchy_depth_inter: 1,
            max_transform_hierarchy_depth_intra: 1,
            scaling_list_enabled: false,
            scaling_list: Default::default(),
            amp_enabled: false,
            sao_enabled: false,
            pcm_enabled: false,
            pcm_bit_depth: 8,
            pcm_bit_depth_chroma: 8,
            log2_min_pcm_cb: 3,
            log2_max_pcm_cb: 3,
            pcm_loop_filter_disabled: false,
            st_rps: Vec::new(),
            long_term_refs_present: false,
            lt_ref_pics: Vec::new(),
            temporal_mvp_enabled: false,
            strong_intra_smoothing: false,
            vui: None,
            max_num_reorder_pics: 0,
        }
    }

    #[test]
    fn order_counts_wrap() {
        let sps = sps_with_lsb_bits(4);
        assert_eq!(picture_order_count(&sps, 3, 14, false), 19);
        assert_eq!(picture_order_count(&sps, 14, 17, false), 14);
        assert_eq!(picture_order_count(&sps, 13, -3, false), -3);
        assert_eq!(picture_order_count(&sps, 5, -3, false), 5);
        assert_eq!(picture_order_count(&sps, 9, 100, true), 9);
    }
}
