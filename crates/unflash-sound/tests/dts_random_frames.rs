//! Random core frames against ffmpeg. ffmpeg's encoder uses few of the
//! tools the syntax offers (one subframe of two subsubframes, 512
//! samples, no joint intensity, no transients, no high frequency VQ, the
//! non-perfect filter bank, LFE 64 times, plain scale factors); frames
//! built here use all of them at random: every arrangement AMODE 0 to 9
//! with and without LFE (interpolated 64 or 128 times), frames of 8 to
//! 128 blocks in 1 to 16 subframes of 1 to 4 subsubframes, subband
//! activity, high frequency VQ (from any subband, also above the active
//! ones), joint intensity from any lower channel with every code book,
//! transient modes with every code book (also past the last
//! subsubframe), scale factors with every code book and both tables, bit
//! allocation with every code book, every quantizer and code book
//! selection (Huffman, block codes, plain) with random scale factor
//! adjustments, ADPCM with random vectors (on subbands without bits too,
//! which then ring on their prediction alone, and on high frequency VQ
//! subbands), frames with and without the predictor history, both filter
//! banks, sum/difference of the front and surround pairs (and AMODE 3),
//! the lossless step sizes (RATE 31), DSYNC after every subsubframe, CRC
//! words, dynamic range coefficients, time stamps, auxiliary data with
//! embedded downmix coefficients, and core extension data to step over.
//! Each frame is built again with smaller scale factors while it would
//! decode past 0.9 of full scale, as real streams keep within it (where
//! samples go past full scale, both decoders hold them to it:
//! `samples_past_full_scale_are_held_to_24_bits`).
//!
//! Joint intensity scales stay at +12 dB and below: a joint subband
//! scales ffmpeg's rounding of its source subband too, and at the table's
//! +32 dB top that rounding (a few millionths of full scale) grows past
//! the tolerance.
//!
//! Where ffmpeg 6.1 does not do what the standard says, the streams keep
//! to what both agree on (see the `dts` module): the frame length
//! and the sum/difference flags are the same in every frame of a stream,
//! frames with the LFE channel are 16 blocks or more, and the LFE channel
//! interpolated 128 times is compared on its own
//! (`lfe_128_differs_from_ffmpegs_only_in_its_tap_order`).

mod common;

use common::dts::{compare, describe, level_range, special_sel, write_frame, Aux, Band, Channel, Frame, Subframe, TOLERANCE};
use common::{ffmpeg_available, ffmpeg_decode_bytes, Lcg};
use unflash_sound::dts::testing::AMODE_CHANNELS;
use unflash_sound::dts::{Decoder, Output};

/// What a stream keeps from frame to frame.
struct Layout {
    amode: u8,
    lff: u8,
    sfreq: u8,
    /// Sum/difference of the front and surround pairs: ffmpeg 6.1 applies
    /// it to the frame's output rather than to its subband samples, which
    /// differs where the flags change.
    sumf: bool,
    sums: bool,
    /// Blocks of 32 samples in every frame: ffmpeg 6.1 loses the ADPCM
    /// and LFE history at a frame longer than all before it (where it
    /// enlarges its buffers), and keeps too little LFE history in frames
    /// of 8 blocks; the standard (E.4) has the frame length constant.
    blocks: usize,
}

fn has_front_pair(amode: u8) -> bool {
    (2..=9).contains(&amode)
}

fn has_surround_pair(amode: u8) -> bool {
    amode == 8 || amode == 9
}

