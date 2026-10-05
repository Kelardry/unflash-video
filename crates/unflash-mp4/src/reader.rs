//! Big-endian cursor and box iteration.

use crate::Error;

#[derive(Clone, Copy)]
pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
    pub fn need(&self, n: usize) -> Result<(), Error> {
        if self.remaining() < n {
            Err(format!("truncated box: need {n} bytes at {}, have {}", self.pos, self.remaining()))
        } else {
            Ok(())
        }
    }
    pub fn skip(&mut self, n: usize) -> Result<(), Error> {
        self.need(n)?;
        self.pos += n;
        Ok(())
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        self.need(n)?;
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.data[self.pos.min(self.data.len())..];
        self.pos = self.data.len();
        s
    }
    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, Error> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub fn u24(&mut self) -> Result<u32, Error> {
        let b = self.bytes(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn i32(&mut self) -> Result<i32, Error> {
        Ok(self.u32()? as i32)
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        let b = self.bytes(8)?;
        Ok(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
    pub fn i64(&mut self) -> Result<i64, Error> {
        Ok(self.u64()? as i64)
    }
    pub fn fourcc(&mut self) -> Result<[u8; 4], Error> {
        let b = self.bytes(4)?;
        Ok([b[0], b[1], b[2], b[3]])
    }
    /// version (1 byte) and flags (3 bytes) of a full box.
    pub fn version_flags(&mut self) -> Result<(u8, u32), Error> {
        let v = self.u8()?;
        let f = self.u24()?;
        Ok((v, f))
    }
}

/// A parsed box header.
#[derive(Clone, Copy, Debug)]
pub struct BoxHeader {
    pub kind: [u8; 4],
    /// Whole box size including the header; `None` means "to the end".
    pub size: Option<u64>,
    pub header_len: u64,
}

/// Parse a box header at the start of `data` (needs up to 32 bytes).
pub fn box_header(data: &[u8]) -> Result<BoxHeader, Error> {
    let mut r = Reader::new(data);
    let size32 = r.u32()?;
    let kind = r.fourcc()?;
    let mut header_len = 8u64;
    let size = match size32 {
        0 => None,
        1 => {
            header_len += 8;
            Some(r.u64()?)
        }
        s => Some(s as u64),
    };
    if &kind == b"uuid" {
        header_len += 16;
    }
    if let Some(s) = size {
        if s < header_len {
            return Err(format!("box {} has size {s} smaller than its header", fourcc_str(&kind)));
        }
    }
    Ok(BoxHeader { kind, size, header_len })
}

pub fn fourcc_str(k: &[u8; 4]) -> String {
    k.iter().map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' }).collect()
}

/// Iterate the child boxes of `data`, calling `f(kind, body, whole)`.
pub fn for_each_box<'a>(data: &'a [u8], mut f: impl FnMut([u8; 4], &'a [u8], &'a [u8]) -> Result<(), Error>) -> Result<(), Error> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let h = box_header(&data[pos..])?;
        // (compared as u64: a size near 4 GB would wrap a 32-bit usize)
        let rest = (data.len() - pos) as u64;
        let size = h.size.unwrap_or(rest);
        if size > rest {
            return Err(format!("box {} overruns its parent ({size} > {rest})", fourcc_str(&h.kind)));
        }
        if size < h.header_len {
            // (a box "to the end": box_header could not know its size)
            return Err(format!("box {} has size {size} smaller than its header", fourcc_str(&h.kind)));
        }
        let size = size as usize;
        let whole = &data[pos..pos + size];
        let body = &data[pos + h.header_len as usize..pos + size];
        f(h.kind, body, whole)?;
        pos += size;
    }
    Ok(())
}

/// The first child box of the given kind.
pub fn find_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut out = None;
    let _ = for_each_box(data, |k, body, _| {
        if out.is_none() && &k == kind {
            out = Some(body);
        }
        Ok(())
    });
    out
}

/// Exp-Golomb and fixed-length fields, most significant bit first.
pub struct Bits<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Bits<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Bits { b, at: 0 }
    }
    pub fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.at / 8)?;
        let v = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Some(v as u32)
    }
    pub fn u(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }
    pub fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.at += n;
        (self.at <= self.b.len() * 8).then_some(())
    }
    pub fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some(((1u64 << zeros) - 1 + self.u(zeros)? as u64) as u32)
    }
    pub fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        Some(if k & 1 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }
}

/// Write helpers for the muxer.
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u24(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes()[1..]);
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    pub fn zeros(&mut self, n: usize) {
        self.buf.resize(self.buf.len() + n, 0);
    }
    /// Open a box; returns the position to pass to `end_box`.
    pub fn begin_box(&mut self, kind: &[u8; 4]) -> usize {
        let at = self.buf.len();
        self.u32(0);
        self.bytes(kind);
        at
    }
    pub fn begin_full_box(&mut self, kind: &[u8; 4], version: u8, flags: u32) -> usize {
        let at = self.begin_box(kind);
        self.u8(version);
        self.u24(flags);
        at
    }
    pub fn end_box(&mut self, at: usize) {
        let size = (self.buf.len() - at) as u32;
        self.buf[at..at + 4].copy_from_slice(&size.to_be_bytes());
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boxes_that_overrun_their_parent() {
        let free = [0, 0, 0, 8, b'f', b'r', b'e', b'e'];
        let each = |data: &[u8]| for_each_box(data, |_, _, _| Ok(()));
        // a size of 4 GB less a byte (this wrapped a 32-bit usize past the
        // check, then panicked slicing: it fails without the fix only when
        // built for 32 bits, as for wasm32)
        assert!(each(&[&free[..], &[0xff, 0xff, 0xff, 0xff, b'a', b'b', b'c', b'd']].concat()).is_err());
        // a 64-bit size that wraps the end back to before the start
        assert!(each(&[&free[..], &[0, 0, 0, 1, b'a', b'b', b'c', b'd'], &u64::MAX.to_be_bytes()].concat()).is_err());
        // a box "to the end" whose header (a uuid's, 24 bytes) is longer than the end
        assert!(each(&[&free[..], &[0, 0, 0, 0, b'u', b'u', b'i', b'd']].concat()).is_err());
        let mut kinds = Vec::new();
        for_each_box(&[&free[..], &free[..]].concat(), |k, body, whole| {
            kinds.push((k, body.len(), whole.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(kinds, vec![(*b"free", 0, 8), (*b"free", 0, 8)]);
    }
}
