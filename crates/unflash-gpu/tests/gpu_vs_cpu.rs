//! The GPU stage must agree with the CPU kernel bit for bit on everything
//! integer (masks, onsets, the whole per-pixel state, the pattern mask and
//! its statistics) and to float tolerance on the window sums. Runs on whatever adapter wgpu finds (a software
//! Vulkan driver such as lavapipe is enough); skips when there is none unless
//! UNFLASH_REQUIRE_GPU is set.

use unflash_core::config::{DetectorConfig, Profile};
use unflash_core::detector::CpuStage;
use unflash_core::grid::{FrameInput, GridGeometry};
use unflash_core::pixel::{KernelParams, MODE_FIRST, MODE_SATURATE};
use unflash_core::time::secs_to_us;
use unflash_gpu::{FrameSource, GpuContext, GpuStage};

fn context() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::new()) {
        Ok(c) => {
            eprintln!("adapter: {:?}", c.info());
            Some(c)
        }
        Err(e) => {
            if std::env::var("UNFLASH_REQUIRE_GPU").is_ok() {
                panic!("{e}");
            }
            eprintln!("skipping GPU test: {e}");
            None
        }
    }
}

fn hash_noise(i: usize, y: usize, x: usize, amp: i32) -> i32 {
    let mut h = (i as u32)
        .wrapping_mul(2654435761)
        .wrapping_add((y as u32).wrapping_mul(2246822519))
        .wrapping_add((x as u32).wrapping_mul(3266489917));
    h ^= h >> 15;
    h = h.wrapping_mul(2246822519);
    h ^= h >> 13;
    h >>= 24;
    (h % (2 * amp as u32 + 1)) as i32 - amp
}

/// A busy synthetic frame: noisy background with a slow drift, a 4 Hz
/// flashing rectangle, a red/grey flashing rectangle, a moving bright bar,
/// and every seventh frame a repeat of the previous picture.
fn gen(i: usize, t: f64, aw: usize, ah: usize) -> Vec<u8> {
    let src_i = if i % 7 == 6 { i - 1 } else { i };
    let t = if i % 7 == 6 { t - 1001.0 / 30000.0 } else { t };
    let mut f = vec![0u8; aw * ah * 3];
    let base = 40 + ((src_i * 120) / 200) as i32;
    for y in 0..ah {
        for x in 0..aw {
            let c = (base + hash_noise(src_i, y, x, 10)).clamp(0, 255) as u8;
            let k = (y * aw + x) * 3;
            f[k] = c;
            f[k + 1] = c;
            f[k + 2] = c;
        }
    }
    let phase = ((t * 8.0).floor() as i64).rem_euclid(2);
    // flashing rectangle top-left, 45% of the window's area or so
    let (x0, y0, x1, y1) = (aw / 10, ah / 10, aw / 10 + aw * 4 / 10, ah / 10 + ah * 4 / 10);
    let c = if phase == 0 { 25 } else { 210 };
    for y in y0..y1 {
        for x in x0..x1 {
            let k = (y * aw + x) * 3;
            f[k] = c;
            f[k + 1] = c;
            f[k + 2] = c;
        }
    }
    // red flashing rectangle bottom-right
    let (x0, y0, x1, y1) = (aw / 2, ah / 2, aw / 2 + aw * 4 / 10, ah / 2 + ah * 4 / 10);
    let rgb = if phase == 0 { [255u8, 0, 0] } else { [144u8, 144, 144] };
    for y in y0..y1 {
        for x in x0..x1 {
            let k = (y * aw + x) * 3;
            f[k..k + 3].copy_from_slice(&rgb);
        }
    }
    // moving bar
    let bx = ((t * aw as f64).floor() as usize) % aw;
    for y in 0..ah {
        for k in 0..(aw / 12).max(1) {
            let x = (bx + k) % aw;
            let idx = (y * aw + x) * 3;
            f[idx] = 235;
            f[idx + 1] = 235;
            f[idx + 2] = 235;
        }
    }
    f
}

