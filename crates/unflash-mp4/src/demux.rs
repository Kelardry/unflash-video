//! The demuxer: a byte-range state machine that finds the `moov` (and any
//! `moof`) boxes, then expands the sample tables.

use serde::Serialize;

use crate::codec::{from_sample_entry, CodecInfo};
use crate::reader::{box_header, for_each_box, fourcc_str, Reader};
use crate::Error;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
    #[default]
    Other,
}

/// One sample (an encoded frame or audio packet).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Absolute byte offset in the file.
    pub offset: u64,
    pub size: u32,
    /// Decode time, track timescale ticks.
    pub dts: i64,
    /// Presentation time, track timescale ticks (edit list applied).
    pub pts: i64,
    /// Duration in ticks.
    pub duration: u32,
    pub sync: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Track {
    pub id: u32,
    pub kind: TrackKind,
    /// The sample entry's fourcc, e.g. `avc1`.
    pub fourcc: String,
    /// WebCodecs codec string.
    pub codec: String,
    /// WebCodecs decoder `description`, if the codec needs one.
    pub description: Option<Vec<u8>>,
    pub timescale: u32,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u32,
    /// The whole sample entry box (header included), for stream copying.
    pub sample_entry: Vec<u8>,
    /// Ticks subtracted from composition times by the edit list (positive
    /// media_time) or added (leading empty edit).
    pub edit_shift: i64,
    pub samples: Vec<Sample>,
    /// Nominal ticks per sample (a video frame, an audio packet), 0 when
    /// the file does not say.
    pub frame_duration: u32,
    /// Bytes every sample starts with that the file leaves out (Matroska
    /// header stripping): put them back in front of each sample read.
    pub prefix: Vec<u8>,
    pub name: String,
    pub language: String,
    /// Why the track cannot be used, when it cannot.
    pub note: String,
}

/// `v` ticks of a clock of `from` a second as ticks of one of `to`, to the
/// nearest (halves away from zero).
pub(crate) fn rescale(v: i64, from: i64, to: i64) -> i64 {
    let x = v as i128 * to as i128;
    let half = from as i128 / 2;
    ((x + if x >= 0 { half } else { -half }) / from as i128) as i64
}

/// Ticks from the first presentation time to the end of the last sample
/// (0 for none).
pub(crate) fn span(samples: &[Sample]) -> i64 {
    let mut lo = i64::MAX;
    let mut hi = i64::MIN;
    for s in samples {
        lo = lo.min(s.pts);
        hi = hi.max(s.pts + s.duration as i64);
    }
    if lo == i64::MAX {
        0
    } else {
        hi - lo
    }
}

impl Track {
    pub fn pts_secs(&self, i: usize) -> f64 {
        self.samples[i].pts as f64 / self.timescale as f64
    }
    pub fn to_secs(&self, ticks: i64) -> f64 {
        ticks as f64 / self.timescale as f64
    }
    pub fn to_us(&self, ticks: i64) -> i64 {
        // exact where the timescale divides 1e6, correctly rounded otherwise
        rescale(ticks, self.timescale as i64, 1_000_000)
    }
    /// Duration from the first pts to the end of the last sample, seconds.
    pub fn duration_secs(&self) -> f64 {
        span(&self.samples) as f64 / self.timescale as f64
    }
    /// Whether an MP4 can carry the track's samples as they are: it has a
    /// sample entry for them, and they are not uncompressed sound (whose
    /// samples here are packets of many frames, which its entry doesn't say).
    pub fn copyable(&self) -> bool {
        !self.sample_entry.is_empty() && !is_pcm(&self.codec)
    }
    /// Presentation times of sync samples, seconds, ascending.
    pub fn keyframe_times(&self) -> Vec<f64> {
        let mut v: Vec<f64> = self.samples.iter().filter(|s| s.sync).map(|s| s.pts as f64 / self.timescale as f64).collect();
        v.sort_by(f64::total_cmp);
        v
    }
    /// Index (decode order) of the last sync sample whose pts is at or
    /// before `t` seconds, or 0.
    pub fn sync_before(&self, t: f64) -> usize {
        let ticks = (t * self.timescale as f64).floor() as i64;
        let mut best = 0;
        for (i, s) in self.samples.iter().enumerate() {
            if s.sync && s.pts <= ticks {
                best = i;
            }
        }
        best
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Movie {
    pub fragmented: bool,
    /// The container: `mp4`, `matroska`, `webm` or `mpegts`.
    pub format: String,
    pub tracks: Vec<Track>,
    /// A transport stream's packet size (188, 192 or 204): its samples'
    /// offsets are places a reader follows its packets from (see
    /// [`crate::ts`]). 0 for the other containers.
    pub packet_size: u32,
}

impl Movie {
    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Video && !t.samples.is_empty())
    }
    pub fn audio(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == TrackKind::Audio && !t.samples.is_empty())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Header,
    Body { size: u64, kind: [u8; 4] },
    Done,
}

/// Byte-range driven parser for any container Unflash reads (MP4 and
/// QuickTime, Matroska and WebM, MPEG transport streams), told apart by
/// their first bytes. Loop:
/// `need()` -> read that range -> `feed()` until `movie()` is `Some`.
pub struct Demuxer {
    file_size: u64,
    inner: Inner,
    bytes_read: u64,
}

enum Inner {
    /// Waiting for the first bytes.
    Sniff,
    Mp4(Mp4Demuxer),
    Mkv(crate::mkv::MkvDemuxer),
    Ts(crate::ts::TsDemuxer),
}

const SNIFF_LEN: u64 = 1024;

