//! The per-pixel kernel, eight lanes at a time, on the `wide` crate: SSE /
//! AVX natively, simd128 on wasm32. Every decision is a blend on a lane
//! mask, so the eight pixels never diverge, and the flash rings are only
//! touched when some lane in the chunk actually completes a flash.
//!
//! Bit-exact with [`crate::pixel::run_frame_scalar`] (tested on random
//! inputs); the frame's tail (`n % 8` pixels) runs through the scalar code.

use bytemuck::cast;
use wide::{f32x8, i32x8, u32x8, CmpGe, CmpGt, CmpLe, CmpLt};

use crate::grid::{
    MASK_EXT_GEN, MASK_EXT_RED, MASK_POOL_GEN_DN, MASK_POOL_GEN_UP, MASK_POOL_RED_DN, MASK_POOL_RED_UP,
    MASK_STROBE_GEN, MASK_STROBE_RED,
};
use crate::pixel::{
    run_range_scalar, FramePlanes, KernelParams, PixelOutputs, PixelState, DIR_MASK, DN, GEN_PEND_SHIFT,
    LUM_DIR_SHIFT, POOL_GEN_SHIFT, POOL_RED_SHIFT, RED_DIR_SHIFT, RED_PEND_SHIFT, UP,
};
use crate::lut::{QU, QV, SAT_BIT};
use crate::time::AGE_MAX;

const LANES: usize = 8;

#[inline(always)]
fn ld_u(s: &[u32], i: usize) -> u32x8 {
    u32x8::from(<[u32; 8]>::try_from(&s[i..i + 8]).unwrap())
}
#[inline(always)]
fn ld_f(s: &[f32], i: usize) -> f32x8 {
    f32x8::from(<[f32; 8]>::try_from(&s[i..i + 8]).unwrap())
}
#[inline(always)]
fn st_u(s: &mut [u32], i: usize, v: u32x8) {
    s[i..i + 8].copy_from_slice(&v.to_array());
}
#[inline(always)]
fn st_f(s: &mut [f32], i: usize, v: f32x8) {
    s[i..i + 8].copy_from_slice(&v.to_array());
}
#[inline(always)]
fn fm(m: f32x8) -> u32x8 {
    cast(m)
}
#[inline(always)]
fn bl_u(m: u32x8, t: u32x8, f: u32x8) -> u32x8 {
    m.blend(t, f)
}
#[inline(always)]
fn bl_f(m: u32x8, t: f32x8, f: f32x8) -> f32x8 {
    cast::<u32x8, f32x8>(m).blend(t, f)
}
/// `age(now, t) <= limit` as a lane mask.
#[inline(always)]
fn age_le(now: u32x8, t: u32x8, limit: u32x8) -> u32x8 {
    !(now - t).cmp_gt(limit)
}
#[inline(always)]
fn age_gt(now: u32x8, t: u32x8, limit: u32x8) -> u32x8 {
    (now - t).cmp_gt(limit)
}
#[inline(always)]
fn age_lt(now: u32x8, t: u32x8, limit: u32x8) -> u32x8 {
    (now - t).cmp_lt(limit)
}

/// [`crate::lut::red_transition`] on eight pairs of packed states: either
/// end saturated red, the ends more than `delta` apart in u′v′.
#[inline(always)]
fn red_transition8(a: u32x8, b: u32x8, delta: f32x8) -> u32x8 {
    let zero = u32x8::splat(0);
    let sat = !((a | b) & u32x8::splat(SAT_BIT)).cmp_eq(zero);
    let m15 = u32x8::splat(0x7fff);
    let m16 = u32x8::splat(0xffff);
    let ia: i32x8 = cast((a >> 16) & m15);
    let ib: i32x8 = cast((b >> 16) & m15);
    let va: i32x8 = cast(a & m16);
    let vb: i32x8 = cast(b & m16);
    // the same operations as the scalar code: each coordinate to f32, the
    // difference, then the scale
    let du = (f32x8::from_i32x8(ia) - f32x8::from_i32x8(ib)) / f32x8::splat(QU);
    let dv = (f32x8::from_i32x8(va) - f32x8::from_i32x8(vb)) / f32x8::splat(QV);
    sat & fm((du * du + dv * dv).cmp_gt(delta * delta))
}

struct Tr8 {
    dir: u32x8,
    base: f32x8,
    ext: f32x8,
    base_t: u32x8,
    aux_base: u32x8,
    aux_ext: u32x8,
}

