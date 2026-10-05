//! Intra sample prediction (8.4.4.2): reference sample substitution and
//! filtering, and the planar, DC and angular modes.
//!
//! The 4N + 1 reference samples of an N×N block are kept in one line in
//! the order the substitution process searches them: `p[−1][2N−1]` up the
//! left column to the corner `p[−1][−1]`, then along the top row to
//! `p[2N−1][−1]`. Substitution and the \[1 2 1\] filter are then plain
//! passes over the line.

use crate::picture::Sample;

/// The longest reference line (32×32 blocks).
pub const MAX_LINE: usize = 4 * 32 + 1;

pub const PLANAR: u32 = 0;
pub const DC: u32 = 1;

/// intraPredAngle (Table 8-5) for modes 2..=34.
const ANGLE: [i32; 35] = [0, 0, 32, 26, 21, 17, 13, 9, 5, 2, 0, -2, -5, -9, -13, -17, -21, -26, -32, -26, -21, -17, -13, -9, -5, -2, 0, 2, 5, 9, 13, 17, 21, 26, 32];
/// invAngle (Table 8-6) for modes 11..=25.
const INV_ANGLE: [i32; 15] = [-4096, -1638, -910, -630, -482, -390, -315, -256, -315, -390, -482, -630, -910, -1638, -4096];

/// 8.4.4.2.2: replace the samples not available for intra prediction.
/// `avail` flags each sample of `line`.
pub fn substitute(line: &mut [i32], avail: &[bool], bit_depth: u32) {
    match avail.iter().position(|&a| a) {
        None => line.fill(1 << (bit_depth - 1)),
        Some(first) => {
            let v = line[first];
            line[..first].fill(v);
            for i in first + 1..line.len() {
                if !avail[i] {
                    line[i] = line[i - 1];
                }
            }
        }
    }
}

/// 8.4.4.2.3: smooth the reference samples of an `n`×`n` luma block when
/// its mode asks for it (`strong` is strong_intra_smoothing_enabled_flag).
pub fn filter(line: &mut [i32], n: usize, mode: u32, strong: bool, bit_depth: u32) {
    if mode == DC || n == 4 {
        return;
    }
    let min_dist = (mode as i32 - 26).abs().min((mode as i32 - 10).abs());
    let thres = match n {
        8 => 7,
        16 => 1,
        _ => 0,
    };
    if min_dist <= thres {
        return;
    }
    let len = 4 * n + 1;
    let corner = line[2 * n];
    let (bottom, right) = (line[0], line[len - 1]);
    if strong && n == 32 && (corner + right - 2 * line[2 * n + n]).abs() < (1 << (bit_depth - 5)) && (corner + bottom - 2 * line[n]).abs() < (1 << (bit_depth - 5)) {
        // bilinear between the corner and the far ends (8-36 .. 8-40)
        for i in 0..63 {
            // p[-1][i] is line[63 - i]; p[i][-1] is line[65 + i]
            line[63 - i] = ((63 - i as i32) * corner + (i as i32 + 1) * bottom + 32) >> 6;
            line[65 + i] = ((63 - i as i32) * corner + (i as i32 + 1) * right + 32) >> 6;
        }
        return;
    }
    let mut prev = line[0];
    for i in 1..len - 1 {
        let cur = line[i];
        line[i] = (prev + 2 * cur + line[i + 1] + 2) >> 2;
        prev = cur;
    }
}

