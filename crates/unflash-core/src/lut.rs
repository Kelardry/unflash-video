//! sRGB -> linear lookup and the per-pixel colour quantities.
//!
//! The table is computed in f64 and stored as f32, exactly as the reference
//! does, so the CPU kernels and the GPU (which is handed this same table in
//! a read-only storage buffer) linearise identically.

static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();

/// The 256-entry sRGB linearisation table (f64 math, f32 storage).
pub fn lut() -> &'static [f32; 256] {
    LUT.get_or_init(|| {
        let mut t = [0f32; 256];
        for (c, slot) in t.iter_mut().enumerate() {
            let v = c as f64 / 255.0;
            let lin = if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
            *slot = lin as f32;
        }
        t
    })
}

/// sRGB's red primary in the CIE 1976 UCS diagram (u′, v′), from the
/// sRGB-to-XYZ matrix: the point the red tracker measures distance from.
pub const RED_U: f32 = 0.4507966;
pub const RED_V: f32 = 0.5228869;
/// A state's chromaticity as a run keeps it: u′ in 15 bits, v′ in 16, in
/// steps of 1/32768 and 1/65536 (every sRGB colour has u′ < 0.46 and v′ <
/// 0.6), with the saturated-red flag in the top bit.
pub const QU: f32 = 32768.0;
pub const QV: f32 = 65536.0;
pub const SAT_BIT: u32 = 1 << 31;
/// The least a red transition that qualifies moves `s`: one end saturated
/// (within 0.143 of the red primary), the ends more than 0.2 apart, and the
/// sRGB gamut a 60° wedge at the red primary leave it at 0.085 or more. The
/// held-frame test and the window-mean gate are set against it, as the
/// luminance ones are against the luminance swing.
pub const RED_LEAST_SWING: f32 = 0.08;

/// Per-pixel colour quantities the detector tracks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelValues {
    /// Relative luminance, linear, 0..1.
    pub l: f32,
    /// How far the colour is from sRGB's red primary in u′v′ (0 for pure
    /// red, 0.26 for any grey, black or white): the red tracker follows it.
    pub s: f32,
    /// The colour's chromaticity and saturation, packed (see `red_values`).
    pub c: u32,
    /// R / (R+G+B) >= red_saturation.
    pub sat: bool,
}

/// The relative luminance of a linear colour, in f32 in exactly the
/// reference's order: numpy evaluates `0.2126*R + 0.7152*G + 0.0722*B` left
/// to right in float32 (and so does the GPU's ingest pass).
#[inline(always)]
pub fn luminance(r: f32, g: f32, b: f32) -> f32 {
    0.2126f32 * r + 0.7152f32 * g + 0.0722f32 * b
}

/// Compute L and the red quantities from 8-bit sRGB.
#[inline]
pub fn pixel_values(r: u8, g: u8, b: u8, red_saturation: f32, flare: f32) -> PixelValues {
    let t = lut();
    let (r, g, b) = (t[r as usize], t[g as usize], t[b as usize]);
    let l = luminance(r, g, b);
    let (s, c) = red_values(r, g, b, red_saturation, flare);
    PixelValues { l, s, c, sat: c & SAT_BIT != 0 }
}

