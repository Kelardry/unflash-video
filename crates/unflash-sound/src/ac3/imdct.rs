//! The inverse transform, windowing and de-interleaving of A/52 §7.9.4:
//! one 512-sample transform for a block, or two 256-sample transforms
//! when the block is switched (`blksw`). Both are computed as the
//! standard describes them, with a pre-twiddle, an N/4-point (or two
//! N/8-point) complex inverse FFT and a post-twiddle; the overlap-add is
//! left to the caller.

use std::f64::consts::PI;

pub struct Imdct {
    /// xcos1[k], xsin1[k] for the 512-sample transform (step 2).
    tw1: [(f32, f32); 128],
    /// xcos2[k], xsin2[k] for the 256-sample transforms.
    tw2: [(f32, f32); 64],
    /// e^(+j 2 pi i / 128), i < 64, the inverse FFT's twiddles.
    fft: [(f32, f32); 64],
    rev128: [u8; 128],
    rev64: [u8; 64],
    window: [f32; 256],
}

fn bit_reverse<const N: usize>(bits: u32) -> [u8; N] {
    let mut t = [0u8; N];
    for (i, v) in t.iter_mut().enumerate() {
        *v = ((i as u32).reverse_bits() >> (32 - bits)) as u8;
    }
    t
}

impl Imdct {
    pub fn new() -> Imdct {
        let n = 512.0;
        let mut tw1 = [(0f32, 0f32); 128];
        for (k, t) in tw1.iter_mut().enumerate() {
            let a = 2.0 * PI * (8.0 * k as f64 + 1.0) / (8.0 * n);
            *t = (-a.cos() as f32, -a.sin() as f32);
        }
        let mut tw2 = [(0f32, 0f32); 64];
        for (k, t) in tw2.iter_mut().enumerate() {
            let a = 2.0 * PI * (8.0 * k as f64 + 1.0) / (4.0 * n);
            *t = (-a.cos() as f32, -a.sin() as f32);
        }
        let mut fft = [(0f32, 0f32); 64];
        for (i, t) in fft.iter_mut().enumerate() {
            let a = 2.0 * PI * i as f64 / 128.0;
            *t = (a.cos() as f32, a.sin() as f32);
        }
        Imdct { tw1, tw2, fft, rev128: bit_reverse::<128>(7), rev64: bit_reverse::<64>(6), window: crate::ac3::tables::window() }
    }

    /// In-place complex inverse FFT (no scaling) of `re`/`im`, whose
    /// length is 64 or 128, after the input was placed in bit-reversed
    /// order.
    fn ifft(&self, re: &mut [f32], im: &mut [f32]) {
        let n = re.len();
        let mut m = 2;
        while m <= n {
            let half = m / 2;
            let stride = 128 / m;
            for k in 0..half {
                let (wr, wi) = self.fft[k * stride];
                let mut s = k;
                while s < n {
                    let (ar, ai) = (re[s], im[s]);
                    let (br, bi) = (re[s + half], im[s + half]);
                    let tr = br * wr - bi * wi;
                    let ti = br * wi + bi * wr;
                    re[s + half] = ar - tr;
                    im[s + half] = ai - ti;
                    re[s] = ar + tr;
                    im[s] = ai + ti;
                    s += m;
                }
            }
            m *= 2;
        }
    }

    /// One 512-sample inverse transform of `x`'s 256 coefficients: the
    /// windowed samples of the whole block (§7.9.4.1 steps 2 to 5).
    pub fn long(&self, x: &[f32; 256], out: &mut [f32; 512]) {
        let (mut re, mut im) = ([0f32; 128], [0f32; 128]);
        for k in 0..128 {
            let (c, s) = self.tw1[k];
            let (a, b) = (x[255 - 2 * k], x[2 * k]);
            let j = self.rev128[k] as usize;
            re[j] = a * c - b * s;
            im[j] = b * c + a * s;
        }
        self.ifft(&mut re, &mut im);
        let (mut yr, mut yi) = ([0f32; 128], [0f32; 128]);
        for n in 0..128 {
            let (c, s) = self.tw1[n];
            yr[n] = re[n] * c - im[n] * s;
            yi[n] = im[n] * c + re[n] * s;
        }
        let w = &self.window;
        for n in 0..64 {
            out[2 * n] = -yi[64 + n] * w[2 * n];
            out[2 * n + 1] = yr[63 - n] * w[2 * n + 1];
            out[128 + 2 * n] = -yr[n] * w[128 + 2 * n];
            out[128 + 2 * n + 1] = yi[127 - n] * w[128 + 2 * n + 1];
            out[256 + 2 * n] = -yr[64 + n] * w[255 - 2 * n];
            out[256 + 2 * n + 1] = yi[63 - n] * w[254 - 2 * n];
            out[384 + 2 * n] = yi[n] * w[127 - 2 * n];
            out[384 + 2 * n + 1] = -yr[127 - n] * w[126 - 2 * n];
        }
    }

