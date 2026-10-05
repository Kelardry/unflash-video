//! The 32-band synthesis filter bank (clause C.3.6) and the interpolation
//! of the LFE channel (clause C.3.7).

use crate::dts::fir;

/// The cosine modulation of C.3.6, computed once.
pub struct Modulation {
    /// cos((2i + 1)(2k + 1) pi / 64), [k][i].
    odd: [[f64; 16]; 16],
    /// cos(i (2k + 1) pi / 32), [k][i].
    even: [[f64; 16]; 16],
    /// 0.25 / (2 cos((2k + 1) pi / 128)) and -0.25 / (2 sin((2k + 1) pi / 128)).
    sum: [f64; 16],
    diff: [f64; 16],
}

impl Modulation {
    pub fn new() -> Modulation {
        let pi = std::f64::consts::PI;
        let mut m = Modulation { odd: [[0.0; 16]; 16], even: [[0.0; 16]; 16], sum: [0.0; 16], diff: [0.0; 16] };
        for k in 0..16 {
            for i in 0..16 {
                m.odd[k][i] = ((2 * i + 1) as f64 * (2 * k + 1) as f64 * pi / 64.0).cos();
                m.even[k][i] = (i as f64 * (2 * k + 1) as f64 * pi / 32.0).cos();
            }
            m.sum[k] = 0.25 / (2.0 * ((2 * k + 1) as f64 * pi / 128.0).cos());
            m.diff[k] = -0.25 / (2.0 * ((2 * k + 1) as f64 * pi / 128.0).sin());
        }
        m
    }
}

/// One channel's synthesis filter bank: the history of C.3.6's modulated
/// samples (`x`, 16 blocks of 32, newest first) and the overlapping part
/// of the output (`z`).
#[derive(Clone)]
pub struct Qmf {
    x: [f64; 512],
    z: [f64; 64],
}

impl Default for Qmf {
    fn default() -> Self {
        Qmf { x: [0.0; 512], z: [0.0; 64] }
    }
}

impl Qmf {
    pub fn reset(&mut self) {
        *self = Qmf::default();
    }

    /// Turn one sample of each of the 32 subbands into 32 output samples,
    /// with the prototype `coeff` (`fir::PERFECT` or `fir::NONPERFECT`).
    pub fn synthesize(&mut self, m: &Modulation, input: &[f64; 32], coeff: &[f64; 512], out: &mut [f64]) {
        // the cosine modulation, as sums and differences of neighbours
        for k in 0..16 {
            let (mut a, mut b) = (0.0, 0.0);
            for i in 0..16 {
                a += (input[2 * i] + input[2 * i + 1]) * m.odd[k][i];
                let prev = if i > 0 { input[2 * i - 1] } else { 0.0 };
                b += (input[2 * i] + prev) * m.even[k][i];
            }
            self.x[k] = m.sum[k] * (a + b);
            self.x[31 - k] = m.diff[k] * (a - b);
        }
        // the prototype filter
        for i in 0..32 {
            let k = 31 - i;
            let (mut lo, mut hi) = (0.0, 0.0);
            for j in (0..512).step_by(64) {
                lo += coeff[i + j] * (self.x[i + j] - self.x[j + k]);
                hi += coeff[32 + i + j] * (-self.x[i + j] - self.x[j + k]);
            }
            self.z[i] += lo;
            self.z[32 + i] += hi;
        }
        out[..32].copy_from_slice(&self.z[..32]);
        self.z.copy_within(32.., 0);
        self.z[32..].fill(0.0);
        self.x.copy_within(..480, 32);
    }
}

/// The LFE channel's interpolation: the decimated samples of the frames
/// so far that the filter still reaches (newest last).
#[derive(Clone, Default)]
pub struct Lfe {
    history: [f64; 8],
}

impl Lfe {
    pub fn reset(&mut self) {
        self.history = [0.0; 8];
    }

    /// Interpolate `input` by `factor` (64 or 128) into `out`
    /// (`factor * input.len()` samples).
    pub fn interpolate(&mut self, input: &[f64], factor: usize, out: &mut [f64]) {
        let coeff = if factor == 128 { &fir::LFE_128 } else { &fir::LFE_64 };
        let taps = 512 / factor;
        for (n, &s) in input.iter().enumerate() {
            self.history.copy_within(1.., 0);
            self.history[7] = s;
            for k in 0..factor {
                let mut acc = 0.0;
                for j in 0..taps {
                    acc += self.history[7 - j] * coeff[k + j * factor];
                }
                out[n * factor + k] = acc;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The synthesis of one channel as a matrix: its output for a unit
    /// sample in each subband and time slot `first..first + slots` of
    /// `total` slots.
    fn impulse_responses(coeff: &[f64; 512], first: usize, slots: usize, total: usize) -> Vec<Vec<f64>> {
        let m = Modulation::new();
        let mut cols = Vec::new();
        for t in first..first + slots {
            for band in 0..32 {
                let mut q = Qmf::default();
                let mut out = vec![0.0; total * 32];
                for s in 0..total {
                    let mut input = [0.0; 32];
                    if s == t {
                        input[band] = 1.0;
                    }
                    q.synthesize(&m, &input, coeff, &mut out[s * 32..]);
                }
                cols.push(out);
            }
        }
        cols
    }

    /// The perfect reconstruction prototype makes an orthogonal synthesis
    /// (so an analysis bank undoes it exactly): unit impulses in any
    /// subband and slot come out with the same energy, 1/1024, and
    /// orthogonal to each other. The non-perfect one comes within 5e-5.
    #[test]
    fn perfect_reconstruction_bank_is_orthogonal() {
        for (name, coeff, tolerance) in [("perfect", &fir::PERFECT, 1e-7), ("non-perfect", &fir::NONPERFECT, 5e-5)] {
            let cols = impulse_responses(coeff, 16, 3, 40);
            let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
            let mut worst: f64 = 0.0;
            for (i, a) in cols.iter().enumerate() {
                assert!((dot(a, a) * 1024.0 - 1.0).abs() < 1e-3, "{name}: energy {}", dot(a, a));
                for b in &cols[i + 1..] {
                    worst = worst.max(dot(a, b).abs() * 1024.0);
                }
            }
            assert!(worst < tolerance, "{name}: {worst:e}");
        }
    }

    /// A constant decimated LFE signal interpolates to the same constant
    /// once the filter is full, at either factor.
    #[test]
    fn lfe_interpolation_keeps_a_constant() {
        for factor in [64, 128] {
            let mut lfe = Lfe::default();
            let input = vec![1.0; 16];
            let mut out = vec![0.0; 16 * factor];
            lfe.interpolate(&input, factor, &mut out);
            let tail = &out[8 * factor..];
            let (lo, hi) = tail.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
            assert!(lo > 0.99 && hi < 1.01, "{factor}: {lo} {hi}");
        }
    }
}