/// Predict an `n`×`n` block into `dst` (stride `ds`) from its reference
/// line. `edge_filters` enables the DC and pure horizontal / vertical
/// boundary smoothing (luma blocks smaller than 32×32).
pub fn predict<P: Sample>(line: &[i32], n: usize, mode: u32, edge_filters: bool, bit_depth: u32, dst: &mut [P], ds: usize) {
    let c = 2 * n; // the corner p[-1][-1]
    // p[-1][y] = line[c - 1 - y]; p[x][-1] = line[c + 1 + x]
    let log2 = n.trailing_zeros();
    match mode {
        PLANAR => {
            let top_right = line[c + 1 + n];
            let bottom_left = line[c - 1 - n];
            for y in 0..n {
                let left = line[c - 1 - y];
                let row = &mut dst[y * ds..y * ds + n];
                for (x, d) in row.iter_mut().enumerate() {
                    let v = ((n - 1 - x) as i32 * left + (x + 1) as i32 * top_right + (n - 1 - y) as i32 * line[c + 1 + x] + (y + 1) as i32 * bottom_left + n as i32) >> (log2 + 1);
                    *d = P::new(v);
                }
            }
        }
        DC => {
            let mut sum = n as i32;
            for i in 0..n {
                sum += line[c + 1 + i] + line[c - 1 - i];
            }
            let dc = sum >> (log2 + 1);
            for y in 0..n {
                dst[y * ds..y * ds + n].fill(P::new(dc));
            }
            if edge_filters {
                dst[0] = P::new((line[c - 1] + 2 * dc + line[c + 1] + 2) >> 2);
                for x in 1..n {
                    dst[x] = P::new((line[c + 1 + x] + 3 * dc + 2) >> 2);
                }
                for y in 1..n {
                    dst[y * ds] = P::new((line[c - 1 - y] + 3 * dc + 2) >> 2);
                }
            }
        }
        _ => angular(line, n, mode as usize, edge_filters, bit_depth, dst, ds),
    }
}