/// Saturated red swapped for a grey of the same relative luminance every
/// three frames over the whole picture, with a faint drifting noise patch in
/// one corner: the held-frame gate must see the colour move even though the
/// luminance does not, and the repeats inside each run must still be held.
fn gen_equilum(i: usize, _t: f64, aw: usize, ah: usize) -> Vec<u8> {
    let rgb = if (i / 3) % 2 == 0 { [250u8, 0, 0] } else { [122u8, 124, 122] };
    let mut f = vec![0u8; aw * ah * 3];
    for px in f.chunks_exact_mut(3) {
        px.copy_from_slice(&rgb);
    }
    // a small noisy patch that changes only every fifth frame (well under the
    // held bar), so the colour swap alone decides whether a frame is new
    let src_i = i / 5;
    for y in 0..(ah / 8).max(1) {
        for x in 0..(aw / 8).max(1) {
            let c = (60 + hash_noise(src_i, y, x, 12)).clamp(0, 255) as u8;
            let k = (y * aw + x) * 3;
            f[k] = c;
            f[k + 1] = c;
            f[k + 2] = c;
        }
    }
    f
}

/// Frames for the pattern pass: fine vertical stripes, coarse diagonal
/// stripes, stripes over part of the picture, noise, low-contrast stripes,
/// each held for a few frames, with a repeat every seventh frame.
fn gen_stripes(i: usize, _t: f64, aw: usize, ah: usize) -> Vec<u8> {
    let src_i = if i % 7 == 6 { i - 1 } else { i };
    let mut f = vec![0u8; aw * ah * 3];
    let phase = (src_i / 5) % 6;
    for y in 0..ah {
        for x in 0..aw {
            let c: u8 = match phase {
                0 => {
                    if (x / 3) % 2 == 0 {
                        20
                    } else {
                        170
                    }
                }
                1 => {
                    if ((x + y) / 6) % 2 == 0 {
                        30
                    } else {
                        200
                    }
                }
                2 => {
                    if x < aw * 2 / 5 && (y / 4) % 2 == 0 {
                        25
                    } else if x < aw * 2 / 5 {
                        180
                    } else {
                        90
                    }
                }
                3 => (90 + hash_noise(src_i, y, x, 80)).clamp(0, 255) as u8,
                4 => {
                    if (x / 3) % 2 == 0 {
                        100
                    } else {
                        118
                    }
                }
                _ => {
                    // moving stripes: strobe-like scrolling grating
                    if ((x + src_i * 2) / 4) % 2 == 0 {
                        20
                    } else {
                        160
                    }
                }
            };
            let k = (y * aw + x) * 3;
            f[k] = c;
            f[k + 1] = c;
            f[k + 2] = c;
        }
    }
    f
}

/// What a generator's frames must provoke, beyond matching the CPU.
#[derive(Clone, Copy, PartialEq)]
enum Expect {
    /// General (luminance) flashing in some cells.
    Flash,
    /// Red flashing in some cells and no general flashing anywhere.
    RedFlash,
    /// Patterned frames.
    Patterns,
}

fn compare(ctx: &GpuContext, cfg: DetectorConfig, src_w: u32, src_h: u32, nframes: usize) {
    compare_with(ctx, cfg, src_w, src_h, nframes, &gen, Expect::Flash)
}