/// What a file's first bytes say it is, when it is not something Unflash
/// reads: a name for it and what to do about it.
fn foreign(b: &[u8]) -> Option<(&'static str, &'static str)> {
    let at = |o: usize, sig: &[u8]| b.len() >= o + sig.len() && &b[o..o + sig.len()] == sig;
    const REMUX: &str = "If its video is H.264 (most are), remux it without re-encoding, for example with `ffmpeg -i input -c copy output.mkv`, and open that.";
    const CONVERT: &str = "Convert it first, for example with HandBrake or `ffmpeg -i input -c:v libx264 -c:a aac output.mp4`.";
    if at(0, b"RIFF") && at(8, b"AVI ") {
        return Some(("an AVI file", CONVERT));
    }
    if at(0, b"RIFF") && at(8, b"WAVE") {
        return Some(("a WAV audio file", "It has no video to check."));
    }
    if at(0, b"FLV") {
        return Some(("a Flash video (FLV) file", REMUX));
    }
    if at(0, b"OggS") {
        return Some(("an Ogg file", CONVERT));
    }
    if at(0, &[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11]) {
        return Some(("a Windows Media (WMV/ASF) file", CONVERT));
    }
    if at(0, &[0x00, 0x00, 0x01, 0xBA]) {
        return Some(("an MPEG program stream (.mpg / .vob)", CONVERT));
    }
    None
}

impl Demuxer {
    pub fn new(file_size: u64) -> Self {
        Demuxer { file_size, inner: Inner::Sniff, bytes_read: 0 }
    }

    /// The (offset, length) the parser needs next, or `None` when done.
    pub fn need(&self) -> Option<(u64, u64)> {
        match &self.inner {
            // (an empty file too: none of its bytes, to be told it holds no movie)
            Inner::Sniff => Some((0, SNIFF_LEN.min(self.file_size))),
            Inner::Mp4(d) => d.need(),
            Inner::Mkv(d) => d.need(),
            Inner::Ts(d) => d.need(),
        }
    }

    pub fn is_done(&self) -> bool {
        match &self.inner {
            Inner::Sniff => false,
            Inner::Mp4(d) => d.is_done(),
            Inner::Mkv(d) => d.is_done(),
            Inner::Ts(d) => d.is_done(),
        }
    }

    /// Bytes requested so far.
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// How far through the work of reading the index the parser is, 0 to
    /// 1 (a Matroska file or a transport stream has to be read through; an
    /// MP4's index is one box or a few).
    pub fn progress(&self) -> f64 {
        match &self.inner {
            Inner::Sniff => 0.0,
            Inner::Mp4(d) => {
                if d.is_done() {
                    1.0
                } else {
                    0.5
                }
            }
            Inner::Mkv(d) => d.progress(),
            Inner::Ts(d) => d.progress(),
        }
    }

    /// `mp4`, `matroska`, `mpegts`, or `` before the first bytes are seen.
    pub fn container(&self) -> &'static str {
        match &self.inner {
            Inner::Sniff => "",
            Inner::Mp4(_) => "mp4",
            Inner::Mkv(_) => "matroska",
            Inner::Ts(_) => "mpegts",
        }
    }

    /// Serve the bytes of the last `need()` range: `data` must start at the
    /// requested offset and cover the requested length (extra is fine).
    pub fn feed(&mut self, offset: u64, data: &[u8]) -> Result<(), Error> {
        let Some((want_off, want_len)) = self.need() else {
            return Err("demuxer is not waiting for data".into());
        };
        if offset != want_off || (data.len() as u64) < want_len {
            return Err(format!("expected {want_len} bytes at {want_off}, got {} at {offset}", data.len()));
        }
        self.bytes_read += want_len;
        match &mut self.inner {
            Inner::Sniff => {
                self.inner = if data.len() >= 4 && data[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
                    Inner::Mkv(crate::mkv::MkvDemuxer::new(self.file_size))
                } else if matches!(data.get(4..8), Some(b"ftyp" | b"moov" | b"mdat" | b"free" | b"skip" | b"wide" | b"styp")) {
                    // (an MP4's first box, whose bytes after it could pass for
                    // a transport stream's packets)
                    Inner::Mp4(Mp4Demuxer::new(self.file_size))
                } else if let Some((packet, first)) = crate::ts::sniff(data) {
                    Inner::Ts(crate::ts::TsDemuxer::new(self.file_size, packet, first))
                } else if let Some((what, advice)) = foreign(data) {
                    return Err(format!("This is {what}. Unflash reads MP4, MOV, M4V, MKV, WebM and MPEG transport stream (.ts, .m2ts, .mts) files. {advice}"));
                } else {
                    Inner::Mp4(Mp4Demuxer::new(self.file_size))
                };
                if let Inner::Mp4(d) = &mut self.inner {
                    if d.is_done() {
                        // a file too short to hold a box: it has no moov
                        d.finish()?;
                    }
                }
                Ok(())
            }
            Inner::Mp4(d) => d.feed(data),
            Inner::Mkv(d) => d.feed(offset, data),
            Inner::Ts(d) => d.feed(offset, data),
        }
    }

    pub fn movie(&self) -> Option<&Movie> {
        match &self.inner {
            Inner::Sniff => None,
            Inner::Mp4(d) => d.movie(),
            Inner::Mkv(d) => d.movie(),
            Inner::Ts(d) => d.movie(),
        }
    }

    pub fn into_movie(self) -> Option<Movie> {
        match self.inner {
            Inner::Sniff => None,
            Inner::Mp4(d) => d.into_movie(),
            Inner::Mkv(d) => d.into_movie(),
            Inner::Ts(d) => d.into_movie(),
        }
    }
}

/// The MP4 / QuickTime parser behind [`Demuxer`].
#[derive(Clone, Debug)]
struct Mp4Demuxer {
    file_size: u64,
    at: u64,
    stage: Stage,
    want: Option<(u64, u64)>,
    moov: Option<Vec<u8>>,
    moofs: Vec<(u64, Vec<u8>)>,
    movie: Option<Movie>,
}

const HEADER_PEEK: u64 = 32;
/// The most a box read whole (`moov`, `moof`) may take: a ten-hour film's
/// `moov` is some 60 MB.
const MAX_INDEX: u64 = 256 << 20;

impl Mp4Demuxer {
    pub fn new(file_size: u64) -> Self {
        let mut d = Mp4Demuxer {
            file_size,
            at: 0,
            stage: Stage::Header,
            want: None,
            moov: None,
            moofs: Vec::new(),
            movie: None,
        };
        d.request_header();
        d
    }

    fn request_header(&mut self) {
        if self.at + 8 > self.file_size {
            self.stage = Stage::Done;
            self.want = None;
        } else {
            self.stage = Stage::Header;
            self.want = Some((self.at, HEADER_PEEK.min(self.file_size - self.at)));
        }
    }

