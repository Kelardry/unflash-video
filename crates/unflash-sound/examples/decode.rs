//! Decode an AC-3 / E-AC-3 or a DTS stream (frames one after the other,
//! as an .ac3, .eac3 or .dts file or a Matroska track's blocks hold them)
//! to interleaved 32-bit float PCM, as `ffmpeg -i IN -f f32le -c:a
//! pcm_f32le OUT` would write it.
//!
//!     cargo run --release -p unflash-sound --example decode -- ac3|dts IN OUT [stereo] [quiet] [drc]
//!
//! `stereo` asks for the Lo/Ro downmix; for AC-3, `quiet` turns dither and
//! the spectral extension noise off; for DTS, `drc` applies the dynamic
//! range coefficients.

use unflash_sound::{ac3, dts, Output};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || !matches!(args[1].as_str(), "ac3" | "dts") {
        eprintln!("usage: decode ac3|dts IN OUT [stereo] [quiet] [drc]");
        std::process::exit(2);
    }
    let data = std::fs::read(&args[2]).expect("read input");
    let output = if args.iter().any(|a| a == "stereo") { Output::Stereo } else { Output::Native };
    let mut out = Vec::new();
    // (the decode alone is timed; what the decoder says of the stream is
    // its sample rate and, for the log, its info and features)
    let (r, secs, rate, about) = if args[1] == "ac3" {
        let mut dec = ac3::Decoder::new(output);
        dec.set_noise(!args.iter().any(|a| a == "quiet"));
        let t0 = std::time::Instant::now();
        let r = dec.decode(&data, &mut out).map_err(|e| e.to_string());
        (r, t0.elapsed().as_secs_f64(), dec.info().map(|i| i.sample_rate), format!("{:?}, {:?}", dec.info(), dec.features()))
    } else {
        let mut dec = dts::Decoder::new(output);
        dec.set_dynamic_range(args.iter().any(|a| a == "drc"));
        let t0 = std::time::Instant::now();
        let r = dec.decode(&data, &mut out).map_err(|e| e.to_string());
        (r, t0.elapsed().as_secs_f64(), dec.info().map(|i| i.sample_rate), format!("{:?}, {:?}", dec.info(), dec.features()))
    };
    match r {
        Ok(d) => {
            eprintln!("{} samples, {} damaged frames, {about}", d.samples, d.damaged);
            if let Some(rate) = rate {
                let dur = d.samples as f64 / rate as f64;
                eprintln!("decoded {dur:.2} s in {:.3} s ({:.0}x real time)", secs, dur / secs);
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
    let n = out.first().map_or(0, |c| c.len());
    let mut bytes = Vec::with_capacity(n * out.len() * 4);
    for i in 0..n {
        for c in &out {
            bytes.extend_from_slice(&c[i].to_le_bytes());
        }
    }
    std::fs::write(&args[3], bytes).expect("write output");
}