fn compare_with(ctx: &GpuContext, cfg: DetectorConfig, src_w: u32, src_h: u32, nframes: usize, gen: &dyn Fn(usize, f64, usize, usize) -> Vec<u8>, expect: Expect) {
    let (aw, ah) = cfg.analysis_dims(src_w, src_h);
    let geom = GridGeometry::new(&cfg, aw, ah);
    eprintln!("analysis {}x{} window {}x{} cells {}", aw, ah, geom.ww, geom.wh, geom.ncells());
    let mut gpu = GpuStage::new(ctx, &cfg, geom.clone()).expect("stage");
    let mut cpu = CpuStage::new(&cfg, geom.clone());
    cpu.use_simd = false;
    let mut cpu_own = CpuStage::new(&cfg, geom.clone());
    cpu_own.use_simd = false;
    let tmpl = KernelParams::template(&cfg, &geom);
    let n = geom.npix();
    let mut held_frames = 0;
    let mut strobe_frames = 0;
    let mut red_strobe_frames = 0;
    let mut pattern_frames = 0;
    for i in 0..nframes {
        let t = i as f64 * 1001.0 / 30000.0;
        let mut p = tmpl;
        p.now = secs_to_us(t);
        p.mode = if i == 0 {
            MODE_FIRST
        } else if i % 37 == 0 {
            MODE_SATURATE
        } else {
            0
        };
        let frame = gen(i, t, aw as usize, ah as usize);
        let capture = i % 5 == 0;
        gpu.submit(p, FrameSource::Rgb8 { data: &frame, width: aw, height: ah }, capture).expect("submit");
        // one frame per batch here: every frame's buffers are compared
        gpu.flush();
        gpu.wait_idle();
        let gf = gpu.poll().expect("a result").expect("no error");
        let gs = gf.stats;
        if capture {
            // at analysis resolution the capture is the frame itself
            let rgba = gf.rgba.expect("captured picture");
            for j in 0..n {
                assert_eq!(&rgba[j * 4..j * 4 + 3], &frame[j * 3..j * 3 + 3], "f{i} px{j}: captured rgb");
                assert_eq!(rgba[j * 4 + 3], 255);
            }
        } else {
            assert!(gf.rgba.is_none());
        }
        let (l, v, c) = gpu.debug_inputs();

        // the GPU's own ingest must match the CPU's ingest of the same bytes
        // (the GPU's division and square root may be a few ulps off, so the
        // chromaticity may round one step the other way)
        let _ = cpu_own.run(p, FrameInput::rgb(&frame));
        let own = cpu_own.planes();
        for j in 0..n {
            assert!((l[j] - own.l[j]).abs() <= 2e-6, "f{i} px{j}: L {} vs {}", l[j], own.l[j]);
            assert!((v[j] - own.v[j]).abs() <= 2e-6, "f{i} px{j}: distance from red {} vs {}", v[j], own.v[j]);
            let (a, b) = (c[j], own.c[j]);
            assert_eq!(a >> 31, b >> 31, "f{i} px{j}: saturated red");
            assert!((((a >> 16) & 0x7fff) as i32 - ((b >> 16) & 0x7fff) as i32).abs() <= 1 && ((a & 0xffff) as i32 - (b & 0xffff) as i32).abs() <= 1, "f{i} px{j}: chromaticity {a:#x} vs {b:#x}");
        }

        // the kernel, run on the GPU's planes, must match exactly
        {
            let pm = cpu.planes_mut();
            pm.l = l;
            pm.v = v;
            pm.c = c;
        }
        let cs = cpu.run_planes(p);
        assert_eq!(gs.held_count, cs.held_count, "f{i}: moved-pixel count");
        assert_eq!(gs.held, cs.held, "f{i}: held");
        if gs.held {
            held_frames += 1;
        }
        assert!((gs.sum_l - cs.sum_l).abs() <= 1e-3 * n as f64, "f{i}: frame luminance {} vs {}", gs.sum_l, cs.sum_l);
        assert_eq!(gs.pattern_count, cs.pattern_count, "f{i}: patterned pixels");
        assert_eq!(gs.pattern_spacing_sum, cs.pattern_spacing_sum, "f{i}: pattern spacing sum");
        assert_eq!(gs.pattern_spacing_n, cs.pattern_spacing_n, "f{i}: pattern spacing count");
        if cfg.flag_patterns() {
            let gmask = gpu.debug_patmask();
            let cmask = cpu.pattern_mask();
            for j in 0..n {
                assert_eq!(gmask[j], cmask[j], "f{i} px{j}: pattern mask");
            }
            if gs.pattern_count as f64 >= 0.25 * n as f64 {
                pattern_frames += 1;
            }
        }
        assert_eq!(gs.cells.len(), cs.cells.len(), "f{i}: cell count");
        for (c, (a, b)) in gs.cells.iter().zip(&cs.cells).enumerate() {
            assert_eq!(a.cnt, b.cnt, "f{i} cell {c}: counts");
            assert_eq!(a.onset_gen, b.onset_gen, "f{i} cell {c}: onset_gen");
            assert_eq!(a.onset_red, b.onset_red, "f{i} cell {c}: onset_red");
            let wp = geom.window_pixels() as f32;
            assert!((a.sum_l - b.sum_l).abs() <= 2e-4 * wp, "f{i} cell {c}: sum_l {} vs {}", a.sum_l, b.sum_l);
            assert!((a.sum_v - b.sum_v).abs() <= 0.05 * wp, "f{i} cell {c}: sum_v {} vs {}", a.sum_v, b.sum_v);
            if a.cnt[0] > 0 {
                strobe_frames += 1;
            }
            if a.cnt[1] > 0 {
                red_strobe_frames += 1;
            }
        }
        let (mask, og, or) = gpu.debug_pixout();
        let out = cpu.outputs();
        for j in 0..n {
            assert_eq!(mask[j], out.mask[j], "f{i} px{j}: mask");
            assert_eq!(og[j], out.onset_gen[j], "f{i} px{j}: onset_gen");
            assert_eq!(or[j], out.onset_red[j], "f{i} px{j}: onset_red");
        }
        let gstate = gpu.debug_state();
        let cstate = cpu.state().to_flat();
        assert_eq!(gstate.len(), cstate.len());
        for (j, (a, b)) in gstate.iter().zip(&cstate).enumerate() {
            assert_eq!(a, b, "f{i}: state word {j} (field {}, pixel {})", j / n, j % n);
        }
    }
    eprintln!("ok: {nframes} frames, {held_frames} held, {strobe_frames} strobing cells ({red_strobe_frames} red), {pattern_frames} patterned frames");
    assert!(held_frames > 0, "the generator repeats frames, some must be held");
    match expect {
        // a failure needs four flashes inside a second, so short runs cannot strobe
        Expect::Flash if nframes >= 40 => {
            assert!(strobe_frames > 0, "the generator flashes, some cells must strobe");
        }
        Expect::RedFlash if nframes >= 40 => {
            assert!(red_strobe_frames > 0, "the generator flashes red, some cells must strobe red");
            assert_eq!(strobe_frames, 0, "an equiluminant red flash must not register as a general flash");
        }
        Expect::Patterns => {
            assert!(pattern_frames >= 10, "the striped generator must produce patterned frames, got {pattern_frames}");
        }
        _ => {}
    }
}