/// 8.4.4.2.6: the angular modes 2..=34.
fn angular<P: Sample>(line: &[i32], n: usize, mode: usize, edge_filters: bool, bit_depth: u32, dst: &mut [P], ds: usize) {
    let c = 2 * n;
    let angle = ANGLE[mode];
    let max = (1 << bit_depth) - 1;
    // ref[x] for x = -n..=2n, stored at buf[n + x]
    let mut buf = [0i32; 3 * 32 + 1];
    let vertical = mode >= 18;
    // the main reference: the top row (vertical modes) or the left column
    for x in 0..=2 * n {
        buf[n + x] = if vertical { line[c + x] } else { line[c - x] };
    }
    if angle < 0 {
        let last = (n as i32 * angle) >> 5;
        if last < -1 {
            let inv = INV_ANGLE[mode - 11];
            let mut x = -1;
            while x >= last {
                // index into the other side: p[-1][k] or p[k][-1]
                let k = -1 + ((x * inv + 128) >> 8);
                buf[(n as i32 + x) as usize] = if vertical { line[(c as i32 - 1 - k) as usize] } else { line[(c as i32 + 1 + k) as usize] };
                x -= 1;
            }
        }
    }
    for j in 0..n {
        let pos = (j as i32 + 1) * angle;
        let idx = pos >> 5;
        let fact = pos & 31;
        let base = (n as i32 + idx + 1) as usize;
        for i in 0..n {
            let v = if fact != 0 { ((32 - fact) * buf[base + i] + fact * buf[base + i + 1] + 16) >> 5 } else { buf[base + i] };
            // vertical modes run along rows, horizontal ones down columns
            let (x, y) = if vertical { (i, j) } else { (j, i) };
            dst[y * ds + x] = P::new(v);
        }
    }
    if edge_filters && (mode == 26 || mode == 10) {
        let corner = line[c];
        for k in 0..n {
            if mode == 26 {
                // predSamples[0][y] = p[0][-1] + ((p[-1][y] - p[-1][-1]) >> 1)
                dst[k * ds] = P::new((line[c + 1] + ((line[c - 1 - k] - corner) >> 1)).clamp(0, max));
            } else {
                dst[k] = P::new((line[c - 1] + ((line[c + 1 + k] - corner) >> 1)).clamp(0, max));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A direct transcription of the spec's p[x][y] formulation, for
    /// comparison.
    fn reference(line: &[i32], n: usize, mode: u32, edge: bool) -> Vec<i32> {
        let c = 2 * n as i32;
        let p = |x: i32, y: i32| -> i32 {
            if x == -1 {
                line[(c - 1 - y) as usize]
            } else {
                line[(c + 1 + x) as usize]
            }
        };
        let mut out = vec![0; n * n];
        let ni = n as i32;
        if mode >= 2 {
            let angle = ANGLE[mode as usize];
            let mut r = std::collections::HashMap::new();
            if mode >= 18 {
                for x in 0..=ni {
                    r.insert(x, p(-1 + x, -1));
                }
                if angle < 0 && (ni * angle) >> 5 < -1 {
                    let inv = INV_ANGLE[mode as usize - 11];
                    for x in ((ni * angle) >> 5)..=-1 {
                        r.insert(x, p(-1, -1 + ((x * inv + 128) >> 8)));
                    }
                } else {
                    for x in ni + 1..=2 * ni {
                        r.insert(x, p(-1 + x, -1));
                    }
                }
                for y in 0..ni {
                    let idx = ((y + 1) * angle) >> 5;
                    let f = ((y + 1) * angle) & 31;
                    for x in 0..ni {
                        out[(y * ni + x) as usize] = if f != 0 { ((32 - f) * r[&(x + idx + 1)] + f * r[&(x + idx + 2)] + 16) >> 5 } else { r[&(x + idx + 1)] };
                    }
                }
                if mode == 26 && edge {
                    for y in 0..ni {
                        out[(y * ni) as usize] = (p(0, -1) + ((p(-1, y) - p(-1, -1)) >> 1)).clamp(0, 255);
                    }
                }
            } else {
                for x in 0..=ni {
                    r.insert(x, p(-1, -1 + x));
                }
                if angle < 0 && (ni * angle) >> 5 < -1 {
                    let inv = INV_ANGLE[mode as usize - 11];
                    for x in ((ni * angle) >> 5)..=-1 {
                        r.insert(x, p(-1 + ((x * inv + 128) >> 8), -1));
                    }
                } else {
                    for x in ni + 1..=2 * ni {
                        r.insert(x, p(-1, -1 + x));
                    }
                }
                for x in 0..ni {
                    let idx = ((x + 1) * angle) >> 5;
                    let f = ((x + 1) * angle) & 31;
                    for y in 0..ni {
                        out[(y * ni + x) as usize] = if f != 0 { ((32 - f) * r[&(y + idx + 1)] + f * r[&(y + idx + 2)] + 16) >> 5 } else { r[&(y + idx + 1)] };
                    }
                }
                if mode == 10 && edge {
                    for x in 0..ni {
                        out[x as usize] = (p(-1, 0) + ((p(x, -1) - p(-1, -1)) >> 1)).clamp(0, 255);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn angular_modes_match_the_spec_formulation() {
        let mut seed = 12345u32;
        for &n in &[4usize, 8, 16, 32] {
            let line: Vec<i32> = (0..4 * n + 1)
                .map(|_| {
                    seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                    (seed >> 16) as i32 & 255
                })
                .collect();
            for mode in 2..35 {
                let mut dst = vec![0u8; n * n];
                let edge = n < 32;
                predict(&line, n, mode, edge, 8, &mut dst, n);
                let want = reference(&line, n, mode, edge);
                let got: Vec<i32> = dst.iter().map(|&v| v as i32).collect();
                assert_eq!(got, want, "mode {mode} size {n}");
            }
        }
    }

    #[test]
    fn substitution_and_dc() {
        let mut line = [0i32; 17];
        let mut avail = [false; 17];
        for i in 9..17 {
            line[i] = 100 + i as i32;
            avail[i] = true;
        }
        substitute(&mut line, &avail, 8);
        assert!(line[..9].iter().all(|&v| v == 109));
        let mut line = [0i32; 17];
        substitute(&mut line, &[false; 17], 10);
        assert!(line.iter().all(|&v| v == 512));
        let line = [80i32; 17];
        let mut dst = [0u8; 16];
        predict(&line, 4, DC, true, 8, &mut dst, 4);
        assert!(dst.iter().all(|&v| v == 80));
    }
}
