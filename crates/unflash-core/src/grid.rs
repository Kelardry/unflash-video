//! Sliding-window geometry and the per-frame statistics a pixel stage hands
//! to the temporal stage.
//!
//! Both pixel stages (CPU kernels here, WGSL in `unflash-gpu`) reduce every
//! frame to one [`GridCell`] per window position: the window's summed
//! luminance and distance from red (for the coherence gate), the count of pixels in
//! each of the eight per-pixel mask classes, and the age of the oldest
//! opening transition still feeding a failure window (for the violation's
//! onset). Everything the detector does after that is a few hundred
//! numbers per frame.

use bytemuck::{Pod, Zeroable};

use crate::config::DetectorConfig;

/// How much of the picture has to move before a frame counts as showing
/// something new, as a fraction of a qualifying swing (see the reference's
/// `HELD_DELTA_RATIO`).
pub const HELD_DELTA_RATIO: f32 = 0.5;
/// ... over this fraction of the area a flash has to cover.
pub const HELD_AREA_RATIO: f64 = 0.10;

/// Window layout of the analysis model for one frame size.
#[derive(Clone, Debug, PartialEq)]
pub struct GridGeometry {
    /// Analysis frame size.
    pub aw: u32,
    pub ah: u32,
    /// Window size in analysis pixels.
    pub ww: u32,
    pub wh: u32,
    /// Pixels a flash has to cover inside one window.
    pub area_thresh: u32,
    /// Window x positions (left edges), ascending, last one flush right.
    pub gxs: Vec<u32>,
    /// Window y positions (top edges), ascending, last one flush bottom.
    pub gys: Vec<u32>,
    /// Luminance delta that counts as "moved" for the held-frame test.
    pub held_delta: f32,
    /// A pixel whose distance from red (u′v′) moved this much has moved too.
    pub held_delta_v: f32,
    /// Fewer moved pixels than this and the frame is a re-show.
    pub held_bar: f64,
}

impl GridGeometry {
    pub fn new(cfg: &DetectorConfig, aw: u32, ah: u32) -> Self {
        let s = cfg.analysis_scale;
        let ww = ((cfg.window_w as f64 * s).round_ties_even() as i64).max(2).min(aw as i64) as u32;
        let wh = ((cfg.window_h as f64 * s).round_ties_even() as i64).max(2).min(ah as i64) as u32;
        let area_thresh =
            ((cfg.area_fraction * ww as f64 * wh as f64).round_ties_even() as i64).max(1) as u32;
        let sx = (ww / 8).max(1);
        let sy = (wh / 8).max(1);
        let mut gxs: Vec<u32> = (0..=aw - ww).step_by(sx as usize).collect();
        gxs.push(aw - ww);
        gxs.sort_unstable();
        gxs.dedup();
        let mut gys: Vec<u32> = (0..=ah - wh).step_by(sy as usize).collect();
        gys.push(ah - wh);
        gys.sort_unstable();
        gys.dedup();
        GridGeometry {
            aw,
            ah,
            ww,
            wh,
            area_thresh,
            gxs,
            gys,
            held_delta: HELD_DELTA_RATIO * cfg.swing_threshold,
            held_delta_v: HELD_DELTA_RATIO * crate::lut::RED_LEAST_SWING,
            held_bar: (HELD_AREA_RATIO * area_thresh as f64).max(1.0),
        }
    }

    #[inline]
    pub fn npix(&self) -> usize {
        self.aw as usize * self.ah as usize
    }

    #[inline]
    pub fn ncells(&self) -> usize {
        self.gxs.len() * self.gys.len()
    }

    #[inline]
    pub fn window_pixels(&self) -> u32 {
        self.ww * self.wh
    }

    /// Integer form of the held bar: `count < held_bar` for an integer count
    /// is `count < ceil(held_bar)`.
    #[inline]
    pub fn held_bar_int(&self) -> u32 {
        self.held_bar.ceil() as u32
    }

    #[inline]
    pub fn is_held(&self, moved_count: u32) -> bool {
        (moved_count as f64) < self.held_bar
    }

    /// (x0, y0, x1, y1) of a window position, `cell` in row-major order.
    #[inline]
    pub fn bbox(&self, cell: usize) -> [u32; 4] {
        let gx = self.gxs[cell % self.gxs.len()];
        let gy = self.gys[cell / self.gxs.len()];
        [gx, gy, gx + self.ww, gy + self.wh]
    }
}

