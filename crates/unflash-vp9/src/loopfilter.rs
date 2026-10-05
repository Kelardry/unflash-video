//! The loop filter (8.8): superblocks in raster order, and in each the
//! vertical edges of a plane (left to right) and then its horizontal edges
//! (top to bottom), block and transform edges alike.
//!
//! The filter decisions of 8.8.2 - 8.8.4 are the same for runs of 8
//! consecutive samples along an edge (one 8x8 block of the plane), so they
//! are made once per run, and the run is filtered by `Pixel::filter_run`:
//! 8 lines at once in SIMD lanes (line by line without the `simd`
//! feature).

use crate::frame::{Pixel, FrameBuf};
use crate::header::{FrameHeader, SEG_LVL_ALT_L};
use crate::tables::{MAX_TX_SIZE, NUM_8X8_HIGH, NUM_8X8_WIDE, UV_BLOCK_SIZE};
use crate::tile::{MiInfo, NEARESTMV, NEWMV, ZEROMV};

const BLOCK_16X16: u8 = 6;

/// The filter strength of each segment, reference frame and mode type
/// (LvlLookup, 8.8.1), and the thresholds of each strength (8.8.4).
pub struct Levels {
    lvl: [[[u8; 2]; 4]; 8],
    limits: [Limits; 64],
}

/// The thresholds of one filter level, at 8-bit scale.
#[derive(Clone, Copy, Default)]
pub struct Limits {
    pub limit: i32,
    pub blimit: i32,
    pub thresh: i32,
}

impl Levels {
    pub fn new(fh: &FrameHeader) -> Levels {
        let lf = &fh.lf;
        let level = lf.level as i32;
        let scale = 1 << (level >> 5);
        let mut lvl = [[[0u8; 2]; 4]; 8];
        for (seg, l) in lvl.iter_mut().enumerate() {
            let mut lvl_seg = level;
            if fh.seg.active(seg as u8, SEG_LVL_ALT_L) {
                let data = fh.seg.data(seg as u8, SEG_LVL_ALT_L);
                lvl_seg = (if fh.seg.abs_delta { data } else { level + data }).clamp(0, 63);
            }
            if !lf.delta_enabled {
                *l = [[lvl_seg as u8; 2]; 4];
                continue;
            }
            l[0][0] = (lvl_seg + lf.ref_deltas[0] as i32 * scale).clamp(0, 63) as u8;
            for (rf, modes) in l.iter_mut().enumerate().skip(1) {
                for (mode, v) in modes.iter_mut().enumerate() {
                    *v = (lvl_seg + lf.ref_deltas[rf] as i32 * scale + lf.mode_deltas[mode] as i32 * scale).clamp(0, 63) as u8;
                }
            }
        }
        let sharp = lf.sharpness as i32;
        let shift = if sharp > 4 {
            2
        } else if sharp > 0 {
            1
        } else {
            0
        };
        let mut limits = [Limits::default(); 64];
        for (l, t) in limits.iter_mut().enumerate() {
            let l = l as i32;
            let limit = if sharp > 0 { (l >> shift).clamp(1, 9 - sharp) } else { (l >> shift).max(1) };
            *t = Limits { limit, blimit: 2 * (l + 2) + limit, thresh: l >> 4 };
        }
        Levels { lvl, limits }
    }

    #[inline]
    fn level(&self, m: &MiInfo) -> usize {
        let mode_type = (m.y_mode >= NEARESTMV && m.y_mode != ZEROMV && m.y_mode <= NEWMV) as usize;
        self.lvl[m.segment_id as usize & 7][m.ref_frame[0] as usize & 3][mode_type] as usize
    }
}

/// Filter the whole frame. (Kept out of line, as it runs once a frame:
/// inlined into the frame's decoding, it compiled to code that ran 0.5 to
/// 1.4% more instructions over the test streams.)
#[inline(never)]
pub fn filter_frame<T: Pixel>(frame: &mut FrameBuf<T>, mi: &[MiInfo], mi_rows: usize, mi_cols: usize, levels: &Levels) {
    let bd = frame.bit_depth as u32;
    for row in (0..mi_rows).step_by(8) {
        for col in (0..mi_cols).step_by(8) {
            for (plane, p) in frame.planes.iter_mut().enumerate() {
                let stride = p.stride;
                for pass in 0..2 {
                    filter_superblock(&mut p.data, stride, mi, mi_rows, mi_cols, levels, plane, pass, row, col, bd);
                }
            }
        }
    }
}

