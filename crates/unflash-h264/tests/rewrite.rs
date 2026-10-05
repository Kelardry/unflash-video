//! The parameter-set rewriter: a renumbered stream decodes to the same
//! pictures, and GOPs of one stream spliced into another (what the
//! smart-cut export does) decode as their sources did, in this decoder and
//! in ffmpeg's.

mod common;

use std::process::Command;

use common::{expected, md5s, mux, read, sample};
use unflash_h264::decoder::sample_nal_units;
use unflash_h264::rewrite::{first_vcl_nal_type, parse_avcc, AvcRegistry};
use unflash_h264::Decoder;
use unflash_mp4::demux::parse_bytes;

/// Decode a file in memory: the MD5 of every frame in presentation order.
fn decode(data: &[u8]) -> Vec<String> {
    let movie = parse_bytes(data).unwrap();
    let track = movie.video().unwrap();
    let mut dec = Decoder::new();
    dec.configure_avcc(track.description.as_ref().unwrap()).unwrap();
    md5s(&mut dec, data, track, "the file", |b| b.to_vec()).unwrap()
}

/// Every conformance stream, renumbered away from the ids another stream
/// holds, decodes to ffmpeg's frames still: CAVLC and CABAC slices, several
/// slices per picture, I_PCM macroblocks (byte-aligned in CAVLC data),
/// B-frames, MBAFF.
#[test]
fn renumbered_streams_decode_the_same() {
    let base = read("splice_b");
    let base_avcc = parse_bytes(&base).unwrap().video().unwrap().description.clone().unwrap();
    for name in ["cb_cavlc", "cb_crop_slices", "main_cabac_b", "main_cavlc_wp", "high_8x8_aq", "high_cqm_wp", "high_cip_nodeblock", "high_opengop_refs", "high_crop_cabac", "pcm_cabac", "pcm_cavlc", "high_interlaced"] {
        let data = read(name);
        let movie = parse_bytes(&data).unwrap();
        let track = movie.video().unwrap();
        let mut reg = AvcRegistry::new(&base_avcc).unwrap();
        let mut rw = reg.register(track.description.as_ref().unwrap()).unwrap();
        assert!(!rw.is_identity(), "{name}: the ids collide with the base, so the stream is renumbered");
        let rec = parse_avcc(&reg.record()).unwrap();
        assert_eq!(rec.sps.len(), 2, "{name}: the base's SPS and the stream's");
        let mut dec = Decoder::new();
        dec.configure_avcc(&reg.record()).unwrap();
        let got = md5s(&mut dec, &data, track, name, |b| rw.rewrite_sample(b).unwrap_or_else(|e| panic!("{name}: {e}"))).unwrap_or_else(|e| panic!("{name}: {e}"));
        let want = expected(name);
        assert_eq!(got.len(), want.len(), "{name}: frame count");
        for (i, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{name}: frame {i} differs after renumbering");
        }
    }
}

/// GOPs of `other` take the place of the odd GOPs of `base` in one track,
/// as the export splices re-encoded spans into copied ones. Both streams
/// have an IDR every 10 frames.
fn splice(base: &str, other: &str) -> Vec<u8> {
    let a = read(base);
    let b = read(other);
    let ma = parse_bytes(&a).unwrap();
    let mb = parse_bytes(&b).unwrap();
    let ta = ma.video().unwrap();
    let tb = mb.video().unwrap();
    assert_eq!(ta.samples.len(), 40);
    assert_eq!(tb.samples.len(), 40);
    let mut reg = AvcRegistry::new(ta.description.as_ref().unwrap()).unwrap();
    let mut rw = reg.register(tb.description.as_ref().unwrap()).unwrap();
    assert!(!rw.is_identity());
    // decode-order runs of 10 samples are the GOPs (closed GOPs: an IDR
    // every 10 frames in both orders)
    for (t, data) in [(ta, &a), (tb, &b)] {
        let len = parse_avcc(t.description.as_ref().unwrap()).unwrap().len_size;
        for (i, s) in t.samples.iter().enumerate() {
            assert_eq!(s.sync, i % 10 == 0, "sync sample {i}");
            assert_eq!(first_vcl_nal_type(sample(data, s), len), if i % 10 == 0 { 5 } else { 1 }, "IDR at {i}");
        }
    }
    let mut payloads: Vec<(Vec<u8>, i64, bool)> = Vec::new();
    for g in 0..4 {
        let (t, data, from_other) = if g % 2 == 0 { (ta, &a, false) } else { (tb, &b, true) };
        for s in &t.samples[g * 10..g * 10 + 10] {
            let bytes = if from_other { rw.rewrite_sample(sample(data, s)).unwrap() } else { sample(data, s).to_vec() };
            payloads.push((bytes, s.pts, s.sync));
        }
    }
    mux(reg.record(), ta, &payloads)
}

