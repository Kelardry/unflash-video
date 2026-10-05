//! Intra prediction (8.3). The caller gathers the neighbouring samples and
//! their availability; unavailable samples must be filled with 128 so an
//! unlikely mode on a broken stream still produces a picture.

#[inline(always)]
fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Neighbouring samples of a block: `above[x]` is p[x, -1], `left[y]` is
/// p[-1, y], `corner` is p[-1, -1].
pub struct Edges<'a> {
    pub above: &'a [u8],
    pub left: &'a [u8],
    pub corner: u8,
    pub avail_above: bool,
    /// all left samples are available
    pub avail_left: bool,
    /// the upper and the lower half of the left samples separately (chroma
    /// DC prediction decides per 4x4 block; in an MBAFF frame the halves can
    /// belong to different macroblocks)
    pub avail_left_half: [bool; 2],
    pub avail_corner: bool,
}

/// The directional modes shared by Intra_4x4 and Intra_8x8 (8.3.1.2.x and
/// 8.3.2.2.x): `t[0]` is p[-1,-1] and `t[1 + x]` p[x,-1] (2N + 1 entries),
/// `l[0]` is p[-1,-1] and `l[1 + y]` p[-1,y] (N + 1 entries).
#[inline(always)]
fn directional<const N: usize>(mode: u32, t: &[i32], l: &[i32], out: &mut [u8]) {
    let n = N as i32;
    match mode {
        0 => {
            for y in 0..N {
                for x in 0..N {
                    out[y * N + x] = t[1 + x] as u8;
                }
            }
        }
        1 => {
            for y in 0..N {
                let v = l[1 + y] as u8;
                out[y * N..y * N + N].fill(v);
            }
        }
        3 => {
            // diagonal down left
            for y in 0..N {
                for x in 0..N {
                    let v = if x == N - 1 && y == N - 1 { (t[2 * N - 1] + 3 * t[2 * N] + 2) >> 2 } else { (t[1 + x + y] + 2 * t[2 + x + y] + t[3 + x + y] + 2) >> 2 };
                    out[y * N + x] = v as u8;
                }
            }
        }
        4 => {
            // diagonal down right
            for y in 0..n {
                for x in 0..n {
                    let v = if x > y {
                        let k = (x - y) as usize;
                        (t[k - 1] + 2 * t[k] + t[k + 1] + 2) >> 2
                    } else if x < y {
                        let k = (y - x) as usize;
                        (l[k - 1] + 2 * l[k] + l[k + 1] + 2) >> 2
                    } else {
                        (t[1] + 2 * t[0] + l[1] + 2) >> 2
                    };
                    out[(y * n + x) as usize] = v as u8;
                }
            }
        }
        5 => {
            // vertical right
            for y in 0..n {
                for x in 0..n {
                    let z = 2 * x - y;
                    let v = if z >= 0 {
                        let k = (x - (y >> 1)) as usize;
                        if z & 1 == 0 {
                            (t[k] + t[k + 1] + 1) >> 1
                        } else {
                            (t[k - 1] + 2 * t[k] + t[k + 1] + 2) >> 2
                        }
                    } else if z == -1 {
                        (l[1] + 2 * t[0] + t[1] + 2) >> 2
                    } else {
                        let k = (y - 2 * x) as usize;
                        (l[k] + 2 * l[k - 1] + l[k - 2] + 2) >> 2
                    };
                    out[(y * n + x) as usize] = v as u8;
                }
            }
        }
        6 => {
            // horizontal down
            for y in 0..n {
                for x in 0..n {
                    let z = 2 * y - x;
                    let v = if z >= 0 {
                        let k = (y - (x >> 1)) as usize;
                        if z & 1 == 0 {
                            (l[k] + l[k + 1] + 1) >> 1
                        } else {
                            (l[k - 1] + 2 * l[k] + l[k + 1] + 2) >> 2
                        }
                    } else if z == -1 {
                        (l[1] + 2 * t[0] + t[1] + 2) >> 2
                    } else {
                        let k = (x - 2 * y) as usize;
                        (t[k] + 2 * t[k - 1] + t[k - 2] + 2) >> 2
                    };
                    out[(y * n + x) as usize] = v as u8;
                }
            }
        }
        7 => {
            // vertical left
            for y in 0..N {
                for x in 0..N {
                    let k = 1 + x + (y >> 1);
                    let v = if y & 1 == 0 { (t[k] + t[k + 1] + 1) >> 1 } else { (t[k] + 2 * t[k + 1] + t[k + 2] + 2) >> 2 };
                    out[y * N + x] = v as u8;
                }
            }
        }
        _ => {
            // horizontal up
            let zmax = 2 * N - 3;
            for y in 0..N {
                for x in 0..N {
                    let z = x + 2 * y;
                    let k = 1 + y + (x >> 1);
                    let v = if z < zmax {
                        if z & 1 == 0 {
                            (l[k] + l[k + 1] + 1) >> 1
                        } else {
                            (l[k] + 2 * l[k + 1] + l[k + 2] + 2) >> 2
                        }
                    } else if z == zmax {
                        (l[N - 1] + 3 * l[N] + 2) >> 2
                    } else {
                        l[N]
                    };
                    out[y * N + x] = v as u8;
                }
            }
        }
    }
}