/// A random frame; `quiet` (0 up) lowers the scale factors.
fn random_frame(rng: &mut Lcg, l: &Layout, quiet: i32) -> Frame {
    let nch = AMODE_CHANNELS[l.amode as usize];
    let mut f = Frame { amode: l.amode, lff: l.lff, sfreq: l.sfreq, sumf: l.sumf, sums: l.sums, ..Default::default() };
    f.rate = if rng.chance(15) { 31 } else { rng.below(25) as u8 };
    f.filts = rng.chance(50);
    f.hflag = rng.chance(85);
    f.dynf = rng.chance(30);
    f.aspf = rng.chance(20);
    f.cpf = rng.chance(10);
    f.vernum = if rng.chance(20) { 6 } else { 7 };
    f.dialnorm = rng.below(16) as u8;
    f.hdcd = rng.chance(10);
    f.pcmr = [0, 1, 2, 3, 5, 6][rng.below(6) as usize];
    f.chist = rng.below(4) as u8;
    if rng.chance(15) {
        f.time_stamp = Some(rng.next() as u32);
    }
    if rng.chance(25) {
        let coded = nch + (l.lff > 0) as usize;
        let kind = rng.below(7) as u8;
        let rows = [1, 2, 2, 3, 3, 4, 4][kind as usize];
        let codes = (0..rows * coded).map(|_| if rng.chance(20) { 0 } else { rng.below(2) << 8 | (1 + rng.below(241)) }).collect();
        f.aux = Some(Aux { time_stamp: rng.chance(50).then(|| rng.next() & 0xf_ffff_ffff), downmix: rng.chance(80).then_some((kind, codes)), bad_crc: false });
    }
    if rng.chance(10) {
        // a core extension to step over: an XCh sync word and noise
        f.ext_audio_id = [0, 2, 6][rng.below(3) as usize];
        let mut ext = vec![0x5a, 0x5a, 0x5a, 0x5a];
        ext.extend((0..rng.range(4, 200)).map(|_| rng.below(256) as u8));
        f.extension = Some(ext);
    }
    // the coding header
    for ch in 0..nch {
        let subs = rng.range(2, 32) as usize;
        let vqsub = match rng.below(10) {
            0..=3 => rng.range(1, subs as i32) as usize,
            4 => rng.range(1, 32) as usize,
            _ => subs,
        };
        let mut c = Channel { subs, vqsub, thuff: rng.below(4) as usize, shuff: rng.below(7) as usize, bhuff: rng.below(7) as usize, ..Default::default() };
        if ch > 0 && rng.chance(35) {
            c.joinx = rng.below(ch as u32) as usize + 1;
        }
        for n in 0..10 {
            c.sel[n] = rng.below(special_sel(n + 1) as u32 + 1) as usize;
            c.adj[n] = if rng.chance(50) { 0 } else { rng.below(4) as usize };
        }
        f.channels.push(c);
    }
    // subframes of 1 to 4 subsubframes making up the stream's frame length
    let mut left = l.blocks / 8;
    while left > 0 {
        let nssc = if f.subframes.len() == 15 { left } else { rng.range(1, left.min(4) as i32) as usize };
        left -= nssc;
        let s = random_subframe(rng, &f, nssc, quiet);
        f.subframes.push(s);
    }
    f
}

fn random_subframe(rng: &mut Lcg, f: &Frame, nssc: usize, quiet: i32) -> Subframe {
    let nch = f.channels.len();
    let mut s = Subframe { nssc, psc: 0, range: rng.below(256) as u8, ..Default::default() };
    for c in &f.channels {
        // scale factor indexes up to about 36 dB below full scale in
        // either table, less `quiet` steps of the 6-bit one
        let (lo, hi) = if c.shuff == 6 { (0, 96 - 2 * quiet) } else { (0, 48 - quiet) };
        let mut bands = Vec::new();
        for n in 0..32 {
            let mut b = Band { levels: vec![0; 8 * nssc], ..Default::default() };
            if n < c.subs {
                // (past a point of quietening, no prediction, which may
                // gain)
                b.pmode = rng.chance(20) && quiet < 24;
                b.pvq = rng.below(4096) as u16;
            }
            if n < c.vqsub {
                b.abits = match c.bhuff {
                    5 => rng.below(16) as u8,
                    6 if rng.chance(60) => rng.below(11) as u8,
                    6 => rng.below(27) as u8,
                    _ => rng.range(1, 12) as u8,
                };
                if c.bhuff >= 5 && rng.chance(15) {
                    b.abits = 0;
                }
                if nssc > 1 && b.abits > 0 && rng.chance(30) {
                    b.tmode = rng.below(4) as u8;
                }
                let abits = b.abits as usize;
                let sel = if (1..=10).contains(&abits) { c.sel[abits - 1] } else { 0 };
                let (min, max) = level_range(abits, sel);
                // plain codes of many bits: mostly small levels
                let (min, max) = if abits > 10 { (min.max(-300), max.min(300)) } else { (min, max) };
                for v in b.levels.iter_mut() {
                    *v = if rng.chance(20) { 0 } else { rng.range(min, max) };
                }
            } else if n < c.subs {
                b.hfvq = rng.below(1024) as u16;
            }
            b.scales = [rng.range(lo, hi.max(lo)), rng.range(lo, hi.max(lo))];
            bands.push(b);
        }
        s.bands.push(bands);
    }
    for c in &f.channels {
        let js = rng.below(7) as usize;
        s.join_shuff.push(js);
        let mut codes = Vec::new();
        if c.joinx > 0 {
            let src = &f.channels[c.joinx - 1];
            // (joint scales of up to +12 dB, less when quietening: a joint
            // subband scales ffmpeg's rounding of its source's fixed point
            // samples too, up to 40 times at +32 dB, past the tolerance;
            // `joint_scales_are_ffmpegs` checks the whole table)
            let top = (24 - quiet).max(0);
            for _ in c.subs..src.subs.max(c.subs) {
                codes.push(match js {
                    5 => rng.range(0, top.min(63)),
                    6 => rng.range(0, top),
                    _ => rng.range(-64, top),
                });
            }
        }
        s.join_codes.push(codes);
    }
    s.down = (0..nch).map(|_| [rng.below(128), rng.below(128)]).collect();
    s.lfe = (0..2 * f.lff as usize * nssc).map(|_| rng.range(-128, 127)).collect();
    s.lfe_scale = rng.below((100 - 2 * quiet).max(1) as u32);
    s
}

