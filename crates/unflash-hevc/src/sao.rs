//! Sample adaptive offset (8.7.3), applied per coding tree block to the
//! deblocked picture.

use crate::meta::{Meta, BYPASS, NO_SLICE, PCM};
use crate::picture::{Picture, Sample};
use crate::ps::{Layout, Pps, Sps};

/// hPos / vPos of the two neighbours per SaoEoClass (Table 8-13).
const EO: [[(isize, isize); 2]; 4] = [[(-1, 0), (1, 0)], [(0, -1), (0, 1)], [(-1, -1), (1, 1)], [(1, -1), (-1, 1)]];

/// Apply SAO to a deblocked picture; `src` is scratch space for a copy of
/// each plane (SAO reads the deblocked samples, not its own output).
pub fn sao<P: Sample>(pic: &mut Picture<P>, meta: &Meta, sps: &Sps, pps: &Pps, layout: &Layout, src: &mut Vec<P>) {
    let (wc, hc) = (layout.width_ctbs as usize, layout.height_ctbs as usize);
    let comps = if sps.chroma_format_idc != 0 { 3 } else { 1 };
    let excluded_flags = if pps.transquant_bypass_enabled { BYPASS } else { 0 } | if sps.pcm_enabled && sps.pcm_loop_filter_disabled { PCM } else { 0 };
    for c in 0..comps {
        if meta.sao.iter().all(|p| p.kind[c] == 0) {
            continue;
        }
        let sub = (c > 0) as usize;
        let bd = if c == 0 { sps.bit_depth } else { sps.bit_depth_chroma };
        let max = (1 << bd) - 1;
        let plane = &mut pic.planes[c];
        let (pw, ph, stride) = (plane.width, plane.height, plane.stride);
        src.clear();
        src.extend_from_slice(&plane.data);
        let ctb = (1usize << sps.log2_ctb) >> sub;
        for ry in 0..hc {
            for rx in 0..wc {
                let rs = ry * wc + rx;
                let p = &meta.sao[rs];
                if p.kind[c] == 0 {
                    continue;
                }
                let area = Area { x0: rx * ctb, y0: ry * ctb, w: ctb.min(pw - rx * ctb), h: ctb.min(ph - ry * ctb), stride };
                let off = p.offsets[c].map(i32::from);
                if p.kind[c] == 1 {
                    band_offset(src, &mut plane.data, &area, bd, p.class[c] as usize, off, max);
                } else {
                    let usable = usable_neighbours(meta, pps, layout, rx, ry);
                    edge_offset(src, &mut plane.data, &area, (pw, ph), EO[p.class[c] as usize], &usable, off, max);
                }
                // samples of lossless or unfiltered PCM blocks keep their values
                if excluded_flags != 0 {
                    let unit = 4 >> sub;
                    for y in (area.y0..area.y0 + area.h).step_by(unit) {
                        for x in (area.x0..area.x0 + area.w).step_by(unit) {
                            if meta.flags[meta.at(x << sub, y << sub)] & excluded_flags != 0 {
                                for yy in y..(y + unit).min(area.y0 + area.h) {
                                    let at = yy * stride + x;
                                    let n = unit.min(area.x0 + area.w - x);
                                    plane.data[at..at + n].copy_from_slice(&src[at..at + n]);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A coding tree block of one component.
struct Area {
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    stride: usize,
}

/// Which of the coding tree block (`rx`, `ry`) and its eight neighbours
/// edge offsets may read, by [dy][dx]: not across a picture edge, nor
/// across a slice or tile boundary where the later slice or the tiles
/// say no.
fn usable_neighbours(meta: &Meta, pps: &Pps, layout: &Layout, rx: usize, ry: usize) -> [[bool; 3]; 3] {
    let (wc, hc) = (layout.width_ctbs as usize, layout.height_ctbs as usize);
    let rs = ry * wc + rx;
    let cur_slice = meta.ctb_slice[rs];
    let mut usable = [[false; 3]; 3];
    for (dy, row) in usable.iter_mut().enumerate() {
        for (dx, u) in row.iter_mut().enumerate() {
            let (Some(nx), Some(ny)) = ((rx + dx).checked_sub(1), (ry + dy).checked_sub(1)) else { continue };
            if nx >= wc || ny >= hc {
                continue;
            }
            let nrs = ny * wc + nx;
            let ns = meta.ctb_slice[nrs];
            if ns == NO_SLICE || cur_slice == NO_SLICE {
                continue;
            }
            let (a, b) = (&meta.slices[cur_slice as usize], &meta.slices[ns as usize]);
            if a.addr != b.addr {
                // across a slice boundary: the later slice's flag decides
                let later = if layout.rs_to_ts[nrs] < layout.rs_to_ts[rs] { a } else { b };
                if !later.loop_filter_across_slices {
                    continue;
                }
            }
            if !pps.loop_filter_across_tiles && layout.tile_id[nrs] != layout.tile_id[rs] {
                continue;
            }
            *u = true;
        }
    }
    usable
}

/// Band offset: the four bands from `band` on get their offsets.
fn band_offset<P: Sample>(src: &[P], dst: &mut [P], a: &Area, bd: u32, band: usize, off: [i32; 4], max: i32) {
    let shift = bd - 5;
    let mut table = [0i32; 32];
    for (k, &o) in off.iter().enumerate() {
        table[(band + k) & 31] = o;
    }
    for y in a.y0..a.y0 + a.h {
        let at = y * a.stride + a.x0;
        let (s, d) = (&src[at..at + a.w], &mut dst[at..at + a.w]);
        #[cfg(feature = "simd")]
        let done = simd::band_row(s, shift, band, off, max, d);
        #[cfg(not(feature = "simd"))]
        let done = 0;
        for (d, s) in d[done..].iter_mut().zip(&s[done..]) {
            let v = s.get();
            *d = P::new((v + table[(v >> shift) as usize & 31]).clamp(0, max));
        }
    }
}

/// Edge offset with the neighbour pair `dirs`: samples that are a local
/// minimum, a concave or convex corner or a local maximum along the
/// direction get the offset of that category. Samples whose neighbours
/// cannot be read keep their values.
#[allow(clippy::too_many_arguments)]
fn edge_offset<P: Sample>(src: &[P], dst: &mut [P], a: &Area, (pw, ph): (usize, usize), dirs: [(isize, isize); 2], usable: &[[bool; 3]; 3], off: [i32; 4], max: i32) {
    let [(ax, ay), (bx, by)] = dirs;
    // the offset by 2 + Sign(v - a) + Sign(v - b)
    let by_edge = [off[0], off[1], 0, off[2], off[3]];
    let (x0, y0, stride) = (a.x0 as isize, a.y0 as isize, a.stride as isize);
    let size_x = a.w as isize;
    let size_y = a.h as isize;
    // whether the sample at (x, y) may be read, by the coding tree block it is in
    let ok = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 || x >= pw as isize || y >= ph as isize {
            return false;
        }
        let dx = if x < x0 { 0 } else if x >= x0 + size_x { 2 } else { 1 };
        let dy = if y < y0 { 0 } else if y >= y0 + size_y { 2 } else { 1 };
        usable[dy][dx]
    };
    let edge = |x: isize, y: isize| -> i32 {
        let v = src[(y * stride + x) as usize].get();
        let a = src[((y + ay) * stride + x + ax) as usize].get();
        let b = src[((y + by) * stride + x + bx) as usize].get();
        (v + by_edge[(2 + (v - a).signum() + (v - b).signum()) as usize]).clamp(0, max)
    };
    for y in y0..y0 + size_y {
        // the columns inside the block read rows y + ay and y + by at dx = 1
        let inner = size_x > 2 && ok(x0 + 1, y + ay) && ok(x0 + 1, y + by);
        if inner {
            let n = a.w - 2;
            let at = |dy: isize, dx: isize| ((y + dy) * stride + x0 + 1 + dx) as usize;
            let (cur, ra, rb) = (&src[at(0, 0)..][..n], &src[at(ay, ax)..][..n], &src[at(by, bx)..][..n]);
            edge_row(cur, ra, rb, &mut dst[at(0, 0)..][..n], &by_edge, max);
        }
        for x in x0..x0 + size_x {
            let border = x == x0 || x == x0 + size_x - 1;
            if (border || !inner) && ok(x + ax, y + ay) && ok(x + bx, y + by) {
                dst[(y * stride + x) as usize] = P::new(edge(x, y));
            }
        }
    }
}

/// Edge offset of a run of samples whose neighbours are all readable.
fn edge_row<P: Sample>(cur: &[P], ra: &[P], rb: &[P], out: &mut [P], by_edge: &[i32; 5], max: i32) {
    #[cfg(feature = "simd")]
    let done = simd::edge_row(cur, ra, rb, by_edge, max, out);
    #[cfg(not(feature = "simd"))]
    let done = 0;
    for (((o, v), a), b) in out[done..].iter_mut().zip(&cur[done..]).zip(&ra[done..]).zip(&rb[done..]) {
        let (v, a, b) = (v.get(), a.get(), b.get());
        *o = P::new((v + by_edge[(2 + (v - a).signum() + (v - b).signum()) as usize]).clamp(0, max));
    }
}

/// Eight-lane versions of the row loops (see the inter prediction's), for
/// the leading multiple of eight samples; each returns how many it did.
#[cfg(feature = "simd")]
mod simd {
    use wide::{i16x8, CmpEq, CmpGt, CmpLt};

    use crate::picture::Sample;

    /// The lanes of `offsets[k]` where `key` equals `keys[k]`, else 0.
    #[inline(always)]
    fn select(key: i16x8, keys: [i16; 4], offsets: [i32; 4]) -> i16x8 {
        keys.iter().zip(offsets).fold(i16x8::ZERO, |acc, (&k, o)| acc | (key.cmp_eq(i16x8::splat(k)) & i16x8::splat(o as i16)))
    }

    pub fn band_row<P: Sample>(s: &[P], shift: u32, band: usize, off: [i32; 4], max: i32, d: &mut [P]) -> usize {
        let (band, mask) = (i16x8::splat(band as i16), i16x8::splat(31));
        let mut i = 0;
        while i + 8 <= d.len() {
            let v = P::load8(&s[i..i + 8]);
            let k = ((v >> shift) - band) & mask;
            P::store8(v + select(k, [0, 1, 2, 3], off), max as i16, &mut d[i..i + 8]);
            i += 8;
        }
        i
    }

    pub fn edge_row<P: Sample>(cur: &[P], ra: &[P], rb: &[P], by_edge: &[i32; 5], max: i32, out: &mut [P]) -> usize {
        // Sign(x - y) from the comparison masks (all ones is -1)
        let sign = |x: i16x8, y: i16x8| x.cmp_lt(y) - x.cmp_gt(y);
        let offsets = [by_edge[0], by_edge[1], by_edge[3], by_edge[4]];
        let mut i = 0;
        while i + 8 <= out.len() {
            let v = P::load8(&cur[i..i + 8]);
            let e = sign(v, P::load8(&ra[i..i + 8])) + sign(v, P::load8(&rb[i..i + 8]));
            P::store8(v + select(e, [-2, -1, 1, 2], offsets), max as i16, &mut out[i..i + 8]);
            i += 8;
        }
        i
    }
}