/// 8.3.1.2: Intra_4x4 prediction; `above` has 8 samples (the caller
/// substitutes p[3,-1] for a missing above-right).
pub fn pred4x4(mode: u32, e: &Edges, out: &mut [u8; 16]) {
    if mode == 2 {
        let a = |x: usize| e.above[x] as i32;
        let l = |y: usize| e.left[y] as i32;
        let v = if e.avail_above && e.avail_left {
            (a(0) + a(1) + a(2) + a(3) + l(0) + l(1) + l(2) + l(3) + 4) >> 3
        } else if e.avail_left {
            (l(0) + l(1) + l(2) + l(3) + 2) >> 2
        } else if e.avail_above {
            (a(0) + a(1) + a(2) + a(3) + 2) >> 2
        } else {
            128
        };
        out.fill(v as u8);
        return;
    }
    let mut t = [0i32; 9];
    let mut l = [0i32; 5];
    t[0] = e.corner as i32;
    l[0] = e.corner as i32;
    for x in 0..8 {
        t[1 + x] = e.above[x] as i32;
    }
    for y in 0..4 {
        l[1 + y] = e.left[y] as i32;
    }
    directional::<4>(mode, &t, &l, out);
}

/// 8.3.2.2: Intra_8x8 prediction with the reference sample filtering.
/// `above` has 16 samples (above-right substituted by the caller).
pub fn pred8x8(mode: u32, e: &Edges, out: &mut [u8; 64]) {
    // 8.3.2.2.1 filtering
    let mut a = [0i32; 16];
    let mut l = [0i32; 8];
    for x in 0..16 {
        a[x] = e.above[x] as i32;
    }
    for y in 0..8 {
        l[y] = e.left[y] as i32;
    }
    let c = e.corner as i32;
    let mut fa = [0i32; 16];
    let mut fl = [0i32; 8];
    let mut fc = c;
    if e.avail_above {
        fa[0] = if e.avail_corner { (c + 2 * a[0] + a[1] + 2) >> 2 } else { (3 * a[0] + a[1] + 2) >> 2 };
        for x in 1..15 {
            fa[x] = (a[x - 1] + 2 * a[x] + a[x + 1] + 2) >> 2;
        }
        fa[15] = (a[14] + 3 * a[15] + 2) >> 2;
    }
    if e.avail_corner {
        fc = if e.avail_above && e.avail_left {
            (a[0] + 2 * c + l[0] + 2) >> 2
        } else if e.avail_above {
            (3 * c + a[0] + 2) >> 2
        } else if e.avail_left {
            (3 * c + l[0] + 2) >> 2
        } else {
            c
        };
    }
    if e.avail_left {
        fl[0] = if e.avail_corner { (c + 2 * l[0] + l[1] + 2) >> 2 } else { (3 * l[0] + l[1] + 2) >> 2 };
        for y in 1..7 {
            fl[y] = (l[y - 1] + 2 * l[y] + l[y + 1] + 2) >> 2;
        }
        fl[7] = (l[6] + 3 * l[7] + 2) >> 2;
    }
    if mode == 2 {
        let v = if e.avail_above && e.avail_left {
            (fa[..8].iter().sum::<i32>() + fl.iter().sum::<i32>() + 8) >> 4
        } else if e.avail_left {
            (fl.iter().sum::<i32>() + 4) >> 3
        } else if e.avail_above {
            (fa[..8].iter().sum::<i32>() + 4) >> 3
        } else {
            128
        };
        out.fill(v as u8);
        return;
    }
    let mut t = [0i32; 17];
    let mut ll = [0i32; 9];
    t[0] = fc;
    ll[0] = fc;
    t[1..17].copy_from_slice(&fa);
    ll[1..9].copy_from_slice(&fl);
    directional::<8>(mode, &t, &ll, out);
}

