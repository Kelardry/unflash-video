//! Cross-check against the Python reference detector.
//!
//! `tests/gen_fixtures.py` runs `unflash/analysis.py` over procedurally
//! generated frames and records what it saw. This test regenerates the
//! identical frames (guarded by CRC-32 over every byte) and asserts the
//! Rust port sees the same thing: per-frame hazard areas, held frames,
//! events, violations and the verdict.

use std::path::PathBuf;

use serde::Deserialize;
use unflash_core::config::Profile;
use unflash_core::detector::CpuDetector;
use unflash_core::grid::FrameInput;
use unflash_core::temporal::{EventKind, ViolationKind};

const W: usize = 128;
const H: usize = 96;

#[derive(Deserialize)]
struct Fixture {
    name: String,
    profile: String,
    w: usize,
    h: usize,
    frames_crc32: u32,
    t: Vec<f64>,
    stats: Stats,
    events: Vec<Event>,
    violations: Vec<Viol>,
    anomalies: usize,
    held: usize,
    safe: bool,
    wcag_safe: bool,
    area_thresh: u32,
    ww: u32,
    wh: u32,
}

#[derive(Deserialize)]
struct Stats {
    tc: Vec<f64>,
    lum: Vec<f64>,
    hazard: Vec<u32>,
    hazard_red: Vec<u32>,
    ext: Vec<u32>,
    ext_red: Vec<u32>,
    up: Vec<u32>,
    down: Vec<u32>,
    red: Vec<u32>,
    onset: Vec<f64>,
    onset_red: Vec<f64>,
    held: Vec<u8>,
}

#[derive(Deserialize)]
struct Event {
    t: f64,
    tc: f64,
    kind: String,
    area: u32,
    bbox: [u32; 4],
}

#[derive(Deserialize)]
struct Viol {
    start: f64,
    end: f64,
    kind: String,
    count: f64,
    onset: f64,
    peak: f64,
}

// ---- the generator, mirrored from gen_fixtures.py ---------------------------

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

fn solid(code: u8) -> Vec<u8> {
    vec![code; W * H * 3]
}

fn fill_rect(f: &mut [u8], x0: usize, y0: usize, x1: usize, y1: usize, rgb: [u8; 3]) {
    for y in y0..y1 {
        for x in x0..x1 {
            let i = (y * W + x) * 3;
            f[i..i + 3].copy_from_slice(&rgb);
        }
    }
}

fn rect_centred(frac: f64) -> (usize, usize, usize, usize) {
    let rw = (W as f64 * frac.powf(0.5)) as usize;
    let rh = (H as f64 * frac.powf(0.5)) as usize;
    let x0 = (W - rw) / 2;
    let y0 = (H - rh) / 2;
    (x0, y0, x0 + rw, y0 + rh)
}

fn phase(t: f64, hz: f64) -> i64 {
    ((t * hz * 2.0).floor() as i64).rem_euclid(2)
}

fn n2997(secs: f64) -> usize {
    (secs * 30000.0 / 1001.0).round() as usize
}

fn times_2997(n: usize) -> Vec<f64> {
    (0..n).map(|i| (i as f64 * 1001.0) / 30000.0).collect()
}

fn gen_square(t: f64, hz: f64, frac: f64, lo: u8, hi: u8, bg: u8) -> Vec<u8> {
    let mut f = solid(bg);
    let (x0, y0, x1, y1) = rect_centred(frac);
    let c = if phase(t, hz) == 0 { lo } else { hi };
    fill_rect(&mut f, x0, y0, x1, y1, [c, c, c]);
    f
}

fn noise_frame(i: usize, base: i32, amp: i32) -> Vec<u8> {
    let mut f = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let c = (base + hash_noise(i, y, x, amp)).clamp(0, 255) as u8;
            let k = (y * W + x) * 3;
            f[k] = c;
            f[k + 1] = c;
            f[k + 2] = c;
        }
    }
    f
}