/// A silent frame of a layout.
fn silent_frame(l: &Layout) -> Frame {
    let nch = AMODE_CHANNELS[l.amode as usize];
    let mut f = Frame { amode: l.amode, lff: l.lff, sfreq: l.sfreq, sumf: l.sumf, sums: l.sums, ..Default::default() };
    f.channels = vec![Channel { subs: 2, vqsub: 2, ..Default::default() }; nch];
    let mut left = l.blocks / 8;
    while left > 0 {
        let nssc = left.min(4);
        left -= nssc;
        let band = Band { levels: vec![0; 8 * nssc], ..Default::default() };
        let lfe = vec![0; 2 * l.lff as usize * nssc];
        f.subframes.push(Subframe { nssc, bands: vec![vec![band.clone(), band]; nch], lfe, ..Default::default() });
    }
    f
}

/// `count` random frames of a layout, each within 0.9 of full scale as
/// this decoder decodes it after the ones before (and the filter bank's
/// tail of it in the frame after).
fn random_stream(rng: &mut Lcg, l: &Layout, count: usize) -> Vec<u8> {
    let silence = write_frame(&silent_frame(l));
    let mut data = Vec::new();
    for _ in 0..count {
        let mut quiet = 0;
        loop {
            let f = random_frame(rng, l, quiet);
            let before = data.len();
            data.extend(write_frame(&f));
            let mut trial = data.clone();
            trial.extend_from_slice(&silence);
            let mut dec = Decoder::new(Output::Native);
            let mut out = Vec::new();
            let d = dec.decode(&trial, &mut out).expect("the frames are found");
            assert_eq!(d.damaged, 0, "a random frame does not decode: {f:?}");
            let last = out[0].len() - 2 * 32 * l.blocks;
            let peak = out.iter().flat_map(|c| &c[last..]).fold(0f32, |m, &v| m.max(v.abs()));
            if peak < 0.9 {
                break;
            }
            data.truncate(before);
            quiet += 4;
            assert!(quiet < 100, "no frame of {} blocks quiet enough", l.blocks);
        }
    }
    data
}

