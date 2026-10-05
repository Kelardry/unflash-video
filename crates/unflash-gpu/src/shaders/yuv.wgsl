// Pass 0 (YUV sources only): 8-bit 4:2:0 planes -> the RGBA source texture.
// Each chroma sample covers its 2x2 luma block (no interpolation), the
// codes convert with the same coefficients as the CPU (unflash_core::yuv)
// and round to 8 bits when stored.

struct YuvParams {
    width: u32,
    height: u32,
    nv12: u32,
    full_range: u32,
    ky: f32,
    kr: f32,
    kgu: f32,
    kgv: f32,
    kb: f32,
    yoff: f32,
    pad0: f32,
    pad1: f32,
}

@group(0) @binding(0) var<uniform> yp: YuvParams;
@group(0) @binding(1) var ytex: texture_2d<f32>;
@group(0) @binding(2) var ctex: texture_2d<f32>;
@group(0) @binding(3) var vtex: texture_2d<f32>;
@group(0) @binding(4) var dst: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let x = gid.x;
    let y = gid.y;
    if (x >= yp.width || y >= yp.height) {
        return;
    }
    let luma = textureLoad(ytex, vec2<u32>(x, y), 0).r * 255.0;
    let cpos = vec2<u32>(x / 2u, y / 2u);
    var u: f32;
    var v: f32;
    if (yp.nv12 != 0u) {
        let uv = textureLoad(ctex, cpos, 0).rg * 255.0;
        u = uv.r;
        v = uv.g;
    } else {
        u = textureLoad(ctex, cpos, 0).r * 255.0;
        v = textureLoad(vtex, cpos, 0).r * 255.0;
    }
    let yy = (luma - yp.yoff) * yp.ky;
    let cb = u - 128.0;
    let cr = v - 128.0;
    let rgb = vec3<f32>(yy + yp.kr * cr, yy - yp.kgu * cb - yp.kgv * cr, yy + yp.kb * cb);
    textureStore(dst, vec2<u32>(x, y), vec4<f32>(clamp(rgb / 255.0, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
