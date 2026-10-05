// Pass A2: the pixels that moved since the last new picture (in luminance
// or in distance from red), counted for the held-frame test. Runs inside the batch,
// after the previous frame's update has stored what it compares against;
// the ingest pass, which runs as pictures arrive, cannot read that yet.

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> inputs: array<u32>;
@group(0) @binding(2) var<storage, read> state: array<u32>;
@group(0) @binding(3) var<storage, read_write> globals: array<atomic<u32>>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    let n = params.npix;
    if (i >= n || (params.mode & MODE_FIRST) != 0u) {
        return;
    }
    let l = bitcast<f32>(inputs[i]);
    let v = bitcast<f32>(inputs[n + i]);
    let prev = bitcast<f32>(state[F_PREV_L * n + i]);
    let prev_v = bitcast<f32>(state[F_PREV_V * n + i]);
    if (abs(l - prev) > params.held_delta || abs(v - prev_v) > params.held_delta_v) {
        atomicAdd(&globals[0], 1u);
    }
}