    /// The (offset, length) the parser needs next, or `None` when done.
    pub fn need(&self) -> Option<(u64, u64)> {
        self.want
    }

    pub fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// Serve the bytes of the last `need()` range (from its offset, at least
    /// its length: [`Demuxer::feed`] sees to that).
    pub fn feed(&mut self, data: &[u8]) -> Result<(), Error> {
        match self.stage {
            Stage::Header => {
                let h = box_header(data)?;
                let size = h.size.unwrap_or(self.file_size - self.at);
                // (the file holds the header's 8 bytes from `at`: no overflow)
                if size < h.header_len || size > self.file_size - self.at {
                    return Err(format!("box {} at {} has an impossible size {size}", fourcc_str(&h.kind), self.at));
                }
                match &h.kind {
                    b"moov" | b"moof" => {
                        if size > MAX_INDEX {
                            return Err(format!("box {} at {} is {size} bytes, too big for an index", fourcc_str(&h.kind), self.at));
                        }
                        if (data.len() as u64) >= size {
                            self.take_body(h.kind, &data[..size as usize]);
                            self.advance(size);
                        } else {
                            self.stage = Stage::Body { size, kind: h.kind };
                            self.want = Some((self.at, size));
                        }
                    }
                    _ => self.advance(size),
                }
            }
            Stage::Body { size, kind } => {
                self.take_body(kind, &data[..size as usize]);
                self.advance(size);
            }
            Stage::Done => return Err("demuxer is done".into()),
        }
        if self.stage == Stage::Done && self.movie.is_none() {
            self.finish()?;
        }
        Ok(())
    }

    fn take_body(&mut self, kind: [u8; 4], whole: &[u8]) {
        match &kind {
            b"moov" => self.moov = Some(whole.to_vec()),
            b"moof" => self.moofs.push((self.at, whole.to_vec())),
            _ => {}
        }
    }

    fn advance(&mut self, size: u64) {
        self.at += size;
        self.request_header();
    }

    pub fn movie(&self) -> Option<&Movie> {
        self.movie.as_ref()
    }

    pub fn into_movie(self) -> Option<Movie> {
        self.movie
    }

    fn finish(&mut self) -> Result<(), Error> {
        let moov = self.moov.as_ref().ok_or("no moov box found (not an MP4 file?)")?;
        let (mut movie, trex) = parse_moov(moov, self.file_size)?;
        if !self.moofs.is_empty() {
            movie.fragmented = true;
            let mut next_dts: Vec<i64> = movie.tracks.iter().map(|t| t.samples.last().map(|s| s.dts + s.duration as i64).unwrap_or(0)).collect();
            for (off, moof) in &self.moofs {
                apply_moof(&mut movie, &trex, self.file_size, *off, moof, &mut next_dts)?;
            }
        }
        for t in &mut movie.tracks {
            finish_track(t);
        }
        self.movie = Some(movie);
        Ok(())
    }
}

/// Parse a whole file already in memory (tests, small files).
pub fn parse_bytes(data: &[u8]) -> Result<Movie, Error> {
    let mut d = Demuxer::new(data.len() as u64);
    while let Some((off, len)) = d.need() {
        let end = (off + len).min(data.len() as u64);
        d.feed(off, &data[off as usize..end as usize])?;
    }
    d.into_movie().ok_or_else(|| "no movie".to_string())
}

// ---- moov ------------------------------------------------------------------

#[derive(Default)]
struct Stbl {
    stts: Vec<(u32, u32)>,
    ctts: Vec<(u32, i64)>,
    stsc: Vec<(u32, u32, u32)>,
    sizes: Vec<u32>,
    fixed_size: u32,
    sample_count: u32,
    chunk_offsets: Vec<u64>,
    stss: Option<Vec<u32>>,
}

#[derive(Default, Clone)]
struct Trex {
    default_duration: u32,
    default_size: u32,
    default_flags: u32,
}

struct TrakParts {
    id: u32,
    /// The track header's enabled flag: the track a player plays (of a
    /// kind, the one enabled; ffmpeg enables the default one).
    enabled: bool,
    kind: TrackKind,
    width: u32,
    height: u32,
    timescale: u32,
    elst: Vec<(i64, i64)>, // (segment_duration in movie ts, media_time in track ts)
    fourcc: [u8; 4],
    entry: Vec<u8>,
    codec: CodecInfo,
    sample_rate: u32,
    channels: u32,
    stbl: Stbl,
    /// Uncompressed sound: its form, and the bytes of one sample frame (0:
    /// not PCM).
    pcm: Option<PcmForm>,
    pcm_frame: u32,
}

/// The form of uncompressed sound: bits per sample, float, little endian,
/// signed (8-bit sound can be either).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PcmForm {
    pub(crate) bits: u32,
    pub(crate) float: bool,
    pub(crate) le: bool,
    pub(crate) signed: bool,
}

/// What an audio sample entry says about its samples beyond the codec.
#[derive(Clone, Copy, Default)]
struct SoundDesc {
    /// The entry's version: QuickTime's 1 and 2 add fields.
    version: u16,
    /// Bits per sample (per channel).
    bits: u32,
    /// Bytes per sample frame (QuickTime version 1 and 2), 0 when not given.
    frame_bytes: u32,
    /// QuickTime version 2's format flags (`lpcm`): float, big endian, signed.
    flags: u32,
}