/// Per-pixel mask bits produced by the kernels (see [`crate::pixel`]).
pub const MASK_STROBE_GEN: u32 = 1 << 0;
pub const MASK_STROBE_RED: u32 = 1 << 1;
pub const MASK_EXT_GEN: u32 = 1 << 2;
pub const MASK_EXT_RED: u32 = 1 << 3;
pub const MASK_POOL_GEN_UP: u32 = 1 << 4;
pub const MASK_POOL_GEN_DN: u32 = 1 << 5;
pub const MASK_POOL_RED_UP: u32 = 1 << 6;
pub const MASK_POOL_RED_DN: u32 = 1 << 7;
pub const MASK_BITS: usize = 8;

/// One window position's reduction of a frame. `repr(C)` and 48 bytes so the
/// GPU can write it directly.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Pod, Zeroable)]
pub struct GridCell {
    /// Summed relative luminance over the window.
    pub sum_l: f32,
    /// Summed red value over the window.
    pub sum_v: f32,
    /// Pixel counts for each mask bit, in bit order.
    pub cnt: [u32; MASK_BITS],
    /// Largest age (µs) of an opening transition among pixels strobing at
    /// the failure rate in this window; 0 when none strobe.
    pub onset_gen: u32,
    pub onset_red: u32,
}

/// Everything the temporal stage needs to know about one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GridStats {
    /// The frame only re-showed the previous picture (see HELD_*).
    pub held: bool,
    /// Pixels that moved more than `held_delta` since the last new picture.
    pub held_count: u32,
    /// Summed relative luminance over the whole frame.
    pub sum_l: f64,
    /// One entry per window position, row-major over (gys, gxs). Empty on
    /// a held frame.
    pub cells: Vec<GridCell>,
    /// Pixels inside a regular pattern (see [`crate::pattern`]), and the
    /// spacing statistics of its stripes.
    pub pattern_count: u32,
    pub pattern_spacing_sum: u32,
    pub pattern_spacing_n: u32,
}

/// A frame handed to a CPU pixel stage: 8-bit sRGB at analysis resolution.
#[derive(Clone, Copy, Debug)]
pub struct FrameInput<'a> {
    pub data: &'a [u8],
    /// Bytes per pixel: 3 (RGB) or 4 (RGBA, alpha ignored).
    pub bpp: usize,
}

impl<'a> FrameInput<'a> {
    pub fn rgb(data: &'a [u8]) -> Self {
        FrameInput { data, bpp: 3 }
    }
    pub fn rgba(data: &'a [u8]) -> Self {
        FrameInput { data, bpp: 4 }
    }
}