    /// The two 256-sample inverse transforms of a switched block, whose
    /// coefficients arrive interleaved (§7.9.4.2).
    pub fn short(&self, x: &[f32; 256], out: &mut [f32; 512]) {
        let mut y = [[[0f32; 64]; 2]; 2]; // [transform][re, im][n]
        for (t, yt) in y.iter_mut().enumerate() {
            let (mut re, mut im) = ([0f32; 64], [0f32; 64]);
            for k in 0..64 {
                let (c, s) = self.tw2[k];
                // X1[k] = X[2k], X2[k] = X[2k+1]
                let (a, b) = (x[2 * (127 - 2 * k) + t], x[2 * (2 * k) + t]);
                let j = self.rev64[k] as usize;
                re[j] = a * c - b * s;
                im[j] = b * c + a * s;
            }
            self.ifft(&mut re, &mut im);
            for n in 0..64 {
                let (c, s) = self.tw2[n];
                yt[0][n] = re[n] * c - im[n] * s;
                yt[1][n] = im[n] * c + re[n] * s;
            }
        }
        let w = &self.window;
        let (yr1, yi1, yr2, yi2) = (&y[0][0], &y[0][1], &y[1][0], &y[1][1]);
        for n in 0..64 {
            out[2 * n] = -yi1[n] * w[2 * n];
            out[2 * n + 1] = yr1[63 - n] * w[2 * n + 1];
            out[128 + 2 * n] = -yr1[n] * w[128 + 2 * n];
            out[128 + 2 * n + 1] = yi1[63 - n] * w[128 + 2 * n + 1];
            out[256 + 2 * n] = -yr2[n] * w[255 - 2 * n];
            out[256 + 2 * n + 1] = yi2[63 - n] * w[254 - 2 * n];
            out[384 + 2 * n] = yi2[n] * w[127 - 2 * n];
            out[384 + 2 * n + 1] = -yr2[63 - n] * w[126 - 2 * n];
        }
    }
}