fn scenario(name: &str) -> (Vec<f64>, Vec<Vec<u8>>) {
    match name {
        "square4hz" => {
            let ts = times_2997(n2997(6.0));
            let fr = ts.iter().map(|&t| gen_square(t, 4.0, 0.5, 20, 200, 20)).collect();
            (ts, fr)
        }
        "square3hz" => {
            let ts = times_2997(n2997(10.0));
            let fr = ts.iter().map(|&t| gen_square(t, 3.0, 0.5, 20, 200, 20)).collect();
            (ts, fr)
        }
        "noise_drift" => {
            let n = n2997(8.0);
            let ts = times_2997(n);
            let fr = (0..n).map(|i| noise_frame(i, 30 + ((i * 170) / n) as i32, 12)).collect();
            (ts, fr)
        }
        "red_grey" => {
            let ts = times_2997(n2997(5.0));
            let fr = ts
                .iter()
                .map(|&t| {
                    let mut f = solid(144);
                    if phase(t, 4.0) == 0 {
                        fill_rect(&mut f, 0, 0, W, H, [255, 0, 0]);
                    }
                    f
                })
                .collect();
            (ts, fr)
        }
        "red_equilum" => {
            // saturated red against a grey of the same relative luminance:
            // no luminance flash at all, only the colour moves
            let ts = times_2997(n2997(5.0));
            let fr = ts
                .iter()
                .map(|&t| {
                    let mut f = solid(0);
                    let rgb = if phase(t, 5.0) == 0 { [250, 0, 0] } else { [122, 124, 122] };
                    fill_rect(&mut f, 0, 0, W, H, rgb);
                    f
                })
                .collect();
            (ts, fr)
        }
        "pan_bar" => {
            let ts = times_2997(n2997(6.0));
            let bw = W / 10;
            let fr = ts
                .iter()
                .map(|&t| {
                    let mut f = solid(20);
                    let x = ((t * W as f64).floor() as i64).rem_euclid(W as i64) as usize;
                    for k in 0..bw {
                        let col = (x + k) % W;
                        for y in 0..H {
                            let i = (y * W + col) * 3;
                            f[i] = 230;
                            f[i + 1] = 230;
                            f[i + 2] = 230;
                        }
                    }
                    f
                })
                .collect();
            (ts, fr)
        }
        "repeat120" => {
            let n = (4.0f64 * 120.0).round() as usize;
            let ts: Vec<f64> = (0..n).map(|i| i as f64 / 120.0).collect();
            let fr = ts.iter().map(|&t| gen_square(t, 4.0, 0.5, 20, 200, 20)).collect();
            (ts, fr)
        }
        "vfr" => {
            let base = times_2997(n2997(8.0));
            let ts = base
                .iter()
                .enumerate()
                .map(|(i, &t)| {
                    let mut t = t;
                    if (60..120).contains(&i) {
                        t -= 0.5;
                    }
                    if i >= 150 {
                        t += 10.0;
                    }
                    t
                })
                .collect();
            let fr = base.iter().map(|&t| gen_square(t, 4.0, 0.5, 20, 200, 20)).collect();
            (ts, fr)
        }
        "ramp" => {
            let n = n2997(6.0);
            let ts = times_2997(n);
            let codes = [40u8, 75, 110, 145, 180, 145, 110, 75];
            let fr = (0..n).map(|i| solid(codes[i % 8])).collect();
            (ts, fr)
        }
        "partial15" | "partial35" => {
            let (rw, rh) = if name == "partial15" { (33, 25) } else { (50, 38) };
            let ts = times_2997(n2997(6.0));
            let fr = ts
                .iter()
                .map(|&t| {
                    let mut f = solid(20);
                    let c = if phase(t, 4.0) == 0 { 20 } else { 200 };
                    fill_rect(&mut f, 0, 0, rw, rh, [c, c, c]);
                    f
                })
                .collect();
            (ts, fr)
        }
        "red_noise" => {
            let n = n2997(6.0);
            let ts = times_2997(n);
            let (x0, y0, x1, y1) = rect_centred(0.5);
            let fr = ts
                .iter()
                .enumerate()
                .map(|(i, &t)| {
                    let mut f = noise_frame(i, 60, 8);
                    let rgb = if phase(t, 4.0) == 0 { [255, 0, 0] } else { [144, 144, 144] };
                    fill_rect(&mut f, x0, y0, x1, y1, rgb);
                    f
                })
                .collect();
            (ts, fr)
        }
        "red_darkred" | "red_green" => {
            // the whole picture swapping between two colours: red against a
            // darker red (one chromaticity: no red flash under WCAG 2.2), and
            // red against a green of the same luminance (a red flash only)
            let (a, b) = if name == "red_darkred" { ([255, 0, 0], [110, 0, 0]) } else { ([200, 0, 0], [0, 116, 0]) };
            let ts = times_2997(n2997(5.0));
            let fr = ts
                .iter()
                .map(|&t| {
                    let mut f = solid(0);
                    fill_rect(&mut f, 0, 0, W, H, if phase(t, 4.0) == 0 { a } else { b });
                    f
                })
                .collect();
            (ts, fr)
        }
        other => panic!("unknown scenario {other}"),
    }
}

fn crc32(data: &[u8], crc: u32) -> u32 {
    let mut c = !crc;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB88320 } else { c >> 1 };
        }
    }
    !c
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures")
}

