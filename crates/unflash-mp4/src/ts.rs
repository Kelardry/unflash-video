//! MPEG transport streams (.ts, .m2ts, .mts): the same [`Movie`] the other
//! readers produce, its samples shaped as an MP4's are.
//!
//! A transport stream keeps no index, and cuts each of its streams into the
//! payloads of 188-byte packets interleaved with the others' (192 bytes in
//! Blu-ray's and AVCHD's .m2ts and .mts, a timecode first; 204 with DVB's
//! parity after). So the file is read front to back, as a Matroska file is,
//! and each stream's packets are followed: its PES headers give the times,
//! its own framing the samples. H.264 and HEVC come as Annex B byte streams:
//! a sample is an access unit (the two fields of a frame coded as fields go
//! together, as in an MP4), its size the size it has as an MP4 sample (each
//! NAL unit after a 4-byte length instead of a start code), and the decoder
//! setup (avcC, hvcC) is built from the parameter sets in the stream. Sound
//! is cut into its frames: AAC (ADTS; the sample leaves the header out),
//! AC-3, E-AC-3 (a frame with its dependent substreams), MPEG audio, DTS
//! (a core frame with the DTS-HD extension after it) and Blu-ray LPCM (the
//! header left out).
//!
//! A sample's bytes are spread over packets, so its `offset` is not a file
//! offset but a place: [`TS_BASE`] + track × [`TS_TRACK`] + 256 × the file
//! offset of the packet (its sync byte) that holds the sample's first byte +
//! that byte's index in the packet. A reader starts there and follows
//! the stream's packets (PES headers skipped) for `size` bytes: as they
//! are for sound, NAL units given lengths instead of start codes for video
//! ([`read_sample`]; the app's is web/ts.js).
//!
//! Times are the PES headers' (90 kHz), unwrapped where the 33-bit clock
//! wraps, a jump of the clock (a recording across a discontinuity) closed
//! up, and counted from the file's first.

use std::collections::BTreeMap;

use crate::annexb::{self, AvcSps, HevcSps};
use crate::codec::{avc_codec, hevc_codec};
use crate::demux::{pcm_codec, rescale, Movie, PcmForm, Sample, Track, TrackKind};
use crate::entry;
use crate::mux::write_video_entry;
use crate::reader::{Bits, Writer};
use crate::Error;

/// What to do with video this reader can't hand to a decoder.
const CONVERT: &str = "Convert it first, for example with HandBrake or `ffmpeg -i input -c:v libx264 -c:a copy output.mkv`.";

/// Where the places of samples begin: above any file offset.
const TS_BASE: u64 = 1 << 52;
/// The span of places of one track.
const TS_TRACK: u64 = 1 << 47;
/// Bytes asked for at a time.
const CHUNK: u64 = 4 << 20;
const SYNC: u8 = 0x47;
/// The bytes of a packet from its sync byte: what is read of it.
const TS: u64 = 188;
const WRAP: i64 = 1 << 33;
const CLOCK: i64 = 90_000;

/// Whether a file's first bytes are a transport stream's: its packet size
/// and where its first sync byte is.
pub(crate) fn sniff(b: &[u8]) -> Option<(u64, u64)> {
    for size in [188usize, 192, 204] {
        for first in 0..size {
            if first + 3 * size >= b.len() {
                break;
            }
            if (0..4).all(|k| b[first + k * size] == SYNC) {
                return Some((size as u64, first as u64));
            }
        }
    }
    None
}

/// A packet's header, as far as following a stream goes: its PID, whether a
/// PES (or a section) starts in it, and where its payload starts (188 when
/// it has none).
fn packet_head(p: &[u8]) -> (u16, bool, usize) {
    let pid = ((p[1] as u16 & 0x1f) << 8) | p[2] as u16;
    let pusi = p[1] & 0x40 != 0;
    let afc = (p[3] >> 4) & 3;
    let start = if afc & 2 != 0 { 5 + p[4] as usize } else { 4 };
    (pid, pusi, if afc & 1 == 0 || start > 188 { 188 } else { start })
}

/// The next place a packet can start after `pos` lost its sync byte: a sync
/// byte with another a packet after it (or the file's end there). None:
/// can't tell from `data` (which holds the file from `offset`).
fn resync(data: &[u8], offset: u64, pos: u64, packet: u64, file_size: u64) -> Result<u64, u64> {
    let end = offset + data.len() as u64;
    let mut q = pos + 1;
    while q < end {
        if data[(q - offset) as usize] == SYNC {
            let next = q + packet;
            if next >= file_size {
                return Ok(q);
            }
            if next >= end {
                return Err(q);
            }
            if data[(next - offset) as usize] == SYNC {
                return Ok(q);
            }
        }
        q += 1;
    }
    Err(end)
}

// ---- PES headers ------------------------------------------------------------------

/// A PES header being read (it may span packets): the header so far, then
/// how many more of its bytes to pass.
#[derive(Default)]
struct PesHead {
    buf: Vec<u8>,
    /// In a header.
    active: bool,
    /// A header that isn't one: the PES is passed over.
    bad: bool,
}

/// What a PES header gave: its times (33-bit, as written).
#[derive(Clone, Copy, Default)]
struct PesTimes {
    pts: Option<i64>,
    dts: Option<i64>,
}

/// Streams whose PES header has no optional part (padding, private stream
/// 2 and the like).
fn optionless(id: u8) -> bool {
    matches!(id, 0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff)
}

fn pes_time(b: &[u8]) -> i64 {
    (((b[0] as i64 >> 1) & 7) << 30) | ((b[1] as i64) << 22) | ((b[2] as i64 >> 1) << 15) | ((b[3] as i64) << 7) | (b[4] as i64 >> 1)
}

impl PesHead {
    fn begin(&mut self) {
        self.buf.clear();
        self.active = true;
        self.bad = false;
    }
    /// Take header bytes from the front of `b`: how many were header, and
    /// the times once the header is whole.
    fn take(&mut self, b: &[u8]) -> (usize, Option<PesTimes>) {
        let mut used = 0;
        while self.active && used < b.len() {
            let n = (self.need() - self.buf.len()).min(b.len() - used);
            self.buf.extend_from_slice(&b[used..used + n]);
            used += n;
            let h = &self.buf;
            if h.len() >= 3 && h[..3] != [0, 0, 1] {
                self.active = false;
                self.bad = true;
                return (b.len(), None);
            }
            if h.len() >= 4 && h.len() == self.need() && (optionless(h[3]) || h.len() >= 9) {
                self.active = false;
                return (used, Some(self.times()));
            }
        }
        (used, None)
    }
    /// The header's length as far as the bytes so far tell: 4 to know the
    /// stream, 9 to know the rest's length, then the whole.
    fn need(&self) -> usize {
        let b = &self.buf;
        if b.len() < 4 {
            4
        } else if optionless(b[3]) {
            6
        } else if b.len() < 9 {
            9
        } else {
            9 + b[8] as usize
        }
    }
    fn times(&self) -> PesTimes {
        let b = &self.buf;
        if b.len() < 9 || optionless(b[3]) {
            return PesTimes::default();
        }
        let flags = b[7] >> 6;
        let pts = (flags & 2 != 0 && b.len() >= 14).then(|| pes_time(&b[9..14]));
        let dts = (flags == 3 && b.len() >= 19).then(|| pes_time(&b[14..19]));
        PesTimes { pts, dts }
    }
}

// ---- video ------------------------------------------------------------------------

/// An access unit: a sample to be.
#[derive(Clone, Debug)]
struct Au {
    place: u64,
    size: u64,
    pts: Option<i64>,
    dts: Option<i64>,
    sync: bool,
    vcl: bool,
    /// Its first slice, where that is a field: (bottom, frame_num).
    field: Option<(bool, u32)>,
    /// A field that the next one completed.
    paired: bool,
}

/// Head bytes of a NAL unit kept to read it by; the whole of a parameter
/// set or an SEI (up to KEEP_ALL).
const HEAD: usize = 32;
const KEEP_ALL: usize = 1 << 16;