impl Default for Imdct {
    fn default() -> Self {
        Imdct::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny deterministic generator for test signals.
    fn noise(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }

    /// §7.9.4.1 written out literally, with the IFFT as a plain sum, in
    /// double precision.
    fn literal_long(x: &[f32; 256]) -> [f64; 512] {
        let n = 512usize;
        let w: Vec<f64> = crate::ac3::tables::window().iter().map(|&v| v as f64).collect();
        let xc = |k: usize| -(2.0 * PI * (8.0 * k as f64 + 1.0) / (8.0 * n as f64)).cos();
        let xs = |k: usize| -(2.0 * PI * (8.0 * k as f64 + 1.0) / (8.0 * n as f64)).sin();
        let mut zr = vec![0.0; n / 4];
        let mut zi = vec![0.0; n / 4];
        for k in 0..n / 4 {
            let a = x[n / 2 - 2 * k - 1] as f64;
            let b = x[2 * k] as f64;
            zr[k] = a * xc(k) - b * xs(k);
            zi[k] = b * xc(k) + a * xs(k);
        }
        let mut yr = vec![0.0; n / 4];
        let mut yi = vec![0.0; n / 4];
        for m in 0..n / 4 {
            let (mut r, mut i) = (0.0, 0.0);
            for k in 0..n / 4 {
                let a = 8.0 * PI * (k * m) as f64 / n as f64;
                r += zr[k] * a.cos() - zi[k] * a.sin();
                i += zi[k] * a.cos() + zr[k] * a.sin();
            }
            yr[m] = r * xc(m) - i * xs(m);
            yi[m] = i * xc(m) + r * xs(m);
        }
        let mut out = [0.0; 512];
        for m in 0..n / 8 {
            out[2 * m] = -yi[n / 8 + m] * w[2 * m];
            out[2 * m + 1] = yr[n / 8 - m - 1] * w[2 * m + 1];
            out[n / 4 + 2 * m] = -yr[m] * w[n / 4 + 2 * m];
            out[n / 4 + 2 * m + 1] = yi[n / 4 - m - 1] * w[n / 4 + 2 * m + 1];
            out[n / 2 + 2 * m] = -yr[n / 8 + m] * w[n / 2 - 2 * m - 1];
            out[n / 2 + 2 * m + 1] = yi[n / 8 - m - 1] * w[n / 2 - 2 * m - 2];
            out[3 * n / 4 + 2 * m] = yi[m] * w[n / 4 - 2 * m - 1];
            out[3 * n / 4 + 2 * m + 1] = -yr[n / 4 - m - 1] * w[n / 4 - 2 * m - 2];
        }
        out
    }

    #[test]
    fn fast_long_transform_matches_the_literal_one() {
        let t = Imdct::new();
        let mut seed = 7;
        let mut x = [0f32; 256];
        for v in x.iter_mut() {
            *v = noise(&mut seed) as f32;
        }
        let mut fast = [0f32; 512];
        t.long(&x, &mut fast);
        let lit = literal_long(&x);
        for i in 0..512 {
            assert!((fast[i] as f64 - lit[i]).abs() < 2e-5, "{i}: {} {}", fast[i], lit[i]);
        }
    }

    /// The forward transform of §8.2.3.2 followed by these inverse
    /// transforms and the overlap-add of §7.9.4 gives the input back, for
    /// long and switched blocks in any order.
    #[test]
    fn transforms_reconstruct_the_signal() {
        let t = Imdct::new();
        let w: Vec<f64> = t.window.iter().map(|&v| v as f64).collect();
        let win = |i: usize| if i < 256 { w[i] } else { w[511 - i] };
        let mut seed = 3;
        let blocks = 8;
        let signal: Vec<f64> = (0..256 * (blocks + 1)).map(|_| noise(&mut seed) * 0.5).collect();
        let switched = [false, true, true, false, true, false, false, true];
        let mut delay = [0f64; 256];
        let mut output = Vec::new();
        for (b, &sw) in switched.iter().enumerate() {
            let seg: Vec<f64> = (0..512).map(|i| signal[256 * b + i] * win(i)).collect();
            let mut coef = [0f32; 256];
            // X_D[k] = -2/N sum x[n] cos(2 pi/(4N) (2n+1)(2k+1) + pi/4 (2k+1)(1+alpha))
            let fwd = |x: &[f64], n: usize, alpha: f64, k: usize| -> f64 {
                let mut s = 0.0;
                for (i, &v) in x.iter().enumerate() {
                    let a = 2.0 * PI / (4.0 * n as f64) * (2 * i + 1) as f64 * (2 * k + 1) as f64 + PI / 4.0 * (2 * k + 1) as f64 * (1.0 + alpha);
                    s += v * a.cos();
                }
                -2.0 / n as f64 * s
            };
            if sw {
                for k in 0..128 {
                    coef[2 * k] = fwd(&seg[..256], 256, -1.0, k) as f32;
                    coef[2 * k + 1] = fwd(&seg[256..], 256, 1.0, k) as f32;
                }
            } else {
                for (k, c) in coef.iter_mut().enumerate() {
                    *c = fwd(&seg, 512, 0.0, k) as f32;
                }
            }
            let mut x = [0f32; 512];
            if sw {
                t.short(&coef, &mut x);
            } else {
                t.long(&coef, &mut x);
            }
            for n in 0..256 {
                output.push(2.0 * (x[n] as f64 + delay[n]));
                delay[n] = x[256 + n] as f64;
            }
        }
        // block b's output is the signal's samples 256 b .. 256 b + 256
        for i in 256..output.len() {
            assert!((output[i] - signal[i]).abs() < 1e-4, "{i}: {} vs {}", output[i], signal[i]);
        }
    }
}