/// The movie the `moov` box describes (a file of `file_size` bytes), and
/// the defaults its `trex` boxes give fragments, by track.
fn parse_moov(moov: &[u8], file_size: u64) -> Result<(Movie, Vec<(u32, Trex)>), Error> {
    let body = &moov[box_header(moov)?.header_len as usize..];
    let mut movie = Movie { format: "mp4".into(), ..Default::default() };
    // (the movie's clock: its edit lists' durations are in it)
    let mut timescale = 1000;
    let mut traks: Vec<TrakParts> = Vec::new();
    let mut trex: Vec<(u32, Trex)> = Vec::new();
    for_each_box(body, |kind, b, _| {
        match &kind {
            b"mvhd" => {
                let mut r = Reader::new(b);
                let (v, _) = r.version_flags()?;
                r.skip(if v == 1 { 16 } else { 8 })?;
                timescale = r.u32()?;
            }
            b"trak" => traks.push(parse_trak(b)?),
            b"mvex" => {
                for_each_box(b, |k, tb, _| {
                    if &k == b"trex" {
                        let mut r = Reader::new(tb);
                        r.version_flags()?;
                        let id = r.u32()?;
                        r.u32()?; // default sample description index
                        let t = Trex { default_duration: r.u32()?, default_size: r.u32()?, default_flags: r.u32()? };
                        trex.push((id, t));
                    }
                    Ok(())
                })?;
            }
            _ => {}
        }
        Ok(())
    })?;
    if traks.is_empty() {
        return Err("moov has no tracks".into());
    }
    // the tracks a player plays first, as the Matroska reader puts the default ones first
    traks.sort_by_key(|t| !t.enabled);
    for mut tp in traks {
        // a sample size that the sample table's frames contradict (ffmpeg 6
        // writes 24-bit ipcm as 32): the table's, which is how the bytes lie
        let fixed = tp.stbl.fixed_size;
        if let Some(form) = tp.pcm {
            let ch = tp.channels.max(1);
            if fixed > 1 && fixed != tp.pcm_frame && fixed % ch == 0 {
                if let Some(c) = pcm_codec(PcmForm { bits: fixed / ch * 8, ..form }) {
                    tp.codec = CodecInfo { codec: c.into(), description: None };
                    tp.pcm_frame = fixed;
                }
            }
        }
        let mut track = Track {
            id: tp.id,
            kind: tp.kind,
            fourcc: fourcc_str(&tp.fourcc),
            codec: tp.codec.codec,
            description: tp.codec.description,
            timescale: tp.timescale.max(1),
            width: tp.width,
            height: tp.height,
            sample_rate: tp.sample_rate,
            channels: tp.channels,
            sample_entry: tp.entry,
            ..Default::default()
        };
        track.edit_shift = edit_shift(&tp.elst, timescale, track.timescale);
        if track.kind == TrackKind::Audio && track.sample_rate == 0 {
            // (a rate the entry can't hold, and no 'srat': the track's clock is its rate)
            track.sample_rate = track.timescale;
        }
        // (a sample takes a byte at least: a count the file can't hold would
        // only ask for memory there is not)
        if tp.stbl.sample_count as u64 * tp.stbl.fixed_size.max(1) as u64 > file_size {
            return Err(format!("track {} lists more samples than the file holds", tp.id));
        }
        // uncompressed sound whose tables count each sample frame (sized as
        // one frame, or as a byte in QuickTime's older files): in packets
        track.samples = if tp.pcm_frame > 0 && tp.stbl.fixed_size != 0 && tp.stbl.fixed_size <= tp.pcm_frame { expand_pcm(&tp.stbl, tp.pcm_frame)? } else { expand_samples(&tp.stbl)? };
        movie.tracks.push(track);
    }
    Ok((movie, trex))
}

fn edit_shift(elst: &[(i64, i64)], movie_ts: u32, track_ts: u32) -> i64 {
    // A leading empty edit (media_time -1) delays the track: add its
    // duration. A normal first edit with media_time > 0 skips into the
    // media: subtract it. Anything more elaborate is ignored.
    let mut shift = 0i64;
    let mut it = elst.iter().copied().peekable();
    if let Some((dur, mt)) = it.peek().copied() {
        if mt == -1 {
            shift += (dur as i128 * track_ts as i128 / movie_ts.max(1) as i128) as i64;
            it.next();
        }
    }
    if let Some((_, mt)) = it.next() {
        if mt > 0 {
            shift -= mt;
        }
    }
    shift
}