#[derive(Default)]
struct VideoEs {
    hevc: bool,
    zeros: u32,
    in_nal: bool,
    /// The next byte begins a NAL unit.
    mark: bool,
    nal_place: u64,
    nal_len: u64,
    nal: Vec<u8>,
    decided: bool,
    times: Option<PesTimes>,
    cur: Option<Au>,
    aus: Vec<Au>,
    avc_sps: BTreeMap<u32, (Vec<u8>, AvcSps)>,
    avc_pps: BTreeMap<u32, (Vec<u8>, u32)>,
    hevc_vps: Vec<Vec<u8>>,
    hevc_sps: Vec<(Vec<u8>, HevcSps)>,
    hevc_pps: Vec<Vec<u8>>,
}

/// The index of the first zero byte in `b`, 8 bytes at a time.
fn find_zero(b: &[u8]) -> usize {
    let mut i = 0;
    while i + 8 <= b.len() {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if (w.wrapping_sub(0x0101_0101_0101_0101) & !w & 0x8080_8080_8080_8080) != 0 {
            break;
        }
        i += 8;
    }
    while i < b.len() && b[i] != 0 {
        i += 1;
    }
    i
}

impl VideoEs {
    fn new(hevc: bool) -> Self {
        VideoEs { hevc, ..Default::default() }
    }

    fn pes_start(&mut self, t: PesTimes) {
        if t.pts.is_some() {
            self.times = Some(t);
        }
    }

    fn feed(&mut self, b: &[u8], place0: u64) {
        let mut i = 0;
        while i < b.len() {
            if self.mark {
                self.nal_place = place0 + i as u64;
                self.mark = false;
            }
            // most of a NAL unit: no zero byte, nothing to keep
            if self.in_nal && self.zeros == 0 && self.decided && self.nal.len() >= self.keep() {
                let k = find_zero(&b[i..]);
                self.nal_len += k as u64;
                i += k;
                if i >= b.len() {
                    break;
                }
            }
            let c = b[i];
            if c == 0 {
                self.zeros += 1;
            } else if c == 1 && self.zeros >= 2 {
                self.start_code();
                self.zeros = 0;
            } else {
                if self.in_nal {
                    self.nal_byte(c);
                }
                self.zeros = 0;
            }
            i += 1;
        }
    }

    /// The bytes of the NAL unit in hand worth keeping.
    fn keep(&self) -> usize {
        match self.nal_type() {
            Some(t) if self.whole(t) => KEEP_ALL,
            _ => HEAD,
        }
    }

    fn whole(&self, t: u8) -> bool {
        if self.hevc {
            matches!(t, 32..=34)
        } else {
            matches!(t, 6..=8)
        }
    }

    fn nal_type(&self) -> Option<u8> {
        let h = *self.nal.first()?;
        Some(if self.hevc { (h >> 1) & 0x3f } else { h & 0x1f })
    }

    fn nal_byte(&mut self, c: u8) {
        // the zeros before it are the NAL unit's own
        self.nal_len += self.zeros as u64 + 1;
        let keep = self.keep();
        for _ in 0..self.zeros {
            if self.nal.len() < keep {
                self.nal.push(0);
            }
        }
        if self.nal.len() < keep {
            self.nal.push(c);
        }
        if !self.decided && self.nal.len() >= if self.hevc { 3 } else { 2 } {
            self.decide();
        }
    }

    fn start_code(&mut self) {
        self.end_nal();
        self.in_nal = true;
        self.mark = true;
        self.nal_len = 0;
        self.nal.clear();
        self.decided = false;
    }

    /// Whether the NAL unit in hand begins an access unit, and if so, the
    /// access unit it begins.
    fn decide(&mut self) {
        self.decided = true;
        let Some(t) = self.nal_type() else { return };
        let (vcl, first_slice, starter) = if self.hevc {
            (t <= 31, self.nal.get(2).map(|b| b & 0x80 != 0).unwrap_or(false), matches!(t, 32..=35 | 39 | 41..=44 | 48..=55))
        } else {
            (matches!(t, 1..=5), self.nal.get(1).map(|b| b & 0x80 != 0).unwrap_or(false), matches!(t, 6..=9 | 14..=18))
        };
        let cur_vcl = self.cur.as_ref().map(|a| a.vcl);
        let begins = match cur_vcl {
            None => true,
            Some(had_vcl) => had_vcl && (starter || (vcl && first_slice)),
        };
        if begins {
            self.finish_au();
            let times = self.times.take().unwrap_or_default();
            self.cur = Some(Au { place: self.nal_place, size: 0, pts: times.pts, dts: times.dts.or(times.pts), sync: false, vcl: false, field: None, paired: false });
        }
    }

    fn end_nal(&mut self) {
        if !self.in_nal {
            return;
        }
        self.in_nal = false;
        if !self.decided {
            self.decide();
        }
        let Some(t) = self.nal_type() else { return };
        let nal = std::mem::take(&mut self.nal);
        if let Some(au) = self.cur.as_mut() {
            au.size += 4 + self.nal_len;
        }
        if self.hevc {
            match t {
                0..=31 => {
                    let au = self.cur.as_mut().unwrap();
                    au.vcl = true;
                    if (16..=23).contains(&t) {
                        au.sync = true;
                    }
                }
                32 if self.hevc_vps.is_empty() => self.hevc_vps.push(nal),
                33 if self.hevc_sps.is_empty() => {
                    if let Some(s) = annexb::parse_hevc_sps(&nal) {
                        self.hevc_sps.push((nal, s));
                    }
                }
                34 if self.hevc_pps.is_empty() => self.hevc_pps.push(nal),
                _ => {}
            }
            return;
        }
        match t {
            1..=5 => {
                let sps = &self.avc_sps;
                let pps = &self.avc_pps;
                let slice = annexb::avc_slice(&nal, |id| pps.get(&id).and_then(|(_, s)| sps.get(s)).map(|(_, s)| s.clone()));
                let au = self.cur.as_mut().unwrap();
                if t == 5 {
                    au.sync = true;
                }
                if !au.vcl {
                    au.field = slice.and_then(|s| s.field.map(|b| (b, s.frame_num)));
                }
                au.vcl = true;
            }
            6 => {
                if annexb::avc_sei_recovery_point(&nal) {
                    if let Some(au) = self.cur.as_mut() {
                        au.sync = true;
                    }
                }
            }
            7 => {
                if let Some(s) = annexb::parse_avc_sps(&nal) {
                    self.avc_sps.entry(s.id).or_insert((nal, s));
                }
            }
            8 => {
                if let Some((p, s)) = annexb::avc_pps_ids(&nal) {
                    self.avc_pps.entry(p).or_insert((nal, s));
                }
            }
            _ => {}
        }
    }

    /// The access unit in hand, done: kept (with the field it completes,
    /// as one sample) if it has a picture.
    fn finish_au(&mut self) {
        let Some(au) = self.cur.take() else { return };
        if !au.vcl {
            return;
        }
        if let (Some((bottom, num)), Some(prev)) = (au.field, self.aus.last_mut()) {
            if let Some((prev_bottom, prev_num)) = prev.field {
                if !prev.paired && prev_bottom != bottom && prev_num == num {
                    prev.size += au.size;
                    prev.paired = true;
                    return;
                }
            }
        }
        self.aus.push(au);
    }

    fn end(&mut self) {
        self.end_nal();
        self.finish_au();
    }
}

// ---- sound ------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sound {
    Adts,
    /// AC-3 and E-AC-3.
    Dolby,
    Mpa,
    Dts,
    /// Blu-ray's LPCM: a header and samples in every PES.
    Lpcm,
}

/// A frame's header, read: its length, the bytes before the sample's
/// (ADTS and LPCM headers, left out of it), its samples, whether it goes
/// with the frame before (E-AC-3's dependent substreams, DTS-HD's
/// extension), its rate and channels.
#[derive(Clone, Copy, Debug)]
struct FrameHead {
    total: u32,
    skip: u32,
    samples: u32,
    merge: bool,
    rate: u32,
    channels: u32,
}

/// A frame of sound: a sample to be. Its time is `anchor` (a PES's, 90 kHz)
/// and `since` samples after it.
#[derive(Clone, Copy, Debug)]
struct Frame {
    place: u64,
    size: u32,
    anchor: i64,
    since: u64,
    samples: u32,
}

impl Sound {
    /// Header bytes to read a frame by.
    fn head_len(self) -> usize {
        match self {
            Sound::Adts => 7,
            Sound::Dolby => 8,
            Sound::Mpa => 4,
            Sound::Dts => 11,
            Sound::Lpcm => 4,
        }
    }