#[test]
fn random_frames_decode_as_ffmpeg_decodes_them() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED random_frames_decode_as_ffmpeg_decodes_them: ffmpeg and ffprobe are not installed");
        return;
    }
    let mut rng = Lcg(19);
    let rates = [13u8, 8, 3, 12, 7, 2, 11, 6, 1];
    let mut seen = unflash_sound::dts::Features::default();
    for amode in 0..10u8 {
        for lff in [0u8, 1, 2] {
            let blocks = [16, 32, 48, 64, 96, 128, 8][rng.below(if lff > 0 { 6 } else { 7 }) as usize];
            let sfreq = rates[(3 * amode as usize + lff as usize) % rates.len()];
            let l = Layout { amode, lff, sfreq, sumf: has_front_pair(amode) && rng.chance(40), sums: has_surround_pair(amode) && rng.chance(40), blocks };
            let data = random_stream(&mut rng, &l, 10);
            let mut dec = Decoder::new(Output::Native);
            let mut ours = Vec::new();
            let d = dec.decode(&data, &mut ours).unwrap();
            assert_eq!((d.damaged, d.samples), (0, 10 * 32 * blocks));
            let f = dec.features().0;
            let what = format!("AMODE {amode} LFF {lff}, {blocks} blocks");
            let reference =
                ffmpeg_decode_bytes(&data, &format!("random-{amode}-{lff}.dts"), &["-f", "dts", "-core_only", "1"], &[]).expect("ffmpeg decodes the frames");
            assert_eq!(reference.len(), ours.len(), "{what}: channels");
            assert_eq!(reference[0].len(), ours[0].len(), "{what}: samples");
            eprintln!("{what}:");
            let lfe = (lff > 0).then(|| common::dts::lfe_index(amode));
            let map: Vec<Option<usize>> = (0..ours.len()).map(Some).collect();
            for (c, s) in compare(&ours, &reference, &map).iter().enumerate() {
                if lff == 1 && Some(c) == lfe {
                    // (see lfe_128_differs_from_ffmpegs_only_in_its_tap_order)
                    eprintln!("  channel {c} (LFE 128 times, ffmpeg's taps in another order): {}", describe(s));
                    assert!(s.rms_diff < 0.05 * s.rms.max(1e-6), "{what}: the LFE channel differs from ffmpeg's by more than its tap order explains");
                    continue;
                }
                eprintln!("  channel {c}: {}", describe(s));
                assert!(s.max_diff <= TOLERANCE, "{what} channel {c}: differs from ffmpeg by {:e} at sample {}", s.max_diff, s.at);
            }
            macro_rules! or {
                ($($x:ident),*) => { $(seen.$x |= f.$x;)* };
            }
            or!(subframes, subsubframes, adpcm, history_reset, high_frequency_vq, joint_intensity, transients, huffman_samples, block_codes, plain_samples);
            or!(huffman_scales, huffman_bit_allocation, scales_7bit, adjustments, perfect_filter, nonperfect_filter, lfe_64, lfe_128, sum_difference);
            or!(dynamic_range, dsync_every_subsubframe, crc_words, lossless_steps, embedded_downmix, core_extension);
        }
    }
    eprintln!("{seen:?}");
    assert!(seen.subframes && seen.subsubframes && seen.adpcm && seen.history_reset && seen.high_frequency_vq && seen.joint_intensity && seen.transients);
    assert!(
        seen.huffman_samples
            && seen.block_codes
            && seen.plain_samples
            && seen.huffman_scales
            && seen.huffman_bit_allocation
            && seen.scales_7bit
            && seen.adjustments
    );
    assert!(seen.perfect_filter && seen.nonperfect_filter && seen.lfe_64 && seen.lfe_128 && seen.sum_difference && seen.dynamic_range);
    assert!(seen.dsync_every_subsubframe && seen.crc_words && seen.lossless_steps && seen.embedded_downmix && seen.core_extension);
}

/// A 3/2 frame of 16 blocks (one subframe of two subsubframes) with the
/// LFE channel, every other channel silent, and the given decimated LFE
/// samples.
fn lfe_frame(lff: u8, lfe: Vec<i32>, scale: u32) -> Frame {
    let mut f = Frame { amode: 9, lff, ..Default::default() };
    f.channels = vec![Channel { subs: 2, vqsub: 2, shuff: 5, bhuff: 6, ..Default::default() }; 5];
    let band = || Band { levels: vec![0; 16], ..Default::default() };
    f.subframes = vec![Subframe { nssc: 2, bands: vec![vec![band(), band()]; 5], lfe, lfe_scale: scale, ..Default::default() }];
    f
}

/// ffmpeg 6.1 interpolates the LFE channel 128 times (LFF 1) with D.8's
/// filter taps in another order than C.3.7's: of the 128 output samples
/// of each decimated sample, the second 64 take the taps of each group of
/// four in reverse order, so that its filter is a sawtooth where the
/// standard's is smooth. Impulses through both decoders show that, and
/// that the rest agrees: the first 64 outputs, and 64 times (LFF 2).
#[test]
fn lfe_128_differs_from_ffmpegs_only_in_its_tap_order() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED lfe_128_differs_from_ffmpegs_only_in_its_tap_order: ffmpeg and ffprobe are not installed");
        return;
    }
    // scale factor index 72 (11482), where ffmpeg's fixed point LFE scale
    // is exact
    for (lff, factor) in [(1u8, 128usize), (2, 64)] {
        let per = 4 * lff as usize;
        for pos in [0, per - 1] {
            let mut data = Vec::new();
            for k in 0..4 {
                let lfe = (0..per).map(|i| if k == 1 && i == pos { 100 } else { 0 }).collect();
                data.extend(write_frame(&lfe_frame(lff, lfe, 72)));
            }
            let ours = common::dts::decode(&data, Output::Native).out;
            let theirs = ffmpeg_decode_bytes(&data, "lfe-impulse.dts", &["-f", "dts"], &[]).expect("ffmpeg decodes the frames");
            // the response starts at the impulse's decimated sample
            let start = (per + pos) * factor;
            let (a, b) = (&ours[3][start..start + 512], &theirs[3][start..start + 512]);
            let peak = a.iter().fold(0f32, |m, &v| m.max(v.abs()));
            assert!(peak > 1e-3 && ours[3][..start].iter().all(|&v| v == 0.0));
            let (mut plain, mut reordered) = (0f32, 0f32);
            for m in 0..512 {
                let (tap, phase) = (m / factor * factor, m % factor);
                let permuted = if factor == 128 && phase >= 64 { tap + 64 + (phase - 64) / 4 * 4 + 3 - (phase % 4) } else { m };
                plain = plain.max((a[m] - b[m]).abs());
                reordered = reordered.max((a[permuted] - b[m]).abs());
            }
            eprintln!("LFE {factor} times, impulse at decimated sample {pos} of a frame: peak {peak:.2e}; ffmpeg differs by {plain:.1e}, by {reordered:.1e} in its tap order");
            assert!(reordered < 1e-6, "{factor} times: {reordered}");
            if factor == 128 {
                assert!(plain > 0.01 * peak, "{factor} times: the tap orders differ");
            } else {
                assert_eq!(plain, reordered);
            }
        }
    }
}