struct TrOut8 {
    rev_up: u32x8,
    rev_dn: u32x8,
    sbase: f32x8,
    sext: f32x8,
    sab: u32x8,
    sae: u32x8,
}

#[inline(always)]
fn tracker_feed8(t: &mut Tr8, x: f32x8, aux: u32x8, now: u32x8, eps: f32x8, max_run: u32x8) -> TrOut8 {
    let up = u32x8::splat(UP);
    let dn = u32x8::splat(DN);
    let zero = u32x8::splat(0);
    let rising = t.dir.cmp_eq(up);
    let falling = t.dir.cmp_eq(dn);
    let flat = t.dir.cmp_eq(zero);
    let new_hi = rising & fm(x.cmp_ge(t.ext));
    let new_lo = falling & fm(x.cmp_le(t.ext));
    let moved = new_hi | new_lo;
    t.ext = bl_f(moved, x, t.ext);
    t.aux_ext = bl_u(moved, aux, t.aux_ext);
    let rev_up = rising & fm(x.cmp_lt(t.ext - eps));
    let rev_dn = falling & fm(x.cmp_gt(t.ext + eps));
    let sbase = t.base;
    let sext = t.ext;
    let sab = t.aux_base;
    let sae = t.aux_ext;
    let rev = rev_up | rev_dn;
    t.base = bl_f(rev, t.ext, t.base);
    t.ext = bl_f(rev, x, t.ext);
    t.base_t = bl_u(rev, now, t.base_t);
    t.dir = bl_u(rev_up, dn, bl_u(rev_dn, up, t.dir));
    t.aux_base = bl_u(rev, t.aux_ext, t.aux_base);
    t.aux_ext = bl_u(rev, aux, t.aux_ext);
    let go_up = flat & fm(x.cmp_gt(t.base + eps));
    let go_dn = flat & fm(x.cmp_lt(t.base - eps));
    let started = go_up | go_dn;
    t.dir = bl_u(go_up, up, bl_u(go_dn, dn, t.dir));
    t.ext = bl_f(started, x, t.ext);
    t.aux_ext = bl_u(started, aux, t.aux_ext);
    let stale = age_gt(now, t.base_t, max_run);
    let idle = t.dir.cmp_eq(zero) & stale;
    t.ext = bl_f(idle, x, t.ext);
    t.aux_ext = bl_u(idle, aux, t.aux_ext);
    t.base = bl_f(stale, t.ext, t.base);
    t.base_t = bl_u(stale, now, t.base_t);
    t.aux_base = bl_u(stale, t.aux_ext, t.aux_base);
    TrOut8 { rev_up, rev_dn, sbase, sext, sab, sae }
}

/// Flash pairing and rate ring for eight pixels of one kind.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn counter8(
    ring: &mut [u32],
    open: &mut [u32],
    last: &mut [u32],
    pend_t: &mut [u32],
    n: usize,
    k: usize,
    i: usize,
    pend_pol: u32x8,
    q_up: u32x8,
    q_dn: u32x8,
    now: u32x8,
    pair: u32x8,
) -> u32x8 {
    let up = u32x8::splat(UP);
    let dn = u32x8::splat(DN);
    let q = q_up | q_dn;
    if q.none() {
        return pend_pol;
    }
    let pol = bl_u(q_up, up, dn);
    let opposite = bl_u(q_up, dn, up);
    let pt = ld_u(pend_t, i);
    let flash = q & pend_pol.cmp_eq(opposite) & age_le(now, pt, pair);
    let rest = q & !flash;
    if flash.any() {
        let mut s = k - 1;
        while s > 0 {
            let r_prev = ld_u(ring, (s - 1) * n + i);
            let r_cur = ld_u(ring, s * n + i);
            st_u(ring, s * n + i, bl_u(flash, r_prev, r_cur));
            let o_prev = ld_u(open, (s - 1) * n + i);
            let o_cur = ld_u(open, s * n + i);
            st_u(open, s * n + i, bl_u(flash, o_prev, o_cur));
            s -= 1;
        }
        st_u(ring, i, bl_u(flash, now, ld_u(ring, i)));
        st_u(open, i, bl_u(flash, pt, ld_u(open, i)));
        st_u(last, i, bl_u(flash, now, ld_u(last, i)));
    }
    let never = now - u32x8::splat(AGE_MAX);
    st_u(pend_t, i, bl_u(flash, never, bl_u(rest, now, pt)));
    bl_u(flash, u32x8::splat(0), bl_u(rest, pol, pend_pol))
}