fn parse_trak(trak: &[u8]) -> Result<TrakParts, Error> {
    let mut tp = TrakParts {
        id: 0,
        enabled: true,
        kind: TrackKind::Other,
        width: 0,
        height: 0,
        timescale: 1000,
        elst: vec![],
        fourcc: *b"????",
        entry: vec![],
        codec: CodecInfo::default(),
        sample_rate: 0,
        channels: 0,
        stbl: Stbl::default(),
        pcm: None,
        pcm_frame: 0,
    };
    for_each_box(trak, |kind, b, _| {
        match &kind {
            b"tkhd" => {
                let mut r = Reader::new(b);
                let (v, flags) = r.version_flags()?;
                tp.enabled = flags & 1 != 0;
                if v == 1 {
                    r.skip(16)?;
                    tp.id = r.u32()?;
                    r.skip(4 + 8)?;
                } else {
                    r.skip(8)?;
                    tp.id = r.u32()?;
                    r.skip(4 + 4)?;
                }
                r.skip(8 + 2 + 2 + 2 + 2 + 36)?;
                tp.width = r.u32()? >> 16;
                tp.height = r.u32()? >> 16;
            }
            b"edts" => {
                if let Some(e) = crate::reader::find_box(b, b"elst") {
                    let mut r = Reader::new(e);
                    let (v, _) = r.version_flags()?;
                    let n = r.u32()?;
                    for _ in 0..n {
                        let (dur, mt) = if v == 1 { (r.u64()? as i64, r.i64()?) } else { (r.u32()? as i64, r.i32()? as i64) };
                        r.skip(4)?; // rate
                        tp.elst.push((dur, mt));
                    }
                }
            }
            b"mdia" => parse_mdia(b, &mut tp)?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(tp)
}

fn parse_mdia(mdia: &[u8], tp: &mut TrakParts) -> Result<(), Error> {
    for_each_box(mdia, |kind, b, _| {
        match &kind {
            b"mdhd" => {
                let mut r = Reader::new(b);
                let (v, _) = r.version_flags()?;
                if v == 1 {
                    r.skip(16)?;
                } else {
                    r.skip(8)?;
                }
                tp.timescale = r.u32()?;
            }
            b"hdlr" => {
                let mut r = Reader::new(b);
                r.version_flags()?;
                r.u32()?;
                let h = r.fourcc()?;
                tp.kind = match &h {
                    b"vide" => TrackKind::Video,
                    b"soun" => TrackKind::Audio,
                    _ => TrackKind::Other,
                };
            }
            b"minf" => {
                if let Some(stbl) = crate::reader::find_box(b, b"stbl") {
                    parse_stbl(stbl, tp)?;
                }
            }
            _ => {}
        }
        Ok(())
    })
}

fn parse_stbl(stbl: &[u8], tp: &mut TrakParts) -> Result<(), Error> {
    for_each_box(stbl, |kind, b, _| {
        let mut r = Reader::new(b);
        match &kind {
            b"stsd" => {
                // (an ISO audio entry of version 1 lives in an stsd of version 1;
                // QuickTime's versions 1 and 2, with their fields, in one of 0)
                let (stsd_version, _) = r.version_flags()?;
                let n = r.u32()?;
                if n == 0 {
                    return Err("empty stsd".into());
                }
                let entries = r.rest();
                let mut first = true;
                for_each_box(entries, |fourcc, body, whole| {
                    if !first {
                        return Ok(());
                    }
                    first = false;
                    tp.fourcc = fourcc;
                    tp.entry = whole.to_vec();
                    let mut er = Reader::new(body);
                    let mut sound = None;
                    match tp.kind {
                        TrackKind::Video => {
                            er.skip(6 + 2 + 2 + 2 + 12)?;
                            let w = er.u16()? as u32;
                            let h = er.u16()? as u32;
                            if tp.width == 0 {
                                tp.width = w;
                            }
                            if tp.height == 0 {
                                tp.height = h;
                            }
                            er.skip(4 + 4 + 4 + 2 + 32 + 2 + 2)?;
                        }
                        TrackKind::Audio => {
                            er.skip(6 + 2)?;
                            let version = er.u16()?;
                            er.skip(2 + 4)?;
                            tp.channels = er.u16()? as u32;
                            let bits = er.u16()? as u32;
                            er.skip(2 + 2)?;
                            tp.sample_rate = er.u32()? >> 16;
                            let mut sd = SoundDesc { version, bits, ..Default::default() };
                            if stsd_version == 0 && version == 1 {
                                // samples per packet, bytes per packet, bytes per frame, bytes per sample
                                er.skip(8)?;
                                sd.frame_bytes = er.u32()?;
                                er.skip(4)?;
                            } else if stsd_version == 0 && version == 2 {
                                er.skip(4)?; // the size of these fields
                                tp.sample_rate = f64::from_bits(er.u64()?).round() as u32;
                                tp.channels = er.u32()?;
                                er.skip(4)?; // 0x7F000000
                                sd.bits = er.u32()?;
                                sd.flags = er.u32()?;
                                let packet_bytes = er.u32()?;
                                let packet_frames = er.u32()?;
                                sd.frame_bytes = if packet_frames > 0 { packet_bytes / packet_frames } else { 0 };
                            }
                            sound = Some(sd);
                        }
                        TrackKind::Other => {}
                    }
                    let children = er.rest();
                    tp.codec = from_sample_entry(&fourcc, children).unwrap_or_else(|_| CodecInfo { codec: fourcc_str(&fourcc), description: None });
                    if let Some(sd) = sound {
                        // a rate the 16.16 field can't hold (96 kHz and up): an 'srat' box's
                        if let Some(srat) = crate::reader::find_box(children, b"srat").filter(|b| b.len() >= 8) {
                            tp.sample_rate = u32::from_be_bytes([srat[4], srat[5], srat[6], srat[7]]);
                        }
                        if let Some((codec, form, frame)) = pcm_layout(&fourcc, &sd, tp.channels, children) {
                            tp.codec = CodecInfo { codec: codec.into(), description: None };
                            tp.pcm = Some(form);
                            tp.pcm_frame = frame;
                        }
                    }
                    Ok(())
                })?;
            }
            b"stts" => {
                r.version_flags()?;
                let n = r.u32()?;
                for _ in 0..n {
                    tp.stbl.stts.push((r.u32()?, r.u32()?));
                }
            }
            b"ctts" => {
                r.version_flags()?;
                let n = r.u32()?;
                for _ in 0..n {
                    // (signed in version 0 too, as some writers make them and ffmpeg reads them)
                    tp.stbl.ctts.push((r.u32()?, r.i32()? as i64));
                }
            }
            b"stsc" => {
                r.version_flags()?;
                let n = r.u32()?;
                for _ in 0..n {
                    tp.stbl.stsc.push((r.u32()?, r.u32()?, r.u32()?));
                }
            }
            b"stsz" => {
                r.version_flags()?;
                tp.stbl.fixed_size = r.u32()?;
                tp.stbl.sample_count = r.u32()?;
                if tp.stbl.fixed_size == 0 {
                    for _ in 0..tp.stbl.sample_count {
                        tp.stbl.sizes.push(r.u32()?);
                    }
                }
            }
            b"stz2" => {
                r.version_flags()?;
                r.u24()?;
                let field = r.u8()?;
                tp.stbl.sample_count = r.u32()?;
                match field {
                    16 => {
                        for _ in 0..tp.stbl.sample_count {
                            tp.stbl.sizes.push(r.u16()? as u32);
                        }
                    }
                    8 => {
                        for _ in 0..tp.stbl.sample_count {
                            tp.stbl.sizes.push(r.u8()? as u32);
                        }
                    }
                    4 => {
                        let mut i = 0;
                        while i < tp.stbl.sample_count {
                            let b = r.u8()?;
                            tp.stbl.sizes.push((b >> 4) as u32);
                            if i + 1 < tp.stbl.sample_count {
                                tp.stbl.sizes.push((b & 15) as u32);
                            }
                            i += 2;
                        }
                    }
                    _ => return Err("bad stz2 field size".into()),
                }
            }
            b"stco" => {
                r.version_flags()?;
                let n = r.u32()?;
                for _ in 0..n {
                    tp.stbl.chunk_offsets.push(r.u32()? as u64);
                }
            }
            b"co64" => {
                r.version_flags()?;
                let n = r.u32()?;
                for _ in 0..n {
                    tp.stbl.chunk_offsets.push(r.u64()?);
                }
            }
            b"stss" => {
                r.version_flags()?;
                let n = r.u32()?;
                // (not sized by the count: the box holds as many as it holds)
                let mut v = Vec::new();
                for _ in 0..n {
                    v.push(r.u32()?);
                }
                tp.stbl.stss = Some(v);
            }
            _ => {}
        }
        Ok(())
    })
}

fn expand_samples(s: &Stbl) -> Result<Vec<Sample>, Error> {
    let n = s.sample_count as usize;
    if n == 0 {
        return Ok(vec![]);
    }
    let size = |i: usize| -> u32 {
        if s.fixed_size != 0 {
            s.fixed_size
        } else {
            s.sizes.get(i).copied().unwrap_or(0)
        }
    };
    // durations / dts (a run's count may be far past the samples)
    let mut dur = Vec::with_capacity(n);
    for &(count, delta) in &s.stts {
        dur.extend(std::iter::repeat_n(delta, (count as usize).min(n - dur.len())));
    }
    while dur.len() < n {
        dur.push(dur.last().copied().unwrap_or(1));
    }
    // composition offsets
    let mut cts = Vec::with_capacity(n);
    for &(count, off) in &s.ctts {
        cts.extend(std::iter::repeat_n(off, (count as usize).min(n - cts.len())));
    }
    while cts.len() < n {
        cts.push(0);
    }
    // chunk layout
    let mut offsets = Vec::with_capacity(n);
    if s.chunk_offsets.is_empty() || s.stsc.is_empty() {
        return Err("stbl without chunk offsets".into());
    }
    let mut sample = 0usize;
    for (coff, spc) in chunks(s) {
        let mut pos = coff;
        for _ in 0..spc {
            if sample >= n {
                break;
            }
            offsets.push(pos);
            pos += size(sample) as u64;
            sample += 1;
        }
        if sample >= n {
            break;
        }
    }
    while offsets.len() < n {
        // more samples than the chunk table accounts for: clamp
        let last = *offsets.last().unwrap_or(&0);
        offsets.push(last);
    }
    let sync: Vec<bool> = match &s.stss {
        None => vec![true; n],
        Some(list) => {
            let mut v = vec![false; n];
            for &k in list {
                if k >= 1 && (k as usize) <= n {
                    v[k as usize - 1] = true;
                }
            }
            v
        }
    };
    let mut out = Vec::with_capacity(n);
    let mut dts = 0i64;
    for i in 0..n {
        out.push(Sample { offset: offsets[i], size: size(i), dts, pts: dts + cts[i], duration: dur[i], sync: sync[i] });
        dts += dur[i] as i64;
    }
    Ok(out)
}

/// Each chunk's offset and how many samples it holds: the count of the
/// last sample-to-chunk run that starts at or before it (the first run's
/// for a chunk before them all), the runs walked once for all the chunks.
fn chunks(s: &Stbl) -> impl Iterator<Item = (u64, u32)> + '_ {
    let mut run = 0;
    s.chunk_offsets.iter().enumerate().map(move |(ci, &off)| {
        let no = ci as u32 + 1;
        while run + 1 < s.stsc.len() && s.stsc[run].0 <= no && s.stsc[run + 1].0 <= no {
            run += 1;
        }
        (off, s.stsc[run].1)
    })
}

/// Frames of uncompressed sound in a packet (43 ms at 48 kHz).
const PCM_PACKET: u32 = 2048;

/// The samples of uncompressed sound whose sample tables count every
/// sample frame: each chunk's frames, which lie one after another, in
/// packets of up to [`PCM_PACKET`] frames, so a film's sound is a few
/// hundred thousand packets rather than hundreds of millions of samples.
fn expand_pcm(s: &Stbl, frame_bytes: u32) -> Result<Vec<Sample>, Error> {
    let n = s.sample_count as u64;
    if n == 0 {
        return Ok(vec![]);
    }
    if s.chunk_offsets.is_empty() || s.stsc.is_empty() {
        return Err("stbl without chunk offsets".into());
    }
    // the durations, walked as the frames go by (past the table, its last goes on)
    let mut runs = s.stts.iter().copied();
    let (mut left, mut delta) = runs.next().unwrap_or((u32::MAX, 1));
    let mut ticks = |mut k: u32| -> u64 {
        let mut sum = 0u64;
        while k > 0 {
            if left == 0 {
                match runs.next() {
                    Some((c, d)) => (left, delta) = (c, d),
                    None => left = u32::MAX,
                }
                continue;
            }
            let m = k.min(left);
            sum += m as u64 * delta as u64;
            k -= m;
            left -= m;
        }
        sum
    };
    let mut out = Vec::new();
    let mut done = 0u64;
    let mut dts = 0i64;
    for (coff, spc) in chunks(s) {
        let mut frames = (spc as u64).min(n - done) as u32;
        done += frames as u64;
        let mut pos = coff;
        while frames > 0 {
            let k = frames.min(PCM_PACKET);
            let dur = ticks(k);
            out.push(Sample { offset: pos, size: k * frame_bytes, dts, pts: dts, duration: dur as u32, sync: true });
            pos += k as u64 * frame_bytes as u64;
            dts += dur as i64;
            frames -= k;
        }
        if done >= n {
            break;
        }
    }
    Ok(out)
}

/// Uncompressed sound, in QuickTime's forms and ISO's (`ipcm`, `fpcm`): the
/// codec string it goes by, its form and the bytes of one sample frame;
/// None when the entry is not PCM.
fn pcm_layout(fourcc: &[u8; 4], sd: &SoundDesc, channels: u32, children: &[u8]) -> Option<(&'static str, PcmForm, u32)> {
    let channels = channels.max(1);
    // QuickTime's 'enda' (in the 'wave' box, or on its own): 1 for little endian
    let little = || {
        let enda = crate::reader::find_box(children, b"wave").and_then(|w| crate::reader::find_box(w, b"enda")).or_else(|| crate::reader::find_box(children, b"enda"));
        enda.map(|e| e.len() >= 2 && u16::from_be_bytes([e[0], e[1]]) != 0).unwrap_or(false)
    };
    let form = |bits: u32, float: bool, le: bool, signed: bool| PcmForm { bits, float, le, signed };
    let f = match fourcc {
        b"raw " | b"NONE" => form(if sd.bits == 16 { 16 } else { 8 }, false, false, false),
        b"twos" => form(sd.bits.max(8), false, false, true),
        b"sowt" => form(sd.bits.max(8), false, true, true),
        b"in24" => form(24, false, little(), true),
        b"in32" => form(32, false, little(), true),
        b"fl32" => form(32, true, little(), true),
        b"fl64" => form(64, true, little(), true),
        b"ulaw" => return Some(("ulaw", form(8, false, false, false), channels)),
        b"alaw" => return Some(("alaw", form(8, false, false, false), channels)),
        b"lpcm" => form(sd.bits, sd.flags & 1 != 0, sd.flags & 2 == 0, sd.flags & 4 != 0),
        b"ipcm" | b"fpcm" => {
            // (a full box: version and flags, the format flags, the sample size)
            let c = crate::reader::find_box(children, b"pcmC").filter(|c| c.len() >= 6)?;
            form(c[5] as u32, fourcc == b"fpcm", c[4] & 1 != 0, true)
        }
        _ => return None,
    };
    let frame = if sd.version > 0 && sd.frame_bytes > 0 { sd.frame_bytes } else { f.bits / 8 * channels };
    Some((pcm_codec(f)?, f, frame))
}

/// The codec string of a form of PCM: WebCodecs' where it has one
/// (`pcm-s16` and the like, little endian), else named alike by the app
/// (`pcm-s16be`, `pcm-f64`).
pub(crate) fn pcm_codec(f: PcmForm) -> Option<&'static str> {
    Some(match (f.bits, f.float, f.le) {
        (8, false, _) => {
            if f.signed {
                "pcm-s8"
            } else {
                "pcm-u8"
            }
        }
        (16, false, true) => "pcm-s16",
        (16, false, false) => "pcm-s16be",
        (24, false, true) => "pcm-s24",
        (24, false, false) => "pcm-s24be",
        (32, false, true) => "pcm-s32",
        (32, false, false) => "pcm-s32be",
        (32, true, true) => "pcm-f32",
        (32, true, false) => "pcm-f32be",
        (64, true, true) => "pcm-f64",
        (64, true, false) => "pcm-f64be",
        _ => return None,
    })
}

/// Whether `codec` is uncompressed sound (the PCM codec strings, G.711).
fn is_pcm(codec: &str) -> bool {
    codec.starts_with("pcm-") || codec == "ulaw" || codec == "alaw"
}

fn finish_track(t: &mut Track) {
    if t.edit_shift != 0 {
        for s in &mut t.samples {
            s.pts += t.edit_shift;
        }
    }
}

// ---- fragments ---------------------------------------------------------------

/// Add the samples of a `moof` (at `moof_offset` in a file of `file_size`
/// bytes) to the movie's tracks, with the `trex` defaults by track.
fn apply_moof(movie: &mut Movie, trex: &[(u32, Trex)], file_size: u64, moof_offset: u64, moof: &[u8], next_dts: &mut [i64]) -> Result<(), Error> {
    let body = &moof[box_header(moof)?.header_len as usize..];
    let mut prev_traf_end: Option<u64> = None;
    for_each_box(body, |kind, traf, _| {
        if &kind != b"traf" {
            return Ok(());
        }
        // tfhd
        let tfhd = crate::reader::find_box(traf, b"tfhd").ok_or("traf without tfhd")?;
        let mut r = Reader::new(tfhd);
        let (_, flags) = r.version_flags()?;
        let track_id = r.u32()?;
        let ti = movie.tracks.iter().position(|t| t.id == track_id).ok_or("traf for unknown track")?;
        let trex = trex.iter().find(|(id, _)| *id == track_id).map(|(_, t)| t.clone()).unwrap_or_default();
        let mut base: Option<u64> = None;
        if flags & 0x1 != 0 {
            base = Some(r.u64()?);
        }
        if flags & 0x2 != 0 {
            r.u32()?;
        }
        let def_dur = if flags & 0x8 != 0 { r.u32()? } else { trex.default_duration };
        let def_size = if flags & 0x10 != 0 { r.u32()? } else { trex.default_size };
        let def_flags = if flags & 0x20 != 0 { r.u32()? } else { trex.default_flags };
        let default_base_is_moof = flags & 0x20000 != 0;
        let base = base.unwrap_or(if default_base_is_moof { moof_offset } else { prev_traf_end.unwrap_or(moof_offset) });
        // tfdt
        let mut dts = next_dts[ti];
        if let Some(tfdt) = crate::reader::find_box(traf, b"tfdt") {
            let mut r = Reader::new(tfdt);
            let (v, _) = r.version_flags()?;
            dts = if v == 1 { r.u64()? as i64 } else { r.u32()? as i64 };
        }
        // truns
        let mut data_pos = base;
        let mut end = base;
        let track = &mut movie.tracks[ti];
        for_each_box(traf, |k, trun, _| {
            if &k != b"trun" {
                return Ok(());
            }
            let mut r = Reader::new(trun);
            let (_, tf) = r.version_flags()?;
            let count = r.u32()?;
            // (samples whose sizes the run doesn't list, each a byte at least:
            // a count the file can't hold would only ask for memory there is not)
            if tf & 0x200 == 0 && count as u64 * def_size.max(1) as u64 > file_size {
                return Err(format!("track {track_id} lists more samples than the file holds"));
            }
            if tf & 0x1 != 0 {
                data_pos = (base as i64 + r.i32()? as i64) as u64;
            }
            let first_flags = if tf & 0x4 != 0 { Some(r.u32()?) } else { None };
            for i in 0..count {
                let d = if tf & 0x100 != 0 { r.u32()? } else { def_dur };
                let sz = if tf & 0x200 != 0 { r.u32()? } else { def_size };
                let fl = if tf & 0x400 != 0 {
                    r.u32()?
                } else if i == 0 {
                    first_flags.unwrap_or(def_flags)
                } else {
                    def_flags
                };
                // (signed in version 0 too, as for ctts)
                let cto = if tf & 0x800 != 0 { r.i32()? as i64 } else { 0 };
                let sync = fl & 0x10000 == 0;
                track.samples.push(Sample { offset: data_pos, size: sz, dts, pts: dts + cto, duration: d, sync });
                data_pos += sz as u64;
                dts += d as i64;
            }
            end = end.max(data_pos);
            Ok(())
        })?;
        next_dts[ti] = dts;
        prev_traf_end = Some(end);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        [&(8 + body.len() as u32).to_be_bytes()[..], &kind[..], body].concat()
    }

    /// A full box of version 0 (flags `flags`) whose body is 32-bit words.
    fn full(kind: &[u8; 4], flags: u32, words: &[u32]) -> Vec<u8> {
        bx(kind, &[flags].iter().chain(words).flat_map(|w| w.to_be_bytes()).collect::<Vec<u8>>())
    }

    /// The `moov` of an MP4 of one track (timed text: its sample entry is a
    /// bare box) whose sample table holds `tables` besides the entry.
    fn moov(tables: &[Vec<u8>]) -> Vec<u8> {
        let tkhd = full(b"tkhd", 1, &[[0, 0, 1].as_slice(), &[0; 17]].concat());
        let mdhd = full(b"mdhd", 0, &[0, 0, 1000]);
        let hdlr = full(b"hdlr", 0, &[0, u32::from_be_bytes(*b"text")]);
        let stsd = bx(b"stsd", &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &bx(b"tx3g", &[])].concat());
        let stbl = bx(b"stbl", &[&[stsd][..], tables].concat().concat());
        let mdia = bx(b"mdia", &[mdhd, hdlr, bx(b"minf", &stbl)].concat());
        bx(b"moov", &bx(b"trak", &[tkhd, mdia].concat()))
    }

    #[test]
    fn counts_the_file_cannot_hold_are_refused() {
        // (each made a tiny file ask for gigabytes: a sync sample table
        // sized by its count, and fixed-size samples or a fragment's run
        // with no sizes of their own expanded to billions)
        let stss = moov(&[full(b"stss", 0, &[u32::MAX])]);
        assert!(parse_bytes(&stss).is_err());
        let stsz = moov(&[full(b"stsz", 0, &[1, u32::MAX]), full(b"stsc", 0, &[1, 1, 1, 1]), full(b"stco", 0, &[1, 100])]);
        let e = parse_bytes(&stsz).unwrap_err();
        assert!(e.contains("more samples than the file holds"), "{e}");
        let traf = bx(b"traf", &[full(b"tfhd", 0, &[1]), full(b"trun", 0, &[u32::MAX])].concat());
        let e = parse_bytes(&[moov(&[]), bx(b"moof", &traf)].concat()).unwrap_err();
        assert!(e.contains("more samples than the file holds"), "{e}");
    }

    #[test]
    fn long_runs_in_the_tables_take_no_time() {
        // a thousand runs of four billion durations for one sample: the
        // durations were counted out one by one past the sample count (for
        // hours in a debug build; a release build skips most of the count)
        let s = Stbl { stts: vec![(u32::MAX, 1); 1000], stsc: vec![(1, 1, 1)], fixed_size: 4, sample_count: 1, chunk_offsets: vec![100], ..Default::default() };
        assert_eq!(expand_samples(&s).unwrap(), vec![Sample { offset: 100, size: 4, dts: 0, pts: 0, duration: 1, sync: true }]);
    }

    #[test]
    fn chunk_runs_are_walked_once() {
        // as many sample-to-chunk runs as chunks: each chunk looked through
        // the runs from the first (a minute for these)
        let n = 500_000u32;
        let s = Stbl { stts: vec![(n, 1)], stsc: (1..=n).map(|c| (c, 1, 1)).collect(), fixed_size: 2, sample_count: n, chunk_offsets: (0..n as u64).map(|c| c * 2).collect(), ..Default::default() };
        let v = expand_samples(&s).unwrap();
        assert_eq!((v.len(), v[n as usize - 1].offset, v[n as usize - 1].dts), (n as usize, 2 * (n as u64 - 1), n as i64 - 1));
    }

    #[test]
    fn a_size_that_wraps_round_is_refused() {
        // after a 24-byte ftyp, a box whose 64-bit size took the read position
        // back to the start of the file, and so round again for ever
        let file = [bx(b"ftyp", &[0; 16]), [&[0, 0, 0, 1][..], b"free", &(u64::MAX - 23).to_be_bytes()].concat()].concat();
        let e = parse_bytes(&file).unwrap_err();
        assert!(e.contains("impossible size"), "{e}");
    }

    #[test]
    fn negative_composition_offsets_in_version_0() {
        // (signed by the standard only from version 1, but some writers put
        // them in version 0, and ffmpeg reads them as signed)
        let tables = [full(b"stts", 0, &[1, 2, 1000]), full(b"ctts", 0, &[2, 1, 1000, 1, 0xffff_fc18]), full(b"stsc", 0, &[1, 1, 2, 1]), full(b"stsz", 0, &[4, 2]), full(b"stco", 0, &[1, 100])];
        let m = parse_bytes(&moov(&tables)).unwrap();
        assert_eq!(m.tracks[0].samples.iter().map(|s| (s.dts, s.pts)).collect::<Vec<_>>(), vec![(0, 1000), (1000, 0)]);
        let traf = bx(b"traf", &[full(b"tfhd", 0x20000, &[1]), full(b"trun", 0x201 | 0x800, &[2, 0, 4, 1000, 4, 0xffff_fc18])].concat());
        let m = parse_bytes(&[moov(&[]), bx(b"moof", &traf)].concat()).unwrap();
        assert_eq!(m.tracks[0].samples.iter().map(|s| s.pts - s.dts).collect::<Vec<_>>(), vec![1000, -1000]);
    }

    #[test]
    fn an_mp4_that_looks_like_a_transport_stream() {
        // a box full of 0x47, the transport stream's sync byte
        let tables = [full(b"stts", 0, &[1, 1, 1000]), full(b"stsc", 0, &[1, 1, 1, 1]), full(b"stsz", 0, &[4, 1]), full(b"stco", 0, &[1, 100])];
        let file = [bx(b"ftyp", b"isom\0\0\0\0isom"), bx(b"free", &[0x47; 1000]), moov(&tables)].concat();
        let m = parse_bytes(&file).unwrap();
        assert_eq!((m.format.as_str(), m.tracks[0].samples.len()), ("mp4", 1));
    }

    #[test]
    fn too_short_for_a_box() {
        for file in [&[][..], &[0; 4][..]] {
            let e = parse_bytes(file).unwrap_err();
            assert!(e.contains("no moov box"), "{} bytes: {e}", file.len());
        }
    }
}