/// Reduce per-pixel kernel outputs to grid cells on the CPU. Sums use f64,
/// like the reference's float64 integral image.
pub fn aggregate(
    geom: &GridGeometry,
    l: &[f32],
    v: &[f32],
    masks: &[u32],
    onset_gen: &[u32],
    onset_red: &[u32],
    cells: &mut Vec<GridCell>,
) {
    let w = geom.aw as usize;
    let h = geom.ah as usize;
    let ww = geom.ww as usize;
    let wh = geom.wh as usize;
    let ngx = geom.gxs.len();
    let ngy = geom.gys.len();
    debug_assert_eq!(l.len(), w * h);

    #[derive(Clone, Copy, Default)]
    struct RowWin {
        sum_l: f64,
        sum_v: f64,
        cnt: [u32; MASK_BITS],
        og: u32,
        or: u32,
    }
    let mut rowwin = vec![RowWin::default(); h * ngx];
    let mut pl = vec![0f64; w + 1];
    let mut pv = vec![0f64; w + 1];
    let mut pc = vec![[0u32; MASK_BITS]; w + 1];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            pl[x + 1] = pl[x] + l[row + x] as f64;
            pv[x + 1] = pv[x] + v[row + x] as f64;
            let m = masks[row + x];
            let prev = pc[x];
            let cur = &mut pc[x + 1];
            for (b, slot) in cur.iter_mut().enumerate() {
                *slot = prev[b] + ((m >> b) & 1);
            }
        }
        for (gi, &gx) in geom.gxs.iter().enumerate() {
            let gx = gx as usize;
            let rw = &mut rowwin[y * ngx + gi];
            rw.sum_l = pl[gx + ww] - pl[gx];
            rw.sum_v = pv[gx + ww] - pv[gx];
            let a = pc[gx];
            let b = pc[gx + ww];
            for k in 0..MASK_BITS {
                rw.cnt[k] = b[k] - a[k];
            }
            let mut og = 0u32;
            let mut or = 0u32;
            for x in gx..gx + ww {
                og = og.max(onset_gen[row + x]);
                or = or.max(onset_red[row + x]);
            }
            rw.og = og;
            rw.or = or;
        }
    }
    cells.clear();
    cells.resize(ngx * ngy, GridCell::default());
    for (gj, &gy) in geom.gys.iter().enumerate() {
        let gy = gy as usize;
        for gi in 0..ngx {
            let mut sum_l = 0f64;
            let mut sum_v = 0f64;
            let mut cnt = [0u32; MASK_BITS];
            let mut og = 0u32;
            let mut or = 0u32;
            for y in gy..gy + wh {
                let rw = &rowwin[y * ngx + gi];
                sum_l += rw.sum_l;
                sum_v += rw.sum_v;
                for k in 0..MASK_BITS {
                    cnt[k] += rw.cnt[k];
                }
                og = og.max(rw.og);
                or = or.max(rw.or);
            }
            cells[gj * ngx + gi] = GridCell {
                sum_l: sum_l as f32,
                sum_v: sum_v as f32,
                cnt,
                onset_gen: og,
                onset_red: or,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_matches_reference() {
        let cfg = DetectorConfig::default();
        let g = GridGeometry::new(&cfg, 256, 192);
        assert_eq!((g.ww, g.wh), (85, 64));
        assert_eq!(g.area_thresh, 1360);
        assert_eq!(g.held_bar, 136.0);
        assert_eq!(g.held_bar_int(), 136);
        // arange(0, 172, 10) + [171]
        let mut want: Vec<u32> = (0..172).step_by(10).collect();
        want.push(171);
        assert_eq!(g.gxs, want);
        let mut wanty: Vec<u32> = (0..129).step_by(8).collect();
        wanty.push(128);
        wanty.sort();
        wanty.dedup();
        assert_eq!(g.gys, wanty);
        assert!(g.is_held(135));
        assert!(!g.is_held(136));
    }

    #[test]
    fn tiny_frames_clamp_window() {
        let cfg = DetectorConfig::default();
        let g = GridGeometry::new(&cfg, 40, 30);
        assert_eq!((g.ww, g.wh), (40, 30));
        assert_eq!(g.gxs, vec![0]);
        assert_eq!(g.gys, vec![0]);
        assert_eq!(g.ncells(), 1);
    }

    #[test]
    fn aggregate_counts_and_onsets() {
        let cfg = DetectorConfig::default();
        let g = GridGeometry::new(&cfg, 16, 12);
        // ww = min(16, 85) = 16, wh = min(12, 64) = 12: a single window
        let n = g.npix();
        let l: Vec<f32> = (0..n).map(|i| (i % 7) as f32 / 7.0).collect();
        let v = vec![1.5f32; n];
        let mut masks = vec![0u32; n];
        let mut og = vec![0u32; n];
        for i in 0..n {
            if i % 3 == 0 {
                masks[i] |= MASK_STROBE_GEN;
                og[i] = 1000 + i as u32;
            }
            if i % 5 == 0 {
                masks[i] |= MASK_POOL_RED_DN;
            }
        }
        let or = vec![0u32; n];
        let mut cells = Vec::new();
        aggregate(&g, &l, &v, &masks, &og, &or, &mut cells);
        assert_eq!(cells.len(), 1);
        let c = cells[0];
        assert_eq!(c.cnt[0] as usize, (n + 2) / 3);
        assert_eq!(c.cnt[7] as usize, (n + 4) / 5);
        assert_eq!(c.onset_gen, 1000 + (n as u32 - 1) / 3 * 3);
        assert_eq!(c.onset_red, 0);
        let want_l: f64 = l.iter().map(|&x| x as f64).sum();
        assert!((c.sum_l as f64 - want_l).abs() < 1e-3);
        assert!((c.sum_v - 1.5 * n as f32).abs() < 1e-3);
    }
}