/// Eight-lane version of [`run_range_scalar`] over the whole frame.
pub fn run_frame_simd(st: &mut PixelState, planes: &FramePlanes, p: &KernelParams, out: &mut PixelOutputs) {
    let n = st.n;
    if p.first() {
        run_range_scalar(st, planes, p, out, 0, n);
        return;
    }
    let k = st.k;
    let main = n - n % LANES;
    let now = u32x8::splat(p.now);
    let zero = u32x8::splat(0);

    if p.saturate() {
        let lim = u32x8::splat(AGE_MAX);
        let never = now - lim;
        let sat_field = |f: &mut [u32], i: usize| {
            let t = ld_u(f, i);
            st_u(f, i, bl_u((now - t).cmp_gt(lim), never, t));
        };
        let mut i = 0;
        while i < main {
            sat_field(&mut st.lum_t, i);
            sat_field(&mut st.red_t, i);
            for s in 0..k {
                sat_field(&mut st.gen_ring, s * n + i);
                sat_field(&mut st.gen_open, s * n + i);
                sat_field(&mut st.red_ring, s * n + i);
                sat_field(&mut st.red_open, s * n + i);
            }
            sat_field(&mut st.gen_last, i);
            sat_field(&mut st.gen_pend_t, i);
            sat_field(&mut st.red_last, i);
            sat_field(&mut st.red_pend_t, i);
            sat_field(&mut st.pool_gen_t, i);
            sat_field(&mut st.pool_red_t, i);
            i += LANES;
        }
    }

    let max_run = u32x8::splat(p.max_run);
    if p.held() {
        let mut i = 0;
        while i < main {
            let lt = ld_u(&st.lum_t, i);
            let stale = age_gt(now, lt, max_run);
            let nb = bl_f(stale, ld_f(&st.lum_ext, i), ld_f(&st.lum_base, i));
            st_f(&mut st.lum_base, i, nb);
            st_u(&mut st.lum_t, i, bl_u(stale, now, lt));
            let rt = ld_u(&st.red_t, i);
            let rstale = age_gt(now, rt, max_run);
            let rb = bl_f(rstale, ld_f(&st.red_ext, i), ld_f(&st.red_base, i));
            st_f(&mut st.red_base, i, rb);
            st_u(&mut st.red_t, i, bl_u(rstale, now, rt));
            let cb = bl_u(rstale, ld_u(&st.red_ext_c, i), ld_u(&st.red_base_c, i));
            st_u(&mut st.red_base_c, i, cb);
            st_u(&mut out.mask, i, zero);
            st_u(&mut out.onset_gen, i, zero);
            st_u(&mut out.onset_red, i, zero);
            i += LANES;
        }
        // the tail: the scalar runner saturates and ages its own range
        // only (the head's were done above), with the same params
        run_range_scalar(st, planes, p, out, main, n);
        return;
    }

    let eps_l = f32x8::splat(p.eps_l);
    let eps_v = f32x8::splat(p.eps_v);
    let swing = f32x8::splat(p.swing);
    let dark = f32x8::splat(p.dark);
    let red_delta = f32x8::splat(p.red_delta);
    let pair = u32x8::splat(p.pair);
    let rate = u32x8::splat(p.rate);
    let fresh = u32x8::splat(p.fresh);
    let pool = u32x8::splat(p.pool);
    let kf = p.k_fail as usize - 1;
    let ke = p.k_ext as usize - 1;
    let dir_mask = u32x8::splat(DIR_MASK);
    let up = u32x8::splat(UP);
    let dn = u32x8::splat(DN);

    let mut i = 0;
    while i < main {
        let l = ld_f(&planes.l, i);
        let v = ld_f(&planes.v, i);
        let c = ld_u(&planes.c, i);
        st_f(&mut st.prev_l, i, l);
        st_f(&mut st.prev_v, i, v);
        let mut flags = ld_u(&st.flags, i);

        // --- luminance run tracker --------------------------------------
        let mut lt = Tr8 {
            dir: (flags >> LUM_DIR_SHIFT) & dir_mask,
            base: ld_f(&st.lum_base, i),
            ext: ld_f(&st.lum_ext, i),
            base_t: ld_u(&st.lum_t, i),
            aux_base: zero,
            aux_ext: zero,
        };
        let lo = tracker_feed8(&mut lt, l, zero, now, eps_l, max_run);
        st_f(&mut st.lum_base, i, lt.base);
        st_f(&mut st.lum_ext, i, lt.ext);
        st_u(&mut st.lum_t, i, lt.base_t);
        flags = (flags & !(dir_mask << LUM_DIR_SHIFT)) | (lt.dir << LUM_DIR_SHIFT);
        let q_up = lo.rev_up & fm((lo.sext - lo.sbase).cmp_ge(swing)) & fm(lo.sbase.cmp_lt(dark));
        let q_dn = lo.rev_dn & fm((lo.sbase - lo.sext).cmp_ge(swing)) & fm(lo.sext.cmp_lt(dark));

        // --- red run tracker (the chromaticity rides along) --------------
        let mut rt = Tr8 {
            dir: (flags >> RED_DIR_SHIFT) & dir_mask,
            base: ld_f(&st.red_base, i),
            ext: ld_f(&st.red_ext, i),
            base_t: ld_u(&st.red_t, i),
            aux_base: ld_u(&st.red_base_c, i),
            aux_ext: ld_u(&st.red_ext_c, i),
        };
        let ro = tracker_feed8(&mut rt, v, c, now, eps_v, max_run);
        st_f(&mut st.red_base, i, rt.base);
        st_f(&mut st.red_ext, i, rt.ext);
        st_u(&mut st.red_t, i, rt.base_t);
        st_u(&mut st.red_base_c, i, rt.aux_base);
        st_u(&mut st.red_ext_c, i, rt.aux_ext);
        flags = (flags & !(dir_mask << RED_DIR_SHIFT)) | (rt.dir << RED_DIR_SHIFT);
        let red_q = red_transition8(ro.sab, ro.sae, red_delta);
        let rq_up = ro.rev_up & red_q;
        let rq_dn = ro.rev_dn & red_q;

        let mut mask = zero;

        // --- general flashes --------------------------------------------
        {
            let pend_pol = (flags >> GEN_PEND_SHIFT) & dir_mask;
            let pend_pol = counter8(&mut st.gen_ring, &mut st.gen_open, &mut st.gen_last, &mut st.gen_pend_t, n, k, i, pend_pol, q_up, q_dn, now, pair);
            flags = (flags & !(dir_mask << GEN_PEND_SHIFT)) | (pend_pol << GEN_PEND_SHIFT);
            let fresh_m = age_le(now, ld_u(&st.gen_last, i), fresh);
            let strobe = fresh_m & age_lt(now, ld_u(&st.gen_ring, kf * n + i), rate);
            let ext_s = fresh_m & age_lt(now, ld_u(&st.gen_ring, ke * n + i), rate);
            let onset = bl_u(strobe, now - ld_u(&st.gen_open, kf * n + i), zero);
            st_u(&mut out.onset_gen, i, onset);
            mask = mask | (strobe & u32x8::splat(MASK_STROBE_GEN)) | (ext_s & u32x8::splat(MASK_EXT_GEN));
        }

        // --- red flashes ------------------------------------------------
        {
            let pend_pol = (flags >> RED_PEND_SHIFT) & dir_mask;
            let pend_pol = counter8(&mut st.red_ring, &mut st.red_open, &mut st.red_last, &mut st.red_pend_t, n, k, i, pend_pol, rq_up, rq_dn, now, pair);
            flags = (flags & !(dir_mask << RED_PEND_SHIFT)) | (pend_pol << RED_PEND_SHIFT);
            let fresh_m = age_le(now, ld_u(&st.red_last, i), fresh);
            let strobe = fresh_m & age_lt(now, ld_u(&st.red_ring, kf * n + i), rate);
            let ext_s = fresh_m & age_lt(now, ld_u(&st.red_ring, ke * n + i), rate);
            let onset = bl_u(strobe, now - ld_u(&st.red_open, kf * n + i), zero);
            st_u(&mut out.onset_red, i, onset);
            mask = mask | (strobe & u32x8::splat(MASK_STROBE_RED)) | (ext_s & u32x8::splat(MASK_EXT_RED));
        }

        // --- pooled transition areas ------------------------------------
        {
            let q = q_up | q_dn;
            let pol = bl_u(q, bl_u(q_up, up, dn), (flags >> POOL_GEN_SHIFT) & dir_mask);
            let t = bl_u(q, now, ld_u(&st.pool_gen_t, i));
            st_u(&mut st.pool_gen_t, i, t);
            flags = (flags & !(dir_mask << POOL_GEN_SHIFT)) | (pol << POOL_GEN_SHIFT);
            let active = age_le(now, t, pool);
            mask = mask | (active & pol.cmp_eq(up) & u32x8::splat(MASK_POOL_GEN_UP)) | (active & pol.cmp_eq(dn) & u32x8::splat(MASK_POOL_GEN_DN));
            let rq = rq_up | rq_dn;
            let rpol = bl_u(rq, bl_u(rq_up, up, dn), (flags >> POOL_RED_SHIFT) & dir_mask);
            let rt2 = bl_u(rq, now, ld_u(&st.pool_red_t, i));
            st_u(&mut st.pool_red_t, i, rt2);
            flags = (flags & !(dir_mask << POOL_RED_SHIFT)) | (rpol << POOL_RED_SHIFT);
            let ractive = age_le(now, rt2, pool);
            mask = mask | (ractive & rpol.cmp_eq(up) & u32x8::splat(MASK_POOL_RED_UP)) | (ractive & rpol.cmp_eq(dn) & u32x8::splat(MASK_POOL_RED_DN));
        }

        st_u(&mut st.flags, i, flags);
        st_u(&mut out.mask, i, mask);
        i += LANES;
    }
    // the tail: saturation for the head was done above, so hand the scalar
    // runner the tail only
    run_range_scalar(st, planes, p, out, main, n);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DetectorConfig, Profile};
    use crate::grid::GridGeometry;
    use crate::pixel::{run_frame_scalar, MODE_FIRST, MODE_HELD, MODE_SATURATE};

    fn lcg(s: &mut u64) -> u32 {
        *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*s >> 33) as u32
    }

    #[test]
    fn simd_matches_scalar_bit_for_bit() {
        for cfg in [Profile::WcagExt.config(), Profile::Strict.config(), DetectorConfig { flash_limit: 1.0, ..Default::default() }] {
            let n = 1003; // not a multiple of 8: exercises the scalar tail
            let geom = GridGeometry::new(&cfg, 17, 59);
            let k = cfg.k_fail() as usize;
            let mut a = PixelState::new(n, k);
            let mut b = PixelState::new(n, k);
            let mut oa = PixelOutputs::new(n);
            let mut ob = PixelOutputs::new(n);
            let mut planes = FramePlanes::new(n);
            let mut seed = 42u64;
            let mut now = 0u32;
            for f in 0..400 {
                // a mix of noise, flicker, and long-held runs so every branch fires
                for i in 0..n {
                    let r = lcg(&mut seed);
                    let flick = if (f / 3) % 2 == 0 { 0.05 } else { 0.5 };
                    let noise = (r % 1000) as f32 / 1000.0 * 0.06;
                    planes.l[i] = if i % 4 == 0 { flick + noise } else { (r % 1000) as f32 / 1000.0 };
                    // red against grey on every fifth pixel, random colours elsewhere
                    let (cr, cg, cb) = if i % 5 == 0 {
                        if (f / 3) % 2 == 0 {
                            (255, 0, 0)
                        } else {
                            (128, 128, 128)
                        }
                    } else {
                        ((r % 256) as u8, ((r >> 8) % 256) as u8, ((r >> 16) % 256) as u8)
                    };
                    let px = crate::lut::pixel_values(cr, cg, cb, cfg.red_saturation, cfg.red_flare);
                    planes.v[i] = px.s;
                    planes.c[i] = px.c;
                }
                let mut p = KernelParams::template(&cfg, &geom);
                now = now.wrapping_add(20_000 + lcg(&mut seed) % 40_000);
                if f == 200 {
                    now = now.wrapping_add(2_500_000_000); // a huge jump: saturation matters
                }
                p.now = now;
                p.mode = if f == 0 {
                    MODE_FIRST
                } else {
                    (if f % 37 == 0 { MODE_SATURATE } else { 0 }) | (if f % 11 == 0 { MODE_HELD } else { 0 })
                };
                run_frame_scalar(&mut a, &planes, &p, &mut oa);
                run_frame_simd(&mut b, &planes, &p, &mut ob);
                assert_eq!(oa.mask, ob.mask, "frame {f}: masks");
                assert_eq!(oa.onset_gen, ob.onset_gen, "frame {f}: onset_gen");
                assert_eq!(oa.onset_red, ob.onset_red, "frame {f}: onset_red");
                let fa = a.to_flat();
                let fb = b.to_flat();
                for (w, (x, y)) in fa.iter().zip(&fb).enumerate() {
                    assert_eq!(x, y, "frame {f}: state word {w} (field {}, pixel {})", w / n, w % n);
                }
            }
        }
    }
}
