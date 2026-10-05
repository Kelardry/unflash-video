//! Taking the contrast out of flashing instead of taking frames out.
//!
//! A frame marked to be blended is mixed with what its unmarked neighbours
//! show: the nearest unmarked frame before it and the nearest after,
//! interpolated by where it sits between them. At full strength a run of
//! marked frames becomes a crossfade between the frames either side of it
//! (the flash gone, the timing kept); at less, the flash stays in part, with
//! less contrast, and so does whatever it shows (a line of subtitles).
//!
//! The blend is linear in 8-bit sRGB code values, the space in which the
//! detector averages pixels down to its analysis size, so a blend of the
//! small cached pictures is (to rounding) the small picture of the same
//! blend made at full size: the section check predicts the export.

/// What a blended frame's picture is mixed with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlendSource {
    /// The nearest unmarked frame before it.
    pub prev: Option<usize>,
    /// The nearest unmarked frame after it.
    pub next: Option<usize>,
    /// How far from `prev` to `next` it sits, 0 to 1.
    pub u: f32,
}

/// For each of `marked.len()` frames: `None` when it is not blended (or no
/// frame around it is unmarked), else what it is mixed with.
pub fn blend_sources(marked: &[bool]) -> Vec<Option<BlendSource>> {
    let n = marked.len();
    let mut out = vec![None; n];
    let mut prev: Option<usize> = None;
    let mut i = 0;
    while i < n {
        if !marked[i] {
            prev = Some(i);
            i += 1;
            continue;
        }
        // a run of marked frames, i..j
        let mut j = i;
        while j < n && marked[j] {
            j += 1;
        }
        let next = (j < n).then_some(j);
        if prev.is_some() || next.is_some() {
            for (k, slot) in out.iter_mut().enumerate().take(j).skip(i) {
                let u = match (prev, next) {
                    (Some(p), Some(q)) => (k - p) as f32 / (q - p) as f32,
                    (None, Some(_)) => 1.0,
                    _ => 0.0,
                };
                *slot = Some(BlendSource { prev, next, u });
            }
        }
        i = j;
    }
    out
}

/// The weights of (the frame itself, `prev`, `next`) at strength `s`
/// (0: the frame as it is, 1: all neighbour).
pub fn blend_weights(src: &BlendSource, s: f32) -> (f32, f32, f32) {
    let s = s.clamp(0.0, 1.0);
    match (src.prev, src.next) {
        (Some(_), Some(_)) => (1.0 - s, s * (1.0 - src.u), s * src.u),
        (Some(_), None) => (1.0 - s, s, 0.0),
        (None, Some(_)) => (1.0 - s, 0.0, s),
        (None, None) => (1.0, 0.0, 0.0),
    }
}

/// `out = round(w0·a + w1·b + w2·c)` byte by byte (a missing picture has
/// no weight).
pub fn mix(out: &mut Vec<u8>, a: &[u8], b: Option<&[u8]>, c: Option<&[u8]>, w: (f32, f32, f32)) {
    out.clear();
    out.reserve(a.len());
    match (b, c) {
        (Some(b), Some(c)) => out.extend(a.iter().zip(b).zip(c).map(|((&x, &y), &z)| (w.0 * x as f32 + w.1 * y as f32 + w.2 * z as f32 + 0.5).min(255.0) as u8)),
        (Some(b), None) => out.extend(a.iter().zip(b).map(|(&x, &y)| (w.0 * x as f32 + w.1 * y as f32 + 0.5).min(255.0) as u8)),
        (None, Some(c)) => out.extend(a.iter().zip(c).map(|(&x, &z)| (w.0 * x as f32 + w.2 * z as f32 + 0.5).min(255.0) as u8)),
        (None, None) => out.extend_from_slice(a),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use crate::detector::CpuDetector;
    use crate::grid::FrameInput;

    #[test]
    fn sources_and_weights() {
        let m = [false, true, true, false, true, false, false, true];
        let s = blend_sources(&m);
        assert_eq!(s[0], None);
        assert_eq!(s[1], Some(BlendSource { prev: Some(0), next: Some(3), u: 1.0 / 3.0 }));
        assert_eq!(s[2], Some(BlendSource { prev: Some(0), next: Some(3), u: 2.0 / 3.0 }));
        assert_eq!(s[4], Some(BlendSource { prev: Some(3), next: Some(5), u: 0.5 }));
        // the last frame has nothing after it: toward the one before
        assert_eq!(s[7], Some(BlendSource { prev: Some(6), next: None, u: 0.0 }));
        assert_eq!(blend_weights(&s[4].unwrap(), 1.0), (0.0, 0.5, 0.5));
        assert_eq!(blend_weights(&s[7].unwrap(), 0.25), (0.75, 0.25, 0.0));
        assert!(blend_sources(&[true, true]).iter().all(|x| x.is_none()));
        let mut out = Vec::new();
        mix(&mut out, &[200, 0], Some(&[0, 100]), Some(&[100, 200]), (0.5, 0.25, 0.25));
        assert_eq!(out, vec![125, 75]);
    }

    /// A strobe over most of the picture, 8 flashes a second: blending its
    /// light frames at full strength takes the flashing out, at none it stays.
    #[test]
    fn blending_the_flash_frames_passes_the_detector() {
        let (w, h) = (64u32, 48u32);
        let frame = |code: u8| -> Vec<u8> {
            let mut f = vec![30u8; (w * h * 3) as usize];
            for y in 0..h {
                for x in 0..w * 3 / 4 {
                    let k = ((y * w + x) * 3) as usize;
                    f[k..k + 3].copy_from_slice(&[code, code, code]);
                }
            }
            f
        };
        let frames: Vec<Vec<u8>> = (0..72).map(|i| frame(if i % 3 == 1 { 230 } else { 25 })).collect();
        let marked: Vec<bool> = (0..72).map(|i| i % 3 == 1).collect();
        let sources = blend_sources(&marked);
        let run = |s: f32| {
            let mut out = Vec::new();
            let pics: Vec<Vec<u8>> = (0..72)
                .map(|i| match sources[i] {
                    Some(src) => {
                        mix(&mut out, &frames[i], src.prev.map(|p| &frames[p][..]), src.next.map(|q| &frames[q][..]), blend_weights(&src, s));
                        out.clone()
                    }
                    None => frames[i].clone(),
                })
                .collect();
            CpuDetector::analyze(Profile::Wcag.config(), w, h, pics.iter().enumerate().map(|(i, p)| (i as f64 / 24.0, FrameInput::rgb(p))))
        };
        assert!(!run(0.0).safe(), "the strobe fails as it is");
        assert!(run(1.0).safe(), "blended all the way, it passes");
        // most of the way is enough here: the flash stays, too weak to count
        assert!(run(0.97).safe());
    }
}
