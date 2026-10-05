//! AC-3 and E-AC-3 streams (`fate ac3`), such as the samples of ffmpeg's
//! FATE suite (film soundtracks: block switching, coupling, spectral
//! extension, the adaptive hybrid transform, dependent substreams), each
//! compared with ffmpeg's decode of the same file.
//!
//! The comparison is the one `tests/common/ac3.rs` describes: every
//! transform coefficient that carries no noise must agree to float
//! precision, and the noise (dither, AHT dither, spectral extension noise)
//! must be about as loud as ours. Two differences are ffmpeg 6.1's, and
//! are left out and reported: at a switched block (`blksw`) ffmpeg
//! overlaps with the wrong channel's previous block (liba52 agrees with
//! this decoder there), and it decodes the AHT's 32-entry vector quantizer
//! (hebap 4) one row off Table E4.4. Channels that a dependent substream
//! replaces in ffmpeg's decode (this decoder plays independent substream 0
//! only) are reported and not compared.

use std::path::Path;

use unflash_sound::ac3::Output;

use crate::common::ac3::{compare, decode, describe};
use crate::common::ffmpeg_decode;
use crate::Checked;

/// Channel names of our output (WAVE order) for a coding mode.
fn our_names(acmod: u8, lfe: bool) -> Vec<&'static str> {
    let mut v: Vec<&str> = match acmod {
        1 => vec!["FC"],
        0 | 2 | 4 | 6 => vec!["FL", "FR"],
        _ => vec!["FL", "FR", "FC"],
    };
    if lfe {
        v.push("LFE");
    }
    match acmod {
        4 | 5 => v.push("BC"),
        6 | 7 => v.extend(["SL", "SR"]),
        _ => {}
    }
    v
}

/// Channel names of ffmpeg's output for a layout name ffprobe prints.
fn ffmpeg_names(layout: &str) -> Option<Vec<&'static str>> {
    Some(match layout {
        "mono" => vec!["FC"],
        "stereo" => vec!["FL", "FR"],
        "2.1" => vec!["FL", "FR", "LFE"],
        "3.0" => vec!["FL", "FR", "FC"],
        "3.0(back)" => vec!["FL", "FR", "BC"],
        "4.0" => vec!["FL", "FR", "FC", "BC"],
        "quad" => vec!["FL", "FR", "BL", "BR"],
        "quad(side)" => vec!["FL", "FR", "SL", "SR"],
        "3.1" => vec!["FL", "FR", "FC", "LFE"],
        "4.1" => vec!["FL", "FR", "FC", "LFE", "BC"],
        "5.0" => vec!["FL", "FR", "FC", "BL", "BR"],
        "5.0(side)" => vec!["FL", "FR", "FC", "SL", "SR"],
        "5.1" => vec!["FL", "FR", "FC", "LFE", "BL", "BR"],
        "5.1(side)" => vec!["FL", "FR", "FC", "LFE", "SL", "SR"],
        "7.1" => vec!["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"],
        _ => return None,
    })
}

fn ffprobe_layout(path: &Path) -> String {
    let out = std::process::Command::new("ffprobe").args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=channel_layout", "-of", "csv=p=0"]).arg(path).output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

/// The channels of the independent substream that the first dependent
/// substream frame replaces (Annex E §3.8.2): those its custom channel
/// map names, or those its coding mode has.
fn replaced_by_dependent(data: &[u8]) -> Vec<&'static str> {
    let mut pos = 0;
    while pos + 16 < data.len() {
        if data[pos] == 0x0b && data[pos + 1] == 0x77 && data[pos + 5] >> 3 > 10 && data[pos + 2] >> 6 == 1 {
            let bits: Vec<u8> = data[pos + 2..pos + 12].iter().flat_map(|b| (0..8).rev().map(move |k| (b >> k) & 1)).collect();
            let mut i = 0;
            let mut read = |n: usize| -> u32 {
                let v = bits[i..i + n].iter().fold(0u32, |a, &b| a << 1 | b as u32);
                i += n;
                v
            };
            read(2 + 3 + 11); // strmtyp, substreamid, frmsiz
            if read(2) == 3 {
                read(2); // fscod2
            } else {
                read(2); // numblkscod
            }
            let acmod = read(3) as u8;
            let lfeon = read(1) == 1;
            read(5 + 5); // bsid, dialnorm
            if read(1) == 1 {
                read(8);
            }
            if acmod == 0 {
                read(5);
                if read(1) == 1 {
                    read(8);
                }
            }
            if read(1) == 1 {
                let map = read(16);
                let names = [(0, "FL"), (1, "FC"), (2, "FR"), (3, "SL"), (4, "SR"), (15, "LFE")];
                return names.iter().filter(|(bit, _)| map & (0x8000 >> bit) != 0).map(|(_, n)| *n).collect();
            }
            return our_names(acmod, lfeon);
        }
        pos += 1;
    }
    Vec::new()
}