/// One run of samples along an edge to filter.
#[derive(Clone, Copy)]
pub struct Run {
    /// Index of the first line's q0 (the first sample past the edge).
    pub start: usize,
    /// The edge is vertical (lines are rows), or horizontal (lines are columns).
    pub vertical: bool,
    /// Lines to filter (up to 8; fewer where the run leaves the frame).
    pub count: usize,
    /// TX_4X4, TX_8X8 or TX_16X16: the widest filter allowed.
    pub size: u8,
    pub limits: Limits,
}

/// The superblock loop filter process (8.8.2) for one plane and direction.
#[allow(clippy::too_many_arguments)]
fn filter_superblock<T: Pixel>(data: &mut [T], stride: usize, mi: &[MiInfo], mi_rows: usize, mi_cols: usize, levels: &Levels, plane: usize, pass: usize, row: usize, col: usize, bd: u32) {
    let s = (plane > 0) as usize;
    for edge in 0..16 >> s {
        for group in 0..(64 >> s) / 8 {
            // luma position of the run's first sample
            let (x, y) = if pass == 0 { (col * 8 + edge * (4 << s), row * 8 + ((group * 8) << s)) } else { (col * 8 + ((group * 8) << s), row * 8 + edge * (4 << s)) };
            if x >= 8 * mi_cols || y >= 8 * mi_rows || (pass == 0 && x == 0) || (pass == 1 && y == 0) {
                continue;
            }
            // the samples of the run inside the frame
            let count = if pass == 0 { (8 * mi_rows - y).div_ceil(1 << s) } else { (8 * mi_cols - x).div_ceil(1 << s) }.min(8);
            let loop_row = ((y >> 3) >> s) << s;
            let loop_col = ((x >> 3) >> s) << s;
            let m = &mi[loop_row * mi_cols + loop_col];
            let tx_size = if plane > 0 {
                if m.size < 3 {
                    0
                } else {
                    m.tx_size.min(MAX_TX_SIZE[UV_BLOCK_SIZE[m.size as usize] as usize])
                }
            } else {
                m.tx_size
            };
            let sb_size = if s == 0 { m.size } else { m.size.max(BLOCK_16X16) } as usize;
            let is_block_edge = if pass == 0 { x % (8 * NUM_8X8_WIDE[sb_size] as usize) == 0 } else { y % (8 * NUM_8X8_HIGH[sb_size] as usize) == 0 };
            // the chroma of the last (half-outside) column of a frame with
            // an odd number of 8x8 columns has no internal horizontal 4x4 edge
            let is_tx_edge = if pass == 1 && s == 1 && mi_cols & 1 == 1 && edge & 1 == 1 && x + 8 >= mi_cols * 8 { false } else { edge % (1 << tx_size) == 0 };
            let is_32_edge = edge % 8 == 0;
            let is_intra = m.ref_frame[0] == 0;
            if !(is_block_edge || (is_tx_edge && (is_intra || !m.skip))) {
                continue;
            }
            // the filter size process (8.8.3)
            let base_size = if tx_size == 0 && is_32_edge { 1 } else { tx_size.min(2) };
            let size = if base_size == 2 && s == 1 && ((pass == 0 && x >> 3 == mi_cols - 1) || (pass == 1 && y >> 3 == mi_rows - 1)) { 1 } else { base_size };
            let lvl = levels.level(m);
            if lvl == 0 {
                continue;
            }
            let run = Run { start: (y >> s) * stride + (x >> s), vertical: pass == 0, count, size, limits: levels.limits[lvl] };
            T::filter_run(data, stride, &run, bd);
        }
    }
}

/// Filter a run line by line (8.8.5): the portable version, for any depth.
pub fn filter_run_lines<T: Pixel>(d: &mut [T], stride: usize, run: &Run, bd: u32) {
    let (along, across) = if run.vertical { (stride, 1) } else { (1, stride) };
    let n = if run.size == 2 { 8 } else { 4 };
    for line in 0..run.count {
        let q0 = run.start + line * along;
        let mut s = [0i32; 16];
        for (k, v) in s[8 - n..8 + n].iter_mut().enumerate() {
            *v = d[q0 + k * across - n * across].get();
        }
        let old = s;
        filter_line(&mut s, run.size, &run.limits, bd);
        for k in 8 - n..8 + n {
            if s[k] != old[k] {
                d[q0 + k * across - 8 * across] = T::new(s[k]);
            }
        }
    }
}

