//! Decode an Annex B stream with the built-in decoder and compare every
//! picture with ffmpeg's output (in presentation order: pictures sorted by
//! IDR period and POC), reporting the first differing samples.
//!
//!     cargo run --release -p unflash-h264 --example compare_annexb -- stream.264 [max_frames]
//!
//! With `H264_NO_DEBLOCK` set, both decoders leave the deblocking filter out.

mod common;

use common::{ffmpeg_i420, locate};
use unflash_h264::decoder::annexb_nal_units;
use unflash_h264::mb::MbKind;
use unflash_h264::yuv::to_i420;
use unflash_h264::Decoder;

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let max: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let no_deblock = std::env::var_os("H264_NO_DEBLOCK").is_some();
    let data = std::fs::read(&path).unwrap();
    let mut dec = Decoder::new();
    dec.set_skip_deblock(no_deblock);
    // (idr period, poc, frame data, kinds, field flags, damaged)
    let mut frames: Vec<(u32, i32, Vec<u8>, Vec<MbKind>, Vec<bool>, bool, u32)> = Vec::new();
    let mut idr_period = 0u32;
    let mut buf = Vec::new();
    let nals = annexb_nal_units(&data);
    let mut collect = |dec: &mut Decoder, frames: &mut Vec<(u32, i32, Vec<u8>, Vec<MbKind>, Vec<bool>, bool, u32)>, idr_period: &mut u32| {
        let out = dec.decode_annexb(&[], 0.0).unwrap_or_default();
        for f in out {
            to_i420(&f.pic, f.crop, &mut buf);
            if f.pic.is_idr {
                *idr_period += 1;
            }
            frames.push((*idr_period, f.pic.poc, buf.clone(), dec.last_mb_kinds().to_vec(), f.pic.mb_field.clone(), f.damaged, f.pic.frame_num));
        }
    };
    for nal in &nals {
        if let Err(e) = dec.decode_nal(nal, 0.0) {
            println!("error: {e}");
            break;
        }
        collect(&mut dec, &mut frames, &mut idr_period);
        if frames.len() >= max {
            break;
        }
    }
    if frames.len() < max {
        if let Ok(Some(f)) = dec.flush() {
            to_i420(&f.pic, f.crop, &mut buf);
            if f.pic.is_idr {
                idr_period += 1;
            }
            frames.push((idr_period, f.pic.poc, buf.clone(), dec.last_mb_kinds().to_vec(), f.pic.mb_field.clone(), f.damaged, f.pic.frame_num));
        }
    }
    let sps = dec.sps().unwrap();
    let (w, h) = sps.cropped_size();
    let (w, h) = (w as usize, h as usize);
    let wm = sps.width_mbs as usize;
    println!("decoded {} frames of {w}x{h}", frames.len());
    frames.sort_by_key(|f| (f.0, f.1));
    let mut args = vec!["-flags", "unaligned"];
    if no_deblock {
        args.extend(["-skip_loop_filter", "all"]);
    }
    let reference = ffmpeg_i420(&args, &path);
    let frame_size = w * h + 2 * (w.div_ceil(2) * h.div_ceil(2));
    let nref = reference.len() / frame_size;
    println!("ffmpeg produced {nref} frames");
    let mut bad = 0;
    for (k, (period, poc, ours, kinds, fields, damaged, frame_num)) in frames.iter().enumerate() {
        if k >= nref {
            break;
        }
        let theirs = &reference[k * frame_size..(k + 1) * frame_size];
        if let Some(spec) = std::env::var("H264_ROW").ok() {
            // H264_ROW=frame,y0,y1,x0,x1: print a window of luma samples from both decoders
            let v: Vec<usize> = spec.split(',').filter_map(|t| t.parse().ok()).collect();
            if v.len() == 5 && v[0] == k {
                for yy in v[1]..v[2] {
                    println!("   y {yy:3} ours   {}", (v[3]..v[4]).map(|xx| format!("{:3}", ours[yy * w + xx])).collect::<Vec<_>>().join(" "));
                    println!("   y {yy:3} theirs {}", (v[3]..v[4]).map(|xx| format!("{:3}", theirs[yy * w + xx])).collect::<Vec<_>>().join(" "));
                }
            }
        }
        if ours == theirs {
            continue;
        }
        bad += 1;
        if bad > 6 {
            continue;
        }
        let i = (0..frame_size).position(|i| ours[i] != theirs[i]).unwrap();
        let (plane, x, y) = locate(i, w, h);
        let ndiff = ours.iter().zip(theirs).filter(|(a, b)| a != b).count();
        let maxdiff = ours.iter().zip(theirs).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        // the macroblock: field macroblocks live in the rows of their parity
        let mb_y_frame = y / 16;
        let addr = mb_y_frame * wm + x / 16;
        let field_mb = fields.get(addr).copied().unwrap_or(false);
        let addr = if field_mb { ((y / 32) * 2 + (y % 2)) * wm + x / 16 } else { addr };
        let kind = kinds.get(addr).copied();
        // rows of the picture that differ
        let mut rows_bad = 0;
        let mut odd = 0;
        for yy in 0..h {
            if ours[yy * w..yy * w + w] != theirs[yy * w..yy * w + w] {
                rows_bad += 1;
                odd += yy % 2;
            }
        }
        if std::env::var("H264_DIFF").ok().and_then(|v| v.parse::<usize>().ok()) == Some(k) {
            // per macroblock (of each field) counts of differing luma samples
            let hm = h.div_ceil(16);
            // per field: macroblock rows of that field
            for parity in 0..2 {
                let rows = hm.div_ceil(2);
                let mut counts = vec![0usize; wm * rows];
                for yy in (parity..h).step_by(2) {
                    let fr = yy / 2;
                    for xx in 0..w {
                        if ours[yy * w + xx] != theirs[yy * w + xx] {
                            counts[(fr / 16) * wm + xx / 16] += 1;
                        }
                    }
                }
                println!("   field {parity}:");
                for my in 0..rows {
                    println!("   MB row {my:2}: {}", (0..wm).map(|mx| format!("{:3}", counts[my * wm + mx])).collect::<Vec<_>>().join(" "));
                }
            }
            let mut shown = 0;
            for i in 0..w * h {
                if ours[i] != theirs[i] {
                    println!("   Y ({}, {}): ours {} theirs {}", i % w, i / w, ours[i], theirs[i]);
                    shown += 1;
                    if shown >= 24 {
                        break;
                    }
                }
            }
            // the macroblock kinds (f = field macroblock)
            println!("   kinds:");
            for my in 0..hm {
                let row = (0..wm)
                    .map(|mx| {
                        let a = my * wm + mx;
                        let k = match kinds.get(a) {
                            Some(k) => format!("{:?}", k),
                            None => "?".into(),
                        };
                        let k: String = k.chars().take(4).collect();
                        format!("{:>4}{}", k, if fields.get(a).copied().unwrap_or(false) { "f" } else { " " })
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                println!("   MB row {my:2}: {row}");
            }
            // chroma: per frame macroblock (8x8 chroma samples), both planes
            let (cw, ch) = (w / 2, h / 2);
            for (name, off) in [("U", w * h), ("V", w * h + cw * ch)] {
                let mut counts = vec![0usize; wm * hm];
                let mut shown = 0;
                for yy in 0..ch {
                    for xx in 0..cw {
                        let i = off + yy * cw + xx;
                        if i < ours.len() && i < theirs.len() && ours[i] != theirs[i] {
                            counts[(yy / 8) * wm + xx / 8] += 1;
                            if shown < 12 {
                                println!("   {name} ({xx}, {yy}) = MB ({}, {}): ours {} theirs {}", xx / 8, yy / 8, ours[i], theirs[i]);
                                shown += 1;
                            }
                        }
                    }
                }
                println!("   {name} per frame MB:");
                for my in 0..hm {
                    println!("   MB row {my:2}: {}", (0..wm).map(|mx| format!("{:3}", counts[my * wm + mx])).collect::<Vec<_>>().join(" "));
                }
            }
        }
        println!("frame {k} (period {period}, poc {poc}, frame_num {frame_num}, damaged {damaged}): {ndiff} samples differ (max |d| {maxdiff}) in {rows_bad} luma rows ({odd} odd); first in {plane} at ({x}, {y}) = MB ({}, {}) field {field_mb} {:?}, ours {} theirs {}", x / 16, y / 16, kind, ours[i], theirs[i]);
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
