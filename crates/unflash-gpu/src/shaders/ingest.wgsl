// Pass A: source texture -> linear luminance L, distance from red S and the
// packed chromaticity C (core::lut::red_values), into this frame's slice of
// the inputs buffer, as pictures arrive. The
// moved-pixel count and the pattern mask are the batch's business (moved.wgsl
// and the clear before the pattern pass).
//
// The source may be any size: each analysis pixel takes the area average of
// the source box it covers (weights are the fractional overlaps), in sRGB
// code space, then rounds to an 8-bit code and linearises through the same
// table the CPU uses. A source already at analysis resolution copies through
// exactly.

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> geo: array<u32>;
@group(0) @binding(2) var<storage, read> lut: array<f32>;
@group(0) @binding(3) var src: texture_2d<f32>;
@group(0) @binding(4) var<storage, read_write> inputs: array<u32>;
@group(0) @binding(5) var<storage, read_write> rgba: array<u32>;

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let aw = geo[GEO_AW];
    let ah = geo[GEO_AH];
    let x = gid.x;
    let y = gid.y;
    if (x < aw && y < ah) {
        let sw = geo[GEO_SRC_W];
        let sh = geo[GEO_SRC_H];
        let n = params.npix;
        var c: vec3<f32>;
        if (sw == aw && sh == ah) {
            c = textureLoad(src, vec2<u32>(x, y), 0).rgb;
        } else {
            let fx0 = f32(x) * f32(sw) / f32(aw);
            let fx1 = f32(x + 1u) * f32(sw) / f32(aw);
            let fy0 = f32(y) * f32(sh) / f32(ah);
            let fy1 = f32(y + 1u) * f32(sh) / f32(ah);
            let ix0 = u32(floor(fx0));
            let ix1 = min(sw, u32(ceil(fx1)));
            let iy0 = u32(floor(fy0));
            let iy1 = min(sh, u32(ceil(fy1)));
            var acc = vec3<f32>(0.0, 0.0, 0.0);
            var wsum = 0.0;
            for (var sy = iy0; sy < iy1; sy = sy + 1u) {
                let wy = min(fy1, f32(sy + 1u)) - max(fy0, f32(sy));
                for (var sx = ix0; sx < ix1; sx = sx + 1u) {
                    let wx = min(fx1, f32(sx + 1u)) - max(fx0, f32(sx));
                    let w = wx * wy;
                    acc = acc + textureLoad(src, vec2<u32>(sx, sy), 0).rgb * w;
                    wsum = wsum + w;
                }
            }
            c = acc / max(wsum, 1e-9);
        }
        if (params.src_bgr != 0u) {
            c = c.bgr;
        }
        let code = clamp(vec3<i32>(round(c * 255.0)), vec3<i32>(0), vec3<i32>(255));
        let r = lut[u32(code.r)];
        let g = lut[u32(code.g)];
        let b = lut[u32(code.b)];
        let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        // WCAG 2.2's red quantities, in the CPU's order (core::lut::red_values)
        let rf = r + params.red_flare;
        let gf = g + params.red_flare;
        let bf = b + params.red_flare;
        let cx = 0.4124 * rf + 0.3576 * gf + 0.1805 * bf;
        let cy = 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
        let cz = 0.0193 * rf + 0.1192 * gf + 0.9505 * bf;
        let d = cx + 15.0 * cy + 3.0 * cz;
        let u = 4.0 * cx / d;
        let v = 9.0 * cy / d;
        let du = u - RED_U;
        let dv = v - RED_V;
        let s = sqrt(du * du + dv * dv);
        let sat = rf >= params.red_saturation * (rf + gf + bf);
        let qu = min(u32(floor(u * QU + 0.5)), 0x7fffu);
        let qv = min(u32(floor(v * QV + 0.5)), 0xffffu);
        let chroma = select(0u, SAT_BIT, sat) | (qu << 16u) | qv;
        let i = y * aw + x;
        rgba[i] = u32(code.r) | (u32(code.g) << 8u) | (u32(code.b) << 16u) | 0xff000000u;
        inputs[i] = bitcast<u32>(l);
        inputs[n + i] = bitcast<u32>(s);
        inputs[2u * n + i] = chroma;
    }
}