/// Subband samples and decimated LFE samples past full scale are held to
/// it (24 bits), as ffmpeg holds them: loud samples of each kind, in
/// streams of a loud frame between silent ones, decode as ffmpeg decodes
/// them. (The loudest streams of DTS's own encoder go a little past full
/// scale: FATE's `xll_51_24_48_768.dtshd` has a decimated LFE sample of
/// -1.004.)
#[test]
fn samples_past_full_scale_are_held_to_24_bits() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED samples_past_full_scale_are_held_to_24_bits: ffmpeg and ffprobe are not installed");
        return;
    }
    // a stereo frame of four subsubframes, silent but for channel 0's
    // band 3 (or 5), which `edit` sets up, and channel 1's joint scale
    // for it
    let frame = |edit: &dyn Fn(&mut Channel, &mut Vec<Band>), joint: Option<i32>| {
        let mut f = Frame { amode: 2, ..Default::default() };
        let mut c = Channel { subs: 32, vqsub: 32, shuff: 6, bhuff: 6, ..Default::default() };
        let mut bands = vec![Band { levels: vec![0; 32], ..Default::default() }; 32];
        edit(&mut c, &mut bands);
        let mut c1 = Channel { subs: 32, vqsub: 32, shuff: 6, bhuff: 6, ..Default::default() };
        let mut s = Subframe { nssc: 4, ..Default::default() };
        if let Some(index) = joint {
            c1 = Channel { subs: 2, vqsub: 2, joinx: 1, ..c1 };
            s.join_shuff = vec![0, 0];
            s.join_codes = vec![vec![], (2..c.subs).map(|n| if n == 3 { index - 64 } else { 0 }).collect()];
        }
        f.channels = vec![c, c1];
        s.bands = vec![bands, vec![Band { levels: vec![0; 32], ..Default::default() }; 32]];
        f.subframes = vec![s];
        write_frame(&f)
    };
    let silent = frame(&|_, _| {}, None);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        // 255 steps at 7-bit scale factor 116: 1.8 times full scale
        (
            "quantized",
            frame(
                &|_, b| b[3] = Band { abits: 12, scales: [116, 0], levels: (0..32).map(|i| [0, 255, -255, 120][i % 4]).collect(), ..Default::default() },
                None,
            ),
        ),
        // a constant residual of 0.39 of full scale through predictor 0,
        // whose gain is 2.6
        ("predicted", frame(&|_, b| b[3] = Band { abits: 12, scales: [106, 0], levels: vec![200; 32], pmode: true, pvq: 0, ..Default::default() }, None)),
        // 0.63 of full scale, 12 dB up in the joint channel
        (
            "joint",
            frame(
                &|_, b| b[3] = Band { abits: 12, scales: [108, 0], levels: (0..32).map(|i| [200, -200, 0, 90][i % 4]).collect(), ..Default::default() },
                Some(88),
            ),
        ),
        // vector 157 has an 89 (over 16) at 6-bit scale factor 60
        (
            "high frequency vector",
            frame(
                &|c, b| {
                    (c.subs, c.vqsub, c.shuff) = (6, 5, 5);
                    b[5] = Band { hfvq: 157, scales: [60, 0], ..Default::default() };
                },
                None,
            ),
        ),
    ];
    let lfe_silent = write_frame(&lfe_frame(2, vec![0; 8], 122));
    let mut streams: Vec<(&str, Vec<u8>)> =
        cases.into_iter().map(|(what, loud)| (what, [silent.clone(), loud, silent.clone(), silent.clone()].concat())).collect();
    // decimated LFE samples of a slow sine up to 57 steps at 7-bit scale
    // factor 122, 1.54 times full scale
    let mut lfe = lfe_silent.clone();
    for k in 1..8 {
        let codes = (0..8).map(|i| (((8 * k + i) as f64 * 0.2).sin() * 57.0).round() as i32).collect();
        lfe.extend(write_frame(&lfe_frame(2, codes, 122)));
    }
    streams.push(("LFE", lfe));
    for (what, data) in streams {
        let ours = common::dts::decode(&data, Output::Native);
        assert_eq!(ours.decoded.damaged, 0, "{what}");
        let theirs = ffmpeg_decode_bytes(&data, "loud.dts", &["-f", "dts"], &[]).expect("ffmpeg decodes the frames");
        let map: Vec<Option<usize>> = (0..ours.out.len()).map(Some).collect();
        let stats = compare(&ours.out, &theirs, &map);
        let (c, s) = stats.iter().enumerate().max_by(|a, b| a.1.peak.total_cmp(&b.1.peak)).unwrap();
        eprintln!("{what}: loudest channel {c}: {}", describe(s));
        assert!(s.peak >= 0.99, "{what}: the samples do not reach full scale");
        for (c, s) in stats.iter().enumerate() {
            assert!(s.max_diff <= TOLERANCE, "{what} channel {c}: {}", describe(s));
        }
    }
}