/// The filter mask process and the filters (8.8.5.1 - 8.8.5.3) on one
/// line: `s` holds p7..p0 then q0..q7 (only p3..q3 below TX_16X16).
#[inline]
fn filter_line(s: &mut [i32; 16], size: u8, l: &Limits, bd: u32) {
    let sh = bd - 8;
    let (p3, p2, p1, p0, q0, q1, q2, q3) = (s[4], s[5], s[6], s[7], s[8], s[9], s[10], s[11]);
    let limit = l.limit << sh;
    if (p3 - p2).abs() > limit || (p2 - p1).abs() > limit || (p1 - p0).abs() > limit || (q1 - q0).abs() > limit || (q2 - q1).abs() > limit || (q3 - q2).abs() > limit || (p0 - q0).abs() * 2 + (p1 - q1).abs() / 2 > l.blimit << sh {
        return;
    }
    let thresh = l.thresh << sh;
    let hev = (p1 - p0).abs() > thresh || (q1 - q0).abs() > thresh;
    let one = 1 << sh;
    let flat = |a: usize, b: usize| (a..b).all(|k| (s[7 - k] - p0).abs() <= one && (s[8 + k] - q0).abs() <= one);
    if size == 0 || !flat(1, 4) {
        narrow(s, hev, bd);
    } else if size == 1 || !flat(4, 8) {
        wide(s, 3);
    } else {
        wide(s, 4);
    }
}

/// The narrow filter (8.8.5.2): p1, p0, q0 and q1 at most.
#[inline]
fn narrow(s: &mut [i32; 16], hev: bool, bd: u32) {
    let lo = -(1 << (bd - 1));
    let hi = (1 << (bd - 1)) - 1;
    let c = |v: i32| v.clamp(lo, hi);
    let off = 0x80 << (bd - 8);
    let (ps1, ps0, qs0, qs1) = (s[6] - off, s[7] - off, s[8] - off, s[9] - off);
    let mut filter = if hev { c(ps1 - qs1) } else { 0 };
    filter = c(filter + 3 * (qs0 - ps0));
    let filter1 = c(filter + 4) >> 3;
    let filter2 = c(filter + 3) >> 3;
    s[8] = c(qs0 - filter1) + off;
    s[7] = c(ps0 + filter2) + off;
    if !hev {
        let f = (filter1 + 1) >> 1;
        s[9] = c(qs1 - f) + off;
        s[6] = c(ps1 + f) + off;
    }
}

/// The wide filter (8.8.5.3) of 2^log2 taps.
#[inline]
fn wide(s: &mut [i32; 16], log2: u32) {
    let n = (1i32 << (log2 - 1)) - 1;
    let at = |k: i32| s[(8 + k) as usize];
    let mut out = [0i32; 16];
    for i in -n..n {
        let mut t = at(i);
        for j in -n..=n {
            t += at((i + j).clamp(-(n + 1), n));
        }
        out[(8 + i) as usize] = (t + (1 << (log2 - 1))) >> log2;
    }
    s[(8 - n) as usize..(8 + n) as usize].copy_from_slice(&out[(8 - n) as usize..(8 + n) as usize]);
}

/// The filters on 8 lines at once, in 16-bit lanes. Every intermediate
/// fits them but the 16-tap sum of the widest filter, which at 12 bits
/// needs all 16 bits unsigned.
#[cfg(feature = "simd")]
pub mod simd {
    use super::{Limits, Run};
    use crate::lanes::Lanes;
    use wide::{i16x8, CmpGt, CmpLt};

