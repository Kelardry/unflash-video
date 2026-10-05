// Pass C: one workgroup per row, one thread per window position. Each
// thread sums its window's L, V and mask counts across the row and takes the
// window maxima of the onset ages; thread 0 also totals the row's luminance
// and its patterned pixels. No workgroup memory and no barriers:
// windows overlap, so the redundant reads are cache hits, and the thread
// count is small enough that this is latency-bound rather than
// bandwidth-bound on any real GPU.

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> geo: array<u32>;
@group(0) @binding(2) var<storage, read> inputs: array<u32>;
@group(0) @binding(3) var<storage, read> pixout: array<u32>;
@group(0) @binding(4) var<storage, read_write> rowwin: array<u32>;
@group(0) @binding(5) var<storage, read_write> rowtot: array<f32>;
@group(0) @binding(6) var<storage, read> patmask: array<u32>;
@group(0) @binding(7) var<storage, read_write> rowpat: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(workgroup_id) wid: vec3<u32>, @builtin(local_invocation_id) lid: vec3<u32>) {
    let y = wid.x;
    let w = params.width;
    let n = params.npix;
    let t = lid.x;
    let ngx = geo[GEO_NGX];
    let ww = geo[GEO_WW];
    if (t == 0u) {
        var s = 0.0;
        var pc = 0u;
        if (params.pat_enabled != 0u) {
            for (var x = 0u; x < w; x = x + 1u) {
                s = s + bitcast<f32>(inputs[y * w + x]);
                pc = pc + u32(patmask[y * w + x] != 0u);
            }
        } else {
            for (var x = 0u; x < w; x = x + 1u) {
                s = s + bitcast<f32>(inputs[y * w + x]);
            }
        }
        rowtot[y] = s;
        rowpat[y] = pc;
    }
    if (t >= ngx) {
        return;
    }
    let gx = geo[GEO_GXS + t];
    var sl = 0.0;
    var sv = 0.0;
    var cnt: array<u32, 8>;
    for (var k = 0u; k < 8u; k = k + 1u) {
        cnt[k] = 0u;
    }
    var og = 0u;
    var orr = 0u;
    for (var x = gx; x < gx + ww; x = x + 1u) {
        let idx = y * w + x;
        sl = sl + bitcast<f32>(inputs[idx]);
        sv = sv + bitcast<f32>(inputs[n + idx]);
        let m = pixout[idx];
        for (var k = 0u; k < 8u; k = k + 1u) {
            cnt[k] = cnt[k] + ((m >> k) & 1u);
        }
        og = max(og, pixout[n + idx]);
        orr = max(orr, pixout[2u * n + idx]);
    }
    let base = (y * ngx + t) * CELL_WORDS;
    rowwin[base + 0u] = bitcast<u32>(sl);
    rowwin[base + 1u] = bitcast<u32>(sv);
    for (var k = 0u; k < 8u; k = k + 1u) {
        rowwin[base + 2u + k] = cnt[k];
    }
    rowwin[base + 10u] = og;
    rowwin[base + 11u] = orr;
}
