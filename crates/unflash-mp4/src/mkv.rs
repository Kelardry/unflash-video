//! Matroska / WebM: the same [`Movie`] the MP4 demuxer produces (tracks,
//! WebCodecs codec strings and decoder descriptions, sample tables), and for
//! every track an MP4 can carry, the sample entry an export copies it under.
//!
//! Matroska keeps no sample table: every block of every cluster has to be
//! visited. The file is read front to back in large chunks through the
//! same `need()` / `feed()` protocol as the MP4 demuxer; a block is parsed
//! from its first bytes and its payload jumped over, so where blocks are
//! large the reads skip them. Unknown-size segments and clusters (written
//! by recorders that stream to disk), all three kinds of lacing and header
//! stripping are handled; damaged data is skipped to the next cluster.

use crate::codec::{av1_codec, avc_codec, hevc_codec};
use crate::demux::{pcm_codec, rescale, Movie, PcmForm, Sample, Track, TrackKind};
use crate::entry;
use crate::mux::{dts_from_cts, write_video_entry};
use crate::reader::{Bits, Writer};
use crate::Error;

const EBML_HEADER: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TIMESTAMP_SCALE: u32 = 0x2A_D7B1;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const FLAG_ENABLED: u32 = 0xB9;
const FLAG_DEFAULT: u32 = 0x88;
const DEFAULT_DURATION: u32 = 0x23_E383;
const NAME: u32 = 0x536E;
const LANGUAGE: u32 = 0x22_B59C;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const CODEC_DELAY: u32 = 0x56AA;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const OUTPUT_SAMPLING_FREQUENCY: u32 = 0x78B5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const CONTENT_ENCODING: u32 = 0x6240;
const CONTENT_ENCODING_SCOPE: u32 = 0x5032;
const CONTENT_ENCODING_TYPE: u32 = 0x5033;
const CONTENT_COMPRESSION: u32 = 0x5034;
const CONTENT_COMP_ALGO: u32 = 0x4254;
const CONTENT_COMP_SETTINGS: u32 = 0x4255;
const CLUSTER: u32 = 0x1F43_B675;
const CLUSTER_TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
const CUES: u32 = 0x1C53_BB6B;
const CHAPTERS: u32 = 0x1043_A770;
const TAGS: u32 = 0x1254_C367;
const ATTACHMENTS: u32 = 0x1941_A469;

/// Elements that only appear at the top of a segment: an unknown-size
/// cluster ends where one of these starts.
fn top_level(id: u32) -> bool {
    matches!(id, CLUSTER | CUES | TAGS | CHAPTERS | ATTACHMENTS | SEEK_HEAD | INFO | TRACKS | SEGMENT | EBML_HEADER)
}

/// Bytes read at a time.
const CHUNK: u64 = 8 << 20;
/// How much of a block is looked at: its header, lace sizes and the first
/// bytes of its frames.
const BLOCK_HEAD: u64 = 16 << 10;
/// The most a header element (EBML header, Info, Tracks) may take.
const MAX_WHOLE: u64 = 64 << 20;
const UNKNOWN: u64 = u64::MAX;
/// How much of a track's first frame is kept to work out its setup.
const FIRST_BYTES: usize = 64;

/// An element ID (the marker bits kept): `None` when `b` is too short,
/// `Some(None)` when the bytes cannot start one.
fn read_id(b: &[u8]) -> Option<Option<(u32, usize)>> {
    let first = *b.first()?;
    let len = first.leading_zeros() as usize + 1;
    if len > 4 {
        return Some(None);
    }
    if b.len() < len {
        return None;
    }
    let mut v = 0u32;
    for &x in &b[..len] {
        v = (v << 8) | x as u32;
    }
    Some(Some((v, len)))
}

/// A variable-size integer (the marker bit removed) and its length; the
/// value is `None` for the reserved all-ones "unknown" size.
fn read_vint(b: &[u8]) -> Option<Option<(Option<u64>, usize)>> {
    let first = *b.first()?;
    if first == 0 {
        return Some(None);
    }
    let len = first.leading_zeros() as usize + 1;
    if b.len() < len {
        return None;
    }
    let mut v = (first as u64) & (0xff >> len);
    let mut all_ones = v == (0xff >> len) as u64;
    for &x in &b[1..len] {
        v = (v << 8) | x as u64;
        all_ones &= x == 0xff;
    }
    Some(Some((if all_ones { None } else { Some(v) }, len)))
}

fn uint(b: &[u8]) -> u64 {
    b.iter().take(8).fold(0u64, |v, &x| (v << 8) | x as u64)
}

fn float(b: &[u8]) -> f64 {
    match b.len() {
        4 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64,
        8 => f64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
        _ => 0.0,
    }
}

fn string(b: &[u8]) -> String {
    let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// The children of a master element held whole in memory: (id, body).
fn children(data: &[u8]) -> Result<Vec<(u32, &[u8])>, Error> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < data.len() {
        let Some(Some((id, il))) = read_id(&data[p..]) else { return Err(format!("damaged element at +{p}")) };
        let Some(Some((size, sl))) = read_vint(&data[p + il..]) else { return Err(format!("damaged element size at +{p}")) };
        let body = p + il + sl;
        // (compared as u64: a size of 4 GB or more would be cut short as a 32-bit usize)
        let end = match size {
            Some(s) if s <= (data.len() - body) as u64 => body + s as usize,
            Some(_) => return Err(format!("element {id:#x} overruns its parent")),
            None => data.len(),
        };
        out.push((id, &data[body..end]));
        p = end;
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug)]
struct Open {
    id: u32,
    end: u64,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    offset: u64,
    size: u32,
    /// Nanoseconds; `i64::MIN` for a laced frame whose time the block
    /// does not give.
    ts: i64,
    key: bool,
    /// Samples in this frame when its own bytes say (Opus), else 0.
    samples: u32,
}

#[derive(Clone, Debug, Default)]
struct MkvTrack {
    number: u64,
    kind: u64,
    enabled: bool,
    default: bool,
    codec_id: String,
    private: Vec<u8>,
    /// Nanoseconds per frame, 0 when not given.
    default_duration: u64,
    codec_delay: u64,
    width: u32,
    height: u32,
    rate: f64,
    out_rate: f64,
    channels: u32,
    bit_depth: u32,
    name: String,
    language: String,
    /// Header stripping: the bytes every frame starts with, left out of the file.
    strip: Vec<u8>,
    /// A content encoding this reader cannot undo (zlib, encryption, ...).
    unreadable: Option<String>,
    frames: Vec<Frame>,
    /// The start of the first frame (stripped bytes put back).
    first: Vec<u8>,
}

#[derive(Default)]
struct Group {
    track: u64,
    frames: Vec<Frame>,
    referenced: bool,
}

/// Byte-range driven Matroska parser: `need()` -> read that range ->
/// `feed()`, until `movie()` is `Some`.
pub struct MkvDemuxer {
    file_size: u64,
    /// Bytes asked for at a time.
    chunk: u64,
    pos: u64,
    want: Option<(u64, u64)>,
    stack: Vec<Open>,
    doc_type: String,
    /// Nanoseconds per timestamp tick.
    scale: u64,
    tracks: Vec<MkvTrack>,
    cluster_ts: i64,
    group: Option<Group>,
    seen_segment: bool,
    /// Skipping damaged data: looking for the next cluster from `pos`.
    resync: bool,
    damaged: u32,
    done: bool,
    movie: Option<Movie>,
}

enum Step {
    /// Parsed; carry on from `pos`.
    Go,
    /// Read from `pos` again with at least this many bytes.
    More(u64),
    /// The end of what can be read.
    Stop,
}