fn check_splice(base: &str, other: &str, got: &[String], who: &str) {
    let ea = expected(base);
    let eb = expected(other);
    assert_eq!(got.len(), 40, "{who}: frame count of {base}+{other}");
    for (i, g) in got.iter().enumerate() {
        let want = if (i / 10) % 2 == 0 { &ea[i] } else { &eb[i] };
        assert_eq!(g, want, "{who}: frame {i} of {base}+{other} (GOP {}) differs from its source", i / 10);
    }
}

/// ffmpeg's per-frame MD5s of a file, when ffmpeg is installed.
fn ffmpeg_md5(file: &[u8], name: &str) -> Option<Vec<String>> {
    let dir = std::env::temp_dir().join(format!("unflash-splice-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("spliced.mp4");
    std::fs::write(&path, file).unwrap();
    let out = match Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-f", "framemd5", "-"]).output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ffmpeg not run ({e}): the cross-check is skipped");
            return None;
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    assert!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    Some(common::framemd5(&String::from_utf8_lossy(&out.stdout)))
}

#[test]
fn spliced_gops_decode_as_their_sources() {
    // High CABAC with B-frames, joined with Main CAVLC without, and with
    // Main CABAC with B-frames; and CAVLC as the base
    for (base, other) in [("splice_a", "splice_b"), ("splice_a", "splice_c"), ("splice_b", "splice_c")] {
        let file = splice(base, other);
        let rec = parse_avcc(parse_bytes(&file).unwrap().video().unwrap().description.as_ref().unwrap()).unwrap();
        assert_eq!((rec.sps.len(), rec.pps.len()), (2, 2), "{base}+{other}: both streams' parameter sets");
        check_splice(base, other, &decode(&file), "this decoder");
        if let Some(md5s) = ffmpeg_md5(&file, &format!("{base}-{other}")) {
            check_splice(base, other, &md5s, "ffmpeg");
        }
    }
}

/// Every slice header survives the renumbering field for field (only the
/// PPS id changes) and the slice data bits are the same: CAVLC data moved
/// by whole bytes, CABAC data re-aligned.
#[test]
fn renumbering_changes_only_the_pps_id() {
    use unflash_h264::bitreader::{unescape, BitReader};
    use unflash_h264::ps::{parse_pps, parse_sps};
    use unflash_h264::slice::parse_slice_header;
    let base = read("splice_b");
    let base_avcc = parse_bytes(&base).unwrap().video().unwrap().description.clone().unwrap();
    for name in ["cb_cavlc", "cb_crop_slices", "main_cabac_b", "main_cavlc_wp", "pcm_cavlc", "high_interlaced"] {
        let data = read(name);
        let movie = parse_bytes(&data).unwrap();
        let track = movie.video().unwrap();
        let orig = parse_avcc(track.description.as_ref().unwrap()).unwrap();
        let mut reg = AvcRegistry::new(&base_avcc).unwrap();
        let mut rw = reg.register(track.description.as_ref().unwrap()).unwrap();
        let merged = parse_avcc(&reg.record()).unwrap();
        let mut spss_old = vec![None; 32];
        let mut ppss_old = vec![None; 256];
        for s in &orig.sps { let x = parse_sps(&unescape(&s[1..])).unwrap(); let id = x.id as usize; spss_old[id] = Some(x); }
        for p in &orig.pps { let x = parse_pps(&unescape(&p[1..])).unwrap(); let id = x.id as usize; ppss_old[id] = Some(x); }
        let mut spss_new = vec![None; 32];
        let mut ppss_new = vec![None; 256];
        for s in &merged.sps { let x = parse_sps(&unescape(&s[1..])).unwrap(); let id = x.id as usize; spss_new[id] = Some(x); }
        for p in &merged.pps { let x = parse_pps(&unescape(&p[1..])).unwrap(); let id = x.id as usize; ppss_new[id] = Some(x); }
        for (si, s) in track.samples.iter().enumerate() {
            let bytes = sample(&data, s);
            let out = rw.rewrite_sample(bytes).unwrap();
            // walk NALs of both
            let nals = |d: &[u8], n: usize| sample_nal_units(d, n).map(|nal| nal.unwrap().to_vec()).collect::<Vec<_>>();
            let a = nals(bytes, orig.len_size);
            let b = nals(&out, merged.len_size);
            assert_eq!(a.len(), b.len(), "{name} sample {si}: NAL count");
            for (na, nb) in a.iter().zip(&b) {
                let t = na[0] & 0x1f;
                if t == 1 || t == 5 {
                    let ra = unescape(&na[1..]);
                    let rb = unescape(&nb[1..]);
                    let mut r1 = BitReader::new(&ra);
                    let h1 = parse_slice_header(&mut r1, t, (na[0] >> 5) & 3, &spss_old, &ppss_old).unwrap();
                    let mut r2 = BitReader::new(&rb);
                    let h2 = parse_slice_header(&mut r2, t, (nb[0] >> 5) & 3, &spss_new, &ppss_new).unwrap();
                    let mut h1c = h1.clone(); h1c.pps_id = h2.pps_id;
                    assert_eq!(h1c, h2, "{name} sample {si}: header fields");
                    let cabac = ppss_old[h1.pps_id as usize].as_ref().unwrap().entropy_coding_mode;
                    if !cabac {
                        let stop = |d: &[u8]| { let mut l = d.len(); while l > 0 && d[l-1] == 0 { l -= 1; } (l-1)*8 + 7 - d[l-1].trailing_zeros() as usize };
                        let (s1, s2) = (stop(&ra), stop(&rb));
                        assert_eq!(s1 - r1.bit_pos(), s2 - r2.bit_pos(), "{name} sample {si}: data length");
                        let mut ra2 = BitReader::new(&ra); ra2.skip(r1.bit_pos() as u32);
                        let mut rb2 = BitReader::new(&rb); rb2.skip(r2.bit_pos() as u32);
                        for k in 0..(s1 - r1.bit_pos()) { assert_eq!(ra2.u(1).unwrap(), rb2.u(1).unwrap(), "{name} sample {si}: data bit {k}"); }
                    } else {
                        let a0 = (r1.bit_pos() + 7) / 8; let b0 = (r2.bit_pos() + 7) / 8;
                        let mut ea = ra.len(); while ea > a0 && ra[ea-1] == 0 { ea -= 1; }
                        let mut eb = rb.len(); while eb > b0 && rb[eb-1] == 0 { eb -= 1; }
                        assert_eq!(&ra[a0..ea], &rb[b0..eb], "{name} sample {si}: cabac data");
                    }
                }
            }
        }
    }
}

/// An encoder whose parameter sets are the base's (byte for byte) keeps
/// their ids, and its samples pass through unchanged; a second encoder
/// with the same sets as the first shares the first's ids.
#[test]
fn identical_parameter_sets_share_ids() {
    let a = read("splice_a");
    let b = read("splice_b");
    let avcc_a = parse_bytes(&a).unwrap().video().unwrap().description.clone().unwrap();
    let avcc_b = parse_bytes(&b).unwrap().video().unwrap().description.clone().unwrap();
    let mut reg = AvcRegistry::new(&avcc_a).unwrap();
    assert!(reg.register(&avcc_a).unwrap().is_identity());
    let mut rw1 = reg.register(&avcc_b).unwrap();
    assert!(!rw1.is_identity());
    let rec = parse_avcc(&reg.record()).unwrap();
    assert_eq!((rec.sps.len(), rec.pps.len()), (2, 2));
    let mut rw2 = reg.register(&avcc_b).unwrap();
    let rec2 = parse_avcc(&reg.record()).unwrap();
    assert_eq!((rec2.sps.len(), rec2.pps.len()), (2, 2), "the second encoder adds nothing");
    let track = parse_bytes(&b).unwrap().video().unwrap().clone();
    let bytes = sample(&b, &track.samples[0]);
    assert_eq!(rw1.rewrite_sample(bytes).unwrap(), rw2.rewrite_sample(bytes).unwrap());
}

/// The record Firefox's Windows encoder writes (its avcC writer puts a
/// NAL header in front of parameter sets that already have one, and leaves
/// the reserved bits of the length size and SPS count clear).
fn firefox_style(avcc: &[u8]) -> Vec<u8> {
    let rec = parse_avcc(avcc).unwrap();
    let mut out = vec![1, rec.profile, rec.compat, rec.level, 3, rec.sps.len() as u8];
    for s in &rec.sps {
        out.extend_from_slice(&((s.len() + 1) as u16).to_be_bytes());
        out.push(s[0]);
        out.extend_from_slice(s);
    }
    out.push(rec.pps.len() as u8);
    for p in &rec.pps {
        out.extend_from_slice(&((p.len() + 1) as u16).to_be_bytes());
        out.push(p[0]);
        out.extend_from_slice(p);
    }
    out
}

/// A sample with parameter sets sent in front of its pictures, as hardware
/// encoders do on every IDR picture.
fn with_inband(sample: &[u8], sets: &[Vec<u8>], len_size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for s in sets {
        let l = s.len();
        for i in (0..len_size).rev() {
            out.push((l >> (8 * i)) as u8);
        }
        out.extend_from_slice(s);
    }
    out.extend_from_slice(sample);
    out
}

/// A record written the way Firefox writes it (every parameter set with its
/// header byte twice) is repaired when read: this decoder decodes the stream
/// with it, and GOPs from such an encoder splice into another stream, their
/// IDR samples carrying their parameter sets in-band (once the record's
/// exact bytes, once an SPS that differs from the record's, which takes
/// effect under the id the record gave it).
#[test]
fn firefox_style_records_and_inband_parameter_sets() {
    let a = read("splice_a");
    let b = read("splice_b");
    let ta = parse_bytes(&a).unwrap().video().unwrap().clone();
    let tb = parse_bytes(&b).unwrap().video().unwrap().clone();
    let b_avcc = tb.description.clone().unwrap();
    let ff = firefox_style(&b_avcc);
    // the damage is real: read as written, the SPS is one byte off
    assert_eq!(&ff[8..10], &[0x67, 0x67]);
    let repaired = parse_avcc(&ff).unwrap();
    assert_eq!(repaired, parse_avcc(&b_avcc).unwrap(), "the repaired record is the original");
    // the decoder takes the damaged record and decodes the stream as ffmpeg does
    let mut dec = Decoder::new();
    dec.configure_avcc(&ff).unwrap();
    assert_eq!(md5s(&mut dec, &b, &tb, "splice_b", |b| b.to_vec()).unwrap(), expected("splice_b"), "decoded through the damaged record");

    // splice: GOPs 1 and 3 from the Firefox-style encoder, in-band sets on its IDRs
    let mut reg = AvcRegistry::new(ta.description.as_ref().unwrap()).unwrap();
    let mut rw = reg.register(&ff).unwrap();
    assert!(!rw.is_identity());
    let rec_b = parse_avcc(&b_avcc).unwrap();
    // an SPS that differs from the record's in a byte that changes nothing
    // the decoder needs (level_idc), so the record doesn't hold it
    let mut sps_other = rec_b.sps[0].clone();
    sps_other[3] = sps_other[3].wrapping_add(1);
    let mut payloads: Vec<(Vec<u8>, i64, bool)> = Vec::new();
    for g in 0..4 {
        let from_b = g % 2 == 1;
        let (t, data) = if from_b { (&tb, &b) } else { (&ta, &a) };
        for (k, s) in t.samples[g * 10..g * 10 + 10].iter().enumerate() {
            let bytes = if from_b {
                let sets = if k == 0 { vec![if g == 1 { rec_b.sps[0].clone() } else { sps_other.clone() }, rec_b.pps[0].clone()] } else { vec![] };
                rw.rewrite_sample(&with_inband(sample(data, s), &sets, rec_b.len_size)).unwrap()
            } else {
                sample(data, s).to_vec()
            };
            payloads.push((bytes, s.pts, s.sync));
        }
    }
    // the record is final once every encoder is registered
    let file = mux(reg.record(), &ta, &payloads);
    check_splice("splice_a", "splice_b", &decode(&file), "this decoder (Firefox-style record, in-band sets)");
    if let Some(md5s) = ffmpeg_md5(&file, "firefox-style") {
        check_splice("splice_a", "splice_b", &md5s, "ffmpeg (Firefox-style record, in-band sets)");
    }
}
