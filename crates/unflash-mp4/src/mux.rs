//! A writer for the files the export produces: `ftyp`, one `mdat` holding
//! every sample in the order they are added, then a `moov` with 64-bit
//! chunk offsets. Video tracks come from WebCodecs encoder output; audio
//! tracks are usually stream-copied with their original sample entry.
//!
//! Usage: `start()` returns the file head (append it); for each sample
//! append the payload yourself and call `add_sample` with its size; finally
//! `finish()` returns the `moov` to append and the 8 bytes to patch into the
//! `mdat` header.

use crate::demux::{span, Sample, TrackKind};
use crate::reader::Writer;
use crate::Error;

/// How to build a track's sample entry.
#[derive(Clone, Debug)]
pub enum TrackDesc {
    /// A video track from encoder output.
    Video {
        /// WebCodecs codec string (`avc1.*`, `hvc1.*`, `av01.*`, `vp09.*`).
        codec: String,
        width: u32,
        height: u32,
        /// Ticks per second for the track's timestamps.
        timescale: u32,
        /// avcC / hvcC / av1C payload from the encoder's `decoderConfig`
        /// (may be empty for VP9 / AV1: a record is synthesised from the
        /// codec string).
        description: Vec<u8>,
    },
    /// Any track whose sample entry box is copied verbatim (stream copy).
    Copy { kind: TrackKind, sample_entry: Vec<u8>, timescale: u32, width: u32, height: u32 },
}

struct MuxTrack {
    desc: TrackDesc,
    samples: Vec<Sample>,
}

/// The 8 bytes to write over the file head at an offset (the `mdat`'s size).
pub type Patch = (u64, [u8; 8]);

pub struct Muxer {
    tracks: Vec<MuxTrack>,
    /// File offset of the `mdat` box header.
    mdat_at: u64,
    /// Bytes written so far (head + payloads).
    pos: u64,
    started: bool,
}

impl Default for Muxer {
    fn default() -> Self {
        Self::new()
    }
}

impl Muxer {
    pub fn new() -> Self {
        Muxer { tracks: Vec::new(), mdat_at: 0, pos: 0, started: false }
    }

    pub fn add_track(&mut self, desc: TrackDesc) -> usize {
        self.tracks.push(MuxTrack { desc, samples: Vec::new() });
        self.tracks.len() - 1
    }

    /// The file head: `ftyp` and the `mdat` header (with a placeholder size).
    pub fn start(&mut self) -> Vec<u8> {
        let mut w = Writer::new();
        let at = w.begin_box(b"ftyp");
        w.bytes(b"isom");
        w.u32(512);
        w.bytes(b"isom");
        w.bytes(b"iso2");
        w.bytes(b"avc1");
        w.bytes(b"mp41");
        w.end_box(at);
        self.mdat_at = w.buf.len() as u64;
        w.u32(1);
        w.bytes(b"mdat");
        w.u64(0); // largesize, patched by finish()
        self.pos = w.buf.len() as u64;
        self.started = true;
        w.buf
    }

    /// Record a sample whose `size` bytes the caller has just appended to
    /// the file. Times are in the track's timescale.
    pub fn add_sample(&mut self, track: usize, dts: i64, pts: i64, duration: u32, sync: bool, size: u32) -> Result<(), Error> {
        if !self.started {
            return Err("call start() first".into());
        }
        let t = self.tracks.get_mut(track).ok_or("no such track")?;
        t.samples.push(Sample { offset: self.pos, size, dts, pts, duration, sync });
        self.pos += size as u64;
        Ok(())
    }

    /// The `moov` box to append, and the `(offset, bytes)` to overwrite in
    /// the already-written head so the `mdat` size is right.
    pub fn finish(&self) -> Result<(Vec<u8>, Patch), Error> {
        if !self.started {
            return Err("call start() first".into());
        }
        let mdat_size = self.pos - self.mdat_at;
        let patch = (self.mdat_at + 8, mdat_size.to_be_bytes());
        let mut max_dur_ms = 0u64;
        for t in &self.tracks {
            max_dur_ms = max_dur_ms.max(track_duration_ms(t));
        }
        let mut w = Writer::new();
        let moov = w.begin_box(b"moov");
        // mvhd
        let at = w.begin_full_box(b"mvhd", 1, 0);
        w.u64(0);
        w.u64(0);
        w.u32(1000); // the movie's timescale: its durations are milliseconds
        w.u64(max_dur_ms);
        w.u32(0x00010000); // rate
        w.u16(0x0100); // volume
        w.zeros(2 + 8);
        for v in [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
            w.u32(v);
        }
        w.zeros(24);
        w.u32(self.tracks.len() as u32 + 1);
        w.end_box(at);
        for (i, t) in self.tracks.iter().enumerate() {
            write_trak(&mut w, t, i as u32 + 1)?;
        }
        w.end_box(moov);
        Ok((w.buf, patch))
    }
}

