// Pass B: the per-pixel state machine. A line-for-line mirror of
// unflash_core::pixel::run_frame_scalar; see there for the reasoning.

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> inputs: array<u32>;
@group(0) @binding(2) var<storage, read_write> state: array<u32>;
@group(0) @binding(3) var<storage, read_write> pixout: array<u32>;
@group(0) @binding(4) var<storage, read> globals: array<u32>;

// (aux: what rides along with the value -- the red run's chromaticity)
struct Tr {
    dir: u32,
    base: f32,
    ext: f32,
    base_t: u32,
    aux_base: u32,
    aux_ext: u32,
};

struct TrOut {
    tr: Tr,
    rev_up: bool,
    rev_dn: bool,
    sbase: f32,
    sext: f32,
    sab: u32,
    sae: u32,
};

fn tracker_feed(t0: Tr, x: f32, aux: u32, now: u32, eps: f32, max_run: u32) -> TrOut {
    var t = t0;
    var rev_up = false;
    var rev_dn = false;
    if (t.dir == UP) {
        if (x >= t.ext) {
            t.ext = x;
            t.aux_ext = aux;
        } else if (x < t.ext - eps) {
            rev_up = true;
        }
    } else if (t.dir == DN) {
        if (x <= t.ext) {
            t.ext = x;
            t.aux_ext = aux;
        } else if (x > t.ext + eps) {
            rev_dn = true;
        }
    }
    let sbase = t.base;
    let sext = t.ext;
    let sab = t.aux_base;
    let sae = t.aux_ext;
    if (rev_up || rev_dn) {
        t.base = t.ext;
        t.ext = x;
        t.base_t = now;
        t.dir = select(UP, DN, rev_up);
        t.aux_base = t.aux_ext;
        t.aux_ext = aux;
    } else if (t.dir == 0u) {
        if (x > t.base + eps) {
            t.dir = UP;
            t.ext = x;
            t.aux_ext = aux;
        } else if (x < t.base - eps) {
            t.dir = DN;
            t.ext = x;
            t.aux_ext = aux;
        }
    }
    let stale = age(now, t.base_t) > max_run;
    if (t.dir == 0u && stale) {
        t.ext = x;
        t.aux_ext = aux;
    }
    if (stale) {
        t.base = t.ext;
        t.base_t = now;
        t.aux_base = t.aux_ext;
    }
    return TrOut(t, rev_up, rev_dn, sbase, sext, sab, sae);
}