#[test]
fn gpu_matches_cpu_default_scale() {
    let Some(ctx) = context() else { return };
    compare(&ctx, Profile::WcagExt.config(), 640, 480, 90);
}

#[test]
fn gpu_matches_cpu_strict_small_odd() {
    let Some(ctx) = context() else { return };
    let cfg = DetectorConfig { analysis_scale: 0.1, ..Profile::Strict.config() };
    compare(&ctx, cfg, 100, 60, 60);
}

#[test]
fn gpu_matches_cpu_full_width() {
    let Some(ctx) = context() else { return };
    let cfg = DetectorConfig { analysis_scale: 1.0, ..Profile::Wcag.config() };
    compare(&ctx, cfg, 1000, 600, 14);
}

#[test]
fn gpu_matches_cpu_equiluminant_red() {
    let Some(ctx) = context() else { return };
    compare_with(&ctx, Profile::Wcag.config(), 640, 360, 60, &gen_equilum, Expect::RedFlash);
    let cfg = DetectorConfig { analysis_scale: 0.2, ..Profile::Strict.config() };
    compare_with(&ctx, cfg, 320, 180, 45, &gen_equilum, Expect::RedFlash);
}

#[test]
fn gpu_matches_cpu_patterns() {
    let Some(ctx) = context() else { return };
    compare_with(&ctx, Profile::WcagExt.config(), 640, 360, 35, &gen_stripes, Expect::Patterns);
}

#[test]
fn gpu_matches_cpu_patterns_odd_size_strict() {
    let Some(ctx) = context() else { return };
    let cfg = DetectorConfig { analysis_scale: 0.3, ..Profile::Strict.config() };
    compare_with(&ctx, cfg, 333, 201, 35, &gen_stripes, Expect::Patterns);
}