impl MkvDemuxer {
    pub fn new(file_size: u64) -> Self {
        Self::with_chunk(file_size, CHUNK)
    }

    /// Reading `chunk` bytes at a time (tests use small ones to stress the
    /// places where an element straddles two reads).
    pub fn with_chunk(file_size: u64, chunk: u64) -> Self {
        let mut d = MkvDemuxer {
            file_size,
            chunk: chunk.max(1),
            pos: 0,
            want: None,
            stack: Vec::new(),
            doc_type: "matroska".into(),
            scale: 1_000_000,
            tracks: Vec::new(),
            cluster_ts: 0,
            group: None,
            seen_segment: false,
            resync: false,
            damaged: 0,
            done: false,
            movie: None,
        };
        d.request(0);
        d
    }

    fn request(&mut self, at_least: u64) {
        if self.pos >= self.file_size {
            self.want = None;
            return;
        }
        let len = self.chunk.max(at_least).min(self.file_size - self.pos);
        self.want = Some((self.pos, len));
    }

    pub fn need(&self) -> Option<(u64, u64)> {
        self.want
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// How far through the file the parse has got, 0 to 1.
    pub fn progress(&self) -> f64 {
        if self.done || self.file_size == 0 {
            1.0
        } else {
            self.pos as f64 / self.file_size as f64
        }
    }

    pub fn movie(&self) -> Option<&Movie> {
        self.movie.as_ref()
    }

    pub fn into_movie(self) -> Option<Movie> {
        self.movie
    }

    /// Serve the bytes of the last `need()` range: from its offset, at least
    /// its length (which [`crate::Demuxer::feed`] sees to).
    pub fn feed(&mut self, offset: u64, data: &[u8]) -> Result<(), Error> {
        // (the bytes asked for, of what may be more)
        let data = &data[..self.want.map_or(0, |(_, n)| n as usize).min(data.len())];
        loop {
            match self.step(offset, data)? {
                Step::Go => continue,
                Step::More(n) => {
                    // a read that already starts here and still falls short
                    // can only mean the file ends early
                    if self.pos >= self.file_size || (self.pos == offset && (n <= data.len() as u64 || self.pos + n > self.file_size)) {
                        self.stop();
                    } else {
                        self.request(n);
                    }
                    break;
                }
                Step::Stop => {
                    self.stop();
                    break;
                }
            }
        }
        if self.done && self.movie.is_none() {
            self.finish()?;
        }
        Ok(())
    }

    fn stop(&mut self) {
        while let Some(o) = self.stack.pop() {
            self.close(o);
        }
        self.done = true;
        self.want = None;
    }

    fn close(&mut self, o: Open) {
        if o.id == BLOCK_GROUP {
            if let Some(g) = self.group.take() {
                if let Some(t) = self.tracks.iter_mut().find(|t| t.number == g.track) {
                    // a frame nothing else refers to is a key frame
                    let key = !g.referenced || t.kind != 1;
                    for mut f in g.frames {
                        f.key = key;
                        t.frames.push(f);
                    }
                }
            }
        }
    }

    /// One element at `pos`, of the buffer `data` that starts at `base`.
    fn step(&mut self, base: u64, data: &[u8]) -> Result<Step, Error> {
        // leave the elements that end here
        while let Some(&top) = self.stack.last() {
            if top.end != UNKNOWN && self.pos >= top.end {
                self.stack.pop();
                self.close(top);
            } else {
                break;
            }
        }
        if self.pos >= self.file_size || (self.seen_segment && self.stack.is_empty()) {
            return Ok(Step::Stop);
        }
        let end_of_data = base + data.len() as u64;
        if self.pos >= end_of_data || self.pos < base {
            return Ok(Step::More(0));
        }
        let rel = (self.pos - base) as usize;
        let avail = &data[rel..];
        if self.resync {
            // the next cluster, if this buffer holds its start
            match avail.windows(4).position(|w| w == [0x1F, 0x43, 0xB6, 0x75]) {
                Some(p) => {
                    self.pos += p as u64;
                    self.resync = false;
                    return Ok(Step::Go);
                }
                None => {
                    if end_of_data >= self.file_size {
                        return Ok(Step::Stop);
                    }
                    self.pos = end_of_data.saturating_sub(3).max(self.pos + 1);
                    // (a read can be shorter than the ID it looks for)
                    return Ok(Step::More(4));
                }
            }
        }
        let (id, il) = match read_id(avail) {
            None => return Ok(Step::More(16)),
            Some(None) => return Ok(self.damaged_here()),
            Some(Some(x)) => x,
        };
        let (size, sl) = match read_vint(&avail[il..]) {
            None => return Ok(Step::More(16)),
            Some(None) => return Ok(self.damaged_here()),
            Some(Some(x)) => x,
        };
        let hl = (il + sl) as u64;
        let body = self.pos + hl;
        let end = size.map(|s| body.saturating_add(s)).unwrap_or(UNKNOWN);
        // an element of the segment ends the cluster (and block group) it would sit in
        if top_level(id) {
            while let Some(&top) = self.stack.last() {
                if top.id == CLUSTER || top.id == BLOCK_GROUP {
                    self.stack.pop();
                    self.close(top);
                } else {
                    break;
                }
            }
        }
        let parent = self.stack.last().map(|o| o.id);
        // an element that claims to run past its parent is damage
        if let (Some(p), Some(_)) = (self.stack.last(), size) {
            if p.end != UNKNOWN && end > p.end && p.id != SEGMENT {
                return Ok(self.damaged_here());
            }
        }
        let whole = || -> Result<Option<&[u8]>, Error> {
            let Some(s) = size else { return Err(format!("element {id:#x} of unknown size")) };
            if s > MAX_WHOLE {
                return Err(format!("element {id:#x} is {s} bytes"));
            }
            if (rel as u64) + hl + s <= data.len() as u64 {
                Ok(Some(&data[rel + hl as usize..rel + (hl + s) as usize]))
            } else {
                Ok(None)
            }
        };
        match (parent, id) {
            (None, EBML_HEADER) => {
                let Some(b) = whole()? else { return Ok(Step::More(hl + size.unwrap_or(0))) };
                for (cid, cb) in children(b)? {
                    if cid == DOC_TYPE {
                        self.doc_type = string(cb);
                    }
                }
                if !matches!(self.doc_type.as_str(), "matroska" | "webm") {
                    return Err(format!("an EBML file of type \"{}\", not Matroska or WebM", self.doc_type));
                }
                self.pos = end;
            }
            (None, SEGMENT) => {
                if self.seen_segment {
                    // a second, chained segment: the first is the movie
                    return Ok(Step::Stop);
                }
                self.seen_segment = true;
                self.stack.push(Open { id: SEGMENT, end: if end == UNKNOWN { self.file_size } else { end.min(self.file_size) } });
                self.pos = body;
            }
            (Some(SEGMENT), INFO) | (Some(SEGMENT), TRACKS) => {
                let Some(b) = whole()? else {
                    if body + size.unwrap_or(0) > self.file_size {
                        return Err("the file ends inside its header".into());
                    }
                    return Ok(Step::More(hl + size.unwrap_or(0)));
                };
                if id == INFO {
                    self.parse_info(b)?;
                } else {
                    self.parse_tracks(b)?;
                }
                self.pos = end;
            }
            (Some(SEGMENT), CLUSTER) => {
                self.stack.push(Open { id: CLUSTER, end: if end == UNKNOWN { UNKNOWN } else { end.min(self.file_size) } });
                self.cluster_ts = 0;
                self.pos = body;
            }
            (Some(CLUSTER), CLUSTER_TIMESTAMP) => {
                // (an unsigned integer of 8 bytes at most: anything else is
                // damage, not a reason to give up on the file)
                if size.is_none_or(|s| s > 8) {
                    return Ok(self.damaged_here());
                }
                let Some(b) = whole()? else { return Ok(Step::More(hl + size.unwrap_or(0))) };
                self.cluster_ts = uint(b) as i64;
                self.pos = end;
            }
            (Some(CLUSTER), SIMPLE_BLOCK) | (Some(BLOCK_GROUP), BLOCK) => {
                let Some(s) = size else { return Ok(self.damaged_here()) };
                if body + s > self.file_size {
                    // the file was cut off inside this block
                    return Ok(Step::Stop);
                }
                let head = s.min(BLOCK_HEAD);
                if (rel as u64) + hl + head > data.len() as u64 {
                    return Ok(Step::More(hl + head));
                }
                let b = &data[rel + hl as usize..rel + (hl + head) as usize];
                match self.block(b, body, s, id == SIMPLE_BLOCK) {
                    Ok(true) => self.pos = end,
                    // lace sizes run past the part looked at: the whole block
                    Ok(false) => {
                        if (rel as u64) + hl + s <= data.len() as u64 {
                            let b = &data[rel + hl as usize..rel + (hl + s) as usize];
                            if !self.block(b, body, s, id == SIMPLE_BLOCK)? {
                                return Ok(self.damaged_here());
                            }
                            self.pos = end;
                        } else {
                            return Ok(Step::More(hl + s));
                        }
                    }
                    Err(_) => return Ok(self.damaged_here()),
                }
            }
            (Some(CLUSTER), BLOCK_GROUP) => {
                if end == UNKNOWN {
                    return Ok(self.damaged_here());
                }
                self.stack.push(Open { id: BLOCK_GROUP, end });
                self.group = Some(Group::default());
                self.pos = body;
            }
            (Some(BLOCK_GROUP), REFERENCE_BLOCK) => {
                if let Some(g) = self.group.as_mut() {
                    g.referenced = true;
                }
                if end == UNKNOWN {
                    return Ok(self.damaged_here());
                }
                self.pos = end;
            }
            (None, _) => {
                if self.seen_segment {
                    return Ok(Step::Stop);
                }
                if end == UNKNOWN {
                    return Err("not a Matroska file".into());
                }
                self.pos = end;
            }
            _ => {
                // anything else (cues, tags, attachments, voids, CRCs, block
                // additions): not needed, and skipped without being read
                if end == UNKNOWN {
                    return Ok(self.damaged_here());
                }
                self.pos = end;
            }
        }
        Ok(Step::Go)
    }

    /// Damaged data at `pos`: look for the next cluster.
    fn damaged_here(&mut self) -> Step {
        self.damaged += 1;
        while let Some(&top) = self.stack.last() {
            if top.id == CLUSTER || top.id == BLOCK_GROUP {
                self.stack.pop();
                self.close(top);
            } else {
                break;
            }
        }
        if !self.seen_segment || self.tracks.is_empty() || self.damaged > 1000 {
            return Step::Stop;
        }
        self.resync = true;
        self.pos += 1;
        Step::Go
    }

    fn parse_info(&mut self, b: &[u8]) -> Result<(), Error> {
        for (id, v) in children(b)? {
            if id == TIMESTAMP_SCALE {
                self.scale = uint(v).max(1);
            }
        }
        Ok(())
    }

    fn parse_tracks(&mut self, b: &[u8]) -> Result<(), Error> {
        for (id, entry) in children(b)? {
            if id != TRACK_ENTRY {
                continue;
            }
            let mut t = MkvTrack { enabled: true, default: true, rate: 8000.0, channels: 1, ..Default::default() };
            for (cid, v) in children(entry)? {
                match cid {
                    TRACK_NUMBER => t.number = uint(v),
                    TRACK_TYPE => t.kind = uint(v),
                    FLAG_ENABLED => t.enabled = uint(v) != 0,
                    FLAG_DEFAULT => t.default = uint(v) != 0,
                    DEFAULT_DURATION => t.default_duration = uint(v),
                    NAME => t.name = string(v),
                    LANGUAGE => t.language = string(v),
                    CODEC_ID => t.codec_id = string(v),
                    CODEC_PRIVATE => t.private = v.to_vec(),
                    CODEC_DELAY => t.codec_delay = uint(v),
                    VIDEO => {
                        for (vid, vv) in children(v)? {
                            match vid {
                                PIXEL_WIDTH => t.width = uint(vv) as u32,
                                PIXEL_HEIGHT => t.height = uint(vv) as u32,
                                _ => {}
                            }
                        }
                    }
                    AUDIO => {
                        for (aid, av) in children(v)? {
                            match aid {
                                SAMPLING_FREQUENCY => t.rate = float(av),
                                OUTPUT_SAMPLING_FREQUENCY => t.out_rate = float(av),
                                CHANNELS => t.channels = uint(av) as u32,
                                BIT_DEPTH => t.bit_depth = uint(av) as u32,
                                _ => {}
                            }
                        }
                    }
                    CONTENT_ENCODINGS => {
                        for (eid, ev) in children(v)? {
                            if eid != CONTENT_ENCODING {
                                continue;
                            }
                            let (mut scope, mut kind, mut algo, mut settings) = (1u64, 0u64, 0u64, Vec::new());
                            let mut compression = false;
                            for (xid, xv) in children(ev)? {
                                match xid {
                                    CONTENT_ENCODING_SCOPE => scope = uint(xv),
                                    CONTENT_ENCODING_TYPE => kind = uint(xv),
                                    CONTENT_COMPRESSION => {
                                        compression = true;
                                        for (cid2, cv) in children(xv)? {
                                            match cid2 {
                                                CONTENT_COMP_ALGO => algo = uint(cv),
                                                CONTENT_COMP_SETTINGS => settings = cv.to_vec(),
                                                _ => {}
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            if scope & 1 == 0 {
                                continue; // not the frames
                            }
                            if kind == 0 && compression && algo == 3 {
                                t.strip.extend_from_slice(&settings);
                            } else if kind == 1 {
                                t.unreadable = Some("encrypted".into());
                            } else {
                                t.unreadable = Some(match algo {
                                    0 => "zlib-compressed",
                                    1 => "bzip2-compressed",
                                    2 => "LZO-compressed",
                                    _ => "compressed",
                                }
                                .into());
                            }
                        }
                    }
                    _ => {}
                }
            }
            if t.number != 0 {
                self.tracks.push(t);
            }
        }
        Ok(())
    }

    /// Parse a block from its head `b` (the payload starting at file offset
    /// `at`, `size` bytes in all). False when the lace sizes run past `b`.
    fn block(&mut self, b: &[u8], at: u64, size: u64, simple: bool) -> Result<bool, Error> {
        let Some(Some((Some(track), tl))) = read_vint(b) else { return Err("bad block track".into()) };
        if b.len() < tl + 3 {
            return Err("short block".into());
        }
        let rel = i16::from_be_bytes([b[tl], b[tl + 1]]) as i64;
        let flags = b[tl + 2];
        let mut p = tl + 3;
        let Some(ti) = self.tracks.iter().position(|t| t.number == track) else { return Ok(true) };
        // the frames of tracks nobody reads are not kept
        if !matches!(self.tracks[ti].kind, 1 | 2) {
            return Ok(true);
        }
        let lacing = (flags >> 1) & 3;
        let mut sizes: Vec<u64> = Vec::new();
        let total = size;
        if lacing != 0 {
            let Some(&count) = b.get(p) else { return Ok(false) };
            p += 1;
            let n = count as usize + 1;
            match lacing {
                1 => {
                    // Xiph: 255s and a last byte below 255, per frame but the last
                    for _ in 0..n - 1 {
                        let mut s = 0u64;
                        loop {
                            let Some(&x) = b.get(p) else { return Ok(false) };
                            p += 1;
                            s += x as u64;
                            if x != 255 {
                                break;
                            }
                        }
                        sizes.push(s);
                    }
                }
                3 => {
                    // EBML: the first size, then signed differences
                    let Some(v) = read_vint(&b[p..]) else { return Ok(false) };
                    let Some((Some(first), l)) = v else { return Err("bad lace size".into()) };
                    p += l;
                    sizes.push(first);
                    let mut prev = first as i64;
                    for _ in 1..n - 1 {
                        let Some(v) = read_vint(&b[p..]) else { return Ok(false) };
                        let Some((Some(raw), l)) = v else { return Err("bad lace size".into()) };
                        p += l;
                        let bias = (1i64 << (7 * l - 1)) - 1;
                        prev += raw as i64 - bias;
                        if prev < 0 {
                            return Err("negative lace size".into());
                        }
                        sizes.push(prev as u64);
                    }
                }
                _ => {
                    // fixed: equal shares
                    let rest = total.checked_sub(p as u64).ok_or("short block")?;
                    if rest % n as u64 != 0 {
                        return Err("uneven fixed lacing".into());
                    }
                    for _ in 0..n - 1 {
                        sizes.push(rest / n as u64);
                    }
                }
            }
            let used: u64 = sizes.iter().sum::<u64>() + p as u64;
            let last = total.checked_sub(used).ok_or("lace sizes exceed the block")?;
            sizes.push(last);
        } else {
            sizes.push(total.checked_sub(p as u64).ok_or("short block")?);
        }
        let t = &mut self.tracks[ti];
        let ts = (self.cluster_ts + rel).saturating_mul(self.scale as i64);
        let key = if simple { flags & 0x80 != 0 || t.kind != 1 } else { false };
        let opus = t.codec_id == "A_OPUS";
        let mut off = at + p as u64;
        for (i, &s) in sizes.iter().enumerate() {
            let rel_off = (off - at) as usize;
            let head = b.get(rel_off..(rel_off + FIRST_BYTES).min(b.len())).unwrap_or(&[]);
            // the start of the track's first frame tells its setup (AC-3, MP3, VP9)
            if t.first.is_empty() && !head.is_empty() {
                t.first = [&t.strip[..], head].concat();
                t.first.truncate(FIRST_BYTES);
            }
            let samples = if opus {
                let h: Vec<u8> = [&t.strip[..], head].concat();
                entry::opus_packet_samples(&h).unwrap_or(0)
            } else {
                0
            };
            let fts = if i == 0 {
                ts
            } else if t.default_duration > 0 {
                ts + (i as i64) * t.default_duration as i64
            } else {
                i64::MIN
            };
            let f = Frame { offset: off, size: s as u32, ts: fts, key, samples };
            if simple {
                t.frames.push(f);
            } else if let Some(g) = self.group.as_mut() {
                g.track = track;
                g.frames.push(f);
            }
            off += s;
        }
        Ok(true)
    }

    fn finish(&mut self) -> Result<(), Error> {
        if self.tracks.is_empty() {
            return Err(if self.seen_segment { "no tracks found in this Matroska file".into() } else { "not a Matroska file (no segment found)".into() });
        }
        let mut movie = Movie { format: self.doc_type.clone(), ..Default::default() };
        // video first, then audio, then the rest; within a kind the default track first
        let mut order: Vec<usize> = (0..self.tracks.len()).collect();
        let rank = |t: &MkvTrack| -> (u8, u8, u8) {
            (
                match t.kind {
                    1 => 0,
                    2 => 1,
                    _ => 2,
                },
                (!(t.default && t.enabled)) as u8,
                (!t.enabled) as u8,
            )
        };
        order.sort_by_key(|&i| rank(&self.tracks[i]));
        for i in order {
            let t = std::mem::take(&mut self.tracks[i]);
            movie.tracks.push(build_track(t, self.scale));
        }
        self.movie = Some(movie);
        Ok(())
    }
}

// ---- tracks --------------------------------------------------------------------

/// Samples per packet when the codec fixes it, or when each packet says.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PacketLength {
    Unknown,
    Fixed(u32),
    /// Each frame's own count (Opus).
    PerFrame,
    /// PCM: bytes per sample frame.
    Pcm(u32),
}

struct Setup {
    codec: String,
    description: Option<Vec<u8>>,
    fourcc: String,
    entry: Vec<u8>,
    /// Ticks per second for the track (audio: its sampling rate).
    timescale: u32,
    rate: u32,
    channels: u32,
    packet: PacketLength,
    note: String,
}

/// VP9's profile and bit depth from the head of a key frame.
fn vp9_frame_info(h: &[u8]) -> Option<(u8, u8)> {
    let mut r = Bits::new(h);
    if r.u(2)? != 2 {
        return None;
    }
    let lo = r.u(1)?;
    let hi = r.u(1)?;
    let profile = (hi << 1) | lo;
    if profile == 3 {
        r.skip(1)?;
    }
    if r.flag()? {
        return None; // show_existing_frame
    }
    let frame_type = r.u(1)?;
    r.skip(2)?; // show_frame, error_resilient_mode
    if frame_type != 0 {
        return None;
    }
    if r.u(24)? != 0x49_8342 {
        return None;
    }
    let depth = if profile >= 2 { if r.flag()? { 12 } else { 10 } } else { 8 };
    Some((profile as u8, depth))
}

/// The lowest VP9 level whose picture size and sample rate take this video.
fn vp9_level(width: u32, height: u32, fps: f64) -> u8 {
    const LEVELS: [(u8, u64, u64); 14] = [
        (10, 36_864, 829_440),
        (11, 73_728, 2_764_800),
        (20, 122_880, 4_608_000),
        (21, 245_760, 9_216_000),
        (30, 552_960, 20_736_000),
        (31, 983_040, 36_864_000),
        (40, 2_228_224, 83_558_400),
        (41, 2_228_224, 160_432_128),
        (50, 8_912_896, 311_951_360),
        (51, 8_912_896, 588_251_136),
        (52, 8_912_896, 1_176_502_272),
        (60, 35_651_584, 1_176_502_272),
        (61, 35_651_584, 2_353_004_544),
        (62, 35_651_584, 4_706_009_088),
    ];
    let size = width as u64 * height as u64;
    let rate = (size as f64 * fps.max(1.0)) as u64;
    LEVELS.iter().find(|&&(_, s, r)| size <= s && rate <= r).map(|l| l.0).unwrap_or(62)
}

fn setup(t: &MkvTrack, fps: f64, ts_timescale: u32) -> Setup {
    let mut s = Setup { codec: t.codec_id.clone(), description: None, fourcc: String::new(), entry: Vec::new(), timescale: ts_timescale, rate: 0, channels: 0, packet: PacketLength::Unknown, note: String::new() };
    let id = t.codec_id.as_str();
    let video_entry = |s: &mut Setup| {
        let mut w = Writer::new();
        if write_video_entry(&mut w, &s.codec, t.width, t.height, s.description.as_deref().unwrap_or(&[])).is_ok() {
            s.fourcc = String::from_utf8_lossy(&w.buf[4..8]).into_owned();
            s.entry = w.buf;
        }
    };
    if t.kind == 1 {
        match id {
            "V_MPEG4/ISO/AVC" => match avc_codec(&t.private) {
                Ok(c) => {
                    s.codec = c.codec;
                    s.description = c.description;
                    video_entry(&mut s);
                }
                Err(e) => s.note = format!("H.264 without its setup record ({e})"),
            },
            "V_MPEGH/ISO/HEVC" => {
                // parameter sets only in the stream: hev1
                let inband = t.private.get(22).map(|&n| n == 0).unwrap_or(true);
                match hevc_codec(&t.private, if inband { "hev1" } else { "hvc1" }) {
                    Ok(c) => {
                        s.codec = c.codec;
                        s.description = c.description;
                        video_entry(&mut s);
                    }
                    Err(e) => s.note = format!("HEVC without its setup record ({e})"),
                }
            }
            "V_AV1" => {
                match av1_codec(&t.private) {
                    Ok(c) => {
                        s.codec = c.codec;
                        s.description = c.description;
                    }
                    Err(_) => s.codec = "av01.0.08M.08".into(),
                }
                video_entry(&mut s);
            }
            "V_VP9" => {
                // the setup: CodecPrivate's features if there, else the first key frame
                let (mut profile, mut level, mut depth) = (None, None, None);
                let p = &t.private;
                let mut i = 0;
                while i + 2 <= p.len() {
                    let (fid, len) = (p[i], p[i + 1] as usize);
                    if let Some(&v) = p.get(i + 2) {
                        match fid {
                            1 => profile = Some(v),
                            2 => level = Some(v),
                            3 => depth = Some(v),
                            _ => {}
                        }
                    }
                    i += 2 + len;
                }
                if let Some((fp, fd)) = vp9_frame_info(&t.first) {
                    profile = profile.or(Some(fp));
                    depth = depth.or(Some(fd));
                }
                let level = level.filter(|&l| l > 0).unwrap_or_else(|| vp9_level(t.width, t.height, fps));
                s.codec = format!("vp09.{:02}.{:02}.{:02}", profile.unwrap_or(0), level, depth.unwrap_or(8));
                video_entry(&mut s);
            }
            "V_VP8" => {
                s.codec = "vp8".into();
                video_entry(&mut s);
            }
            _ => {
                s.note = match id {
                    "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" | "V_MS/VFW/FOURCC" => "MPEG-4 Part 2 (XviD / DivX) or another older codec".into(),
                    "V_MPEG1" | "V_MPEG2" => "MPEG-1/2 video".into(),
                    _ => format!("{id} video"),
                };
            }
        }
        return s;
    }
    if t.kind != 2 {
        return s;
    }
    // ---- audio
    let rate = if t.out_rate > 0.0 { t.out_rate } else { t.rate }.round() as u32;
    s.rate = rate;
    s.channels = t.channels.max(1);
    s.timescale = if rate >= 1000 { rate } else { ts_timescale };
    match id {
        _ if id == "A_AAC" || id.starts_with("A_AAC/") => {
            let core = t.rate.round() as u32;
            let asc = if t.private.len() >= 2 {
                t.private.clone()
            } else {
                let aot = if id.contains("/MAIN") {
                    1
                } else if id.contains("/SSR") {
                    3
                } else if id.contains("/LTP") {
                    4
                } else {
                    2
                };
                let sbr = (id.ends_with("/SBR") || t.out_rate > t.rate * 1.5).then_some(if t.out_rate > 0.0 { t.out_rate.round() as u32 } else { core * 2 });
                entry::make_asc(aot, core, t.channels, sbr)
            };
            let a = entry::parse_asc(&asc);
            let (aot, core_rate, sbr, short) = a.map(|a| (a.aot, a.rate, a.sbr_rate, a.short_frames)).unwrap_or((2, core, None, false));
            // the output rate: explicit SBR, or a Matroska output rate twice the core's
            let out = sbr.unwrap_or(if t.out_rate > t.rate * 1.5 { t.out_rate.round() as u32 } else { core_rate });
            let per = (if short { 960 } else { 1024 }) * (out / core_rate.max(1)).max(1);
            s.codec = format!("mp4a.40.{aot}");
            s.description = Some(asc.clone());
            s.rate = out;
            s.timescale = out;
            s.packet = PacketLength::Fixed(per);
            s.fourcc = "mp4a".into();
            s.entry = entry::aac_entry(&asc, core_rate, s.channels);
        }
        "A_MPEG/L3" | "A_MPEG/L2" | "A_MPEG/L1" => {
            if let Some((r, ch, per)) = entry::mpeg_audio_frame(&t.first) {
                s.rate = r;
                s.channels = ch;
                s.timescale = r;
                s.packet = PacketLength::Fixed(per);
            }
            s.codec = if id == "A_MPEG/L3" { "mp3".into() } else { "mp4a.6B".into() };
            s.fourcc = "mp4a".into();
            s.entry = entry::mpeg_audio_entry(s.rate, s.channels);
        }
        "A_AC3" | "A_AC3/BSID9" | "A_AC3/BSID10" => {
            s.codec = "ac-3".into();
            s.packet = PacketLength::Fixed(1536);
            if let Some((e, r, ch)) = entry::ac3_entry(&t.first) {
                s.entry = e;
                s.fourcc = "ac-3".into();
                s.rate = r;
                s.channels = ch;
                s.timescale = r;
            }
        }
        "A_EAC3" => {
            s.codec = "ec-3".into();
            if let Some((e, r, ch, per)) = entry::eac3_entry(&t.first) {
                s.entry = e;
                s.fourcc = "ec-3".into();
                s.rate = r;
                s.channels = ch;
                s.timescale = r;
                s.packet = PacketLength::Fixed(per);
            }
        }
        "A_OPUS" => {
            let head = if t.private.starts_with(b"OpusHead") {
                t.private.clone()
            } else {
                entry::make_opus_head(t.channels, (t.codec_delay * 48_000 / 1_000_000_000) as u16, rate)
            };
            s.codec = "opus".into();
            s.description = Some(head.clone());
            s.rate = 48000;
            s.timescale = 48000;
            s.packet = PacketLength::PerFrame;
            if let Ok(e) = entry::opus_entry(&head) {
                s.entry = e;
                s.fourcc = "Opus".into();
            }
        }
        "A_FLAC" => {
            s.codec = "flac".into();
            s.description = Some(t.private.clone());
            if let Some(si) = entry::flac_streaminfo(&t.private) {
                let (r, ch, block) = entry::flac_info(si);
                s.rate = r;
                s.channels = ch;
                s.timescale = r.max(1);
                if block > 0 {
                    s.packet = PacketLength::Fixed(block);
                }
                s.entry = entry::flac_entry(si);
                s.fourcc = "fLaC".into();
            }
        }
        _ if id == "A_DTS" || id.starts_with("A_DTS/") => {
            // (the core is what the app decodes; DTS Express has none)
            s.codec = match id {
                "A_DTS/EXPRESS" => "dtse",
                "A_DTS/LOSSLESS" => "dtsl",
                _ => "dtsc",
            }
            .into();
            if let Some((r, ch, per, _)) = entry::dts_frame(&t.first) {
                s.rate = r;
                s.channels = ch;
                s.timescale = r;
                s.packet = PacketLength::Fixed(per);
            }
        }
        "A_ALAC" => {
            // (named, for what says which sound this is: no browser decodes it)
            s.codec = "alac".into();
            s.description = Some(t.private.clone());
        }
        "A_VORBIS" => {
            s.codec = "vorbis".into();
            s.description = Some(t.private.clone());
        }
        "A_PCM/INT/LIT" | "A_PCM/INT/BIG" | "A_PCM/FLOAT/IEEE" => {
            // (Matroska's float is little endian, its 8-bit integers unsigned)
            let bytes = (t.bit_depth / 8).max(1);
            let form = PcmForm { bits: t.bit_depth, float: id == "A_PCM/FLOAT/IEEE", le: id != "A_PCM/INT/BIG", signed: t.bit_depth > 8 };
            s.codec = pcm_codec(form).map(String::from).unwrap_or_else(|| format!("{id} ({} bit)", t.bit_depth));
            s.packet = PacketLength::Pcm(bytes * s.channels);
        }
        _ => {}
    }
    s
}

fn build_track(mut t: MkvTrack, scale: u64) -> Track {
    let kind = match t.kind {
        1 => TrackKind::Video,
        2 => TrackKind::Audio,
        _ => TrackKind::Other,
    };
    // the file's own clock: ticks of `scale` ns, as a whole number per second
    // when it can be; video on at least a microsecond clock, so a stated
    // frame duration (1/30 s) survives
    let mut ts_timescale = if 1_000_000_000 % scale == 0 && 1_000_000_000 / scale <= u32::MAX as u64 { (1_000_000_000 / scale) as u32 } else { 1_000_000 };
    if kind == TrackKind::Video {
        ts_timescale = ts_timescale.max(1_000_000);
    }
    // (setup() doesn't look at the frames)
    let mut frames = std::mem::take(&mut t.frames);
    fill_laced_times(&mut frames, t.default_duration);
    if kind == TrackKind::Video {
        snap_to_frame_grid(&mut frames, t.default_duration, scale);
    }
    // a codec's built-in delay (Opus's pre-skip) is part of the block times
    // and comes off them
    if t.codec_delay > 0 {
        for f in &mut frames {
            f.ts -= t.codec_delay as i64;
        }
    }
    let fps = frame_rate(&frames, t.default_duration);
    let s = setup(&t, fps, ts_timescale);
    let ticks = |ns: i64| rescale(ns, 1_000_000_000, s.timescale as i64);
    let mut samples: Vec<Sample> = frames.iter().map(|f| Sample { offset: f.offset, size: f.size, dts: 0, pts: ticks(f.ts), duration: 0, sync: f.key }).collect();
    let nominal = if t.default_duration > 0 { ticks(t.default_duration as i64).max(1) as u32 } else { 0 };
    match kind {
        TrackKind::Video => video_timing(&mut samples, nominal),
        TrackKind::Audio => audio_timing(&mut samples, &frames, &s, nominal),
        TrackKind::Other => {}
    }
    let mut unreadable = t.unreadable;
    if kind == TrackKind::Video && !t.strip.is_empty() {
        unreadable = Some("stored with header stripping".into());
    }
    let frame_duration = match s.packet {
        PacketLength::Fixed(n) => n,
        _ => nominal,
    };
    Track {
        id: t.number as u32,
        kind,
        fourcc: s.fourcc,
        codec: s.codec,
        description: s.description,
        timescale: s.timescale.max(1),
        width: t.width,
        height: t.height,
        sample_rate: s.rate,
        channels: s.channels,
        sample_entry: if unreadable.is_some() { Vec::new() } else { s.entry },
        samples: if unreadable.is_some() && kind == TrackKind::Video { Vec::new() } else { samples },
        frame_duration,
        prefix: t.strip,
        name: t.name,
        language: t.language,
        note: unreadable.map(|u| format!("{} is {u}, which Unflash cannot read", t.codec_id)).unwrap_or(s.note),
        ..Default::default()
    }
}

/// Times for laced frames the block did not give: shared out evenly
/// between the frames that have them.
fn fill_laced_times(frames: &mut [Frame], default_duration: u64) {
    let n = frames.len();
    let mut i = 0;
    while i < n {
        if frames[i].ts != i64::MIN {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && frames[i].ts == i64::MIN {
            i += 1;
        }
        // frames[start-1] is known (the first frame of a block always is)
        let Some(prev) = start.checked_sub(1) else { continue };
        let t0 = frames[prev].ts;
        let steps = (i - prev) as i64;
        let t1 = if i < n {
            frames[i].ts
        } else {
            // after the last block: by the default duration, else the rate before
            let step = if default_duration > 0 {
                default_duration as i64
            } else if prev >= 1 && frames[prev - 1].ts != i64::MIN {
                (t0 - frames[prev - 1].ts).max(1)
            } else {
                20_000_000
            };
            t0 + step * steps
        };
        for (k, f) in frames[start..i].iter_mut().enumerate() {
            f.ts = t0 + (t1 - t0) * (k as i64 + 1) / steps;
        }
    }
}

/// Matroska rounds every time to its tick (a millisecond, usually): 29.97
/// fps shows as 0, 33, 67, 100 ... ms. When the track states its frame
/// duration and every time is within that rounding of a whole number of
/// frames, the times go back onto the grid.
fn snap_to_frame_grid(frames: &mut [Frame], default_duration: u64, scale: u64) {
    if default_duration == 0 || frames.is_empty() {
        return;
    }
    let dd = default_duration as f64;
    let t0 = frames.iter().map(|f| f.ts).min().unwrap();
    let slack = scale as f64 / 2.0 + 1000.0;
    let on_grid = |f: &Frame| {
        let k = ((f.ts - t0) as f64 / dd).round();
        ((f.ts - t0) as f64 - k * dd).abs() <= slack
    };
    if !frames.iter().all(on_grid) {
        return;
    }
    for f in frames.iter_mut() {
        let k = ((f.ts - t0) as f64 / dd).round();
        f.ts = t0 + (k * dd).round() as i64;
    }
}

/// Frames per second from the default duration, else the median gap.
fn frame_rate(frames: &[Frame], default_duration: u64) -> f64 {
    if default_duration > 0 {
        return 1e9 / default_duration as f64;
    }
    let mut ts: Vec<i64> = frames.iter().map(|f| f.ts).collect();
    ts.sort_unstable();
    let mut gaps: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).filter(|&g| g > 0).collect();
    if gaps.is_empty() {
        return 30.0;
    }
    gaps.sort_unstable();
    1e9 / gaps[gaps.len() / 2] as f64
}

/// Matroska stores presentation times; decode times are the sorted
/// presentation times moved back by the largest reordering, each lasting
/// to the next (as an MP4's and a transport stream's do), the last the
/// stated frame duration or else the usual gap.
fn video_timing(samples: &mut [Sample], nominal: u32) {
    let pts: Vec<i64> = samples.iter().map(|s| s.pts).collect();
    let mut sorted = pts.clone();
    sorted.sort_unstable();
    let mut gaps: Vec<i64> = sorted.windows(2).map(|w| w[1] - w[0]).filter(|&g| g > 0).collect();
    gaps.sort_unstable();
    let last = if nominal > 0 { nominal } else { gaps.get(gaps.len() / 2).map_or(1, |&g| g.min(u32::MAX as i64) as u32) };
    for (s, (dts, duration)) in samples.iter_mut().zip(dts_from_cts(&pts, last)) {
        s.dts = dts;
        s.duration = duration;
    }
}

/// Audio on the sampling rate's clock. Matroska's times are rounded to
/// its tick (a millisecond, usually); where the codec fixes how many
/// samples a packet holds, the times follow that count exactly and only
/// jump where the file's own times do (a gap in the audio).
fn audio_timing(samples: &mut [Sample], frames: &[Frame], s: &Setup, nominal: u32) {
    let n = samples.len();
    if n == 0 {
        return;
    }
    let length = |i: usize| -> Option<u32> {
        match s.packet {
            PacketLength::Fixed(k) => Some(k),
            PacketLength::PerFrame => Some(frames[i].samples).filter(|&k| k > 0),
            PacketLength::Pcm(bytes) if bytes > 0 => Some(frames[i].size / bytes),
            _ => None,
        }
    };
    let tol = (s.timescale as i64 * 3 / 1000).max(1);
    for i in 1..n {
        if let Some(k) = length(i - 1) {
            let expected = samples[i - 1].pts + k as i64;
            if (samples[i].pts - expected).abs() <= tol {
                samples[i].pts = expected;
            }
        }
    }
    for i in 0..n {
        let d = if i + 1 < n {
            samples[i + 1].pts - samples[i].pts
        } else {
            length(i).map(|k| k as i64).unwrap_or(if nominal > 0 { nominal as i64 } else if n > 1 { samples[n - 1].pts - samples[n - 2].pts } else { 1 })
        };
        samples[i].duration = d.max(1) as u32;
        samples[i].dts = samples[i].pts;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (not read: a block's duration says nothing the times don't)
    const BLOCK_DURATION: u32 = 0x9B;

    /// A tiny EBML writer for building test files.
    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let idb = id.to_be_bytes();
        let skip = idb.iter().position(|&b| b != 0).unwrap_or(3);
        v.extend_from_slice(&idb[skip..]);
        let n = body.len() as u64;
        // eight-byte sizes throughout: simple and valid
        v.push(0x01);
        v.extend_from_slice(&n.to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }
    fn el_unknown(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let idb = id.to_be_bytes();
        let skip = idb.iter().position(|&b| b != 0).unwrap_or(3);
        v.extend_from_slice(&idb[skip..]);
        v.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        v.extend_from_slice(body);
        v
    }
    fn u(id: u32, v: u64) -> Vec<u8> {
        el(id, &v.to_be_bytes())
    }
    fn s(id: u32, v: &str) -> Vec<u8> {
        el(id, v.as_bytes())
    }
    fn f(id: u32, v: f64) -> Vec<u8> {
        el(id, &v.to_be_bytes())
    }

    fn header(doc: &str) -> Vec<u8> {
        el(EBML_HEADER, &s(DOC_TYPE, doc))
    }

    fn track_entry(number: u64, kind: u64, codec: &str, extra: &[u8]) -> Vec<u8> {
        el(TRACK_ENTRY, &[u(TRACK_NUMBER, number), u(TRACK_TYPE, kind), s(CODEC_ID, codec), extra.to_vec()].concat())
    }

    fn simple_block(track: u8, rel: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0x80 | track];
        b.extend_from_slice(&rel.to_be_bytes());
        b.push(flags);
        b.extend_from_slice(payload);
        el(SIMPLE_BLOCK, &b)
    }

    fn parse_all(data: &[u8], chunk: Option<u64>) -> Movie {
        let mut d = MkvDemuxer::with_chunk(data.len() as u64, chunk.unwrap_or(CHUNK));
        while let Some((off, len)) = d.need() {
            let end = (off + len).min(data.len() as u64);
            d.feed(off, &data[off as usize..end as usize]).unwrap();
        }
        assert!(d.is_done());
        d.into_movie().expect("a movie")
    }

    fn opus_track() -> Vec<u8> {
        let head = entry::make_opus_head(2, 312, 48000);
        track_entry(2, 2, "A_OPUS", &[el(CODEC_PRIVATE, &head), el(AUDIO, &[f(SAMPLING_FREQUENCY, 48000.0), u(CHANNELS, 2)].concat())].concat())
    }

    fn vp8_track() -> Vec<u8> {
        track_entry(1, 1, "V_VP8", &el(VIDEO, &[u(PIXEL_WIDTH, 64), u(PIXEL_HEIGHT, 48)].concat()))
    }

    #[test]
    fn laced_blocks_of_every_kind() {
        // three Opus packets (20 ms each: TOC 0xfc) per block, laced three ways
        let pk = |n: usize, fill: u8| -> Vec<u8> {
            let mut v = vec![0xfc];
            v.resize(n, fill);
            v
        };
        let (a, b, c) = (pk(300, 1), pk(20, 2), pk(7, 3));
        let xiph = [vec![2u8], vec![255, 45], vec![20], a.clone(), b.clone(), c.clone()].concat();
        // EBML lacing: 300 as a 2-byte vint (0x412C), then 20 - 300 = -280 as a signed 2-byte vint
        let diff = (-280i64 + 8191) as u16 | 0x4000;
        let ebml = [vec![2u8], vec![0x41, 0x2C], diff.to_be_bytes().to_vec(), a.clone(), b.clone(), c.clone()].concat();
        let fixed = [vec![2u8], pk(10, 4), pk(10, 5), pk(10, 6)].concat();
        let cluster = [
            u(CLUSTER_TIMESTAMP, 1000),
            simple_block(1, 0, 0x80, &[0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a]),
            simple_block(2, 0, 0x80 | 0x02, &xiph),
            simple_block(2, 60, 0x80 | 0x06, &ebml),
            simple_block(2, 120, 0x80 | 0x04, &fixed),
            simple_block(1, 40, 0x00, &[0x11, 0x02, 0x00]),
        ]
        .concat();
        let seg = [el(INFO, &u(TIMESTAMP_SCALE, 1_000_000)), el(TRACKS, &[vp8_track(), opus_track()].concat()), el(CLUSTER, &cluster)].concat();
        let file = [header("webm"), el(SEGMENT, &seg)].concat();
        for chunk in [None, Some(100), Some(7), Some(1)] {
            let m = parse_all(&file, chunk);
            assert_eq!(m.format, "webm");
            let v = m.video().unwrap();
            assert_eq!(v.codec, "vp8");
            assert_eq!(v.samples.len(), 2);
            assert!(v.samples[0].sync && !v.samples[1].sync);
            let at = m.audio().unwrap();
            assert_eq!(at.codec, "opus");
            assert_eq!(at.timescale, 48000);
            let sizes: Vec<u32> = at.samples.iter().map(|s| s.size).collect();
            assert_eq!(sizes, vec![300, 20, 7, 300, 20, 7, 10, 10, 10], "chunk {chunk:?}");
            // each packet's bytes where the table says
            for (smp, fill) in at.samples.iter().zip([1u8, 2, 3, 1, 2, 3, 4, 5, 6]) {
                let o = smp.offset as usize;
                assert_eq!(file[o], 0xfc);
                assert!(file[o + 1..o + smp.size as usize].iter().all(|&x| x == fill));
            }
            // 20 ms apart on the 48 kHz clock, from 1 s
            let pts: Vec<i64> = at.samples.iter().map(|s| s.pts).collect();
            assert_eq!(pts, (0..9).map(|i| 48000 + 960 * i).collect::<Vec<_>>());
            assert!(at.samples.iter().all(|s| s.duration == 960));
            assert!(!at.sample_entry.is_empty());
        }
    }

    #[test]
    fn unknown_sizes_block_groups_and_damage() {
        // a live recording: segment and clusters of unknown size, and a
        // block group whose frame is referenced (not a keyframe)
        let group = el(BLOCK_GROUP, &[el(BLOCK, &[0x81, 0x00, 0x21, 0x00, 0x11, 0x22]), u(REFERENCE_BLOCK, 1), u(BLOCK_DURATION, 33)].concat());
        let c1 = [u(CLUSTER_TIMESTAMP, 0), simple_block(1, 0, 0x80, &[1, 2, 3]), group].concat();
        let c2 = [u(CLUSTER_TIMESTAMP, 66), simple_block(1, 0, 0x80, &[4, 5, 6, 7])].concat();
        let seg = [el(INFO, &u(TIMESTAMP_SCALE, 1_000_000)), el(TRACKS, &vp8_track()), el_unknown(CLUSTER, &c1), vec![0xff, 0x00, 0x13, 0x37], el_unknown(CLUSTER, &c2)].concat();
        let file = [header("webm"), el_unknown(SEGMENT, &seg)].concat();
        for chunk in [None, Some(13), Some(1)] {
            let m = parse_all(&file, chunk);
            let v = m.video().unwrap();
            let got: Vec<(i64, bool, u32)> = v.samples.iter().map(|s| (s.pts, s.sync, s.size)).collect();
            assert_eq!(got, vec![(0, true, 3), (33_000, false, 2), (66_000, true, 4)], "chunk {chunk:?}");
            assert_eq!(v.timescale, 1_000_000);
        }
    }

    #[test]
    fn a_damaged_cluster_timestamp_is_skipped() {
        // the second cluster's timestamp has the "unknown" size, which only a
        // master element can have: damage, passed over to the next cluster
        let c1 = [u(CLUSTER_TIMESTAMP, 0), simple_block(1, 0, 0x80, &[1, 2, 3])].concat();
        let c2 = [vec![0xe7, 0xff, 0x21], simple_block(1, 0, 0x80, &[4, 5])].concat();
        let c3 = [u(CLUSTER_TIMESTAMP, 80), simple_block(1, 0, 0x80, &[6, 7, 8, 9])].concat();
        let seg = [el(INFO, &u(TIMESTAMP_SCALE, 1_000_000)), el(TRACKS, &vp8_track()), el(CLUSTER, &c1), el(CLUSTER, &c2), el(CLUSTER, &c3)].concat();
        let file = [header("webm"), el(SEGMENT, &seg)].concat();
        for chunk in [None, Some(13), Some(1)] {
            let m = parse_all(&file, chunk);
            let got: Vec<(i64, u32)> = m.video().unwrap().samples.iter().map(|s| (s.pts, s.size)).collect();
            assert_eq!(got, vec![(0, 3), (80_000, 4)], "chunk {chunk:?}");
        }
    }

    #[test]
    fn b_frames_get_decode_times() {
        // I0 P3 B1 B2 in decode order (presentation 0, 100, 33, 66 ms)
        let c = [u(CLUSTER_TIMESTAMP, 0), simple_block(1, 0, 0x80, &[0]), simple_block(1, 100, 0, &[1]), simple_block(1, 33, 0, &[2]), simple_block(1, 66, 0, &[3])].concat();
        let seg = [el(INFO, &u(TIMESTAMP_SCALE, 1_000_000)), el(TRACKS, &vp8_track()), el(CLUSTER, &c)].concat();
        let file = [header("matroska"), el(SEGMENT, &seg)].concat();
        let m = parse_all(&file, None);
        let v = m.video().unwrap();
        let dts: Vec<i64> = v.samples.iter().map(|s| s.dts).collect();
        let pts: Vec<i64> = v.samples.iter().map(|s| s.pts).collect();
        assert_eq!(pts, vec![0, 100_000, 33_000, 66_000]);
        for (d, p) in dts.iter().zip(&pts) {
            assert!(d <= p);
        }
        assert!(dts.windows(2).all(|w| w[1] > w[0]));
        // each lasts to the next decode time; the last the usual gap
        let durs: Vec<u32> = v.samples.iter().map(|s| s.duration).collect();
        assert_eq!(durs, vec![33_000, 33_000, 34_000, 33_000]);
    }

    #[test]
    fn millisecond_times_go_back_onto_the_frame_grid() {
        // 30000/1001 fps: frames 33.3667 ms apart, stored rounded to the millisecond
        let dd = 1_000_000_000u64 * 1001 / 30000;
        let blocks: Vec<Vec<u8>> = (0..40).map(|i| simple_block(1, ((i as u64 * dd + 500_000) / 1_000_000) as i16, if i == 0 { 0x80 } else { 0 }, &[i as u8])).collect();
        let c = [u(CLUSTER_TIMESTAMP, 0), blocks.concat()].concat();
        let track = track_entry(1, 1, "V_VP8", &[u(DEFAULT_DURATION, dd), el(VIDEO, &[u(PIXEL_WIDTH, 64), u(PIXEL_HEIGHT, 48)].concat())].concat());
        let seg = [el(TRACKS, &track), el(CLUSTER, &c)].concat();
        let m = parse_all(&[header("matroska"), el(SEGMENT, &seg)].concat(), None);
        let v = m.video().unwrap();
        assert_eq!(v.timescale, 1_000_000);
        assert_eq!(v.frame_duration, 33_367);
        for (i, smp) in v.samples.iter().enumerate() {
            assert_eq!(smp.pts, ((i as u64 * dd + 500) / 1000) as i64, "frame {i}");
        }
        // without the stated duration the times stay as stored
        let track = track_entry(1, 1, "V_VP8", &el(VIDEO, &[u(PIXEL_WIDTH, 64), u(PIXEL_HEIGHT, 48)].concat()));
        let seg = [el(TRACKS, &track), el(CLUSTER, &c)].concat();
        let m = parse_all(&[header("matroska"), el(SEGMENT, &seg)].concat(), None);
        assert_eq!(m.video().unwrap().samples[1].pts, 33_000);
    }

    #[test]
    fn header_stripping_is_put_back_for_the_setup() {
        // an AC-3 track whose frames lost their 0B 77 sync word to header stripping
        let enc = el(CONTENT_ENCODINGS, &el(CONTENT_ENCODING, &el(CONTENT_COMPRESSION, &[u(CONTENT_COMP_ALGO, 3), el(CONTENT_COMP_SETTINGS, &[0x0b, 0x77])].concat())));
        let ac3 = track_entry(2, 2, "A_AC3", &[el(AUDIO, &[f(SAMPLING_FREQUENCY, 48000.0), u(CHANNELS, 6)].concat()), enc].concat());
        let frame = [0u8, 0, 0x1c, 0x40, 0xe1, 0xf0, 0, 0];
        let c = [u(CLUSTER_TIMESTAMP, 0), simple_block(1, 0, 0x80, &[9]), simple_block(2, 0, 0x80, &frame), simple_block(2, 32, 0x80, &frame)].concat();
        let seg = [el(TRACKS, &[vp8_track(), ac3].concat()), el(CLUSTER, &c)].concat();
        let m = parse_all(&[header("matroska"), el(SEGMENT, &seg)].concat(), None);
        let a = m.audio().unwrap();
        assert_eq!(a.codec, "ac-3");
        assert_eq!(a.prefix, vec![0x0b, 0x77]);
        assert_eq!((a.sample_rate, a.channels, a.timescale), (48000, 6, 48000));
        assert_eq!(a.samples.iter().map(|s| (s.pts, s.duration)).collect::<Vec<_>>(), vec![(0, 1536), (1536, 1536)]);
        assert_eq!(&a.sample_entry[4..8], b"ac-3");
    }

    #[test]
    fn not_matroska() {
        let mut d = MkvDemuxer::new(40);
        let (off, len) = d.need().unwrap();
        let data = [header("dvd"), vec![0; 20]].concat();
        let e = d.feed(off, &data[..len as usize]).unwrap_err();
        assert!(e.contains("dvd"), "{e}");
    }

    #[test]
    fn vp9_setup_from_a_key_frame() {
        // frame marker 2, profile 0, show_existing 0, key frame, shown, not resilient, sync code
        let h = [0b1000_0010, 0x49, 0x83, 0x42, 0x00];
        assert_eq!(vp9_frame_info(&h), Some((0, 8)));
        // profile 2 (low 0, high 1), 10-bit
        let h = [0b1001_0010, 0x49, 0x83, 0x42, 0x00];
        assert_eq!(vp9_frame_info(&h), Some((2, 10)));
        assert_eq!(vp9_level(1920, 1080, 30.0), 40);
        assert_eq!(vp9_level(3840, 2160, 60.0), 51);
        assert_eq!(vp9_level(64, 48, 30.0), 10);
    }
}