    fn sync_start(self, c: u8) -> bool {
        match self {
            Sound::Adts | Sound::Mpa => c == 0xff,
            Sound::Dolby => c == 0x0b,
            Sound::Dts => c == 0x7f || c == 0x64,
            Sound::Lpcm => false,
        }
    }

    /// Whether the header bytes so far still look like a frame's.
    fn so_far(self, h: &[u8]) -> bool {
        let n = h.len();
        match self {
            Sound::Adts => n < 2 || h[1] & 0xf6 == 0xf0,
            Sound::Mpa => n < 2 || h[1] & 0xe0 == 0xe0,
            Sound::Dolby => n < 2 || h[1] == 0x77,
            Sound::Dts => {
                let core = [0x7f, 0xfe, 0x80, 0x01];
                let ext = [0x64, 0x58, 0x20, 0x25];
                let k = n.min(4);
                h[..k] == core[..k] || h[..k] == ext[..k]
            }
            Sound::Lpcm => true,
        }
    }

    fn parse(self, h: &[u8]) -> Option<FrameHead> {
        match self {
            Sound::Adts => {
                let crc = h[1] & 1 == 0;
                let total = ((h[3] as u32 & 3) << 11) | ((h[4] as u32) << 3) | (h[5] as u32 >> 5);
                let skip = if crc { 9 } else { 7 };
                let rate = *entry::AAC_RATES.get(((h[2] >> 2) & 0xf) as usize)?;
                let channels = (((h[2] & 1) << 2) | (h[3] >> 6)) as u32;
                let blocks = (h[6] & 3) as u32 + 1;
                (total > skip).then_some(FrameHead { total, skip, samples: 1024 * blocks, merge: false, rate, channels: if channels == 0 { 2 } else if channels == 7 { 8 } else { channels } })
            }
            Sound::Dolby => entry::dolby_frame(h).map(|d| FrameHead { total: d.bytes, skip: 0, samples: d.samples, merge: d.follows, rate: d.rate, channels: d.channels }),
            Sound::Mpa => {
                let (rate, channels, samples) = entry::mpeg_audio_frame(h)?;
                let version = (h[1] >> 3) & 3;
                let layer = (h[1] >> 1) & 3;
                let bi = (h[2] >> 4) as usize;
                let pad = ((h[2] >> 1) & 1) as u32;
                const L1: [u32; 15] = [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
                const L2: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
                const L3: [u32; 15] = [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
                const LSF1: [u32; 15] = [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
                const LSF: [u32; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
                let kbps = match (version == 3, layer) {
                    (true, 3) => L1,
                    (true, 2) => L2,
                    (true, _) => L3,
                    (false, 3) => LSF1,
                    (false, _) => LSF,
                }
                .get(bi)
                .copied()
                .filter(|&k| k > 0)?;
                let total = match layer {
                    3 => (12 * kbps * 1000 / rate + pad) * 4,
                    1 if version != 3 => 72 * kbps * 1000 / rate + pad,
                    _ => 144 * kbps * 1000 / rate + pad,
                };
                (total > 4).then_some(FrameHead { total, skip: 0, samples, merge: false, rate, channels })
            }
            Sound::Dts => {
                if h[0] == 0x64 {
                    // a DTS-HD extension substream: with the core frame before it
                    // (its size after the sync word, user data, index and a flag
                    // for the long fields, and the header's size)
                    let mut r = Bits::new(h);
                    r.skip(42)?;
                    let long = r.flag()?;
                    r.skip(if long { 12 } else { 8 })?;
                    let total = r.u(if long { 20 } else { 16 })? + 1;
                    return Some(FrameHead { total, skip: 0, samples: 0, merge: true, rate: 0, channels: 0 });
                }
                let (rate, channels, samples, total) = entry::dts_frame(h)?;
                Some(FrameHead { total, skip: 0, samples, merge: false, rate, channels })
            }
            Sound::Lpcm => {
                let size = u16::from_be_bytes([h[0], h[1]]) as u32;
                let channels = [0u32, 1, 0, 2, 3, 3, 4, 4, 5, 6, 7, 8].get((h[2] >> 4) as usize).copied().filter(|&c| c > 0)?;
                let rate = match h[2] & 0xf {
                    1 => 48000,
                    4 => 96000,
                    5 => 192000,
                    _ => return None,
                };
                let bytes = match h[3] >> 6 {
                    1 => 2,
                    2 | 3 => 3,
                    _ => return None,
                };
                // (an odd number of channels is stored with one more)
                let stored = channels + (channels & 1);
                Some(FrameHead { total: size + 4, skip: 4, samples: size / (bytes * stored), merge: false, rate, channels: stored })
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SoundState {
    Scan,
    Head,
    Body,
}

struct SoundEs {
    kind: Sound,
    state: SoundState,
    head: [u8; 16],
    head_place: [u64; 16],
    head_len: usize,
    /// The frame's header, once read (its CRC may follow).
    parsed: Option<FrameHead>,
    /// Bytes of the frame still to pass.
    left: u64,
    /// The next byte begins the sample (after an ADTS or LPCM header).
    mark: bool,
    /// The time a PES gave that no frame has taken, and the one the frame
    /// in hand took.
    pending: Option<i64>,
    took: Option<i64>,
    anchor: Option<i64>,
    since: u64,
    frames: Vec<Frame>,
    /// What the first frame said.
    first: Option<(FrameHead, [u8; 16])>,
    /// E-AC-3 seen (else AC-3).
    eac3: bool,
}

impl SoundEs {
    fn new(kind: Sound) -> Self {
        SoundEs {
            kind,
            state: SoundState::Scan,
            head: [0; 16],
            head_place: [0; 16],
            head_len: 0,
            parsed: None,
            left: 0,
            mark: false,
            pending: None,
            took: None,
            anchor: None,
            since: 0,
            frames: Vec::new(),
            first: None,
            eac3: false,
        }
    }

    fn pes_start(&mut self, t: PesTimes) {
        if let Some(p) = t.pts {
            self.pending = Some(p);
        }
        if self.kind == Sound::Lpcm {
            // each PES: a header, then its samples
            self.state = SoundState::Head;
            self.head_len = 0;
            self.parsed = None;
            self.mark = false;
            self.took = self.pending.take();
        }
    }

    fn feed(&mut self, b: &[u8], place0: u64) {
        let mut i = 0;
        while i < b.len() {
            match self.state {
                SoundState::Body => {
                    if self.mark {
                        self.mark = false;
                        self.new_sample(place0 + i as u64);
                    }
                    let n = (self.left.min((b.len() - i) as u64)) as usize;
                    self.left -= n as u64;
                    i += n;
                    if self.left == 0 {
                        self.state = SoundState::Scan;
                    }
                }
                SoundState::Scan => {
                    if self.kind.sync_start(b[i]) {
                        self.head_len = 0;
                        self.parsed = None;
                        self.took = self.pending.take();
                        self.state = SoundState::Head;
                        continue;
                    }
                    i += 1;
                }
                SoundState::Head => {
                    self.head_byte(b[i], place0 + i as u64);
                    i += 1;
                }
            }
        }
    }

    fn head_byte(&mut self, c: u8, place: u64) {
        let n = self.head_len;
        self.head[n] = c;
        self.head_place[n] = place;
        self.head_len += 1;
        if !self.kind.so_far(&self.head[..self.head_len]) {
            return self.reject();
        }
        if self.parsed.is_none() && self.head_len >= self.kind.head_len() {
            match self.kind.parse(&self.head[..self.head_len]) {
                Some(f) => self.parsed = Some(f),
                None => return self.reject(),
            }
        }
        let Some(f) = self.parsed else { return };
        // (an ADTS header with a CRC is two bytes longer)
        if (self.head_len as u32) < f.skip {
            return;
        }
        if (f.total as usize) < self.head_len {
            return self.reject();
        }
        self.left = f.total as u64 - self.head_len as u64;
        self.state = if self.left > 0 { SoundState::Body } else { SoundState::Scan };
        if f.merge {
            // with the frame before: its size grows; the time goes back
            if let Some(last) = self.frames.last_mut() {
                last.size += f.total;
            }
            self.pending = self.pending.or(self.took.take());
            return;
        }
        if f.skip > 0 {
            // the sample starts after the header
            self.mark = true;
            if self.left == 0 {
                self.mark = false;
            }
        } else {
            self.new_sample(self.head_place[0]);
        }
    }

    /// The frame whose header is in hand starts its sample at `place`.
    fn new_sample(&mut self, place: u64) {
        let Some(f) = self.parsed else { return };
        if self.first.is_none() {
            self.first = Some((f, self.head));
        }
        if self.kind == Sound::Dolby && self.head[5] >> 3 > 10 {
            self.eac3 = true;
        }
        if let Some(t) = self.took.take() {
            self.anchor = Some(t);
            self.since = 0;
        }
        // (sound before the first time can't be placed)
        let Some(anchor) = self.anchor else { return };
        self.frames.push(Frame { place, size: f.total - f.skip, anchor, since: self.since, samples: f.samples });
        self.since += f.samples as u64;
    }

    /// Not a frame after all: look again from the byte after the first.
    fn reject(&mut self) {
        let head = self.head;
        let places = self.head_place;
        let n = self.head_len;
        self.pending = self.pending.or(self.took.take());
        self.head_len = 0;
        self.parsed = None;
        self.state = SoundState::Scan;
        if self.kind == Sound::Lpcm {
            return;
        }
        for k in 1..n {
            match self.state {
                SoundState::Scan => {
                    if self.kind.sync_start(head[k]) {
                        self.took = self.pending.take();
                        self.state = SoundState::Head;
                        self.head_byte(head[k], places[k]);
                    }
                }
                SoundState::Head => self.head_byte(head[k], places[k]),
                SoundState::Body => {
                    // (a frame can't be shorter than its header: not reached)
                    self.left = self.left.saturating_sub(1);
                }
            }
        }
    }
}

// ---- streams ----------------------------------------------------------------------

enum Es {
    Video(Box<VideoEs>),
    Sound(Box<SoundEs>),
    /// A stream this reader doesn't read: its kind, and why.
    Unread(TrackKind, String),
}

struct Stream {
    pid: u16,
    stream_type: u8,
    language: String,
    /// A codec string the descriptors settle (DTS's kind).
    codec: String,
    pes: PesHead,
    started: bool,
    es: Es,
    /// The last time seen, unwrapped: what the next is unwrapped near.
    last: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    None,
    Pat,
    Pmt,
    Es(usize),
}

/// The transport stream reader behind [`crate::Demuxer`]: `need()` ->
/// read that range -> `feed()`, until `movie()` is `Some`.
pub(crate) struct TsDemuxer {
    file_size: u64,
    packet: u64,
    /// The next packet's sync byte.
    at: u64,
    want: Option<(u64, u64)>,
    /// A sync byte found after a loss, whose next packet's is still to be seen.
    unconfirmed: Option<u64>,
    slots: Vec<Slot>,
    sections: BTreeMap<u16, Vec<u8>>,
    streams: Vec<Stream>,
    /// Blu-ray's (HDMV): stream type 0x80 is its LPCM.
    hdmv: bool,
    /// The file's first time: what a stream's first is unwrapped near.
    first_time: Option<i64>,
    movie: Option<Movie>,
}

impl TsDemuxer {
    /// A stream of `packet`-byte packets whose first sync byte is at `first`.
    pub fn new(file_size: u64, packet: u64, first: u64) -> Self {
        let mut slots = vec![Slot::None; 8192];
        slots[0] = Slot::Pat;
        let mut d = TsDemuxer {
            file_size,
            packet,
            at: first,
            want: None,
            unconfirmed: None,
            slots,
            sections: BTreeMap::new(),
            streams: Vec::new(),
            hdmv: packet == 192,
            first_time: None,
            movie: None,
        };
        d.ask();
        d
    }

    fn ask(&mut self) {
        self.want = (self.at + TS <= self.file_size).then(|| (self.at, CHUNK.max(self.packet * 2).min(self.file_size - self.at)));
    }

    pub fn need(&self) -> Option<(u64, u64)> {
        self.want
    }

    pub fn is_done(&self) -> bool {
        self.movie.is_some()
    }

    pub fn progress(&self) -> f64 {
        if self.movie.is_some() {
            1.0
        } else {
            self.at as f64 / self.file_size.max(1) as f64
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
        let end = offset + data.len() as u64;
        let mut pos = self.at;
        while pos + TS <= end {
            let i = (pos - offset) as usize;
            let mut lost = data[i] != SYNC;
            if !lost && self.unconfirmed == Some(pos) {
                // a sync byte found after a loss: its next packet's has to be one too
                let next = pos + self.packet;
                if next < self.file_size {
                    if next >= end {
                        break;
                    }
                    lost = data[(next - offset) as usize] != SYNC;
                }
                self.unconfirmed = None;
            }
            if lost {
                match resync(data, offset, pos, self.packet, self.file_size) {
                    Ok(q) => pos = q,
                    Err(q) => {
                        pos = q;
                        self.unconfirmed = Some(q);
                        break;
                    }
                }
                continue;
            }
            self.packet(pos, &data[i..i + TS as usize]);
            pos += self.packet;
        }
        self.at = pos;
        self.ask();
        if self.want.is_none() {
            self.finish()?;
        }
        Ok(())
    }

    fn packet(&mut self, pos: u64, p: &[u8]) {
        let (pid, pusi, start) = packet_head(p);
        match self.slots[pid as usize] {
            Slot::None => {}
            Slot::Pat | Slot::Pmt => {
                if start < 188 {
                    self.section(pid, pusi, &p[start..]);
                }
            }
            Slot::Es(k) => {
                if start < 188 {
                    self.es_payload(k, pusi, &p[start..], pos * 256 + start as u64);
                }
            }
        }
    }

    fn es_payload(&mut self, k: usize, pusi: bool, payload: &[u8], place: u64) {
        let s = &mut self.streams[k];
        let mut b = payload;
        let mut place = place;
        if pusi {
            s.pes.begin();
            s.started = true;
        }
        if !s.started {
            return;
        }
        if s.pes.active {
            let (used, times) = s.pes.take(b);
            b = &b[used..];
            place += used as u64;
            if let Some(t) = times {
                let t = PesTimes { pts: t.pts.map(|v| unwrap(&mut s.last, &mut self.first_time, v)), dts: t.dts.map(|v| unwrap(&mut s.last, &mut self.first_time, v)) };
                match &mut s.es {
                    Es::Video(v) => v.pes_start(t),
                    Es::Sound(a) => a.pes_start(t),
                    Es::Unread(..) => {}
                }
            }
        }
        if s.pes.bad || s.pes.active || b.is_empty() {
            return;
        }
        match &mut s.es {
            Es::Video(v) => v.feed(b, place),
            Es::Sound(a) => a.feed(b, place),
            Es::Unread(..) => {}
        }
    }

    fn section(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
        let mut b = payload;
        if pusi {
            let pointer = b[0] as usize;
            if 1 + pointer > b.len() {
                return;
            }
            // (the end of the section before, then the new one)
            if let Some(buf) = self.sections.get_mut(&pid) {
                buf.extend_from_slice(&b[1..1 + pointer]);
            }
            self.section_done(pid);
            self.sections.insert(pid, Vec::new());
            b = &b[1 + pointer..];
        }
        if let Some(buf) = self.sections.get_mut(&pid) {
            buf.extend_from_slice(b);
        }
        self.section_done(pid);
    }

    fn section_done(&mut self, pid: u16) {
        let Some(buf) = self.sections.get(&pid) else { return };
        if buf.len() < 3 {
            return;
        }
        let len = ((buf[1] as usize & 0x0f) << 8 | buf[2] as usize) + 3;
        if buf.len() < len {
            return;
        }
        let sec = self.sections.remove(&pid).unwrap();
        let sec = &sec[..len];
        if len < 12 {
            return;
        }
        match (sec[0], self.slots[pid as usize]) {
            (0x00, Slot::Pat) => {
                for e in sec[8..len - 4].chunks_exact(4) {
                    let program = u16::from_be_bytes([e[0], e[1]]);
                    let pmt = ((e[2] as u16 & 0x1f) << 8) | e[3] as u16;
                    if program != 0 && self.slots[pmt as usize] == Slot::None {
                        self.slots[pmt as usize] = Slot::Pmt;
                    }
                }
            }
            (0x02, Slot::Pmt) => self.pmt(sec),
            _ => {}
        }
    }

    fn pmt(&mut self, sec: &[u8]) {
        let len = sec.len();
        let info_len = (sec[10] as usize & 0x0f) << 8 | sec[11] as usize;
        let mut i = 12 + info_len;
        if i > len - 4 {
            return;
        }
        for (tag, body) in descriptors(&sec[12..i]) {
            if tag == 0x05 && body.starts_with(b"HDMV") {
                self.hdmv = true;
            }
        }
        while i + 5 <= len - 4 {
            let stream_type = sec[i];
            let pid = ((sec[i + 1] as u16 & 0x1f) << 8) | sec[i + 2] as u16;
            let es_len = (sec[i + 3] as usize & 0x0f) << 8 | sec[i + 4] as usize;
            let desc = &sec[(i + 5).min(len - 4)..(i + 5 + es_len).min(len - 4)];
            i += 5 + es_len;
            if self.slots[pid as usize] != Slot::None {
                continue;
            }
            let Some(s) = self.stream(pid, stream_type, desc) else { continue };
            self.slots[pid as usize] = Slot::Es(self.streams.len());
            self.streams.push(s);
        }
    }

    /// A stream of the PMT, from its type and descriptors.
    fn stream(&self, pid: u16, stream_type: u8, desc: &[u8]) -> Option<Stream> {
        let mut language = String::new();
        let mut registration = [0u8; 4];
        let mut tags = Vec::new();
        for (tag, body) in descriptors(desc) {
            tags.push(tag);
            if tag == 0x0a && body.len() >= 3 {
                language = String::from_utf8_lossy(&body[..3]).trim_end_matches('\0').to_string();
            }
            if tag == 0x05 && body.len() >= 4 {
                registration.copy_from_slice(&body[..4]);
            }
        }
        let video = |note: &str| Es::Unread(TrackKind::Video, note.to_string());
        let sound = |note: &str| Es::Unread(TrackKind::Audio, note.to_string());
        let mut codec = String::new();
        let es = match stream_type {
            0x1b => Es::Video(Box::new(VideoEs::new(false))),
            0x24 => Es::Video(Box::new(VideoEs::new(true))),
            0x01 | 0x02 => video(&format!("MPEG-2 video (as TV recordings and DVDs have), which browsers don't decode and Unflash doesn't either. {CONVERT}")),
            0x10 => video(&format!("MPEG-4 Part 2 video, which browsers don't decode. {CONVERT}")),
            0xea => video(&format!("VC-1 video, which browsers don't decode. {CONVERT}")),
            0x0f => Es::Sound(Box::new(SoundEs::new(Sound::Adts))),
            0x11 => sound("AAC in LATM"),
            0x03 | 0x04 => Es::Sound(Box::new(SoundEs::new(Sound::Mpa))),
            0x81 | 0x87 | 0x84 | 0xa1 => Es::Sound(Box::new(SoundEs::new(Sound::Dolby))),
            // (0x85 and 0x86 are DTS-HD on Blu-ray only; elsewhere 0x86 is a cable operator's cue table)
            0x82 | 0x8a => {
                codec = "dtsc".into();
                Es::Sound(Box::new(SoundEs::new(Sound::Dts)))
            }
            0x85 | 0x86 | 0xa2 if self.hdmv => {
                codec = match stream_type {
                    0x85 | 0xa2 => "dtsh",
                    0x86 => "dtsl",
                    _ => "dtsc",
                }
                .into();
                Es::Sound(Box::new(SoundEs::new(Sound::Dts)))
            }
            0x80 if self.hdmv => Es::Sound(Box::new(SoundEs::new(Sound::Lpcm))),
            0x83 if self.hdmv => sound("Dolby TrueHD"),
            0x06 => {
                if tags.contains(&0x6a) || &registration == b"AC-3" || tags.contains(&0x7a) || &registration == b"EAC3" {
                    Es::Sound(Box::new(SoundEs::new(Sound::Dolby)))
                } else if tags.contains(&0x7b) || registration.starts_with(b"DTS") {
                    codec = "dtsc".into();
                    Es::Sound(Box::new(SoundEs::new(Sound::Dts)))
                } else if &registration == b"Opus" {
                    sound("Opus in a transport stream")
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        Some(Stream { pid, stream_type, language, codec, pes: PesHead::default(), started: false, es, last: None })
    }

    fn finish(&mut self) -> Result<(), Error> {
        for s in &mut self.streams {
            match &mut s.es {
                Es::Video(v) => v.end(),
                Es::Sound(a) => {
                    // a frame cut off by the file's end is left out
                    if a.state == SoundState::Body && a.left > 0 {
                        if let Some(f) = a.frames.last() {
                            if !a.parsed.map(|p| p.merge).unwrap_or(false) && f.place >= a.head_place[0] {
                                a.frames.pop();
                            }
                        }
                    }
                }
                Es::Unread(..) => {}
            }
        }
        if self.streams.is_empty() {
            return Err(if self.slots.contains(&Slot::Pmt) { "this transport stream has no video or sound this reader knows".into() } else { "no program found in this transport stream (no PAT/PMT)".into() });
        }
        // the file's start: the earliest time of a sample (video's with the
        // clock's jumps closed up, as its samples' times will be)
        let mut start = i64::MAX;
        for s in &self.streams {
            match &s.es {
                Es::Video(v) => {
                    let (aus, dts, offsets, _) = video_times(v, 0);
                    // (a stream that gives no time has none to say)
                    if aus.iter().any(|a| a.dts.is_some()) {
                        if let Some(p) = dts.iter().zip(&offsets).map(|(d, o)| d + o).min() {
                            start = start.min(p);
                        }
                    }
                }
                Es::Sound(a) => {
                    if let Some(f) = a.frames.first() {
                        start = start.min(f.anchor);
                    }
                }
                Es::Unread(..) => {}
            }
        }
        if start == i64::MAX {
            start = 0;
        }
        let mut order: Vec<usize> = (0..self.streams.len()).collect();
        let rank = |s: &Stream| match &s.es {
            Es::Video(_) => 0,
            Es::Unread(TrackKind::Video, _) => 1,
            Es::Sound(_) => 2,
            _ => 3,
        };
        order.sort_by_key(|&k| rank(&self.streams[k]));
        let mut movie = Movie { format: "mpegts".into(), packet_size: self.packet as u32, ..Default::default() };
        for (n, k) in order.into_iter().enumerate() {
            let base = TS_BASE + (n as u64).min(31) * TS_TRACK;
            let s = &self.streams[k];
            let track = match &s.es {
                Es::Video(v) => video_track(s, v, base, start),
                Es::Sound(a) => sound_track(s, a, base, start),
                Es::Unread(kind, note) => blank_track(s, *kind, note),
            };
            movie.tracks.push(track);
        }
        self.movie = Some(movie);
        Ok(())
    }
}

/// `raw` (33 bits) as the time nearest the stream's last (the file's first,
/// for a stream's first).
fn unwrap(last: &mut Option<i64>, first: &mut Option<i64>, raw: i64) -> i64 {
    let near = last.unwrap_or_else(|| *first.get_or_insert(raw));
    let v = raw + (near - raw + WRAP / 2).div_euclid(WRAP) * WRAP;
    *last = Some(v);
    v
}

/// (tag, body) of each descriptor in `b`.
fn descriptors(b: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 <= b.len() {
        let n = b[i + 1] as usize;
        let body = &b[i + 2..(i + 2 + n).min(b.len())];
        out.push((b[i], body));
        i += 2 + n;
    }
    out
}

/// Close up jumps of the clock in `times` (a stream's, in order): a step
/// back, or forward by more than `gap`, continues from where the time
/// before left off (`step` after it) instead.
fn close_jumps(times: &mut [i64], step: &[i64], gap: i64) {
    let mut shift = 0;
    for i in 1..times.len() {
        let t = times[i] + shift;
        let expect = times[i - 1] + step[i - 1];
        if t < times[i - 1] - CLOCK || t > expect + gap {
            shift += expect - (times[i] + shift);
        }
        times[i] += shift;
    }
}

fn median(v: &mut [i64]) -> i64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn blank_track(s: &Stream, kind: TrackKind, note: &str) -> Track {
    Track { id: s.pid as u32, kind, codec: format!("stream type {:#04x}", s.stream_type), timescale: CLOCK as u32, language: s.language.clone(), note: note.to_string(), ..Default::default() }
}

/// A video stream's pictures from the first that decoding can start at;
/// their decode times, in order (a missing one a step after the one before,
/// or for the first the first time there is, else `none`; jumps of the
/// clock closed up); how much later each is shown; and the step (the usual
/// time between pictures).
fn video_times(v: &VideoEs, none: i64) -> (Vec<&Au>, Vec<i64>, Vec<i64>, i64) {
    let aus: Vec<&Au> = v.aus.iter().skip_while(|a| !a.sync).filter(|a| a.size > 0).collect();
    let mut deltas: Vec<i64> = aus.windows(2).filter_map(|w| Some(w[1].dts? - w[0].dts?)).filter(|&d| d > 0).collect();
    let step = median(&mut deltas).max(1);
    let first = aus.iter().find_map(|a| a.dts).unwrap_or(none);
    let mut dts: Vec<i64> = Vec::with_capacity(aus.len());
    for a in &aus {
        let d = a.dts.unwrap_or_else(|| dts.last().map(|p| p + step).unwrap_or(first));
        dts.push(d);
    }
    let offsets: Vec<i64> = aus.iter().map(|a| a.pts.map(|p| p - a.dts.unwrap_or(p)).unwrap_or(0)).collect();
    let steps = vec![step; dts.len()];
    close_jumps(&mut dts, &steps, 10 * CLOCK);
    (aus, dts, offsets, step)
}

fn video_track(s: &Stream, v: &VideoEs, base: u64, start: i64) -> Track {
    let mut t = blank_track(s, TrackKind::Video, "");
    // decoding starts at a picture that can be decoded on its own
    let (aus, dts, offsets, step) = video_times(v, start);
    let setup = if v.hevc {
        v.hevc_sps.first().map(|(sps, info)| {
            let vps: Vec<&[u8]> = v.hevc_vps.iter().map(|x| &x[..]).collect();
            let pps: Vec<&[u8]> = v.hevc_pps.iter().map(|x| &x[..]).collect();
            let rec = annexb::hvcc(&vps, &[&sps[..]], &pps, info);
            (hevc_codec(&rec, "hev1").map(|c| c.codec), rec, info.width, info.height)
        })
    } else {
        v.avc_sps.values().next().map(|(sps, info)| {
            let pps: Vec<&[u8]> = v.avc_pps.values().filter(|(_, s)| *s == info.id).map(|(p, _)| &p[..]).collect();
            let rec = annexb::avcc(&[&sps[..]], &pps, info);
            (avc_codec(&rec).map(|c| c.codec), rec, info.width, info.height)
        })
    };
    let Some((Ok(codec), rec, width, height)) = setup else {
        t.note = format!("{} without the parameter sets it needs", if v.hevc { "HEVC" } else { "H.264" });
        return t;
    };
    if aus.is_empty() {
        t.note = "no picture that decoding can start at".into();
    }
    t.codec = codec;
    t.fourcc = t.codec[..4].to_string();
    t.width = width;
    t.height = height;
    let mut w = Writer::new();
    if write_video_entry(&mut w, &t.codec, width, height, &rec).is_ok() {
        t.sample_entry = w.buf;
    }
    t.description = Some(rec);
    for (i, a) in aus.iter().enumerate() {
        let d = dts[i] - start;
        let duration = if i + 1 < dts.len() { (dts[i + 1] - dts[i]).clamp(1, u32::MAX as i64) } else { step };
        t.samples.push(Sample { offset: base + a.place, size: a.size.min(u32::MAX as u64) as u32, dts: d, pts: d + offsets[i], duration: duration as u32, sync: a.sync });
    }
    t
}

fn sound_track(s: &Stream, a: &SoundEs, base: u64, start: i64) -> Track {
    let mut t = blank_track(s, TrackKind::Audio, "");
    let Some((f, head)) = a.first else {
        t.note = "no sound frames found".into();
        return t;
    };
    let rate = f.rate.max(1);
    t.sample_rate = rate;
    t.channels = f.channels.max(1);
    t.timescale = rate;
    t.frame_duration = f.samples;
    match a.kind {
        Sound::Adts => {
            let aot = (head[2] >> 6) + 1;
            let asc = entry::make_asc(aot, rate, t.channels, None);
            t.codec = format!("mp4a.40.{aot}");
            t.fourcc = "mp4a".into();
            t.sample_entry = entry::aac_entry(&asc, rate, t.channels);
            t.description = Some(asc);
        }
        Sound::Dolby => {
            if a.eac3 {
                t.codec = "ec-3".into();
                if let Some((e, ..)) = entry::eac3_entry(&head) {
                    t.sample_entry = e;
                }
            } else {
                t.codec = "ac-3".into();
                if let Some((e, ..)) = entry::ac3_entry(&head) {
                    t.sample_entry = e;
                }
            }
            t.fourcc = t.codec.clone();
        }
        Sound::Mpa => {
            let layer = (head[1] >> 1) & 3;
            t.codec = if layer == 1 { "mp3".into() } else { "mp4a.6B".into() };
            t.fourcc = "mp4a".into();
            t.sample_entry = entry::mpeg_audio_entry(rate, t.channels);
        }
        Sound::Dts => {
            t.codec = if s.codec.is_empty() { "dtsc".into() } else { s.codec.clone() };
            t.fourcc = t.codec.clone();
        }
        Sound::Lpcm => {
            t.codec = pcm_codec(PcmForm { bits: if head[3] >> 6 == 1 { 16 } else { 24 }, float: false, le: false, signed: true }).unwrap_or_default().into();
        }
    }
    // times: a PES's time and the samples since, in the track's own clock
    let mut ticks: Vec<i64> = a.frames.iter().map(|f| rescale(f.anchor - start, CLOCK, rate as i64) + f.since as i64).collect();
    let steps: Vec<i64> = a.frames.iter().map(|f| f.samples as i64).collect();
    close_jumps(&mut ticks, &steps, 10 * rate as i64);
    for (i, f) in a.frames.iter().enumerate() {
        if f.size == 0 {
            continue;
        }
        t.samples.push(Sample { offset: base + f.place, size: f.size, dts: ticks[i], pts: ticks[i], duration: f.samples.max(1), sync: true });
    }
    t
}

// ---- reading a sample -----------------------------------------------------------

/// Annex B turned into lengths as it comes: `size` bytes of it make the
/// sample (a NAL unit's end is where the next start code begins, its
/// trailing zeros left out).
struct Lengths {
    out: Vec<u8>,
    nal: Vec<u8>,
    zeros: usize,
    size: usize,
}

impl Lengths {
    fn push(&mut self, b: &[u8]) -> Result<bool, Error> {
        for &c in b {
            if c == 0 {
                self.zeros += 1;
            } else if c == 1 && self.zeros >= 2 {
                self.zeros = 0;
                if self.end()? {
                    return Ok(true);
                }
            } else {
                self.nal.extend(std::iter::repeat_n(0, self.zeros));
                self.nal.push(c);
                self.zeros = 0;
            }
        }
        Ok(false)
    }
    /// The NAL unit in hand ends: whether the sample is whole. (An empty
    /// one, two start codes together, is no NAL unit.)
    fn end(&mut self) -> Result<bool, Error> {
        if self.nal.is_empty() {
            return Ok(self.out.len() == self.size);
        }
        self.out.extend_from_slice(&(self.nal.len() as u32).to_be_bytes());
        self.out.append(&mut self.nal);
        if self.out.len() > self.size {
            return Err(format!("a sample came out {} bytes long, not {}", self.out.len(), self.size));
        }
        Ok(self.out.len() == self.size)
    }
}

/// The bytes of the sample at `place` (`size` of them), out of the whole
/// file `file`, as a reader of the app does it: from the packet and byte
/// the place names, the stream `pid`'s packets followed (PES headers
/// passed over, a lost sync byte found again as the demuxer finds it);
/// with `annexb`, NAL units after lengths instead of start codes.
pub fn read_sample(file: &[u8], packet: u64, pid: u16, annexb: bool, place: u64, size: u32) -> Result<Vec<u8>, Error> {
    let place = (place - TS_BASE) % TS_TRACK;
    let mut pos = place >> 8;
    let mut from = (place & 255) as usize;
    let size = size as usize;
    let mut raw = Vec::new();
    let mut nals = Lengths { out: Vec::with_capacity(size), nal: Vec::new(), zeros: 0, size };
    let mut pes = PesHead::default();
    let mut first = true;
    let file_size = file.len() as u64;
    while size > 0 && pos + TS <= file_size {
        if file[pos as usize] != SYNC {
            match resync(file, 0, pos, packet, file_size) {
                Ok(q) => {
                    pos = q;
                    continue;
                }
                Err(_) => break,
            }
        }
        let p = &file[pos as usize..(pos + TS) as usize];
        let (id, pusi, start) = packet_head(p);
        if id == pid && start < 188 {
            let mut b = &p[start..];
            if first {
                b = &p[from.max(start)..];
                first = false;
            } else {
                if pusi {
                    pes.begin();
                }
                if pes.active {
                    let (used, _) = pes.take(b);
                    b = &b[used..];
                }
                if pes.bad || pes.active {
                    b = &[];
                }
            }
            if annexb {
                if nals.push(b)? {
                    return Ok(nals.out);
                }
            } else {
                let n = (size - raw.len()).min(b.len());
                raw.extend_from_slice(&b[..n]);
                if raw.len() == size {
                    return Ok(raw);
                }
            }
        }
        from = 0;
        pos += packet;
    }
    if annexb && nals.end()? {
        return Ok(nals.out);
    }
    Err(format!("the file ended {} bytes into a sample of {size}", if annexb { nals.out.len() } else { raw.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport stream writer for the tests: each PES cut into packets,
    /// the last one filled out by an adaptation field.
    struct Mux {
        out: Vec<u8>,
        packet: usize,
        cc: [u8; 8192],
    }

    impl Mux {
        fn new(packet: usize) -> Self {
            Mux { out: Vec::new(), packet, cc: [0; 8192] }
        }
        fn raw(&mut self, pid: u16, pusi: bool, payload: &[u8]) {
            assert!(payload.len() <= 184);
            if self.packet == 192 {
                self.out.extend_from_slice(&[0, 0, 0, 0]);
            }
            let stuff = 184 - payload.len();
            let cc = self.cc[pid as usize];
            self.cc[pid as usize] = (cc + 1) & 15;
            self.out.extend_from_slice(&[SYNC, (pusi as u8) << 6 | (pid >> 8) as u8, pid as u8, (if stuff > 0 { 0x30 } else { 0x10 }) | cc]);
            if stuff > 0 {
                self.out.push((stuff - 1) as u8);
                if stuff > 1 {
                    self.out.push(0);
                    self.out.extend(std::iter::repeat_n(0xff, stuff - 2));
                }
            }
            self.out.extend_from_slice(payload);
        }
        fn section(&mut self, pid: u16, body: &[u8]) {
            let mut p = vec![0u8];
            p.extend_from_slice(body);
            p.extend_from_slice(&[0, 0, 0, 0]); // (the CRC isn't checked)
            self.raw(pid, true, &p);
        }
        fn tables(&mut self, streams: &[(u8, u16)]) {
            self.section(0, &[0x00, 0xb0, 13, 0, 1, 0xc1, 0, 0, 0, 1, 0xf0, 0x00]);
            let mut pmt = vec![0x02, 0xb0, 0, 0, 1, 0xc1, 0, 0, 0xe1, 0x00, 0xf0, 0x00];
            for &(t, pid) in streams {
                pmt.extend_from_slice(&[t, 0xe0 | (pid >> 8) as u8, pid as u8, 0xf0, 0]);
            }
            pmt[2] = (pmt.len() - 3 + 4) as u8;
            self.section(0x1000, &pmt);
        }
        fn pes(&mut self, pid: u16, stream_id: u8, pts: Option<i64>, data: &[u8], first: usize) {
            let mut p = vec![0, 0, 1, stream_id, 0, 0, 0x80];
            match pts {
                Some(t) => {
                    p.extend_from_slice(&[0x80, 5]);
                    p.extend_from_slice(&[0x21 | ((t >> 29) & 0x0e) as u8, (t >> 22) as u8, 0x01 | ((t >> 14) & 0xfe) as u8, (t >> 7) as u8, 0x01 | ((t << 1) & 0xfe) as u8]);
                }
                None => p.extend_from_slice(&[0, 0]),
            }
            p.extend_from_slice(data);
            // the first packet holds `first` bytes, the rest are full
            let mut at = 0;
            let mut pusi = true;
            while at < p.len() {
                let n = if pusi { first.min(184) } else { 184 }.min(p.len() - at);
                let chunk = p[at..at + n].to_vec();
                self.raw(pid, pusi, &chunk);
                pusi = false;
                at += n;
            }
        }
    }

    fn parse(file: &[u8], chunk: Option<u64>) -> Movie {
        let (packet, first) = sniff(file).expect("a transport stream");
        let mut d = TsDemuxer::new(file.len() as u64, packet, first);
        while let Some((off, len)) = d.need() {
            let len = chunk.map(|c| c.min(len)).unwrap_or(len);
            d.want = Some((off, len));
            d.feed(off, &file[off as usize..(off + len) as usize]).unwrap();
        }
        d.into_movie().unwrap()
    }

    /// An H.264 access unit: AUD, (SPS, PPS,) a slice of `n` bytes.
    fn au(idr: bool, n: usize, seed: u8) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1, 0x09, 0xf0];
        if idr {
            v.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x28, 0xda, 0x01, 0xe0, 0x08, 0x9f, 0x97, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x2a]);
            v.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xce, 0x0f, 0xc8]);
        }
        v.extend_from_slice(&[0, 0, 1, if idr { 0x65 } else { 0x41 }, 0x88]);
        // a slice body with zeros in it, never three
        v.extend((0..n).map(|i| if i % 7 == 3 { 0 } else { seed.wrapping_add(i as u8) | 2 }));
        v
    }

    fn lengths(annexb: &[u8]) -> Vec<u8> {
        let mut l = Lengths { out: Vec::new(), nal: Vec::new(), zeros: 0, size: usize::MAX };
        let mut b = annexb;
        // (the reader starts at a NAL unit's first byte)
        while b.len() >= 3 && b[..3] != [0, 0, 1] {
            b = &b[1..];
        }
        l.push(&b[3..]).unwrap();
        l.end().unwrap();
        l.out
    }

    #[test]
    fn video_and_sound_come_back_whole() {
        for packet in [188, 192] {
            let mut m = Mux::new(packet);
            m.tables(&[(0x1b, 0x100), (0x0f, 0x101)]);
            let mut aus = Vec::new();
            let mut frames = Vec::new();
            for k in 0..12i64 {
                let a = au(k % 6 == 0, 150 + 97 * k as usize, k as u8);
                // the PES header takes a varying share of its first packet
                m.pes(0x100, 0xe0, Some(126_000 + k * 3600), &a, 20 + 13 * k as usize);
                aus.push(a);
                // two ADTS frames a PES, the second's header split over packets
                let mut pes = Vec::new();
                for j in 0..2 {
                    let body: Vec<u8> = (0..(90 + 11 * j + k as usize)).map(|i| (i as u8) ^ 0x5a).collect();
                    let len = body.len() + 7;
                    pes.extend_from_slice(&[0xff, 0xf1, 0x50, 0x80 | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1f, 0xfc]);
                    pes.extend_from_slice(&body);
                    frames.push(body);
                }
                m.pes(0x101, 0xc0, Some(126_000 + k * 3840), &pes, 60);
            }
            let file = m.out.clone();
            for chunk in [None, Some(1000), Some(packet as u64 * 3 + 7)] {
                let movie = parse(&file, chunk);
                assert_eq!(movie.format, "mpegts");
                let v = &movie.tracks[0];
                assert_eq!((v.kind, v.codec.as_str(), v.width, v.height), (TrackKind::Video, "avc1.42C028", 1920, 1080));
                assert_eq!(v.samples.len(), 12, "{chunk:?}");
                for (s, a) in v.samples.iter().zip(&aus) {
                    let got = read_sample(&file, packet as u64, 0x100, true, s.offset, s.size).unwrap();
                    assert_eq!(got, lengths(a));
                }
                assert!(v.samples[0].sync && v.samples[6].sync && !v.samples[1].sync);
                assert_eq!(v.samples.iter().map(|s| s.pts).collect::<Vec<_>>(), (0..12).map(|k| k * 3600).collect::<Vec<_>>());
                let a = &movie.tracks[1];
                assert_eq!((a.codec.as_str(), a.sample_rate, a.channels), ("mp4a.40.2", 44100, 2));
                assert_eq!(a.samples.len(), 24);
                for (s, f) in a.samples.iter().zip(&frames) {
                    assert_eq!(&read_sample(&file, packet as u64, 0x101, false, s.offset, s.size).unwrap(), f);
                }
                // a PES's time for its first frame (rounded to the rate's clock), the next 1024 samples on
                assert_eq!(a.samples[2].pts - a.samples[0].pts, (3840 * 44100 + 45000) / 90000);
                assert_eq!(a.samples[1].pts - a.samples[0].pts, 1024);
            }
        }
    }

    #[test]
    fn lost_sync_is_found_again() {
        let mut m = Mux::new(188);
        m.tables(&[(0x1b, 0x100)]);
        let mut aus = Vec::new();
        for k in 0..6i64 {
            let a = au(k == 0, 900, k as u8);
            m.pes(0x100, 0xe0, Some(k * 3000), &a, 100);
            aus.push(a);
            if k == 2 {
                // garbage between packets, with a false sync byte in it
                m.out.extend_from_slice(&[1, 2, SYNC, 4, 5]);
            }
        }
        let file = m.out;
        let movie = parse(&file, Some(1500));
        let v = &movie.tracks[0];
        assert_eq!(v.samples.len(), 6);
        for (s, a) in v.samples.iter().zip(&aus) {
            assert_eq!(read_sample(&file, 188, 0x100, true, s.offset, s.size).unwrap(), lengths(a));
        }
    }

    #[test]
    fn the_clock_wraps() {
        let mut m = Mux::new(188);
        m.tables(&[(0x1b, 0x100)]);
        for k in 0..4i64 {
            m.pes(0x100, 0xe0, Some((WRAP - 7200 + k * 3600) % WRAP), &au(k == 0, 300, 1), 100);
        }
        let movie = parse(&m.out, None);
        assert_eq!(movie.tracks[0].samples.iter().map(|s| s.pts).collect::<Vec<_>>(), vec![0, 3600, 7200, 10800]);
    }

    #[test]
    fn the_clock_steps_back() {
        // a recording across a discontinuity: the clock starts again from 0
        // after two pictures (closed up, the pictures follow on; the file
        // starts at the first, not hours in at the least time it gives)
        let mut m = Mux::new(188);
        m.tables(&[(0x1b, 0x100)]);
        for (k, t) in [500_000_000i64, 500_003_600, 0, 3600].into_iter().enumerate() {
            m.pes(0x100, 0xe0, Some(t), &au(k == 0, 300, 1), 100);
        }
        let movie = parse(&m.out, None);
        assert_eq!(movie.tracks[0].samples.iter().map(|s| s.pts).collect::<Vec<_>>(), vec![0, 3600, 7200, 10800]);
    }

    #[test]
    fn dolby_frames_and_dependent_substreams() {
        // AC-3, 48 kHz, 5.1 at 448 kbit/s (1792 bytes), as two PES of three frames
        let mut m = Mux::new(188);
        m.tables(&[(0x81, 0x102), (0x87, 0x103)]);
        let ac3: Vec<u8> = {
            let mut f = vec![0x0b, 0x77, 0, 0, 0x1e, 0x40, 0xe1, 0xf0];
            f.resize(1792, 0x33);
            f
        };
        for k in 0..2 {
            m.pes(0x102, 0xbd, Some(k * 8640), &ac3.repeat(3), 184);
        }
        // E-AC-3: an independent frame (6 blocks, 48 kHz) and its dependent substream, every time
        let ind = {
            let mut f = vec![0x0b, 0x77, 0x00, 0x7f, 0x3f, 0x80, 0, 0];
            f.resize(256, 0x11);
            f
        };
        let dep = {
            let mut f = vec![0x0b, 0x77, 0x40, 0x3f, 0x3f, 0x80, 0, 0];
            f.resize(128, 0x22);
            f
        };
        let unit = [ind.clone(), dep.clone()].concat();
        m.pes(0x103, 0xbd, Some(0), &unit.repeat(4), 184);
        let movie = parse(&m.out, None);
        let a = &movie.tracks[0];
        assert_eq!((a.codec.as_str(), a.channels, a.sample_rate, a.samples.len()), ("ac-3", 6, 48000, 6));
        assert!(a.samples.iter().all(|s| s.size == 1792 && s.duration == 1536));
        assert_eq!(a.samples[3].pts, 8640 * 48000 / 90000);
        let e = &movie.tracks[1];
        assert_eq!((e.codec.as_str(), e.samples.len()), ("ec-3", 4));
        for s in &e.samples {
            assert_eq!(s.size, 384);
            assert_eq!(read_sample(&m.out, 188, 0x103, false, s.offset, s.size).unwrap(), unit);
        }
    }

    #[test]
    fn video_it_cannot_read_is_named() {
        let mut m = Mux::new(188);
        m.tables(&[(0x02, 0x100), (0x04, 0x101)]);
        for k in 0..3 {
            m.pes(0x100, 0xe0, Some(k * 3600), &[0, 0, 1, 0xb3, 1, 2, 3], 100);
        }
        let movie = parse(&m.out, None);
        let v = &movie.tracks[0];
        assert_eq!(v.kind, TrackKind::Video);
        assert!(v.samples.is_empty() && v.note.starts_with("MPEG-2 video") && v.note.contains("HandBrake"), "{}", v.note);
        let a = &movie.tracks[1];
        assert!(a.samples.is_empty() && a.note == "no sound frames found");
    }

    #[test]
    fn field_pairs_go_together() {
        // an interlaced SPS (frame_mbs_only 0); a field's slice header:
        // first_mb 0, slice type, pps 0, frame_num (4 bits), field_pic 1, bottom
        let sps = [0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x04, 0x4f, 0xde, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x06, 0x43, 0xe2, 0xc5, 0xb2, 0xc0];
        let info = annexb::parse_avc_sps(&sps).unwrap();
        assert!(!info.frame_mbs_only);
        let bits = info.log2_max_frame_num as usize;
        let slice = |idr: bool, frame_num: u32, bottom: bool| -> Vec<u8> {
            // ue(0)=1, ue(7 or 5: I or P) , ue(0)=1, frame_num, 1, bottom
            let mut w: Vec<bool> = vec![true];
            let st = if idr { 7u32 } else { 5 };
            let v = st + 1;
            let n = 32 - v.leading_zeros() as usize;
            w.extend(std::iter::repeat_n(false, n - 1));
            w.extend((0..n).rev().map(|i| (v >> i) & 1 == 1));
            w.push(true);
            w.extend((0..bits).rev().map(|i| (frame_num >> i) & 1 == 1));
            w.push(true);
            w.push(bottom);
            w.push(true);
            while !w.len().is_multiple_of(8) {
                w.push(false);
            }
            let mut b = vec![0, 0, 1, if idr { 0x65 } else { 0x41 }];
            b.extend(w.chunks(8).map(|c| c.iter().fold(0u8, |a, &x| a << 1 | x as u8)));
            b.extend_from_slice(&[0x55; 40]);
            b
        };
        let mut m = Mux::new(188);
        m.tables(&[(0x1b, 0x100)]);
        let head = [vec![0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1], sps.to_vec(), vec![0, 0, 0, 1, 0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]].concat();
        let mut t = 0;
        for f in 0..3u32 {
            for bottom in [false, true] {
                let mut a = if f == 0 && !bottom { head.clone() } else { vec![0, 0, 0, 1, 0x09, 0xf0] };
                a.extend(slice(f == 0 && !bottom, f, bottom));
                m.pes(0x100, 0xe0, Some(t), &a, 184);
                t += 1800;
            }
        }
        let movie = parse(&m.out, None);
        let v = &movie.tracks[0];
        assert_eq!(v.samples.len(), 3, "each field pair one sample");
        assert_eq!(v.samples.iter().map(|s| s.pts).collect::<Vec<_>>(), vec![0, 3600, 7200]);
        assert!(v.samples[0].sync);
    }
}