#[test]
fn pipelined_submissions_arrive_in_order() {
    let Some(ctx) = context() else { return };
    let cfg = Profile::Wcag.config();
    let (aw, ah) = cfg.analysis_dims(640, 480);
    let geom = GridGeometry::new(&cfg, aw, ah);
    let mut gpu = GpuStage::new(&ctx, &cfg, geom.clone()).unwrap();
    let mut cpu = CpuStage::new(&cfg, geom.clone());
    let tmpl = KernelParams::template(&cfg, &geom);
    let frames: Vec<Vec<u8>> = (0..12).map(|i| gen(i, i as f64 / 30.0, aw as usize, ah as usize)).collect();
    let mut expected = Vec::new();
    let mut got = Vec::new();
    let mut i = 0;
    while got.len() < frames.len() {
        while i < frames.len() && gpu.can_submit() {
            let mut p = tmpl;
            p.now = secs_to_us(i as f64 / 30.0);
            p.mode = if i == 0 { MODE_FIRST } else { 0 };
            expected.push(cpu.run(p, FrameInput::rgb(&frames[i])));
            gpu.submit(p, FrameSource::Rgb8 { data: &frames[i], width: aw, height: ah }, i % 2 == 0).unwrap();
            i += 1;
        }
        gpu.flush();
        assert!(gpu.in_flight() <= gpu.capacity());
        gpu.wait_idle();
        while let Some(r) = gpu.poll() {
            got.push(r.unwrap().stats);
        }
    }
    for (k, (g, e)) in got.iter().zip(&expected).enumerate() {
        assert_eq!(g.held, e.held, "frame {k}");
        assert_eq!(g.held_count, e.held_count, "frame {k}");
        for (c, (a, b)) in g.cells.iter().zip(&e.cells).enumerate() {
            assert_eq!(a.cnt, b.cnt, "frame {k} cell {c}");
        }
    }
}

/// Frames batched sixteen to a command buffer must give exactly the stats
/// (and captures) that one frame per batch gives: the held decision, the
/// pattern statistics and every cell count, over a generator with repeats,
/// flashing and stripes.
#[test]
fn batched_frames_match_single_frames() {
    let Some(ctx) = context() else { return };
    type Gen = fn(usize, f64, usize, usize) -> Vec<u8>;
    let cases: Vec<(DetectorConfig, Gen, u32, u32, usize)> = vec![
        (Profile::WcagExt.config(), gen, 640, 480, 70),
        (Profile::WcagExt.config(), gen_stripes, 640, 360, 40),
        (DetectorConfig { analysis_scale: 0.3, ..Profile::Strict.config() }, gen_equilum, 320, 180, 45),
    ];
    for (cfg, gen, w, h, n) in cases {
        let (aw, ah) = cfg.analysis_dims(w, h);
        let geom = GridGeometry::new(&cfg, aw, ah);
        let mut single = GpuStage::with_options(&ctx, &cfg, geom.clone(), 2, 1).unwrap();
        let mut batched = GpuStage::with_options(&ctx, &cfg, geom.clone(), 2, 16).unwrap();
        let tmpl = KernelParams::template(&cfg, &geom);
        let mut want = Vec::new();
        let mut got = Vec::new();
        for i in 0..n {
            let t = i as f64 * 1001.0 / 30000.0;
            let mut p = tmpl;
            p.now = secs_to_us(t);
            p.mode = if i == 0 { MODE_FIRST } else if i % 37 == 0 { MODE_SATURATE } else { 0 };
            let frame = gen(i, t, aw as usize, ah as usize);
            let capture = i % 7 == 3;
            single.submit(p, FrameSource::Rgb8 { data: &frame, width: aw, height: ah }, capture).unwrap();
            single.flush();
            single.wait_idle();
            want.push(single.poll().unwrap().unwrap());
            while !batched.can_submit() {
                batched.wait_idle();
                while let Some(r) = batched.poll() {
                    got.push(r.unwrap());
                }
            }
            batched.submit(p, FrameSource::Rgb8 { data: &frame, width: aw, height: ah }, capture).unwrap();
        }
        batched.flush();
        batched.wait_idle();
        while let Some(r) = batched.poll() {
            got.push(r.unwrap());
        }
        assert_eq!(got.len(), want.len());
        for (k, (g, e)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g.stats.held, e.stats.held, "frame {k}: held");
            assert_eq!(g.stats.held_count, e.stats.held_count, "frame {k}: moved pixels");
            assert_eq!(g.stats.pattern_count, e.stats.pattern_count, "frame {k}: patterned pixels");
            assert_eq!(g.stats.pattern_spacing_sum, e.stats.pattern_spacing_sum, "frame {k}: spacing sum");
            assert_eq!(g.stats.pattern_spacing_n, e.stats.pattern_spacing_n, "frame {k}: spacing count");
            assert_eq!(g.stats.cells.len(), e.stats.cells.len(), "frame {k}: cells");
            for (c, (a, b)) in g.stats.cells.iter().zip(&e.stats.cells).enumerate() {
                assert_eq!(a.cnt, b.cnt, "frame {k} cell {c}: counts");
                assert_eq!(a.onset_gen, b.onset_gen, "frame {k} cell {c}: onset_gen");
                assert_eq!(a.onset_red, b.onset_red, "frame {k} cell {c}: onset_red");
                assert!((a.sum_l - b.sum_l).abs() <= 1e-6 * a.sum_l.abs().max(1.0), "frame {k} cell {c}: sum_l");
            }
            assert_eq!(g.rgba.is_some(), e.rgba.is_some(), "frame {k}: capture");
            assert_eq!(g.rgba, e.rgba, "frame {k}: captured picture");
        }
        assert_eq!(single.debug_state(), batched.debug_state(), "state after the run");
    }
}

