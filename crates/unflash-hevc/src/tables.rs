//! Constant tables: scan orders (6.5.3 – 6.5.5), the default scaling
//! lists (Table 7-5, Table 7-6) and the chroma QP mapping (Table 8-10).

/// QpC for qPi 30..=43 (Table 8-10).
const QPC: [i32; 14] = [29, 30, 31, 32, 33, 33, 34, 34, 35, 35, 36, 36, 37, 37];

/// QpC as a function of qPi for 4:2:0 (Table 8-10).
pub fn qpc(qpi: i32) -> i32 {
    if qpi < 30 {
        qpi
    } else if qpi > 43 {
        qpi - 6
    } else {
        QPC[(qpi - 30) as usize]
    }
}

/// A scan of an n×n block: `(x, y)` per scan position.
pub type Scan<const N: usize> = [(u8, u8); N];

/// 6.5.3: the up-right diagonal scan of a `S`×`S` block (`N` = S²).
const fn diagonal<const N: usize>(s: usize) -> Scan<N> {
    let mut out = [(0u8, 0u8); N];
    let mut i = 0;
    let mut x: isize = 0;
    let mut y: isize = 0;
    while i < N {
        while y >= 0 {
            if (x as usize) < s && (y as usize) < s {
                out[i] = (x as u8, y as u8);
                i += 1;
            }
            y -= 1;
            x += 1;
        }
        y = x;
        x = 0;
    }
    out
}

/// 6.5.4: the horizontal (row by row) scan.
const fn horizontal<const N: usize>(s: usize) -> Scan<N> {
    let mut out = [(0u8, 0u8); N];
    let mut i = 0;
    while i < N {
        out[i] = ((i % s) as u8, (i / s) as u8);
        i += 1;
    }
    out
}

/// 6.5.5: the vertical (column by column) scan.
const fn vertical<const N: usize>(s: usize) -> Scan<N> {
    let mut out = [(0u8, 0u8); N];
    let mut i = 0;
    while i < N {
        out[i] = ((i / s) as u8, (i % s) as u8);
        i += 1;
    }
    out
}

pub const DIAG2: Scan<4> = diagonal::<4>(2);
pub const DIAG4: Scan<16> = diagonal::<16>(4);
pub const DIAG8: Scan<64> = diagonal::<64>(8);
pub const HOR2: Scan<4> = horizontal::<4>(2);
pub const HOR4: Scan<16> = horizontal::<16>(4);
pub const VER2: Scan<4> = vertical::<4>(2);
pub const VER4: Scan<16> = vertical::<16>(4);

/// `ScanOrder[2][scanIdx]`: positions inside a 4x4 sub-block, per scanIdx
/// (0 up-right diagonal, 1 horizontal, 2 vertical).
pub const SCAN4X4: [Scan<16>; 3] = [DIAG4, HOR4, VER4];

/// The scan position of each raster position of a 4x4 block, per scanIdx.
pub const SCAN4X4_POS: [[u8; 16]; 3] = {
    let mut t = [[0u8; 16]; 3];
    let mut s = 0;
    while s < 3 {
        let mut n = 0;
        while n < 16 {
            let (x, y) = SCAN4X4[s][n];
            t[s][y as usize * 4 + x as usize] = n as u8;
            n += 1;
        }
        s += 1;
    }
    t
};

/// Table 7-6: the default 8x8 intra scaling list (diagonal scan order).
pub const DEFAULT_INTRA_8X8: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25, 27, 30, 27, 25, 25, 29, 31, 35, 35, 31, 29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
/// Table 7-6: the default 8x8 inter scaling list.
pub const DEFAULT_INTER_8X8: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 28, 28, 28, 28, 28, 28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagonal_scans() {
        assert_eq!(DIAG4[..6], [(0, 0), (0, 1), (1, 0), (0, 2), (1, 1), (2, 0)]);
        assert_eq!(DIAG4[15], (3, 3));
        assert_eq!(DIAG8[63], (7, 7));
        assert_eq!(DIAG2, [(0, 0), (0, 1), (1, 0), (1, 1)]);
        assert_eq!(HOR4[5], (1, 1));
        assert_eq!(VER4[4], (1, 0));
        for s in 0..3 {
            for (n, &(x, y)) in SCAN4X4[s].iter().enumerate() {
                assert_eq!(SCAN4X4_POS[s][y as usize * 4 + x as usize] as usize, n);
            }
        }
    }
}
