//! The integer clock the per-pixel kernels run on.
//!
//! The reference keeps every per-pixel time in float64, because a float32
//! holding 4259 s only resolves a quarter of a millisecond and the tests
//! compare ages against windows of 0.125 s and 1 s to the millisecond. GPUs
//! have no float64, so the kernels here keep time as **unsigned 32-bit
//! microseconds** instead. Integer subtraction is exact, so an age measured
//! at second 4259 is the same number it would be at second 4, which is the
//! property the float64 rule was protecting.
//!
//! A u32 wraps after 71 minutes, so ages are computed with wrapping
//! subtraction and every stored time is periodically *saturated*: anything
//! older than [`AGE_MAX`] (about 18 minutes) is pulled forward to exactly
//! that age. Nothing the detector keeps is relevant past a few seconds, so
//! a saturated time behaves exactly like the reference's `-1e12` "never".
//! The saturation pass must run more often than every 53 minutes; the
//! detector runs it every 256 frames or 2^29 µs, whichever comes first.

/// Ages are clamped to this many microseconds (2^30 ≈ 17.9 min).
pub const AGE_MAX: u32 = 1 << 30;

/// Run the saturation pass at least this often (2^29 µs ≈ 9 min).
pub const SATURATE_EVERY_US: u32 = 1 << 29;
/// ... and at least every this many frames.
pub const SATURATE_EVERY_FRAMES: u32 = 256;

/// Microseconds since `t`, wrapping.
#[inline(always)]
pub fn age(now: u32, t: u32) -> u32 {
    now.wrapping_sub(t)
}

/// A time so old it reads as "never happened" (age == AGE_MAX).
#[inline(always)]
pub fn never(now: u32) -> u32 {
    now.wrapping_sub(AGE_MAX)
}

/// Pull a stored time forward so its age never exceeds AGE_MAX.
#[inline(always)]
pub fn saturate(now: u32, t: u32) -> u32 {
    if age(now, t) > AGE_MAX {
        never(now)
    } else {
        t
    }
}

/// Seconds -> whole microseconds (round to nearest), wrapping into u32.
#[inline]
pub fn secs_to_us(s: f64) -> u32 {
    ((s * 1e6).round() as i64) as u32
}

/// Seconds -> whole microseconds for a *duration* threshold (never wraps).
#[inline]
pub fn dur_to_us(s: f64) -> u32 {
    (s * 1e6).round().clamp(0.0, (AGE_MAX - 1) as f64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_ages() {
        let now = 5u32;
        let t = now.wrapping_sub(10);
        assert_eq!(age(now, t), 10);
        assert_eq!(age(0, never(0)), AGE_MAX);
        assert_eq!(saturate(0, never(0).wrapping_sub(1)), never(0));
        assert_eq!(saturate(100, 90), 90);
    }

    #[test]
    fn conversions() {
        assert_eq!(secs_to_us(0.125), 125_000);
        assert_eq!(dur_to_us(1.0 - 1e-3), 999_000);
        assert_eq!(secs_to_us(4259.0), 4_259_000_000);
    }
}
