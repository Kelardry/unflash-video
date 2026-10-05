//! Print what each frame of an IVF or WebM file uses, one line per frame
//! (header fields and macroblock kinds), then which coding tools the whole
//! stream exercised: for checking that the test streams cover them.
//!
//!     cargo run --release -p unflash-vp8 --example inspect -- <file.ivf|file.webm>

use std::collections::BTreeSet;

use unflash_vp8::header::{parse_frame, State};
use unflash_vp8::modes::{ALTREF, GOLDEN, INTRA, LAST};
use unflash_vp8::tables::*;
use unflash_vp8::Decoder;

/// The frames of an IVF file, or of the video track of a WebM file.
fn frames(data: &[u8]) -> Vec<&[u8]> {
    if let Some(frames) = unflash_mp4::ivf::frames(data) {
        return frames.into_iter().map(|(_, f)| f).collect();
    }
    let movie = unflash_mp4::demux::parse_bytes(data).expect("an IVF or WebM file");
    let track = movie.video().expect("a video track");
    track.samples.iter().map(|s| &data[s.offset as usize..(s.offset + s.size as u64) as usize]).collect()
}

fn main() {
    let path = std::env::args().nth(1).expect("an IVF or WebM file");
    let data = std::fs::read(&path).expect("read");
    let mut st = State::default();
    let mut dec = Decoder::new(&[]).unwrap();
    let mut tools = BTreeSet::new();
    for (n, frame) in frames(&data).into_iter().enumerate() {
        let mut next = st.clone();
        let f = match parse_frame(frame, &mut next) {
            Ok(f) => f,
            Err(e) => {
                println!("{n:4} {e}");
                continue;
            }
        };
        let h = &f.header;
        let seg = &next.segmentation;
        let d = &next.deltas;
        let mut line = format!("{n:4} {} v{} {:6} bytes", if h.tag.key_frame { "key  " } else { "inter" }, h.tag.version, frame.len());
        if h.tag.key_frame {
            line += &format!(" {}x{} scale {:?}", h.width, h.height, h.scale);
            if h.color_space || h.clamping_type {
                line += &format!(" colour space {} clamping {}", h.color_space as u8, h.clamping_type as u8);
                tools.insert("colour space / clamping bits");
            }
        }
        if !h.tag.show_frame {
            line += " invisible";
            tools.insert("invisible frames");
        }
        line += &format!(" parts {} q {} {:?} lf {}{} sharp {}", f.num_partitions, h.q_index, h.q_deltas, if h.simple_filter { "simple " } else { "" }, h.filter_level, h.sharpness);
        if d.enabled {
            line += &format!(" deltas ref {:?} mode {:?}", d.ref_frame, d.mode);
            tools.insert("loop filter deltas");
        }
        if seg.enabled {
            line += &format!(" seg{}{} q {:?} lf {:?}", if seg.update_map { " map" } else { "" }, if seg.absolute { " abs" } else { "" }, seg.quant, seg.filter_level);
            tools.insert(if seg.update_map { "segment map updates" } else { "segment map kept" });
            if seg.quant.iter().any(|&q| q != 0) {
                tools.insert("segment quantisers");
            }
            if seg.filter_level.iter().any(|&l| l != 0) {
                tools.insert("segment filter levels");
            }
        }
        if !h.tag.key_frame {
            line += &format!(" refresh g{} a{} l{} copy g{} a{} bias g{} a{}", h.refresh_golden as u8, h.refresh_altref as u8, h.refresh_last as u8, h.copy_to_golden, h.copy_to_altref, h.sign_bias_golden as u8, h.sign_bias_altref as u8);
            if h.refresh_golden || h.refresh_altref {
                tools.insert("golden / alt-ref refreshes");
            }
            if h.copy_to_golden != 0 || h.copy_to_altref != 0 {
                tools.insert("golden / alt-ref copies");
            }
            if h.sign_bias_golden || h.sign_bias_altref {
                tools.insert("sign bias");
            }
            if !h.refresh_last {
                tools.insert("frames that leave the last frame alone");
            }
        }
        if !h.refresh_entropy {
            line += " no-entropy-refresh";
            tools.insert("probabilities for one frame only");
        }
        if h.skip_prob.is_none() {
            line += " no-skip";
        }
        if f.num_partitions > 1 {
            tools.insert("several token partitions");
        }
        if h.sharpness > 0 {
            tools.insert("sharpness");
        }
        tools.insert(match (h.simple_filter, h.filter_level) {
            (_, 0) => "no loop filter",
            (true, _) => "simple loop filter",
            (false, _) => "normal loop filter",
        });
        if h.tag.version != 0 {
            tools.insert("bilinear filter (version 1-3)");
        }
        let decoded = dec.decode(frame, n as f64);
        let mut kinds = [0usize; 10];
        let mut refs = [0usize; 4];
        let mut splits = [0usize; 4];
        for mb in dec.macroblocks() {
            kinds[mb.y_mode as usize] += 1;
            refs[mb.ref_frame as usize] += 1;
            if mb.y_mode == SPLITMV {
                splits[mb.split as usize] += 1;
            }
        }
        line += &format!(" | 16x16 {} B {} | last {} golden {} altref {} | nearest {} near {} zero {} new {} split {:?}", kinds[..4].iter().sum::<usize>(), kinds[B_PRED as usize], refs[LAST as usize], refs[GOLDEN as usize], refs[ALTREF as usize], kinds[NEARESTMV as usize], kinds[NEARMV as usize], kinds[ZEROMV as usize], kinds[NEWMV as usize], splits);
        for (count, tool) in [(kinds[B_PRED as usize], "subblock intra prediction"), (refs[INTRA as usize] * !h.tag.key_frame as usize, "intra macroblocks in inter frames"), (refs[GOLDEN as usize], "golden references"), (refs[ALTREF as usize], "alt-ref references"), (kinds[NEWMV as usize], "new motion vectors"), (splits[0] + splits[1], "16x8 / 8x16 splits"), (splits[2], "8x8 splits"), (splits[3], "4x4 splits")] {
            if count > 0 {
                tools.insert(tool);
            }
        }
        match decoded {
            Ok(frames) if frames.iter().any(|f| f.damaged) => line += " DAMAGED",
            Err(e) => line += &format!(" {e}"),
            _ => {}
        }
        println!("{line}");
        st = next;
        if let Some(saved) = f.saved_probs {
            st.probs = saved;
        }
    }
    println!("tools: {}", tools.into_iter().collect::<Vec<_>>().join(", "));
}
