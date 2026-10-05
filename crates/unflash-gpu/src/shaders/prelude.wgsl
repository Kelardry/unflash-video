// Shared definitions, prepended to every pass. `{{K}}` is the flash ring
// depth (floor(flash_limit) + 1) and is substituted at pipeline creation.

struct Params {
    now: u32,
    mode: u32,
    npix: u32,
    width: u32,
    eps_l: f32,
    eps_v: f32,
    swing: f32,
    dark: f32,
    red_delta: f32,
    max_run: u32,
    pair: u32,
    rate: u32,
    fresh: u32,
    pool: u32,
    k_fail: u32,
    k_ext: u32,
    held_delta: f32,
    held_bar: u32,
    height: u32,
    red_saturation: f32,
    // regular patterns (core::pattern)
    pat_swing: f32,
    pat_coherence: f32,
    pat_min_transitions: u32,
    pat_reg_num: u32,
    pat_reg_den: u32,
    pat_enabled: u32,
    held_delta_v: f32,
    src_bgr: u32,
    red_flare: f32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
};

const MODE_FIRST: u32 = 1u;
const MODE_SATURATE: u32 = 4u;
const AGE_MAX: u32 = 1073741824u;
const K: u32 = {{K}};

// geometry buffer layout (u32 words)
const GEO_SRC_W: u32 = 0u;
const GEO_SRC_H: u32 = 1u;
const GEO_AW: u32 = 2u;
const GEO_AH: u32 = 3u;
const GEO_WW: u32 = 4u;
const GEO_WH: u32 = 5u;
const GEO_NGX: u32 = 6u;
const GEO_NGY: u32 = 7u;
const GEO_GXS: u32 = 8u;
const GEO_GYS: u32 = 72u;
// half-extent of the pattern pass's line family (core::pattern::line_radius)
const GEO_PAT_R: u32 = 136u;

// state buffer fields (each a run of npix words); see core::pixel::StateLayout
const F_LUM_BASE: u32 = 0u;
const F_LUM_EXT: u32 = 1u;
const F_LUM_T: u32 = 2u;
const F_RED_BASE: u32 = 3u;
const F_RED_EXT: u32 = 4u;
const F_RED_T: u32 = 5u;
const F_FLAGS: u32 = 6u;
const F_GEN_RING: u32 = 7u;
const F_GEN_OPEN: u32 = 7u + K;
const F_GEN_LAST: u32 = 7u + 2u * K;
const F_GEN_PEND_T: u32 = 8u + 2u * K;
const F_RED_RING: u32 = 9u + 2u * K;
const F_RED_OPEN: u32 = 9u + 3u * K;
const F_RED_LAST: u32 = 9u + 4u * K;
const F_RED_PEND_T: u32 = 10u + 4u * K;
const F_POOL_GEN_T: u32 = 11u + 4u * K;
const F_POOL_RED_T: u32 = 12u + 4u * K;
const F_PREV_L: u32 = 13u + 4u * K;
const F_PREV_V: u32 = 14u + 4u * K;
// the red run's chromaticity at its base and extremum (core::lut::red_values)
const F_RED_BASE_C: u32 = 15u + 4u * K;
const F_RED_EXT_C: u32 = 16u + 4u * K;

// mask bits
const MASK_STROBE_GEN: u32 = 1u;
const MASK_STROBE_RED: u32 = 2u;
const MASK_EXT_GEN: u32 = 4u;
const MASK_EXT_RED: u32 = 8u;
const MASK_POOL_GEN_UP: u32 = 16u;
const MASK_POOL_GEN_DN: u32 = 32u;
const MASK_POOL_RED_UP: u32 = 64u;
const MASK_POOL_RED_DN: u32 = 128u;

// flags word layout
const LUM_DIR_SHIFT: u32 = 0u;
const RED_DIR_SHIFT: u32 = 2u;
const GEN_PEND_SHIFT: u32 = 6u;
const RED_PEND_SHIFT: u32 = 8u;
const POOL_GEN_SHIFT: u32 = 10u;
const POOL_RED_SHIFT: u32 = 12u;
const DIR_MASK: u32 = 3u;
const UP: u32 = 1u;
const DN: u32 = 2u;

// output buffer: header words (frame luminance, moved pixels, two unused,
// pattern pixels, pattern spacing sum, pattern spacing count, one unused)
// then 12 words per grid cell
const OUT_HEADER: u32 = 8u;
const CELL_WORDS: u32 = 12u;

// WCAG 2.2's red flash (core::lut): sRGB's red primary in u'v', and a
// state's chromaticity packed as u' (15 bits) | v' (16 bits) | saturated (top bit)
const RED_U: f32 = 0.4507966;
const RED_V: f32 = 0.5228869;
const QU: f32 = 32768.0;
const QV: f32 = 65536.0;
const SAT_BIT: u32 = 0x80000000u;

// Whether a red run's two ends make a red transition: either end saturated
// red, and the ends more than `delta` apart in u'v'.
fn red_transition(a: u32, b: u32, delta: f32) -> bool {
    let du = (f32((a >> 16u) & 0x7fffu) - f32((b >> 16u) & 0x7fffu)) / QU;
    let dv = (f32(a & 0xffffu) - f32(b & 0xffffu)) / QV;
    return ((a | b) & SAT_BIT) != 0u && du * du + dv * dv > delta * delta;
}

fn age(now: u32, t: u32) -> u32 {
    return now - t;
}
fn never(now: u32) -> u32 {
    return now - AGE_MAX;
}
fn sat_time(now: u32, t: u32) -> u32 {
    return select(t, now - AGE_MAX, (now - t) > AGE_MAX);
}