fn track_timescale(t: &MuxTrack) -> u32 {
    match &t.desc {
        TrackDesc::Video { timescale, .. } | TrackDesc::Copy { timescale, .. } => (*timescale).max(1),
    }
}

fn track_duration_ticks(t: &MuxTrack) -> u64 {
    span(&t.samples).max(0) as u64
}

fn track_duration_ms(t: &MuxTrack) -> u64 {
    let ts = track_timescale(t) as u128;
    (track_duration_ticks(t) as u128 * 1000 / ts) as u64
}

fn write_trak(w: &mut Writer, t: &MuxTrack, id: u32) -> Result<(), Error> {
    let ts = track_timescale(t);
    let (kind, width, height) = match &t.desc {
        TrackDesc::Video { width, height, .. } => (TrackKind::Video, *width, *height),
        TrackDesc::Copy { kind, width, height, .. } => (*kind, *width, *height),
    };
    let dur_ticks = track_duration_ticks(t);
    let dur_ms = track_duration_ms(t);
    let first_pts = t.samples.iter().map(|s| s.pts).min().unwrap_or(0);
    let first_dts = t.samples.first().map(|s| s.dts).unwrap_or(0);

    let trak = w.begin_box(b"trak");
    // tkhd
    let at = w.begin_full_box(b"tkhd", 1, 3);
    w.u64(0);
    w.u64(0);
    w.u32(id);
    w.u32(0);
    w.u64(dur_ms);
    w.zeros(8);
    w.u16(0); // layer
    w.u16(0); // alternate group
    w.u16(if kind == TrackKind::Audio { 0x0100 } else { 0 });
    w.u16(0);
    for v in [0x00010000u32, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000] {
        w.u32(v);
    }
    w.u32(width << 16);
    w.u32(height << 16);
    w.end_box(at);
    // edts: the file's decode times start at 0 (stts holds durations only),
    // so its composition times are the caller's less the first decode time;
    // the presentation starts at the earliest of them, skipping the
    // reordering delay
    let media_start = first_pts - first_dts;
    if media_start > 0 {
        let edts = w.begin_box(b"edts");
        let at = w.begin_full_box(b"elst", 1, 0);
        w.u32(1);
        w.u64(dur_ms);
        w.u64(media_start as u64);
        w.i16(1);
        w.i16(0);
        w.end_box(at);
        w.end_box(edts);
    }
    // mdia
    let mdia = w.begin_box(b"mdia");
    let at = w.begin_full_box(b"mdhd", 1, 0);
    w.u64(0);
    w.u64(0);
    w.u32(ts);
    w.u64(dur_ticks);
    w.u16(0x55c4); // 'und'
    w.u16(0);
    w.end_box(at);
    let at = w.begin_full_box(b"hdlr", 0, 0);
    w.u32(0);
    match kind {
        TrackKind::Video => {
            w.bytes(b"vide");
            w.zeros(12);
            w.bytes(b"VideoHandler\0");
        }
        TrackKind::Audio => {
            w.bytes(b"soun");
            w.zeros(12);
            w.bytes(b"SoundHandler\0");
        }
        TrackKind::Other => {
            w.bytes(b"meta");
            w.zeros(12);
            w.bytes(b"Handler\0");
        }
    }
    w.end_box(at);
    let minf = w.begin_box(b"minf");
    match kind {
        TrackKind::Video => {
            let at = w.begin_full_box(b"vmhd", 0, 1);
            w.zeros(8);
            w.end_box(at);
        }
        TrackKind::Audio => {
            let at = w.begin_full_box(b"smhd", 0, 0);
            w.zeros(4);
            w.end_box(at);
        }
        TrackKind::Other => {
            let at = w.begin_full_box(b"nmhd", 0, 0);
            w.end_box(at);
        }
    }
    let dinf = w.begin_box(b"dinf");
    let at = w.begin_full_box(b"dref", 0, 0);
    w.u32(1);
    let url = w.begin_full_box(b"url ", 0, 1);
    w.end_box(url);
    w.end_box(at);
    w.end_box(dinf);
    // stbl
    let stbl = w.begin_box(b"stbl");
    let at = w.begin_full_box(b"stsd", 0, 0);
    w.u32(1);
    match &t.desc {
        TrackDesc::Video { codec, width, height, description, .. } => {
            write_video_entry(w, codec, *width, *height, description)?;
        }
        TrackDesc::Copy { sample_entry, .. } => w.bytes(sample_entry),
    }
    w.end_box(at);
    // stts
    let at = w.begin_full_box(b"stts", 0, 0);
    let runs = run_length(t.samples.iter().map(|s| s.duration as i64));
    w.u32(runs.len() as u32);
    for (count, d) in &runs {
        w.u32(*count);
        w.u32(*d as u32);
    }
    w.end_box(at);
    // ctts
    if t.samples.iter().any(|s| s.pts != s.dts) {
        let at = w.begin_full_box(b"ctts", 1, 0);
        let runs = run_length(t.samples.iter().map(|s| s.pts - s.dts));
        w.u32(runs.len() as u32);
        for (count, o) in &runs {
            w.u32(*count);
            w.i32(*o as i32);
        }
        w.end_box(at);
    }
    // stss
    if t.samples.iter().any(|s| !s.sync) {
        let at = w.begin_full_box(b"stss", 0, 0);
        let syncs: Vec<u32> = t.samples.iter().enumerate().filter(|(_, s)| s.sync).map(|(i, _)| i as u32 + 1).collect();
        w.u32(syncs.len() as u32);
        for s in syncs {
            w.u32(s);
        }
        w.end_box(at);
    }
    // stsc: one sample per chunk
    let at = w.begin_full_box(b"stsc", 0, 0);
    w.u32(1);
    w.u32(1);
    w.u32(1);
    w.u32(1);
    w.end_box(at);
    // stsz
    let at = w.begin_full_box(b"stsz", 0, 0);
    w.u32(0);
    w.u32(t.samples.len() as u32);
    for s in &t.samples {
        w.u32(s.size);
    }
    w.end_box(at);
    // co64
    let at = w.begin_full_box(b"co64", 0, 0);
    w.u32(t.samples.len() as u32);
    for s in &t.samples {
        w.u64(s.offset);
    }
    w.end_box(at);
    w.end_box(stbl);
    w.end_box(minf);
    w.end_box(mdia);
    w.end_box(trak);
    Ok(())
}

