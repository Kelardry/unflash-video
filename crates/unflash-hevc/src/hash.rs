//! Decoded picture hash SEI messages (D.3.20): the encoder's MD5, CRC or
//! checksum of each colour component of a picture. Checking them is a
//! self-test on streams that carry them (the conformance streams do), and
//! covers pictures that are never output.

use crate::picture::{Picture, Sample};

/// One hash per colour component (one for 4:0:0 pictures).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PictureHash {
    Md5(Vec<[u8; 16]>),
    Crc(Vec<u16>),
    Checksum(Vec<u32>),
}

/// The decoded picture hash in a suffix SEI NAL unit's payload (after the
/// NAL unit header, emulation prevention removed), if it carries one.
pub fn parse_sei(rbsp: &[u8], components: usize) -> Option<PictureHash> {
    let mut p = 0;
    // each message: payload type and size (runs of 0xFF bytes plus a last byte)
    let value = |p: &mut usize| -> Option<usize> {
        let mut v = 0usize;
        loop {
            let b = *rbsp.get(*p)?;
            *p += 1;
            v = v.saturating_add(b as usize);
            if b != 0xff {
                return Some(v);
            }
        }
    };
    // the last byte holds the RBSP stop bit
    while p + 1 < rbsp.len() {
        let kind = value(&mut p)?;
        let size = value(&mut p)?;
        let payload = rbsp.get(p..p.checked_add(size)?)?;
        p += size;
        if kind == 132 {
            return parse_hash(payload, components);
        }
    }
    None
}

fn parse_hash(payload: &[u8], components: usize) -> Option<PictureHash> {
    let (&kind, rest) = payload.split_first()?;
    match kind {
        0 => (0..components).map(|c| rest.get(16 * c..16 * c + 16)?.try_into().ok()).collect::<Option<_>>().map(PictureHash::Md5),
        1 => (0..components).map(|c| Some(u16::from_be_bytes(rest.get(2 * c..2 * c + 2)?.try_into().ok()?))).collect::<Option<_>>().map(PictureHash::Crc),
        2 => (0..components).map(|c| Some(u32::from_be_bytes(rest.get(4 * c..4 * c + 4)?.try_into().ok()?))).collect::<Option<_>>().map(PictureHash::Checksum),
        _ => None,
    }
}

/// Whether the picture's samples have the hash (computed over the whole
/// decoded picture, before cropping).
pub fn matches<P: Sample>(pic: &Picture<P>, bit_depth: u32, hash: &PictureHash) -> bool {
    let components = match hash {
        PictureHash::Md5(v) => v.len(),
        PictureHash::Crc(v) => v.len(),
        PictureHash::Checksum(v) => v.len(),
    };
    let wide = bit_depth > 8;
    (0..components).all(|c| {
        let Some(plane) = pic.planes.get(c) else { return false };
        // pictureData (D-22): each sample as one byte, or two (low byte first) above 8 bits
        let mut data = Vec::with_capacity(plane.width * plane.height * (1 + wide as usize));
        for y in 0..plane.height {
            for s in &plane.data[y * plane.stride..][..plane.width] {
                let s = s.get();
                data.push(s as u8);
                if wide {
                    data.push((s >> 8) as u8);
                }
            }
        }
        match hash {
            PictureHash::Md5(v) => md5(&data) == v[c],
            PictureHash::Crc(v) => crc(&data) == v[c],
            PictureHash::Checksum(v) => checksum(&data, plane.width, wide) == v[c],
        }
    })
}

/// The CRC of D-24: CRC-16 with the polynomial 0x1021 over the data and
/// two zero bytes, from 0xFFFF.
fn crc(data: &[u8]) -> u16 {
    let mut crc = 0xffffu32;
    for &byte in data.iter().chain(&[0, 0]) {
        for bit in (0..8).rev() {
            let msb = (crc >> 15) & 1;
            crc = (((crc << 1) + ((byte >> bit) & 1) as u32) & 0xffff) ^ (msb * 0x1021);
        }
    }
    crc as u16
}