/// 8.3.3: Intra_16x16 prediction (`above` and `left` hold 16 samples).
pub fn pred16x16(mode: u32, e: &Edges, out: &mut [u8; 256]) {
    let a = |x: i32| -> i32 { e.above[x as usize] as i32 };
    let l = |y: i32| -> i32 { e.left[y as usize] as i32 };
    match mode {
        0 => {
            for y in 0..16 {
                out[y * 16..y * 16 + 16].copy_from_slice(&e.above[..16]);
            }
        }
        1 => {
            for y in 0..16 {
                out[y * 16..y * 16 + 16].fill(e.left[y]);
            }
        }
        2 => {
            let v = if e.avail_above && e.avail_left {
                ((0..16).map(a).sum::<i32>() + (0..16).map(l).sum::<i32>() + 16) >> 5
            } else if e.avail_left {
                ((0..16).map(l).sum::<i32>() + 8) >> 4
            } else if e.avail_above {
                ((0..16).map(a).sum::<i32>() + 8) >> 4
            } else {
                128
            };
            out.fill(v as u8);
        }
        _ => {
            let c = e.corner as i32;
            let pa = |x: i32| if x < 0 { c } else { a(x) };
            let pl = |y: i32| if y < 0 { c } else { l(y) };
            let mut h = 0;
            let mut v = 0;
            for k in 0..8i32 {
                h += (k + 1) * (pa(8 + k) - pa(6 - k));
                v += (k + 1) * (pl(8 + k) - pl(6 - k));
            }
            let aa = 16 * (l(15) + a(15));
            let b = (5 * h + 32) >> 6;
            let cc = (5 * v + 32) >> 6;
            for y in 0..16i32 {
                let row = aa + cc * (y - 7) + 16;
                for x in 0..16i32 {
                    out[(y * 16 + x) as usize] = clip((row + b * (x - 7)) >> 5);
                }
            }
        }
    }
}

