//! Decode an MP4 with the built-in decoder and compare every frame with
//! ffmpeg's output, reporting the first differing frame and sample.
//!
//!     cargo run --release -p unflash-h264 --example compare -- file.mp4 [max_frames]

mod common;

use common::{ffmpeg_i420, locate};
use unflash_h264::yuv::to_i420;
use unflash_h264::Decoder;
use unflash_mp4::demux::parse_bytes;

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let max: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let data = std::fs::read(&path).unwrap();
    let movie = parse_bytes(&data).unwrap();
    let track = movie.video().unwrap();
    let mut dec = Decoder::new();
    dec.configure_avcc(track.description.as_ref().unwrap()).unwrap();
    let mut frames: Vec<(i64, Vec<u8>, usize)> = Vec::new();
    let mut kinds: Vec<Vec<unflash_h264::mb::MbKind>> = Vec::new();
    let mut buf = Vec::new();
    let t0 = std::time::Instant::now();
    if std::env::var("H264_DEBUG").is_ok() {
        for id in 0..256 {
            if let Some(pps) = dec.pps(id) {
                println!("pps {id}: cabac {} t8x8 {} scaling_present {}", pps.entropy_coding_mode, pps.transform_8x8_mode, pps.scaling_present);
                println!("  sent4 {:?}\n  sent8 {:?}", pps.sent4, pps.sent8);
            }
        }
    }
    for (i, s) in track.samples.iter().enumerate() {
        let bytes = &data[s.offset as usize..(s.offset + s.size as u64) as usize];
        match dec.decode_sample(bytes, s.pts as f64) {
            Ok(Some(f)) => {
                to_i420(&f.pic, f.crop, &mut buf);
                if f.damaged {
                    println!("sample {i} (pts {}): damaged", s.pts);
                }
                frames.push((s.pts, buf.clone(), i));
                kinds.push(dec.last_mb_kinds().to_vec());
                if std::env::var("H264_DEBUG").is_ok() && frames.len() == 1 {
                    let sps = dec.sps().unwrap();
                    println!("sps: profile {} level {} poc_type {} refs {} direct_8x8_inference {} crop {:?} scaling_present {}", sps.profile_idc, sps.level_idc, sps.poc_type, sps.max_num_ref_frames, sps.direct_8x8_inference, sps.crop, sps.scaling_present);
                    println!("  scaling4[0] {:?} scaling8[0][..16] {:?}", sps.scaling4[0], &sps.scaling8[0][..16]);
                }
            }
            Ok(None) => println!("sample {i}: no frame"),
            Err(e) => {
                println!("sample {i} (pts {}): error {e}", s.pts);
                break;
            }
        }
        if frames.len() >= max {
            break;
        }
    }
    let dt = t0.elapsed();
    let sps = dec.sps().unwrap();
    let (w, h) = sps.cropped_size();
    let (w, h) = (w as usize, h as usize);
    println!("decoded {} frames of {w}x{h} in {:.1} ms ({:.0} fps)", frames.len(), dt.as_secs_f64() * 1e3, frames.len() as f64 / dt.as_secs_f64());
    let order: Vec<usize> = {
        let mut idx: Vec<usize> = (0..frames.len()).collect();
        idx.sort_by_key(|&i| frames[i].0);
        idx
    };
    let kinds_sorted: Vec<Vec<unflash_h264::mb::MbKind>> = order.iter().map(|&i| kinds[i].clone()).collect();
    frames.sort_by_key(|f| f.0);
    let reference = ffmpeg_i420(&[], &path);
    let frame_size = w * h + 2 * (w.div_ceil(2) * h.div_ceil(2));
    let nref = reference.len() / frame_size;
    println!("ffmpeg produced {nref} frames");
    let mut bad = 0;
    for (k, (pts, ours, sample)) in frames.iter().enumerate() {
        if k >= nref {
            break;
        }
        let theirs = &reference[k * frame_size..(k + 1) * frame_size];
        if ours == theirs {
            continue;
        }
        bad += 1;
        if bad > 3 {
            continue;
        }
        let i = (0..frame_size).position(|i| ours[i] != theirs[i]).unwrap();
        let (plane, x, y) = locate(i, w, h);
        let ndiff = ours.iter().zip(theirs).filter(|(a, b)| a != b).count();
        let maxdiff = ours.iter().zip(theirs).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        let mbw = w.div_ceil(16);
        let kind = kinds_sorted[k].get((y / 16) * mbw + x / 16).copied();
        if std::env::var("H264_DIFF").ok().and_then(|v| v.parse::<usize>().ok()) == Some(k) {
            let mut shown = 0;
            for i in 0..frame_size {
                if ours[i] == theirs[i] {
                    continue;
                }
                let (plane, px, py) = locate(i, w, h);
                let kd = kinds_sorted[k].get((py / 16) * mbw + px / 16).copied();
                println!("   {plane} ({px}, {py}) MB ({}, {}) {:?}: ours {} theirs {}", px / 16, py / 16, kd, ours[i], theirs[i]);
                shown += 1;
                if shown >= 60 {
                    break;
                }
            }
        }
        println!("frame {k} (pts {pts}, sample {sample}): {ndiff} samples differ (max |d| {maxdiff}); first in {plane} at ({x}, {y}) = MB ({}, {}) {:?}, ours {} theirs {}", x / 16, y / 16, kind, ours[i], theirs[i]);
    }
    if std::env::var("H264_DEBUG").is_ok() {
        let mut hist = std::collections::BTreeMap::new();
        for ks in &kinds {
            for k in ks {
                *hist.entry(format!("{k:?}")).or_insert(0usize) += 1;
            }
        }
        println!("macroblock kinds: {hist:?}");
    }
    // as many frames as ffmpeg's (at most as many when cut short at max_frames)
    let missing = if frames.len() < max { frames.len() != nref } else { frames.len() > nref };
    if missing {
        println!("{} frames here, {nref} from ffmpeg", frames.len());
    }
    if bad == 0 && !missing {
        println!("all {} frames identical", frames.len());
    } else {
        if bad > 0 {
            println!("{bad} frames differ");
        }
        std::process::exit(1);
    }
}
