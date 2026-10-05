// Pass P: regular patterns (stripes, gratings). One thread per (orientation,
// line). The thread walks its line through the luminance plane with the
// state machine of unflash_core::pattern (the same fixed-point positions,
// tests and order), ORs the orientation's bit into the mask of every pixel a
// qualifying stretch crosses, and adds its spacing statistics to the
// globals. Runs after the mask is cleared (a buffer clear before each
// frame's passes in its batch) and before rows (which counts the marked
// pixels). ORs and integer adds commute, so the result is the CPU's
// exactly, whatever the thread order.

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> geo: array<u32>;
@group(0) @binding(2) var<storage, read> inputs: array<u32>;
@group(0) @binding(3) var<storage, read_write> patmask: array<atomic<u32>>;
@group(0) @binding(4) var<storage, read_write> globals: array<atomic<u32>>;

const PAT_RING: u32 = 16u;
const PAT_ORIENTATIONS: u32 = 8u;

// (cos, sin) of k * 22.5 degrees in 16.16 fixed point; core::pattern::DIRS
fn pat_dir(k: u32) -> vec2<i32> {
    switch (k) {
        case 0u: { return vec2<i32>(65536, 0); }
        case 1u: { return vec2<i32>(60547, 25080); }
        case 2u: { return vec2<i32>(46341, 46341); }
        case 3u: { return vec2<i32>(25080, 60547); }
        case 4u: { return vec2<i32>(0, 65536); }
        case 5u: { return vec2<i32>(-25080, 60547); }
        case 6u: { return vec2<i32>(-46341, 46341); }
        default: { return vec2<i32>(-60547, 25080); }
    }
}

// nearest pixel of a fixed-point position, or -1 outside the picture
fn pat_index(px: i32, py: i32, w: i32, h: i32) -> i32 {
    let xi = (px + 32768) >> 16u;
    let yi = (py + 32768) >> 16u;
    if (xi < 0 || xi >= w || yi < 0 || yi >= h) {
        return -1;
    }
    return yi * w + xi;
}

fn pat_lum(i: i32) -> f32 {
    return bitcast<f32>(inputs[u32(i)]);
}

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let w = i32(params.width);
    let h = i32(params.height);
    let r = i32(geo[GEO_PAT_R]);
    let nlines = u32(2 * r + 1);
    let k = gid.x / nlines;
    if (k >= PAT_ORIENTATIONS) {
        return;
    }
    let j = i32(gid.x % nlines) - r;
    let d = pat_dir(k);
    let dx = d.x;
    let dy = d.y;
    let nx = -dy;
    let ny = dx;
    let bit = 1u << k;
    let ox = w * 32768 + j * nx;
    let oy = h * 32768 + j * ny;
    let p_swing = params.pat_swing;
    let p_dark = params.dark;
    let p_eps = params.eps_l;
    let p_coh = params.pat_coherence;
    let p_min = params.pat_min_transitions;
    let p_rn = params.pat_reg_num;
    let p_rd = params.pat_reg_den;

    var init = false;
    var dir = 0u;
    var base = 0.0;
    var ext = 0.0;
    var ext_t = 0;
    var n_q = 0u;
    var ring: array<i32, PAT_RING>;
    var n_ext = 0u;
    var marked = false;
    var marked_upto = 0;
    var in_stretch = false;
    var spacing_sum = 0u;
    var spacing_n = 0u;

    for (var t = -r; t <= r; t = t + 1) {
        let idx = pat_index(ox + t * dx, oy + t * dy, w, h);
        if (idx < 0) {
            init = false;
            in_stretch = false;
            continue;
        }
        let v = pat_lum(idx);
        if (!init) {
            init = true;
            dir = 0u;
            base = v;
            ext = v;
            ext_t = t;
            n_q = 0u;
            n_ext = 1u;
            ring[0] = t;
            marked = false;
            in_stretch = false;
            continue;
        }
        var finish = false;
        var q = false;
        var fe_t = 0;
        if (dir == UP) {
            if (v >= ext) {
                ext = v;
                ext_t = t;
            } else if (v < ext - p_eps) {
                q = (ext - base) >= p_swing && base < p_dark;
                finish = true;
                fe_t = ext_t;
                base = ext;
                ext = v;
                ext_t = t;
                dir = DN;
            }
        } else if (dir == DN) {
            if (v <= ext) {
                ext = v;
                ext_t = t;
            } else if (v > ext + p_eps) {
                q = (base - ext) >= p_swing && ext < p_dark;
                finish = true;
                fe_t = ext_t;
                base = ext;
                ext = v;
                ext_t = t;
                dir = UP;
            }
        } else if (v > base + p_eps) {
            dir = UP;
            ext = v;
            ext_t = t;
        } else if (v < base - p_eps) {
            dir = DN;
            ext = v;
            ext_t = t;
        }
        if (!finish) {
            continue;
        }
        // a stripe is uniform along its length: the extremum has to agree
        // with the pixel one step perpendicular to the line
        if (q) {
            let a = pat_index(ox + fe_t * dx, oy + fe_t * dy, w, h);
            let b = pat_index(ox + nx + fe_t * dx, oy + ny + fe_t * dy, w, h);
            q = a >= 0 && b >= 0 && abs(pat_lum(a) - pat_lum(b)) <= p_coh;
        }
        // finish_run
        ring[n_ext % PAT_RING] = fe_t;
        n_ext = n_ext + 1u;
        if (q) {
            n_q = n_q + 1u;
        } else {
            n_q = 0u;
            in_stretch = false;
            continue;
        }
        if (n_q < p_min) {
            continue;
        }
        var smin = 0xffffffffu;
        var smax = 0u;
        for (var b = 0u; b < p_min; b = b + 1u) {
            let s = u32(ring[(n_ext - 1u - b) % PAT_RING] - ring[(n_ext - 2u - b) % PAT_RING]);
            smin = min(smin, s);
            smax = max(smax, s);
        }
        if (smax * p_rd > smin * p_rn) {
            in_stretch = false;
            continue;
        }
        let first = ring[(n_ext - 1u - p_min) % PAT_RING];
        if (in_stretch) {
            spacing_sum = spacing_sum + u32(fe_t - ring[(n_ext - 2u) % PAT_RING]);
            spacing_n = spacing_n + 1u;
        } else {
            spacing_sum = spacing_sum + u32(fe_t - first);
            spacing_n = spacing_n + p_min;
            in_stretch = true;
        }
        var t0 = first;
        if (marked) {
            t0 = max(first, marked_upto + 1);
        }
        marked_upto = fe_t;
        marked = true;
        for (var tt = t0; tt <= fe_t; tt = tt + 1) {
            let mi = pat_index(ox + tt * dx, oy + tt * dy, w, h);
            if (mi >= 0) {
                atomicOr(&patmask[u32(mi)], bit);
            }
        }
    }
    if (spacing_n != 0u) {
        atomicAdd(&globals[1], spacing_sum);
        atomicAdd(&globals[2], spacing_n);
    }
}