/// The checksum of D-25: every byte of the picture data XORed with a mask
/// from its sample's position, summed.
fn checksum(data: &[u8], width: usize, wide: bool) -> u32 {
    let per_sample = 1 + wide as usize;
    data.chunks_exact(per_sample).enumerate().fold(0u32, |sum, (i, bytes)| {
        let (x, y) = (i % width, i / width);
        let mask = ((x & 0xff) ^ (y & 0xff) ^ (x >> 8) ^ (y >> 8)) as u32;
        bytes.iter().fold(sum, |sum, &b| sum.wrapping_add(b as u32 ^ mask))
    })
}

/// Per-round shift amounts of MD5 (IETF RFC 1321).
const SHIFTS: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];

/// MD5's additive constants, floor(abs(sin(i + 1)) * 2^32).
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453,
    0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05,
    0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// The MD5 digest of `data` (IETF RFC 1321).
fn md5(data: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut tail = data.chunks_exact(64).remainder().to_vec();
    tail.push(0x80);
    while tail.len() % 64 != 56 {
        tail.push(0);
    }
    tail.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for block in data.chunks_exact(64).chain(tail.chunks_exact(64)) {
        let m: Vec<u32> = block.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(K[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(SHIFTS[(i / 16) * 4 + i % 4]));
        }
        for (s, v) in state.iter_mut().zip([a, b, c, d]) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 16];
    for (o, s) in out.chunks_exact_mut(4).zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: [u8; 16]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn md5_matches_rfc_1321() {
        assert_eq!(hex(md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(hex(md5(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")), "57edf4a22be3c955ac49da2e2107b67a");
    }

    #[test]
    fn crc_is_the_augmented_ccitt_crc() {
        // CRC-16/AUG-CCITT's check value
        assert_eq!(crc(b"123456789"), 0xe5cc);
    }

    #[test]
    fn hashes_of_a_picture() {
        let mut pic = Picture::<u16>::new(0, 8, 8, true, 0);
        for (i, s) in pic.planes[0].data.iter_mut().enumerate() {
            *s = (i * 37 % 1024) as u16;
        }
        let bytes: Vec<u8> = pic.planes[0].data.iter().flat_map(|s| s.to_le_bytes()).collect();
        let zeros = md5(&[0u8; 32]);
        assert!(matches(&pic, 10, &PictureHash::Md5(vec![md5(&bytes), zeros, zeros])));
        assert!(!matches(&pic, 10, &PictureHash::Md5(vec![md5(&bytes[2..]), zeros, zeros])));
        let zero_crc = crc(&[0; 32]);
        assert!(matches(&pic, 10, &PictureHash::Crc(vec![crc(&bytes), zero_crc, zero_crc])));
        // a zero plane's checksum is the sum of its masks, twice for two-byte samples
        let masks: u32 = (0..4u32).flat_map(|y| (0..4u32).map(move |x| 2 * (x ^ y))).sum();
        assert_eq!(checksum(&[0; 32], 4, true), masks);
        assert!(matches(&pic, 10, &PictureHash::Checksum(vec![checksum(&bytes, 8, true), masks, masks])));
    }

    #[test]
    fn sei_payloads() {
        // a user data message (type 5, 3 bytes) before a CRC picture hash (type 132)
        let rbsp = [5, 3, 1, 2, 3, 132, 7, 1, 0x12, 0x34, 0, 1, 0xab, 0xcd, 0x80];
        assert_eq!(parse_sei(&rbsp, 3), Some(PictureHash::Crc(vec![0x1234, 1, 0xabcd])));
        assert_eq!(parse_sei(&rbsp[..10], 3), None);
        assert_eq!(parse_sei(&[0xff, 0xff, 0xff], 3), None);
    }
}