/// Decode `path` and compare it with ffmpeg's decode.
pub fn check_file(path: &Path) -> Checked {
    let data = std::fs::read(path).expect("read");
    let t0 = std::time::Instant::now();
    let noisy = decode(&data, Output::Native, true);
    let secs = t0.elapsed().as_secs_f64();
    let quiet = decode(&data, Output::Native, false);
    let info = noisy.info.expect("stream info");
    let seconds = noisy.decoded.samples as f64 / info.sample_rate as f64;
    let f = &noisy.features;
    let mut used = Vec::new();
    for (on, what) in [
        (f.block_switching, "block switching"),
        (f.coupling, "coupling"),
        (f.phase_flags, "phase flags"),
        (f.rematrixing, "rematrixing"),
        (f.delta_bit_allocation, "delta bit allocation"),
        (f.spectral_extension, "SPX"),
        (f.spx_attenuation, "SPX attenuation"),
        (f.aht, "AHT"),
        (f.aht_vq, "AHT vector quantization"),
        (f.aht_gaq, "AHT gain-adaptive quantization"),
        (f.transient_pre_noise, "transient pre-noise data"),
        (f.skip_fields, "skip fields"),
        (f.dynrng, "dynrng"),
        (f.dither, "dither"),
        (noisy.skipped_substream_frames > 0, "dependent / other substreams"),
    ] {
        if on {
            used.push(what);
        }
    }
    println!(
        "  {} {} Hz, acmod {}{}, {:.2} s, {} damaged frames, {} frames of other substreams skipped; decoded in {:.1} ms ({:.0}x real time)",
        if info.eac3 { "E-AC-3" } else { "AC-3" },
        info.sample_rate,
        info.acmod,
        if info.lfe { " + LFE" } else { "" },
        seconds,
        noisy.decoded.damaged,
        noisy.skipped_substream_frames,
        secs * 1e3,
        seconds / secs
    );
    println!("  uses: {}", used.join(", "));
    let Some(reference) = ffmpeg_decode(path, &[], &[]) else {
        println!("  ffmpeg could not decode it");
        return Checked { ok: false, audio: seconds, time: secs };
    };
    // which of ffmpeg's channels each of ours is
    let ours = our_names(info.acmod, info.lfe);
    let replaced = if noisy.skipped_substream_frames > 0 { replaced_by_dependent(&data) } else { Vec::new() };
    let map: Vec<Option<usize>> = if reference.len() == ours.len() {
        (0..ours.len()).map(Some).collect()
    } else {
        let theirs = ffmpeg_names(&ffprobe_layout(path)).unwrap_or_default();
        ours.iter().map(|n| theirs.iter().position(|t| t == n)).collect()
    };
    let map: Vec<Option<usize>> = map.iter().zip(&ours).map(|(m, n)| if replaced.contains(n) { None } else { *m }).collect();
    if reference.len() != ours.len() {
        println!("  ffmpeg decodes {} channels, this decoder independent substream 0's {}", reference.len(), ours.len());
    }
    let len = |v: &Vec<Vec<f32>>| v.first().map_or(0, |c| c.len());
    if len(&reference) != len(&noisy.out) {
        println!("  lengths differ: ours {} samples, ffmpeg {}", len(&noisy.out), len(&reference));
    }
    let stats = compare(&quiet.out, &noisy.out, &noisy.trace, &reference, &map);
    let mut ok = true;
    for (c, s) in stats.iter().enumerate() {
        if map[c].is_none() {
            println!("  {:>3}: not compared: {}", ours[c], if replaced.contains(&ours[c]) { "ffmpeg plays the dependent substream's channel here" } else { "ffmpeg has no such channel" });
            continue;
        }
        let mut notes = Vec::new();
        if s.over > 0 {
            ok = false;
            notes.push("coefficients differ");
        }
        for (what, n) in [("dither", s.dither), ("AHT dither", s.aht_dither), ("SPX noise", s.spx)] {
            if n.bins >= 200 && n.rms_ours() > 1e-7 && !(0.5..=2.0).contains(&n.ratio()) {
                ok = false;
                notes.push(what);
            }
        }
        println!("  {:>3}: {}{}", ours[c], describe(s), if notes.is_empty() { String::new() } else { format!("  <-- {}", notes.join(", ")) });
    }
    Checked { ok, audio: seconds, time: secs }
}