    /// Filter a run: gather p7..q7 of each line into lanes (loads of rows
    /// for horizontal edges, a transpose for vertical ones), filter,
    /// scatter back.
    pub fn filter_run<T: Lanes>(d: &mut [T], stride: usize, run: &Run, bd: u32) {
        let n = if run.size == 2 { 8 } else { 4 };
        let mut v = [i16x8::ZERO; 16];
        if run.vertical {
            // (loops rather than `array::from_fn`, which compilers leave out
            // of line for WebAssembly)
            let (mut lo, mut hi) = ([i16x8::ZERO; 8], [i16x8::ZERO; 8]);
            for i in 0..8 {
                let row = &d[run.start + i * stride - n..];
                if n == 8 {
                    [lo[i], hi[i]] = T::load2(row);
                } else {
                    lo[i] = T::load(row);
                }
            }
            v[8 - n..8 - n + 8].copy_from_slice(&i16x8::transpose(lo));
            if n == 8 {
                v[8..].copy_from_slice(&i16x8::transpose(hi));
            }
        } else {
            for (k, vk) in v.iter_mut().enumerate().take(8 + n).skip(8 - n) {
                *vk = T::load(&d[run.start + k * stride - 8 * stride..]);
            }
        }
        let active = i16x8::new([0, 1, 2, 3, 4, 5, 6, 7]).cmp_lt(i16x8::splat(run.count as i16));
        let old = v;
        filter_lanes(&mut v, run.size, &run.limits, active, bd);
        if run.vertical {
            let lo = i16x8::transpose(v[8 - n..8 - n + 8].try_into().unwrap());
            let hi = if n == 8 { i16x8::transpose(v[8..16].try_into().unwrap()) } else { lo };
            for i in 0..run.count {
                let row = &mut d[run.start + i * stride - n..];
                T::store::<8>(lo[i], row);
                if n == 8 {
                    T::store::<8>(hi[i], &mut row[8..]);
                }
            }
        } else {
            for k in 8 - n..8 + n {
                if v[k] != old[k] {
                    T::store::<8>(v[k], &mut d[run.start + k * stride - 8 * stride..]);
                }
            }
        }
    }

    #[inline(always)]
    fn absd(a: i16x8, b: i16x8) -> i16x8 {
        (a - b).abs()
    }

    /// 8.8.5 on eight lines: `v` holds p7..q7 per lane.
    #[inline(always)]
    fn filter_lanes(v: &mut [i16x8; 16], size: u8, l: &Limits, active: i16x8, bd: u32) {
        let sh = bd - 8;
        let (p3, p2, p1, p0, q0, q1, q2, q3) = (v[4], v[5], v[6], v[7], v[8], v[9], v[10], v[11]);
        let limit = i16x8::splat((l.limit << sh) as i16);
        let over = absd(p3, p2).max(absd(p2, p1)).max(absd(p1, p0)).max(absd(q1, q0)).max(absd(q2, q1)).max(absd(q3, q2)).cmp_gt(limit);
        let edge = (absd(p0, q0) * 2i16 + (absd(p1, q1) >> 1_i32)).cmp_gt(i16x8::splat((l.blimit << sh) as i16));
        let mask = active & !(over | edge);
        if mask.none() {
            return;
        }
        let thresh = i16x8::splat((l.thresh << sh) as i16);
        let hev = absd(p1, p0).cmp_gt(thresh) | absd(q1, q0).cmp_gt(thresh);
        let flat_limit = i16x8::splat(1 << sh);
        let flat = if size > 0 { mask & !(absd(p1, p0).max(absd(q1, q0)).max(absd(p2, p0)).max(absd(q2, q0)).max(absd(p3, p0)).max(absd(q3, q0)).cmp_gt(flat_limit)) } else { i16x8::ZERO };
        let flat2 = if size == 2 && flat.any() { flat & !(absd(v[0], p0).max(absd(v[15], q0)).max(absd(v[1], p0)).max(absd(v[14], q0)).max(absd(v[2], p0)).max(absd(v[13], q0)).max(absd(v[3], p0)).max(absd(v[12], q0)).cmp_gt(flat_limit)) } else { i16x8::ZERO };
        let src = *v;
        // the narrow filter where the lines are not flat
        let narrow = mask & !flat;
        if narrow.any() {
            let (lo, hi) = (i16x8::splat(-(1 << (bd - 1))), i16x8::splat((1 << (bd - 1)) - 1));
            let c = |x: i16x8| x.max(lo).min(hi);
            let off = i16x8::splat(0x80 << sh);
            let (ps1, ps0, qs0, qs1) = (p1 - off, p0 - off, q0 - off, q1 - off);
            let f = c(ps1 - qs1) & hev;
            let f = c(f + (qs0 - ps0) * 3i16);
            let f1 = c(f + i16x8::splat(4)) >> 3_i32;
            let f2 = c(f + i16x8::splat(3)) >> 3_i32;
            v[8] = narrow.blend(c(qs0 - f1) + off, v[8]);
            v[7] = narrow.blend(c(ps0 + f2) + off, v[7]);
            let f = (f1 + i16x8::splat(1)) >> 1_i32;
            let outer = narrow & !hev;
            v[9] = outer.blend(c(qs1 - f) + off, v[9]);
            v[6] = outer.blend(c(ps1 + f) + off, v[6]);
        }
        // the 8-tap filter where only the inner lines are flat
        let f8 = flat & !flat2;
        if f8.any() {
            wide(&src, v, 3, f8);
        }
        if flat2.any() {
            wide(&src, v, 4, flat2);
        }
    }

