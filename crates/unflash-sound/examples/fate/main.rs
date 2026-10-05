//! Decode real streams, such as the samples of ffmpeg's FATE suite, and
//! compare each with ffmpeg's decode of the same file, which must be on
//! the path:
//!
//!     cargo run --release -p unflash-sound --example fate -- ac3|dts <dir> [name-filter]
//!
//! `ac3` takes the directory's AC-3 and E-AC-3 files and compares them as
//! `ac3.rs` says, `dts` its DTS ones as `dts.rs` says; either prints each
//! file's verdict, and a summary with the decoding speed.

#[path = "../../tests/common/mod.rs"]
mod common;

mod ac3;
mod dts;

use std::path::{Path, PathBuf};

use common::ffmpeg_available;

/// What checking a file gave: whether it passed, the seconds of sound
/// decoded and the seconds the decode took.
pub struct Checked {
    pub ok: bool,
    pub audio: f64,
    pub time: f64,
}

fn main() {
    let codec = std::env::args().nth(1).unwrap_or_default();
    // (the file names each decoder's FATE samples have)
    let (extensions, check): (&[&str], fn(&Path) -> Checked) = match codec.as_str() {
        "ac3" => (&["ac3", "eac3", "ec3"], ac3::check_file),
        "dts" => (&["dts", "dtshd", "cpt", "ts", "m2ts"], dts::check_file),
        _ => {
            eprintln!("usage: fate ac3|dts <dir> [name-filter]");
            std::process::exit(2);
        }
    };
    let dir = PathBuf::from(std::env::args().nth(2).expect("a directory of AC-3 / E-AC-3 or DTS files"));
    let filter = std::env::args().nth(3).unwrap_or_default().to_ascii_lowercase();
    if !ffmpeg_available() {
        eprintln!("ffmpeg and ffprobe must be on the path");
        std::process::exit(2);
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read the directory")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()).is_some_and(|e| extensions.contains(&e.to_ascii_lowercase().as_str())))
        .filter(|p| p.file_name().unwrap().to_string_lossy().to_ascii_lowercase().contains(&filter))
        .collect();
    files.sort();
    let (mut passed, mut failed) = (0, 0);
    let (mut total_audio, mut total_time) = (0.0, 0.0);
    for path in files {
        println!("{}", path.file_name().unwrap().to_string_lossy());
        let checked = check(&path);
        total_audio += checked.audio;
        total_time += checked.time;
        if checked.ok {
            passed += 1;
            println!("  ok");
        } else {
            failed += 1;
            println!("  FAIL");
        }
    }
    println!(
        "\n{passed} passed, {failed} failed; {total_audio:.1} s of audio decoded in {:.3} s ({:.0}x real time)",
        total_time,
        total_audio / total_time.max(1e-9)
    );
    if failed > 0 {
        std::process::exit(1);
    }
}
