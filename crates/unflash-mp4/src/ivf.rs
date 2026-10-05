//! IVF, the file format of libvpx's tools (most of the VP8 and VP9 decoders'
//! test streams are IVF files): a 32-byte file header ("DKIF", version,
//! header length, codec, size, rate, frame count), then per frame a 4-byte
//! size, an 8-byte timestamp (both little-endian) and the frame.

/// The frames of an IVF file with their timestamps, or None when `data` is
/// not one. A frame the file ends inside is kept as far as it goes.
pub fn frames(data: &[u8]) -> Option<Vec<(u64, &[u8])>> {
    if data.len() < 32 || !data.starts_with(b"DKIF") {
        return None;
    }
    let mut p = u16::from_le_bytes([data[6], data[7]]) as usize;
    let mut out = Vec::new();
    while p + 12 <= data.len() {
        let size = u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        let ts = u64::from_le_bytes(data[p + 4..p + 12].try_into().unwrap());
        let end = (p + 12).saturating_add(size).min(data.len());
        out.push((ts, &data[p + 12..end]));
        p = end;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_cut_files() {
        let mut file = b"DKIF\0\0\x20\0VP90".to_vec();
        file.resize(32, 0);
        for (ts, frame) in [(0u64, &b"abc"[..]), (7, &b"de"[..])] {
            file.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            file.extend_from_slice(&ts.to_le_bytes());
            file.extend_from_slice(frame);
        }
        assert_eq!(frames(&file), Some(vec![(0, &b"abc"[..]), (7, &b"de"[..])]));
        // the last frame cut short, then its frame header
        assert_eq!(frames(&file[..file.len() - 1]), Some(vec![(0, &b"abc"[..]), (7, &b"d"[..])]));
        assert_eq!(frames(&file[..52]), Some(vec![(0, &b"abc"[..])]));
        // not IVF, or not even a whole file header
        assert_eq!(frames(b"\x1aE\xdf\xa3 not IVF but long enough for its header"), None);
        assert_eq!(frames(&file[..31]), None);
    }
}