fn check(fix: &Fixture) {
    assert_eq!((fix.w, fix.h), (W, H));
    let (ts, frames) = scenario(&fix.name);
    assert_eq!(ts.len(), fix.t.len(), "{}: frame count", fix.name);
    let mut crc = 0u32;
    for f in &frames {
        crc = crc32(f, crc);
    }
    assert_eq!(crc, fix.frames_crc32, "{}: regenerated frames differ from the Python generator", fix.name);
    for (i, (a, b)) in ts.iter().zip(&fix.t).enumerate() {
        assert_eq!(a, b, "{}: timestamp {i}", fix.name);
    }

    let cfg = Profile::from_name(&fix.profile).unwrap().config();
    let mut det = CpuDetector::new(cfg, W as u32, H as u32);
    let g = det.geometry().clone();
    assert_eq!((g.ww, g.wh, g.area_thresh), (fix.ww, fix.wh, fix.area_thresh));

    let name = &fix.name;
    for (i, (&t, f)) in fix.t.iter().zip(&frames).enumerate() {
        let rec = det.feed(t, FrameInput::rgb(f));
        let s = &fix.stats;
        assert_eq!(rec.tc, s.tc[i], "{name} f{i}: clock");
        assert_eq!(rec.held, s.held[i] != 0, "{name} f{i}: held");
        assert_eq!(rec.hazard, s.hazard[i], "{name} f{i}: hazard area");
        assert_eq!(rec.hazard_red, s.hazard_red[i], "{name} f{i}: red hazard area");
        assert_eq!(rec.ext, s.ext[i], "{name} f{i}: extended area");
        assert_eq!(rec.ext_red, s.ext_red[i], "{name} f{i}: red extended area");
        assert!((rec.lum as f64 - s.lum[i]).abs() < 2e-5, "{name} f{i}: lum {} vs {}", rec.lum, s.lum[i]);
        assert!((rec.hazard_onset - s.onset[i]).abs() < 3e-6, "{name} f{i}: onset {} vs {}", rec.hazard_onset, s.onset[i]);
        assert!((rec.hazard_red_onset - s.onset_red[i]).abs() < 3e-6, "{name} f{i}: red onset");
        // the reference searches every window position for the chart
        // areas; the port only the grid, so it can only report less
        assert!(rec.up_area <= s.up[i], "{name} f{i}: up area {} > {}", rec.up_area, s.up[i]);
        assert!(rec.down_area <= s.down[i], "{name} f{i}: down area");
        assert!(rec.red_area <= s.red[i], "{name} f{i}: red area");
        if s.up[i] == 0 {
            assert_eq!(rec.up_area, 0);
        }
    }
    let res = det.finish();
    assert_eq!(res.anomalies, fix.anomalies, "{name}: anomalies");
    assert_eq!(res.held, fix.held, "{name}: held frames");
    assert_eq!(res.safe(), fix.safe, "{name}: safe");
    assert_eq!(res.wcag_safe(), fix.wcag_safe, "{name}: wcag_safe");

    assert_eq!(res.events.len(), fix.events.len(), "{name}: event count {:?}", res.events);
    for (i, (a, b)) in res.events.iter().zip(&fix.events).enumerate() {
        assert_eq!(a.t, b.t, "{name} event {i}: t");
        assert_eq!(a.tc, b.tc, "{name} event {i}: tc");
        let kind = match a.kind {
            EventKind::General => "general",
            EventKind::Red => "red",
        };
        assert_eq!(kind, b.kind, "{name} event {i}: kind");
        assert_eq!(a.area, b.area, "{name} event {i}: area");
        assert_eq!(a.bbox, b.bbox, "{name} event {i}: bbox");
    }

    assert_eq!(res.violations.len(), fix.violations.len(), "{name}: violations {:?} vs {:?}", res.violations, fix.violations.iter().map(|v| (v.kind.clone(), v.start, v.end)).collect::<Vec<_>>());
    for (i, (a, b)) in res.violations.iter().zip(&fix.violations).enumerate() {
        let kind = match a.kind {
            ViolationKind::Flash => "flash",
            ViolationKind::Red => "red",
            ViolationKind::Extended => "extended",
            ViolationKind::Pattern => "pattern",
        };
        assert_eq!(kind, b.kind, "{name} violation {i}: kind");
        assert!((a.start - b.start).abs() < 1e-9, "{name} violation {i}: start {} vs {}", a.start, b.start);
        assert!((a.end - b.end).abs() < 1e-9, "{name} violation {i}: end {} vs {}", a.end, b.end);
        assert!((a.peak - b.peak).abs() < 1e-9, "{name} violation {i}: peak {} vs {}", a.peak, b.peak);
        assert!((a.onset - b.onset).abs() < 3e-6, "{name} violation {i}: onset {} vs {}", a.onset, b.onset);
        assert!((a.count - b.count).abs() < 0.011, "{name} violation {i}: count {} vs {}", a.count, b.count);
    }
}

#[test]
fn matches_python_reference() {
    let dir = fixtures_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("no fixtures in {}: {e}; run tests/gen_fixtures.py", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures found in {}", dir.display());
    for p in paths {
        let fix: Fixture = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        check(&fix);
        eprintln!("ok  {} ({}): {} frames, {} violations", fix.name, fix.profile, fix.t.len(), fix.violations.len());
    }
}