/// A BGRX picture uploaded as it came (the channels swapped back by the
/// ingest pass) must be analysed exactly like the same picture as RGBA, at
/// analysis size and scaled down.
#[test]
fn bgra_sources_match_rgba_sources() {
    let Some(ctx) = context() else { return };
    let cfg = Profile::WcagExt.config();
    for (w, h) in [(640u32, 480u32), (333, 250)] {
        let (aw, ah) = cfg.analysis_dims(w, h);
        let geom = GridGeometry::new(&cfg, aw, ah);
        let mut a = GpuStage::with_options(&ctx, &cfg, geom.clone(), 2, 4).unwrap();
        let mut b = GpuStage::with_options(&ctx, &cfg, geom.clone(), 2, 4).unwrap();
        let tmpl = KernelParams::template(&cfg, &geom);
        let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
        for i in 0..24 {
            let t = i as f64 / 30.0;
            let mut p = tmpl;
            p.now = secs_to_us(t);
            p.mode = if i == 0 { MODE_FIRST } else { 0 };
            // a source-size picture with colour in every channel (red flashes too)
            let rgb = gen(i, t, w as usize, h as usize);
            let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
            let mut bgra = Vec::with_capacity(rgb.len() / 3 * 4);
            for px in rgb.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                bgra.extend_from_slice(&[px[2], px[1], px[0], 255]);
            }
            for (stage, got, src) in [(&mut a, &mut got_a, FrameSource::Rgba8 { data: &rgba, width: w, height: h }), (&mut b, &mut got_b, FrameSource::Bgra8 { data: &bgra, width: w, height: h })] {
                while !stage.can_submit() {
                    stage.wait_idle();
                    while let Some(r) = stage.poll() {
                        got.push(r.unwrap());
                    }
                }
                stage.submit(p, src, i % 5 == 2).unwrap();
            }
        }
        for (stage, got) in [(&mut a, &mut got_a), (&mut b, &mut got_b)] {
            stage.flush();
            stage.wait_idle();
            while let Some(r) = stage.poll() {
                got.push(r.unwrap());
            }
        }
        assert_eq!(got_a.len(), 24);
        assert_eq!(got_b.len(), 24);
        for (k, (x, y)) in got_a.iter().zip(&got_b).enumerate() {
            assert_eq!(x.rgba, y.rgba, "{w}x{h} frame {k}: captured picture");
            assert_eq!(x.stats.held_count, y.stats.held_count, "{w}x{h} frame {k}: moved pixels");
            for (c, (p, q)) in x.stats.cells.iter().zip(&y.stats.cells).enumerate() {
                assert_eq!(p.cnt, q.cnt, "{w}x{h} frame {k} cell {c}: counts");
                assert_eq!(p.sum_l, q.sum_l, "{w}x{h} frame {k} cell {c}: sum_l");
            }
        }
        assert_eq!(a.debug_state(), b.debug_state(), "{w}x{h}: state after the run");
    }
}
