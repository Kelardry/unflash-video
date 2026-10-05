//! What both decoders do with a frame's channels once it is decoded: size
//! the output for the frame's layout, and mix the channels down to Lo/Ro.

use crate::Output;

/// Size `out` for a frame of `channels` channels in `Output::Native` (2 in
/// `Output::Stereo`), as the decoders' `decode` promises: new channels
/// start with silence as long as the others so far, and channels past the
/// count are removed with their samples. `sized` is whether this call to
/// `decode` sized `out` already: the first frame of a call does in any case
/// (every channel as long as the longest), later ones when the count
/// changes.
pub(crate) fn fit(output: Output, channels: usize, out: &mut Vec<Vec<f32>>, sized: &mut bool) {
    let n = match output {
        Output::Stereo => 2,
        Output::Native => channels,
    };
    if out.len() != n || !*sized {
        let len = out.iter().map(|c| c.len()).max().unwrap_or(0);
        out.truncate(n);
        while out.len() < n {
            out.push(Vec::new());
        }
        for c in out.iter_mut() {
            c.resize(len.max(c.len()), 0.0);
        }
        *sized = true;
    }
}

/// Scale a stereo downmix (the gains of each coded channel into the left
/// and right outputs) so that neither output's gains sum to more than 1
/// (then channels at full scale mix to full scale at most).
pub(crate) fn limit_gains<const N: usize>(g: &mut [[f32; N]; 2]) {
    let sum = g[0].iter().sum::<f32>().max(g[1].iter().sum::<f32>());
    if sum > 1.0 {
        for row in g.iter_mut() {
            for v in row.iter_mut() {
                *v /= sum;
            }
        }
    }
}

/// Append `samples` samples to each channel of `out`: the frame's coded
/// channels `pcm`, each times its gain in that output's row of `gains`,
/// added up in channel order (as far as both the row and `pcm` go; a gain
/// of 0 leaves its channel out).
pub(crate) fn mix<G: AsRef<[f32]>>(gains: &[G], pcm: &[Vec<f32>], samples: usize, out: &mut [Vec<f32>]) {
    for (o, row) in gains.iter().enumerate() {
        let start = out[o].len();
        out[o].resize(start + samples, 0.0);
        let dst = &mut out[o][start..];
        for (&gain, src) in row.as_ref().iter().zip(pcm) {
            if gain == 0.0 {
                continue;
            }
            for (d, &s) in dst.iter_mut().zip(src) {
                *d += gain * s;
            }
        }
    }
}