    /// The wide filter (8.8.5.3) as a running sum, into the lanes of `m`.
    /// The sum wraps as a signed value at 12 bits but is exact unsigned, so
    /// the shift is a logical one.
    #[inline(always)]
    fn wide(s: &[i16x8; 16], v: &mut [i16x8; 16], log2: i32, m: i16x8) {
        let n = (1i32 << (log2 - 1)) - 1;
        let at = |k: i32| s[(8 + k.clamp(-(n + 1), n)) as usize];
        let mut t = at(-n);
        for j in -n..=n {
            t += at(-n + j);
        }
        let round = i16x8::splat(1 << (log2 - 1));
        let low = i16x8::splat(((1 << (16 - log2)) - 1) as i16);
        for i in -n..n {
            if i > -n {
                t = t - at(i - 1) + at(i) - at(i - 1 - n) + at(i + n);
            }
            let k = (8 + i) as usize;
            v[k] = m.blend(((t + round) >> log2) & low, v[k]);
        }
    }
}

#[cfg(all(test, feature = "simd"))]
mod tests {
    use super::*;

    fn check<T: crate::lanes::Lanes>(bd: u32, trials: usize, rnd: &mut impl FnMut() -> u64) {
        let stride = 48;
        let sh = bd - 8;
        for trial in 0..trials {
            // smooth data with steps, so that every filter gets chosen; near
            // the top of the range too, where the widest sum needs 16 bits
            let base = (rnd() % 256) as i32;
            let step = (rnd() % 24) as i32 - 12;
            let noise = 1 + (rnd() % [2, 3, 8, 40][trial % 4]) as i32;
            let mut d = vec![T::default(); stride * 32];
            for (i, s) in d.iter_mut().enumerate() {
                let (x, y) = (i % stride, i / stride);
                let edge = if (trial % 2 == 0 && x >= 16) || (trial % 2 == 1 && y >= 16) { step } else { 0 };
                let fine = (rnd() % (1 << sh)) as i32;
                *s = T::new((((base + edge + (rnd() % noise as u64) as i32) << sh) + fine).clamp(0, (1 << bd) - 1));
            }
            let level = (rnd() % 64) as i32;
            let sharp = (rnd() % 8) as i32;
            let limit = (level >> ((sharp > 0) as i32 + (sharp > 4) as i32)).clamp(1, if sharp > 0 { 9 - sharp } else { 63 });
            let run = Run { start: 16 * stride + 16, vertical: trial % 2 == 0, count: 1 + (rnd() % 8) as usize, size: (rnd() % 3) as u8, limits: Limits { limit, blimit: 2 * (level + 2) + limit, thresh: level >> 4 } };
            let mut a = d.clone();
            filter_run_lines(&mut a, stride, &run, bd);
            simd::filter_run(&mut d, stride, &run, bd);
            assert!(a == d, "{bd}-bit trial {trial}");
        }
    }

    /// The SIMD filters agree with the line-by-line ones.
    #[test]
    fn simd_matches_scalar() {
        let mut seed = 0x1234_5678_u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        check::<u8>(8, 20_000, &mut rnd);
        check::<u16>(10, 10_000, &mut rnd);
        check::<u16>(12, 10_000, &mut rnd);
    }
}