/// WCAG 2.2's red-flash quantities of one linear colour: (s, c).
///
/// A screen's black is never perfectly black, and black has no colour to
/// measure, so `flare` (a share of white) is added to every channel first,
/// as the light a screen and its room add. Then the colour's chromaticity
/// (u′, v′) in the CIE 1976 UCS diagram, from the sRGB-to-XYZ matrix; `s`,
/// its distance from sRGB's red primary; and `c`, the chromaticity packed
/// (u′ in bits 16..31, v′ in 0..16) with the saturated-red flag
/// R/(R+G+B) >= `red_saturation` in the top bit.
#[inline(always)]
pub fn red_values(r: f32, g: f32, b: f32, red_saturation: f32, flare: f32) -> (f32, u32) {
    let rf = r + flare;
    let gf = g + flare;
    let bf = b + flare;
    let x = 0.4124f32 * rf + 0.3576f32 * gf + 0.1805f32 * bf;
    let y = luminance(rf, gf, bf);
    let z = 0.0193f32 * rf + 0.1192f32 * gf + 0.9505f32 * bf;
    let d = x + 15.0f32 * y + 3.0f32 * z;
    let u = 4.0f32 * x / d;
    let v = 9.0f32 * y / d;
    let du = u - RED_U;
    let dv = v - RED_V;
    let s = (du * du + dv * dv).sqrt();
    let sat = rf >= red_saturation * (rf + gf + bf);
    let qu = ((u * QU + 0.5f32).floor() as u32).min(0x7fff);
    let qv = ((v * QV + 0.5f32).floor() as u32).min(0xffff);
    (s, if sat { SAT_BIT } else { 0 } | qu << 16 | qv)
}

/// The squared u′v′ distance between two states `red_values` packed.
#[inline(always)]
pub fn chroma_dist2(a: u32, b: u32) -> f32 {
    let du = (((a >> 16) & 0x7fff) as f32 - ((b >> 16) & 0x7fff) as f32) / QU;
    let dv = ((a & 0xffff) as f32 - (b & 0xffff) as f32) / QV;
    du * du + dv * dv
}

/// Whether a red run's two ends make a red transition under WCAG 2.2:
/// either end saturated red, and the ends more than `delta` apart in u′v′.
#[inline(always)]
pub fn red_transition(a: u32, b: u32, delta: f32) -> bool {
    (a | b) & SAT_BIT != 0 && chroma_dist2(a, b) > delta * delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lut_endpoints() {
        let t = lut();
        assert_eq!(t[0], 0.0);
        assert_eq!(t[255], 1.0);
        assert!((t[128] - 0.2158605).abs() < 1e-6);
    }

    const FLARE: f32 = 0.0035;

    #[test]
    fn white_and_red() {
        let w = pixel_values(255, 255, 255, 0.8, FLARE);
        assert!((w.l - 1.0).abs() < 1e-6);
        assert!(!w.sat);
        // any grey sits at the D65 white point, 0.259 from red
        assert!((w.s - 0.2588).abs() < 1e-3, "{}", w.s);
        let r = pixel_values(255, 0, 0, 0.8, FLARE);
        assert!(r.sat);
        assert!(r.s < 0.01, "{}", r.s);
        assert!((r.l - 0.2126).abs() < 1e-6);
        let k = pixel_values(0, 0, 0, 0.8, FLARE);
        assert!(!k.sat);
        assert!((k.s - w.s).abs() < 1e-4, "black has white's chromaticity: {} {}", k.s, w.s);
    }

    #[test]
    fn red_transitions_under_wcag_2_2() {
        let px = |r, g, b| pixel_values(r, g, b, 0.8, FLARE).c;
        let red = px(255, 0, 0);
        // red to black, white, grey (of the same luminance too), green, blue: red flashes
        for other in [px(0, 0, 0), px(255, 255, 255), px(128, 128, 128), px(0, 255, 0), px(0, 0, 255)] {
            assert!(red_transition(red, other, 0.2) && red_transition(other, red, 0.2));
        }
        // red to a darker red: the same chromaticity, not a red flash (a general one, if bright enough)
        assert!(!red_transition(red, px(120, 0, 0), 0.2));
        // two saturated reds are never 0.2 apart
        assert!(!red_transition(px(255, 60, 0), px(255, 0, 60), 0.2));
        // no saturated red at either end: not a red flash
        assert!(!red_transition(px(0, 255, 0), px(0, 0, 255), 0.2));
        // against black, pure red counts from about code 71, where WCAG 2.0's
        // (R-G-B)*320 > 20 puts it too
        let first = (0..=255u8).find(|&c| red_transition(px(c, 0, 0), px(0, 0, 0), 0.2)).unwrap();
        assert!((68..=74).contains(&first), "{first}");
    }
}