/// The stereo output with the frames' embedded Lo/Ro (or Lt/Rt)
/// coefficients is what ffmpeg's `-downmix stereo` gives, for every
/// arrangement of more than two coded channels; for two, both output the
/// channels as they are. (Coefficients of L into Ro or of R into Lo,
/// which a Lo/Ro downmix does not have, ffmpeg 6.1 multiplies by L's and
/// R's own, as it mixes in place: they are zero here.)
#[test]
fn embedded_downmix_is_ffmpegs() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED embedded_downmix_is_ffmpegs: ffmpeg and ffprobe are not installed");
        return;
    }
    let mut rng = Lcg(23);
    for (amode, lff) in [(9u8, 2u8), (9, 0), (8, 2), (7, 0), (6, 0), (5, 2), (2, 2), (2, 0), (1, 0), (3, 0)] {
        let nch = AMODE_CHANNELS[amode as usize];
        let coded = nch + (lff > 0) as usize;
        let (l, r) = match amode {
            5 | 7 | 9 => (1, 2),
            _ => (0, 1),
        };
        let layout = Layout { amode, lff, sfreq: 13, sumf: false, sums: false, blocks: 32 };
        let mut data = Vec::new();
        for _ in 0..6 {
            let mut f = random_frame(&mut rng, &layout, 12);
            let codes = (0..2 * coded)
                .map(|i| {
                    let (row, col) = (i / coded, i % coded);
                    if (row == 0 && col == r) || (row == 1 && col == l) {
                        0
                    } else {
                        rng.below(2) << 8 | (1 + rng.below(241))
                    }
                })
                .collect();
            f.aux = Some(Aux { time_stamp: None, downmix: Some((1 + rng.below(2) as u8, codes)), bad_crc: false });
            f.extension = None;
            data.extend(write_frame(&f));
        }
        let stereo = common::dts::decode(&data, Output::Stereo);
        assert_eq!(stereo.decoded.damaged, 0);
        assert!(stereo.features.embedded_downmix);
        let reference =
            ffmpeg_decode_bytes(&data, "downmix.dts", &["-f", "dts", "-core_only", "1", "-downmix", "stereo"], &[]).expect("ffmpeg decodes the frames");
        for (side, s) in compare(&stereo.out, &reference, &[Some(0), Some(1)]).iter().enumerate() {
            eprintln!("AMODE {amode} LFF {lff} side {side}: {}", describe(s));
            assert!(s.max_diff <= TOLERANCE, "AMODE {amode} LFF {lff} side {side}");
        }
    }
}

/// A stereo frame of one subframe of four subsubframes, silent but for
/// channel 0's subbands, which `edit` sets up.
fn probe_frame(edit: impl FnOnce(&mut Channel, &mut Vec<Band>)) -> Frame {
    let mut f = Frame { amode: 2, filts: true, ..Default::default() };
    let mut c = Channel { subs: 4, vqsub: 4, shuff: 6, bhuff: 6, ..Default::default() };
    let mut bands = vec![Band { levels: vec![0; 32], ..Default::default() }; 32];
    edit(&mut c, &mut bands);
    f.channels = vec![c, Channel { subs: 4, vqsub: 4, shuff: 6, bhuff: 6, ..Default::default() }];
    f.subframes = vec![Subframe { nssc: 4, bands: vec![bands, vec![Band { levels: vec![0; 32], ..Default::default() }; 32]], ..Default::default() }];
    f
}

