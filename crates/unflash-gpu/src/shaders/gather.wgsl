// Pass D: one thread per window position sums its rows' window values into
// one grid cell of the output; thread 0 also totals the frame's luminance
// and patterned pixels and copies (then clears) the moved-pixel count and
// the pattern spacing statistics. No barriers.

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> geo: array<u32>;
@group(0) @binding(2) var<storage, read> rowwin: array<u32>;
@group(0) @binding(3) var<storage, read> rowtot: array<f32>;
@group(0) @binding(4) var<storage, read_write> globals: array<atomic<u32>>;
@group(0) @binding(5) var<storage, read_write> out: array<u32>;
@group(0) @binding(6) var<storage, read> rowpat: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let cell = gid.x;
    let ngx = geo[GEO_NGX];
    let ngy = geo[GEO_NGY];
    let wh = geo[GEO_WH];
    if (cell == 0u) {
        var s = 0.0;
        var pc = 0u;
        for (var y = 0u; y < params.height; y = y + 1u) {
            s = s + rowtot[y];
            pc = pc + rowpat[y];
        }
        out[0] = bitcast<u32>(s);
        out[1] = atomicLoad(&globals[0]);
        atomicStore(&globals[0], 0u);
        out[4] = pc;
        out[5] = atomicLoad(&globals[1]);
        out[6] = atomicLoad(&globals[2]);
        atomicStore(&globals[1], 0u);
        atomicStore(&globals[2], 0u);
    }
    if (cell >= ngx * ngy) {
        return;
    }
    let gi = cell % ngx;
    let gj = cell / ngx;
    let gy = geo[GEO_GYS + gj];
    var sl = 0.0;
    var sv = 0.0;
    var cnt: array<u32, 8>;
    for (var k = 0u; k < 8u; k = k + 1u) {
        cnt[k] = 0u;
    }
    var og = 0u;
    var orr = 0u;
    for (var r = 0u; r < wh; r = r + 1u) {
        let base = ((gy + r) * ngx + gi) * CELL_WORDS;
        sl = sl + bitcast<f32>(rowwin[base]);
        sv = sv + bitcast<f32>(rowwin[base + 1u]);
        for (var k = 0u; k < 8u; k = k + 1u) {
            cnt[k] = cnt[k] + rowwin[base + 2u + k];
        }
        og = max(og, rowwin[base + 10u]);
        orr = max(orr, rowwin[base + 11u]);
    }
    let base = OUT_HEADER + cell * CELL_WORDS;
    out[base] = bitcast<u32>(sl);
    out[base + 1u] = bitcast<u32>(sv);
    for (var k = 0u; k < 8u; k = k + 1u) {
        out[base + 2u + k] = cnt[k];
    }
    out[base + 10u] = og;
    out[base + 11u] = orr;
}