/// 8.3.4: chroma prediction of one 8x8 (4:2:0) component; `above` and
/// `left` hold 8 samples.
pub fn pred_chroma(mode: u32, e: &Edges, out: &mut [u8; 64]) {
    let a = |x: i32| -> i32 { e.above[x as usize] as i32 };
    let l = |y: i32| -> i32 { e.left[y as usize] as i32 };
    match mode {
        0 => {
            // DC, per 4x4 chroma block
            for by in 0..2i32 {
                let avail_left = e.avail_left_half[by as usize];
                for bx in 0..2i32 {
                    let sa: i32 = (0..4).map(|k| a(bx * 4 + k)).sum();
                    let sl: i32 = (0..4).map(|k| l(by * 4 + k)).sum();
                    let v = if (bx == 0 && by == 0) || (bx > 0 && by > 0) {
                        if e.avail_above && avail_left {
                            (sa + sl + 4) >> 3
                        } else if avail_left {
                            (sl + 2) >> 2
                        } else if e.avail_above {
                            (sa + 2) >> 2
                        } else {
                            128
                        }
                    } else if bx > 0 {
                        if e.avail_above {
                            (sa + 2) >> 2
                        } else if avail_left {
                            (sl + 2) >> 2
                        } else {
                            128
                        }
                    } else if avail_left {
                        (sl + 2) >> 2
                    } else if e.avail_above {
                        (sa + 2) >> 2
                    } else {
                        128
                    };
                    for y in 0..4 {
                        let o = ((by * 4 + y) * 8 + bx * 4) as usize;
                        out[o..o + 4].fill(v as u8);
                    }
                }
            }
        }
        1 => {
            for y in 0..8 {
                out[y * 8..y * 8 + 8].fill(e.left[y]);
            }
        }
        2 => {
            for y in 0..8 {
                out[y * 8..y * 8 + 8].copy_from_slice(&e.above[..8]);
            }
        }
        _ => {
            let c = e.corner as i32;
            let pa = |x: i32| if x < 0 { c } else { a(x) };
            let pl = |y: i32| if y < 0 { c } else { l(y) };
            let mut h = 0;
            let mut v = 0;
            for k in 0..4i32 {
                h += (k + 1) * (pa(4 + k) - pa(2 - k));
                v += (k + 1) * (pl(4 + k) - pl(2 - k));
            }
            let aa = 16 * (l(7) + a(7));
            let b = (34 * h + 32) >> 6;
            let cc = (34 * v + 32) >> 6;
            for y in 0..8i32 {
                let row = aa + cc * (y - 3) + 16;
                for x in 0..8i32 {
                    out[(y * 8 + x) as usize] = clip((row + b * (x - 3)) >> 5);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 4x4 modes written out as in the standard, for cross-checking.
    fn reference4x4(mode: u32, e: &Edges) -> [u8; 16] {
        let a = |x: i32| -> i32 { e.above[x as usize] as i32 };
        let l = |y: i32| -> i32 { e.left[y as usize] as i32 };
        let c = e.corner as i32;
        let p = |x: i32, y: i32| -> i32 {
            if y < 0 {
                if x < 0 {
                    c
                } else {
                    a(x)
                }
            } else {
                l(y)
            }
        };
        let mut out = [0u8; 16];
        for y in 0..4i32 {
            for x in 0..4i32 {
                let v = match mode {
                    0 => p(x, -1),
                    1 => p(-1, y),
                    2 => (a(0) + a(1) + a(2) + a(3) + l(0) + l(1) + l(2) + l(3) + 4) >> 3,
                    3 => {
                        if x == 3 && y == 3 {
                            (a(6) + 3 * a(7) + 2) >> 2
                        } else {
                            (a(x + y) + 2 * a(x + y + 1) + a(x + y + 2) + 2) >> 2
                        }
                    }
                    4 => {
                        if x > y {
                            (p(x - y - 2, -1) + 2 * p(x - y - 1, -1) + p(x - y, -1) + 2) >> 2
                        } else if x < y {
                            (p(-1, y - x - 2) + 2 * p(-1, y - x - 1) + p(-1, y - x) + 2) >> 2
                        } else {
                            (p(0, -1) + 2 * p(-1, -1) + p(-1, 0) + 2) >> 2
                        }
                    }
                    5 => {
                        let z = 2 * x - y;
                        if z >= 0 && z % 2 == 0 {
                            (p(x - (y >> 1) - 1, -1) + p(x - (y >> 1), -1) + 1) >> 1
                        } else if z >= 0 {
                            (p(x - (y >> 1) - 2, -1) + 2 * p(x - (y >> 1) - 1, -1) + p(x - (y >> 1), -1) + 2) >> 2
                        } else if z == -1 {
                            (p(-1, 0) + 2 * p(-1, -1) + p(0, -1) + 2) >> 2
                        } else {
                            (p(-1, y - 1) + 2 * p(-1, y - 2) + p(-1, y - 3) + 2) >> 2
                        }
                    }
                    6 => {
                        let z = 2 * y - x;
                        if z >= 0 && z % 2 == 0 {
                            (p(-1, y - (x >> 1) - 1) + p(-1, y - (x >> 1)) + 1) >> 1
                        } else if z >= 0 {
                            (p(-1, y - (x >> 1) - 2) + 2 * p(-1, y - (x >> 1) - 1) + p(-1, y - (x >> 1)) + 2) >> 2
                        } else if z == -1 {
                            (p(-1, 0) + 2 * p(-1, -1) + p(0, -1) + 2) >> 2
                        } else {
                            (p(x - 1, -1) + 2 * p(x - 2, -1) + p(x - 3, -1) + 2) >> 2
                        }
                    }
                    7 => {
                        if y % 2 == 0 {
                            (a(x + (y >> 1)) + a(x + (y >> 1) + 1) + 1) >> 1
                        } else {
                            (a(x + (y >> 1)) + 2 * a(x + (y >> 1) + 1) + a(x + (y >> 1) + 2) + 2) >> 2
                        }
                    }
                    _ => {
                        let z = x + 2 * y;
                        if z < 5 && z % 2 == 0 {
                            (l(y + (x >> 1)) + l(y + (x >> 1) + 1) + 1) >> 1
                        } else if z < 5 {
                            (l(y + (x >> 1)) + 2 * l(y + (x >> 1) + 1) + l(y + (x >> 1) + 2) + 2) >> 2
                        } else if z == 5 {
                            (l(2) + 3 * l(3) + 2) >> 2
                        } else {
                            l(3)
                        }
                    }
                };
                out[(y * 4 + x) as usize] = v as u8;
            }
        }
        out
    }

    /// The 8x8 directional modes as in the standard, from already filtered samples.
    fn reference8x8(mode: u32, fa: &[i32; 16], fl: &[i32; 8], fc: i32) -> [u8; 64] {
        let a = |x: i32| -> i32 { fa[x as usize] };
        let l = |y: i32| -> i32 { fl[y as usize] };
        let p = |x: i32, y: i32| -> i32 {
            if y < 0 {
                if x < 0 {
                    fc
                } else {
                    a(x)
                }
            } else {
                l(y)
            }
        };
        let mut out = [0u8; 64];
        for y in 0..8i32 {
            for x in 0..8i32 {
                let v = match mode {
                    0 => a(x),
                    1 => l(y),
                    3 => {
                        if x == 7 && y == 7 {
                            (a(14) + 3 * a(15) + 2) >> 2
                        } else {
                            (a(x + y) + 2 * a(x + y + 1) + a(x + y + 2) + 2) >> 2
                        }
                    }
                    4 => {
                        if x > y {
                            (p(x - y - 2, -1) + 2 * p(x - y - 1, -1) + p(x - y, -1) + 2) >> 2
                        } else if x < y {
                            (p(-1, y - x - 2) + 2 * p(-1, y - x - 1) + p(-1, y - x) + 2) >> 2
                        } else {
                            (p(0, -1) + 2 * p(-1, -1) + p(-1, 0) + 2) >> 2
                        }
                    }
                    5 => {
                        let z = 2 * x - y;
                        if z >= 0 && z % 2 == 0 {
                            (p(x - (y >> 1) - 1, -1) + p(x - (y >> 1), -1) + 1) >> 1
                        } else if z >= 0 {
                            (p(x - (y >> 1) - 2, -1) + 2 * p(x - (y >> 1) - 1, -1) + p(x - (y >> 1), -1) + 2) >> 2
                        } else if z == -1 {
                            (p(-1, 0) + 2 * p(-1, -1) + p(0, -1) + 2) >> 2
                        } else {
                            (p(-1, y - 2 * x - 1) + 2 * p(-1, y - 2 * x - 2) + p(-1, y - 2 * x - 3) + 2) >> 2
                        }
                    }
                    6 => {
                        let z = 2 * y - x;
                        if z >= 0 && z % 2 == 0 {
                            (p(-1, y - (x >> 1) - 1) + p(-1, y - (x >> 1)) + 1) >> 1
                        } else if z >= 0 {
                            (p(-1, y - (x >> 1) - 2) + 2 * p(-1, y - (x >> 1) - 1) + p(-1, y - (x >> 1)) + 2) >> 2
                        } else if z == -1 {
                            (p(-1, 0) + 2 * p(-1, -1) + p(0, -1) + 2) >> 2
                        } else {
                            (p(x - 2 * y - 1, -1) + 2 * p(x - 2 * y - 2, -1) + p(x - 2 * y - 3, -1) + 2) >> 2
                        }
                    }
                    7 => {
                        if y % 2 == 0 {
                            (a(x + (y >> 1)) + a(x + (y >> 1) + 1) + 1) >> 1
                        } else {
                            (a(x + (y >> 1)) + 2 * a(x + (y >> 1) + 1) + a(x + (y >> 1) + 2) + 2) >> 2
                        }
                    }
                    _ => {
                        let z = x + 2 * y;
                        if z < 13 && z % 2 == 0 {
                            (l(y + (x >> 1)) + l(y + (x >> 1) + 1) + 1) >> 1
                        } else if z < 13 {
                            (l(y + (x >> 1)) + 2 * l(y + (x >> 1) + 1) + l(y + (x >> 1) + 2) + 2) >> 2
                        } else if z == 13 {
                            (l(6) + 3 * l(7) + 2) >> 2
                        } else {
                            l(7)
                        }
                    }
                };
                out[(y * 8 + x) as usize] = v as u8;
            }
        }
        out
    }

    fn noise(n: usize, seed: &mut u32) -> Vec<u8> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (*seed >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn directional_modes_match_the_standard() {
        let mut seed = 11;
        for _ in 0..50 {
            let above = noise(16, &mut seed);
            let left = noise(8, &mut seed);
            let corner = noise(1, &mut seed)[0];
            let e = Edges { above: &above, left: &left, corner, avail_above: true, avail_left: true, avail_left_half: [true; 2], avail_corner: true };
            for mode in 0..9 {
                let mut out = [0u8; 16];
                pred4x4(mode, &e, &mut out);
                assert_eq!(out, reference4x4(mode, &e), "4x4 mode {mode}");
            }
            // 8x8: compare the directional part on the filtered samples
            let mut t = [0i32; 17];
            let mut l = [0i32; 9];
            let fa: [i32; 16] = std::array::from_fn(|i| above[i] as i32);
            let fl: [i32; 8] = std::array::from_fn(|i| left[i] as i32);
            t[0] = corner as i32;
            l[0] = corner as i32;
            t[1..].copy_from_slice(&fa);
            l[1..].copy_from_slice(&fl);
            for mode in [0, 1, 3, 4, 5, 6, 7, 8] {
                let mut out = [0u8; 64];
                directional::<8>(mode, &t, &l, &mut out);
                assert_eq!(out, reference8x8(mode, &fa, &fl, corner as i32), "8x8 mode {mode}");
            }
        }
    }
}