/// Decode frames with ffmpeg and with this decoder, and return the
/// frames (of 1024 samples) that differ.
fn differing_frames(frames: &[Frame], name: &str) -> Vec<usize> {
    let data: Vec<u8> = frames.iter().flat_map(write_frame).collect();
    let ours = common::dts::decode(&data, Output::Native);
    assert_eq!(ours.decoded.damaged, 0, "{name}");
    let theirs = ffmpeg_decode_bytes(&data, &format!("{name}.dts"), &["-f", "dts"], &[]).expect("ffmpeg decodes the frames");
    assert_eq!(theirs[0].len(), ours.out[0].len(), "{name}");
    (0..frames.len()).filter(|&k| (0..2).any(|c| (1024 * k..1024 * (k + 1)).any(|i| (ours.out[c][i] - theirs[c][i]).abs() > 1e-5))).collect()
}

/// Every code of every Huffman book (Annex D.5) decodes as ffmpeg decodes
/// it: each quantization level alone in a subband, each scale factor
/// difference between two subbands, each bit allocation index and each
/// transient mode, one frame each.
#[test]
fn every_huffman_code_decodes_as_ffmpegs() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED every_huffman_code_decodes_as_ffmpegs: ffmpeg and ffprobe are not installed");
        return;
    }
    let mut bad = Vec::new();
    // quantization indexes, ABITS 1 to 10, the Huffman selections
    for abits in 1..=10usize {
        for sel in 0..special_sel(abits) {
            let book = unflash_sound::dts::testing::AUDIO[abits - 1][sel];
            let levels: Vec<i32> = book.iter().map(|e| e.0 as i32).collect();
            let frames: Vec<Frame> = levels
                .iter()
                .map(|&level| {
                    probe_frame(|c, bands| {
                        c.sel[abits - 1] = sel;
                        bands[1].abits = abits as u8;
                        bands[1].scales = [60, 0];
                        bands[1].levels[0] = level;
                    })
                })
                .collect();
            for k in differing_frames(&frames, &format!("book-{abits}-{sel}")) {
                bad.push(format!("ABITS {abits} SEL {sel} ({} levels): level {}", levels.len(), levels[k]));
            }
        }
    }
    // scale factor differences, SHUFF 0 to 4 (the 6-bit table, 0 to 62)
    for shuff in 0..5 {
        let diffs: Vec<i32> = (-62..=62).collect();
        let frames: Vec<Frame> = diffs
            .iter()
            .map(|&d| {
                probe_frame(|c, bands| {
                    c.shuff = shuff;
                    let (s1, s2) = if d >= 0 { (0, d) } else { (-d, 0) };
                    // (level 1: within full scale at the largest scale factors)
                    for (n, s) in [(1, s1), (2, s2)] {
                        bands[n].abits = 12;
                        bands[n].scales = [s, 0];
                        bands[n].levels[0] = 1;
                    }
                })
            })
            .collect();
        for k in differing_frames(&frames, &format!("scales-{shuff}")) {
            bad.push(format!("SHUFF {shuff}: difference {}", diffs[k]));
        }
    }
    // bit allocation indexes, BHUFF 0 to 4
    for bhuff in 0..5 {
        let frames: Vec<Frame> = (1..=12)
            .map(|abits: usize| {
                probe_frame(|c, bands| {
                    c.bhuff = bhuff;
                    c.sel = [0, 3, 3, 3, 3, 7, 7, 7, 7, 7];
                    // (these books have no ABITS 0: the other subbands
                    // get the smallest quantizer, and silence)
                    for n in [0, 2, 3] {
                        bands[n].abits = 1;
                    }
                    bands[1].abits = abits as u8;
                    bands[1].scales = [60, 0];
                    let (_, max) = level_range(abits, c.sel.get(abits - 1).copied().unwrap_or(0));
                    bands[1].levels[0] = max;
                })
            })
            .collect();
        for k in differing_frames(&frames, &format!("abits-{bhuff}")) {
            bad.push(format!("BHUFF {bhuff}: ABITS {}", k + 1));
        }
    }
    // transient modes, THUFF 0 to 3: an impulse in each subsubframe, the
    // second scale factor another
    for thuff in 0..4 {
        let frames: Vec<Frame> = (0..4)
            .map(|tmode| {
                probe_frame(|c, bands| {
                    c.thuff = thuff;
                    bands[1].abits = 12;
                    bands[1].tmode = tmode;
                    bands[1].scales = [50, 80];
                    for s in 0..4 {
                        bands[1].levels[8 * s] = 100;
                    }
                })
            })
            .collect();
        for k in differing_frames(&frames, &format!("tmode-{thuff}")) {
            bad.push(format!("THUFF {thuff}: TMODE {k}"));
        }
    }
    assert!(bad.is_empty(), "codes that decode otherwise than ffmpeg's: {bad:#?}");
}