/// Decode times for samples in decode order, from their composition times:
/// the k-th decode time is the k-th smallest composition time, all moved
/// earlier by the largest reordering delay, so no sample decodes after it
/// is shown and the decode times rise by the presentation intervals.
/// Returns (dts, duration) per sample; the last sample gets `last_duration`.
/// Works for any splice of streams whose pictures are all shown, whatever
/// each stream's own reordering.
pub fn dts_from_cts(cts: &[i64], last_duration: u32) -> Vec<(i64, u32)> {
    let mut sorted = cts.to_vec();
    sorted.sort_unstable();
    let shift = cts.iter().enumerate().map(|(i, &c)| sorted[i] - c).max().unwrap_or(0).max(0);
    (0..cts.len())
        .map(|i| {
            let dur = if i + 1 < cts.len() { (sorted[i + 1] - sorted[i]).max(1) as u32 } else { last_duration };
            (sorted[i] - shift, dur)
        })
        .collect()
}

fn run_length(vals: impl Iterator<Item = i64>) -> Vec<(u32, i64)> {
    let mut out: Vec<(u32, i64)> = Vec::new();
    for v in vals {
        match out.last_mut() {
            Some((c, last)) if *last == v => *c += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

pub(crate) fn write_video_entry(w: &mut Writer, codec: &str, width: u32, height: u32, description: &[u8]) -> Result<(), Error> {
    let fourcc: &[u8; 4] = if codec.starts_with("avc1") || codec.starts_with("avc3") {
        b"avc1"
    } else if codec.starts_with("hvc1") || codec.starts_with("hev1") {
        b"hvc1"
    } else if codec.starts_with("av01") {
        b"av01"
    } else if codec.starts_with("vp09") {
        b"vp09"
    } else if codec.starts_with("vp8") {
        b"vp08"
    } else {
        return Err(format!("unsupported video codec for muxing: {codec}"));
    };
    let entry = w.begin_box(fourcc);
    w.zeros(6);
    w.u16(1); // data reference index
    w.zeros(2 + 2 + 12);
    w.u16(width as u16);
    w.u16(height as u16);
    w.u32(0x00480000);
    w.u32(0x00480000);
    w.u32(0);
    w.u16(1);
    w.zeros(32);
    w.u16(0x0018);
    w.i16(-1);
    match fourcc {
        b"avc1" => {
            if description.is_empty() {
                return Err("avc1 needs an avcC description from the encoder".into());
            }
            let at = w.begin_box(b"avcC");
            w.bytes(description);
            w.end_box(at);
        }
        b"hvc1" => {
            if description.is_empty() {
                return Err("hvc1 needs an hvcC description from the encoder".into());
            }
            let at = w.begin_box(b"hvcC");
            w.bytes(description);
            w.end_box(at);
        }
        b"av01" => {
            let at = w.begin_box(b"av1C");
            if description.is_empty() {
                w.bytes(&synth_av1c(codec));
            } else {
                w.bytes(description);
            }
            w.end_box(at);
        }
        b"vp09" => {
            let at = w.begin_full_box(b"vpcC", 1, 0);
            w.bytes(&synth_vpcc(codec));
            w.end_box(at);
        }
        _ => {}
    }
    w.end_box(entry);
    Ok(())
}

/// av1C from `av01.P.LLT.DD`.
fn synth_av1c(codec: &str) -> Vec<u8> {
    let parts: Vec<&str> = codec.split('.').collect();
    let profile: u8 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let (level, tier) = parts
        .get(2)
        .map(|s| {
            let lvl: u8 = s[..s.len().saturating_sub(1)].parse().unwrap_or(0);
            let tier = if s.ends_with('H') { 1u8 } else { 0 };
            (lvl, tier)
        })
        .unwrap_or((0, 0));
    let depth: u8 = parts.get(3).and_then(|p| p.parse().ok()).unwrap_or(8);
    let high = (depth > 8) as u8;
    let twelve = (depth == 12) as u8;
    // marker|version, profile|level, tier|high|twelve|mono|subx|suby|pos, reserved|initial_presentation_delay
    vec![0x81, (profile << 5) | (level & 0x1f), (tier << 7) | (high << 6) | (twelve << 5) | (1 << 3) | (1 << 2), 0]
}

/// vpcC payload (after the full-box header) from `vp09.PP.LL.DD`.
fn synth_vpcc(codec: &str) -> Vec<u8> {
    let parts: Vec<&str> = codec.split('.').collect();
    let profile: u8 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let level: u8 = parts.get(2).and_then(|p| p.parse().ok()).unwrap_or(10);
    let depth: u8 = parts.get(3).and_then(|p| p.parse().ok()).unwrap_or(8);
    // profile, level, bitDepth(4)|chromaSubsampling(3)|fullRange(1), primaries, transfer, matrix, codecInitializationDataSize(2)
    vec![profile, level, (depth << 4) | (1 << 1), 1, 1, 1, 0, 0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_times_from_composition_times() {
        // I0 P3 B1 B2 | I4 (no reordering) spliced after a stream with two B-frames
        let cts = [0i64, 3, 1, 2, 4, 5, 6];
        let out = dts_from_cts(&cts, 1);
        let dts: Vec<i64> = out.iter().map(|o| o.0).collect();
        assert_eq!(dts, vec![-1, 0, 1, 2, 3, 4, 5]);
        for (i, &c) in cts.iter().enumerate() {
            assert!(dts[i] <= c);
        }
        assert!(dts.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(out.iter().map(|o| o.1).collect::<Vec<_>>(), vec![1, 1, 1, 1, 1, 1, 1]);
        assert_eq!(dts_from_cts(&[0, 2, 4], 7), vec![(0, 2), (2, 2), (4, 7)]);
        assert!(dts_from_cts(&[], 1).is_empty());
    }
}
