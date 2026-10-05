//! Throughput of the detector stages on synthetic frames.
//!
//!     cargo run --release -p unflash-gpu --example bench -- [source WxH] [analysis scale] [frames]
//!
//! Reports ns/pixel, frames/s and the implied memory traffic for the scalar
//! CPU kernel, the SIMD kernel and the GPU stage (whatever adapter wgpu
//! finds; a software driver here, real hardware on a user's machine). The
//! GPU stage runs its default batch; the last, partial batch is flushed.

use std::time::Instant;

use unflash_core::config::{DetectorConfig, Profile};
use unflash_core::detector::CpuStage;
use unflash_core::grid::{FrameInput, GridGeometry};
use unflash_core::pixel::MODE_FIRST;
use unflash_core::time::secs_to_us;
use unflash_gpu::{FrameSource, GpuContext, GpuStage};

fn gen(i: usize, aw: usize, ah: usize) -> Vec<u8> {
    let mut f = vec![0u8; aw * ah * 3];
    let phase = ((i / 3) % 2) as u8;
    for y in 0..ah {
        for x in 0..aw {
            let k = (y * aw + x) * 3;
            let inrect = x > aw / 5 && x < aw * 4 / 5 && y > ah / 5 && y < ah * 4 / 5;
            let c = if inrect { if phase == 1 { 200 } else { 30 } } else { (60 + (x * 7 + y * 13 + i) % 17) as u8 };
            f[k] = c;
            f[k + 1] = c;
            f[k + 2] = c;
        }
    }
    f
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (sw, sh) = args.first().and_then(|s| s.split_once('x')).map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())).unwrap_or((1920u32, 1080u32));
    let scale: f64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(0.25);
    let frames: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let cfg = DetectorConfig { analysis_scale: scale, ..Profile::WcagExt.config() };
    let (aw, ah) = cfg.analysis_dims(sw, sh);
    let geom = GridGeometry::new(&cfg, aw, ah);
    let npix = geom.npix();
    let src: Vec<Vec<u8>> = (0..12).map(|i| gen(i, aw as usize, ah as usize)).collect();
    println!("source {sw}x{sh} -> analysis {aw}x{ah} ({npix} px), window {}x{}, {} cells, {frames} frames", geom.ww, geom.wh, geom.ncells());
    let state_bytes = (unflash_core::pixel::StateLayout { k: cfg.k_fail() as usize }.fields() * npix * 4) as f64;
    println!("per-pixel state: {} B ({:.1} MB total)", state_bytes as usize / npix, state_bytes / 1e6);

    for (label, simd) in [("cpu scalar", false), ("cpu simd  ", true)] {
        let mut stage = CpuStage::new(&cfg, geom.clone());
        stage.use_simd = simd;
        let tmpl = unflash_core::pixel::KernelParams::template(&cfg, &geom);
        let t0 = Instant::now();
        let mut kernel_ns = 0u128;
        for i in 0..frames {
            let mut p = tmpl;
            p.now = secs_to_us(i as f64 / 60.0);
            p.mode = if i == 0 { MODE_FIRST } else { 0 };
            let t1 = Instant::now();
            let _ = stage.run(p, FrameInput::rgb(&src[i % 12]));
            kernel_ns += t1.elapsed().as_nanos();
        }
        let secs = t0.elapsed().as_secs_f64();
        let per_frame = kernel_ns as f64 / frames as f64;
        println!(
            "{label}: {:.2} ms/frame = {:.0} fps, {:.2} ns/px, ≈{:.2} GB/s of state traffic (read+write ≈ 130 B/px)",
            per_frame / 1e6,
            frames as f64 / secs,
            per_frame / npix as f64,
            (npix as f64 * 130.0) * (frames as f64 / secs) / 1e9
        );
    }

    match pollster::block_on(GpuContext::new()) {
        Err(e) => println!("gpu: unavailable ({e})"),
        Ok(ctx) => {
            println!("gpu adapter: {} ({:?})", ctx.info().name, ctx.info().backend);
            let mut gpu = GpuStage::new(&ctx, &cfg, geom.clone()).unwrap();
            let tmpl = unflash_core::pixel::KernelParams::template(&cfg, &geom);
            // warm-up: one frame on its own, run at once
            let mut p = tmpl;
            p.mode = MODE_FIRST;
            gpu.submit(p, FrameSource::Rgb8 { data: &src[0], width: aw, height: ah }, false).unwrap();
            gpu.flush();
            gpu.wait_idle();
            while gpu.poll().is_none() {
                gpu.wait_idle();
            }
            let t0 = Instant::now();
            let mut done = 0;
            let mut i = 1;
            while done < frames {
                while i <= frames && gpu.can_submit() {
                    let mut p = tmpl;
                    p.now = secs_to_us(i as f64 / 60.0);
                    gpu.submit(p, FrameSource::Rgb8 { data: &src[i % 12], width: aw, height: ah }, false).unwrap();
                    i += 1;
                }
                if i > frames {
                    // no more frames: run the partial batch rather than wait for it to fill
                    gpu.flush();
                }
                let mut got = false;
                while let Some(r) = gpu.poll() {
                    r.unwrap();
                    done += 1;
                    got = true;
                }
                if done < frames && !got {
                    gpu.wait_idle();
                }
            }
            let secs = t0.elapsed().as_secs_f64();
            let fps = frames as f64 / secs;
            println!(
                "gpu stage : {:.2} ms/frame = {:.0} fps, {:.2} ns/px (pipelined, includes the RGB upload), ≈{:.2} GB/s of state traffic",
                secs / frames as f64 * 1e3,
                fps,
                secs / frames as f64 * 1e9 / npix as f64,
                gpu.bytes_per_frame() as f64 * fps / 1e9
            );
        }
    }
}