/// Every joint intensity scale (D.3) is ffmpeg's: for each index, a frame
/// in which channel 1 takes channel 0's only coded subband with it (then
/// a silent frame for the filter banks' tails), and the gain from channel
/// 0 to channel 1 measured in both decodes. (ffmpeg's are the printed
/// values, which stray from 10^(i/40 - 1.6) by up to 0.13 %. The source
/// is as loud as the gain allows short of clipping: the louder, the less
/// ffmpeg's fixed point rounding weighs in the measure.)
#[test]
fn joint_scales_are_ffmpegs() {
    if !ffmpeg_available() {
        eprintln!("SKIPPED joint_scales_are_ffmpegs: ffmpeg and ffprobe are not installed");
        return;
    }
    let mut rng = Lcg(29);
    let frame = |rng: &mut Lcg, index: Option<i32>| {
        let mut f = Frame { amode: 2, ..Default::default() };
        let source = Channel { subs: 32, vqsub: 32, shuff: 6, bhuff: 6, ..Default::default() };
        f.channels = vec![source, Channel { subs: 2, vqsub: 2, joinx: 1, shuff: 6, bhuff: 6, ..Default::default() }];
        let mut bands = vec![Band { levels: vec![0; 16], ..Default::default() }; 32];
        if let Some(index) = index {
            // (7-bit scale factor 100 peaks at about 0.12 here; 1.1 dB a
            // step less for each 0.5 dB of gain past 0.5 of full scale)
            let scale = 100 - ((index - 89).max(0) * 5 + 10) / 11;
            bands[3] = Band { abits: 12, scales: [scale, 0], levels: (0..16).map(|_| rng.range(-100, 100)).collect(), ..Default::default() };
        }
        let index = index.unwrap_or(64);
        let book = index as usize % 5;
        let codes = (2..32).map(|n| if n == 3 { index - 64 } else { 0 }).collect();
        let silent = vec![Band { levels: vec![0; 16], ..Default::default() }; 32];
        f.subframes = vec![Subframe { nssc: 2, bands: vec![bands, silent], join_shuff: vec![0, book], join_codes: vec![vec![], codes], ..Default::default() }];
        write_frame(&f)
    };
    let silence = frame(&mut rng, None);
    let mut data = silence.clone();
    for index in 0..129 {
        data.extend(frame(&mut rng, Some(index)));
        data.extend_from_slice(&silence);
    }
    let ours = common::dts::decode(&data, Output::Native);
    assert_eq!(ours.decoded.damaged, 0);
    assert!(ours.features.joint_intensity);
    let theirs = ffmpeg_decode_bytes(&data, "joint.dts", &["-f", "dts", "-core_only", "1"], &[]).expect("ffmpeg decodes the frames");
    let peak = theirs[1].iter().fold(0f32, |m, &v| m.max(v.abs()));
    assert!(peak < 0.9, "channel 1 peaks at {peak}: too near full scale, where the subbands are held");
    let gain = |out: &[Vec<f32>], index: usize| {
        let at = 512 + 1024 * index;
        let (mut xy, mut xx) = (0f64, 0f64);
        for (&x, &y) in out[0][at..at + 1024].iter().zip(&out[1][at..at + 1024]) {
            xy += y as f64 * x as f64;
            xx += (x as f64).powi(2);
        }
        xy / xx
    };
    let mut worst = (0f64, 0);
    for index in 0..129 {
        let (a, b) = (gain(&ours.out, index), gain(&theirs, index));
        let off = (a / b - 1.0).abs();
        if off > worst.0 {
            worst = (off, index);
        }
        assert!(off < 3e-4, "joint scale index {index}: ours {a}, ffmpeg's {b}");
    }
    eprintln!("joint scales: at most {:.1e} from ffmpeg's (index {}); channel 1 peaks at {peak:.2}", worst.0, worst.1);
}