// Flash pairing + rate ring for one pixel of one kind. Returns the new
// pending polarity; ring, open, last and pend_t are updated in place.
fn counter_transition(ring_f: u32, open_f: u32, last_f: u32, pend_t_f: u32, i: u32, n: u32,
                      pend_pol: u32, pol: u32, now: u32, pair: u32) -> u32 {
    let opposite = select(UP, DN, pol == UP);
    let pend_t = state[pend_t_f * n + i];
    if (pend_pol == opposite && age(now, pend_t) <= pair) {
        for (var s = K - 1u; s > 0u; s = s - 1u) {
            state[(ring_f + s) * n + i] = state[(ring_f + s - 1u) * n + i];
            state[(open_f + s) * n + i] = state[(open_f + s - 1u) * n + i];
        }
        state[ring_f * n + i] = now;
        state[open_f * n + i] = pend_t;
        state[last_f * n + i] = now;
        state[pend_t_f * n + i] = never(now);
        return 0u;
    }
    state[pend_t_f * n + i] = now;
    return pol;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let n = params.npix;
    if (i >= n) {
        return;
    }
    let now = params.now;
    let first = (params.mode & MODE_FIRST) != 0u;
    let saturate = (params.mode & MODE_SATURATE) != 0u;
    let held = !first && globals[0] < params.held_bar;
    let l = bitcast<f32>(inputs[i]);
    let v = bitcast<f32>(inputs[n + i]);
    let c = inputs[2u * n + i];

    if (first) {
        let nv = never(now);
        state[F_LUM_BASE * n + i] = bitcast<u32>(l);
        state[F_LUM_EXT * n + i] = bitcast<u32>(l);
        state[F_LUM_T * n + i] = now;
        state[F_RED_BASE * n + i] = bitcast<u32>(v);
        state[F_RED_EXT * n + i] = bitcast<u32>(v);
        state[F_RED_T * n + i] = now;
        state[F_RED_BASE_C * n + i] = c;
        state[F_RED_EXT_C * n + i] = c;
        state[F_FLAGS * n + i] = 0u;
        for (var s = 0u; s < K; s = s + 1u) {
            state[(F_GEN_RING + s) * n + i] = nv;
            state[(F_GEN_OPEN + s) * n + i] = nv;
            state[(F_RED_RING + s) * n + i] = nv;
            state[(F_RED_OPEN + s) * n + i] = nv;
        }
        state[F_GEN_LAST * n + i] = nv;
        state[F_GEN_PEND_T * n + i] = nv;
        state[F_RED_LAST * n + i] = nv;
        state[F_RED_PEND_T * n + i] = nv;
        state[F_POOL_GEN_T * n + i] = nv;
        state[F_POOL_RED_T * n + i] = nv;
        state[F_PREV_L * n + i] = bitcast<u32>(l);
        state[F_PREV_V * n + i] = bitcast<u32>(v);
        pixout[i] = 0u;
        pixout[n + i] = 0u;
        pixout[2u * n + i] = 0u;
        return;
    }

    if (saturate) {
        state[F_LUM_T * n + i] = sat_time(now, state[F_LUM_T * n + i]);
        state[F_RED_T * n + i] = sat_time(now, state[F_RED_T * n + i]);
        for (var s = 0u; s < K; s = s + 1u) {
            state[(F_GEN_RING + s) * n + i] = sat_time(now, state[(F_GEN_RING + s) * n + i]);
            state[(F_GEN_OPEN + s) * n + i] = sat_time(now, state[(F_GEN_OPEN + s) * n + i]);
            state[(F_RED_RING + s) * n + i] = sat_time(now, state[(F_RED_RING + s) * n + i]);
            state[(F_RED_OPEN + s) * n + i] = sat_time(now, state[(F_RED_OPEN + s) * n + i]);
        }
        state[F_GEN_LAST * n + i] = sat_time(now, state[F_GEN_LAST * n + i]);
        state[F_GEN_PEND_T * n + i] = sat_time(now, state[F_GEN_PEND_T * n + i]);
        state[F_RED_LAST * n + i] = sat_time(now, state[F_RED_LAST * n + i]);
        state[F_RED_PEND_T * n + i] = sat_time(now, state[F_RED_PEND_T * n + i]);
        state[F_POOL_GEN_T * n + i] = sat_time(now, state[F_POOL_GEN_T * n + i]);
        state[F_POOL_RED_T * n + i] = sat_time(now, state[F_POOL_RED_T * n + i]);
    }

    var flags = state[F_FLAGS * n + i];

    if (held) {
        // nothing moved, so there is nothing to track -- but time passed
        if (age(now, state[F_LUM_T * n + i]) > params.max_run) {
            state[F_LUM_BASE * n + i] = state[F_LUM_EXT * n + i];
            state[F_LUM_T * n + i] = now;
        }
        if (age(now, state[F_RED_T * n + i]) > params.max_run) {
            state[F_RED_BASE * n + i] = state[F_RED_EXT * n + i];
            state[F_RED_T * n + i] = now;
            state[F_RED_BASE_C * n + i] = state[F_RED_EXT_C * n + i];
        }
        pixout[i] = 0u;
        pixout[n + i] = 0u;
        pixout[2u * n + i] = 0u;
        return;
    }

    state[F_PREV_L * n + i] = bitcast<u32>(l);
    state[F_PREV_V * n + i] = bitcast<u32>(v);

    // --- luminance run tracker --------------------------------------------
    var lt: Tr;
    lt.dir = (flags >> LUM_DIR_SHIFT) & DIR_MASK;
    lt.base = bitcast<f32>(state[F_LUM_BASE * n + i]);
    lt.ext = bitcast<f32>(state[F_LUM_EXT * n + i]);
    lt.base_t = state[F_LUM_T * n + i];
    lt.aux_base = 0u;
    lt.aux_ext = 0u;
    let lo = tracker_feed(lt, l, 0u, now, params.eps_l, params.max_run);
    state[F_LUM_BASE * n + i] = bitcast<u32>(lo.tr.base);
    state[F_LUM_EXT * n + i] = bitcast<u32>(lo.tr.ext);
    state[F_LUM_T * n + i] = lo.tr.base_t;
    flags = (flags & ~(DIR_MASK << LUM_DIR_SHIFT)) | (lo.tr.dir << LUM_DIR_SHIFT);
    let q_up = lo.rev_up && (lo.sext - lo.sbase) >= params.swing && lo.sbase < params.dark;
    let q_dn = lo.rev_dn && (lo.sbase - lo.sext) >= params.swing && lo.sext < params.dark;

    // --- red run tracker (the chromaticity rides along) --------------------
    var rt: Tr;
    rt.dir = (flags >> RED_DIR_SHIFT) & DIR_MASK;
    rt.base = bitcast<f32>(state[F_RED_BASE * n + i]);
    rt.ext = bitcast<f32>(state[F_RED_EXT * n + i]);
    rt.base_t = state[F_RED_T * n + i];
    rt.aux_base = state[F_RED_BASE_C * n + i];
    rt.aux_ext = state[F_RED_EXT_C * n + i];
    let ro = tracker_feed(rt, v, c, now, params.eps_v, params.max_run);
    state[F_RED_BASE * n + i] = bitcast<u32>(ro.tr.base);
    state[F_RED_EXT * n + i] = bitcast<u32>(ro.tr.ext);
    state[F_RED_T * n + i] = ro.tr.base_t;
    state[F_RED_BASE_C * n + i] = ro.tr.aux_base;
    state[F_RED_EXT_C * n + i] = ro.tr.aux_ext;
    flags = (flags & ~(DIR_MASK << RED_DIR_SHIFT)) | (ro.tr.dir << RED_DIR_SHIFT);
    // WCAG 2.2: to or from saturated red, the two states more than 0.2 apart in u'v'
    let red_q = red_transition(ro.sab, ro.sae, params.red_delta);
    let rq_up = ro.rev_up && red_q;
    let rq_dn = ro.rev_dn && red_q;

    var mask = 0u;
    let kf = params.k_fail - 1u;
    let ke = params.k_ext - 1u;

    // --- general flashes --------------------------------------------------
    {
        var pend_pol = (flags >> GEN_PEND_SHIFT) & DIR_MASK;
        if (q_up || q_dn) {
            pend_pol = counter_transition(F_GEN_RING, F_GEN_OPEN, F_GEN_LAST, F_GEN_PEND_T, i, n,
                                          pend_pol, select(DN, UP, q_up), now, params.pair);
        }
        flags = (flags & ~(DIR_MASK << GEN_PEND_SHIFT)) | (pend_pol << GEN_PEND_SHIFT);
        let fresh = age(now, state[F_GEN_LAST * n + i]) <= params.fresh;
        let strobe = fresh && age(now, state[(F_GEN_RING + kf) * n + i]) < params.rate;
        let ext_s = fresh && age(now, state[(F_GEN_RING + ke) * n + i]) < params.rate;
        var onset = 0u;
        if (strobe) {
            mask = mask | MASK_STROBE_GEN;
            onset = age(now, state[(F_GEN_OPEN + kf) * n + i]);
        }
        pixout[n + i] = onset;
        if (ext_s) {
            mask = mask | MASK_EXT_GEN;
        }
    }

    // --- red flashes ------------------------------------------------------
    {
        var pend_pol = (flags >> RED_PEND_SHIFT) & DIR_MASK;
        if (rq_up || rq_dn) {
            pend_pol = counter_transition(F_RED_RING, F_RED_OPEN, F_RED_LAST, F_RED_PEND_T, i, n,
                                          pend_pol, select(DN, UP, rq_up), now, params.pair);
        }
        flags = (flags & ~(DIR_MASK << RED_PEND_SHIFT)) | (pend_pol << RED_PEND_SHIFT);
        let fresh = age(now, state[F_RED_LAST * n + i]) <= params.fresh;
        let strobe = fresh && age(now, state[(F_RED_RING + kf) * n + i]) < params.rate;
        let ext_s = fresh && age(now, state[(F_RED_RING + ke) * n + i]) < params.rate;
        var onset = 0u;
        if (strobe) {
            mask = mask | MASK_STROBE_RED;
            onset = age(now, state[(F_RED_OPEN + kf) * n + i]);
        }
        pixout[2u * n + i] = onset;
        if (ext_s) {
            mask = mask | MASK_EXT_RED;
        }
    }

    // --- pooled transition areas (chart statistics only) ------------------
    {
        var pol = (flags >> POOL_GEN_SHIFT) & DIR_MASK;
        if (q_up) {
            pol = UP;
            state[F_POOL_GEN_T * n + i] = now;
        } else if (q_dn) {
            pol = DN;
            state[F_POOL_GEN_T * n + i] = now;
        }
        flags = (flags & ~(DIR_MASK << POOL_GEN_SHIFT)) | (pol << POOL_GEN_SHIFT);
        let gactive = age(now, state[F_POOL_GEN_T * n + i]) <= params.pool;
        if (gactive && pol == UP) {
            mask = mask | MASK_POOL_GEN_UP;
        }
        if (gactive && pol == DN) {
            mask = mask | MASK_POOL_GEN_DN;
        }
        var rpol = (flags >> POOL_RED_SHIFT) & DIR_MASK;
        if (rq_up) {
            rpol = UP;
            state[F_POOL_RED_T * n + i] = now;
        } else if (rq_dn) {
            rpol = DN;
            state[F_POOL_RED_T * n + i] = now;
        }
        flags = (flags & ~(DIR_MASK << POOL_RED_SHIFT)) | (rpol << POOL_RED_SHIFT);
        let ractive = age(now, state[F_POOL_RED_T * n + i]) <= params.pool;
        if (ractive && rpol == UP) {
            mask = mask | MASK_POOL_RED_UP;
        }
        if (ractive && rpol == DN) {
            mask = mask | MASK_POOL_RED_DN;
        }
    }

    state[F_FLAGS * n + i] = flags;
    pixout[i] = mask;
}
