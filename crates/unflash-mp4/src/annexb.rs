//! H.264 and HEVC as Annex B byte streams (as a transport stream carries
//! them): the few fields of their parameter sets and slice headers a
//! container needs (the picture size, the profile and level for the codec
//! string, the bit depths, where a picture and a field pair begin), and the
//! decoder setup records (avcC, hvcC) built from the parameter sets.

use crate::reader::Bits;

/// The RBSP of a NAL unit: its emulation prevention bytes taken out.
pub fn rbsp(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// What an H.264 sequence parameter set says that a container needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AvcSps {
    pub id: u32,
    pub profile: u8,
    pub chroma_format: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    /// The picture size after cropping.
    pub width: u32,
    pub height: u32,
    pub log2_max_frame_num: u32,
    pub frame_mbs_only: bool,
    pub separate_colour_plane: bool,
}

fn skip_scaling_list(b: &mut Bits, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + b.se()? + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// An H.264 SPS NAL unit (its header byte first).
pub fn parse_avc_sps(nal: &[u8]) -> Option<AvcSps> {
    let r = rbsp(nal.get(1..)?);
    let mut b = Bits::new(&r);
    let profile = b.u(8)? as u8;
    // (the constraint flags and the level: an avcC copies them from the SPS's bytes)
    b.skip(16)?;
    let id = b.ue()?;
    let (mut chroma, mut separate, mut depth_luma, mut depth_chroma) = (1, false, 8, 8);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = b.ue()?;
        if chroma == 3 {
            separate = b.flag()?;
        }
        depth_luma = 8 + b.ue()?;
        depth_chroma = 8 + b.ue()?;
        b.flag()?; // qpprime_y_zero_transform_bypass
        if b.flag()? {
            for i in 0..(if chroma != 3 { 8 } else { 12 }) {
                if b.flag()? {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num = b.ue()? + 4;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.flag()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.flag()?; // gaps_in_frame_num_value_allowed
    let width_mbs = b.ue()? + 1;
    let height_units = b.ue()? + 1;
    let frame_mbs_only = b.flag()?;
    if !frame_mbs_only {
        b.flag()?; // mb_adaptive_frame_field
    }
    b.flag()?; // direct_8x8_inference
    let mut crop = [0u32; 4];
    if b.flag()? {
        for c in &mut crop {
            *c = b.ue()?;
        }
    }
    // the crop's units: chroma samples, and field pairs' rows
    let (sub_w, sub_h) = match (chroma, separate) {
        (1, _) => (2, 2),
        (2, _) => (2, 1),
        _ => (1, 1),
    };
    let fields = 2 - frame_mbs_only as u32;
    let width = (width_mbs * 16).saturating_sub(sub_w * (crop[0] + crop[1]));
    let height = (fields * height_units * 16).saturating_sub(sub_h * fields * (crop[2] + crop[3]));
    Some(AvcSps { id, profile, chroma_format: chroma, bit_depth_luma: depth_luma, bit_depth_chroma: depth_chroma, width, height, log2_max_frame_num, frame_mbs_only, separate_colour_plane: separate })
}

/// (pps id, sps id) of an H.264 PPS NAL unit.
pub fn avc_pps_ids(nal: &[u8]) -> Option<(u32, u32)> {
    let r = rbsp(nal.get(1..)?);
    let mut b = Bits::new(&r);
    Some((b.ue()?, b.ue()?))
}

/// The start of an H.264 slice header, as far as a container cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AvcSlice {
    pub frame_num: u32,
    /// A field picture: Some(bottom).
    pub field: Option<bool>,
}

/// The head of an H.264 slice NAL unit (types 1 and 5), the SPS it uses
/// given by `sps_for(pps_id)`.
pub fn avc_slice(nal: &[u8], sps_for: impl Fn(u32) -> Option<AvcSps>) -> Option<AvcSlice> {
    let r = rbsp(nal.get(1..)?);
    let mut b = Bits::new(&r);
    b.ue()?; // first_mb_in_slice
    b.ue()?; // slice_type
    let pps_id = b.ue()?;
    let sps = sps_for(pps_id)?;
    if sps.separate_colour_plane {
        b.u(2)?;
    }
    let frame_num = b.u(sps.log2_max_frame_num as usize)?;
    let field = if !sps.frame_mbs_only && b.flag()? { Some(b.flag()?) } else { None };
    Some(AvcSlice { frame_num, field })
}

/// Whether an H.264 SEI NAL unit holds a recovery point (a picture that
/// decoding can start at, as an IDR is).
pub fn avc_sei_recovery_point(nal: &[u8]) -> bool {
    let Some(body) = nal.get(1..) else { return false };
    let r = rbsp(body);
    let mut i = 0;
    let read = |i: &mut usize| -> Option<usize> {
        let mut v = 0usize;
        loop {
            let c = *r.get(*i)?;
            *i += 1;
            v += c as usize;
            if c != 0xff {
                return Some(v);
            }
        }
    };
    // (the trailing bits: a lone 0x80)
    while i + 1 < r.len() {
        let (Some(kind), Some(size)) = (read(&mut i), read(&mut i)) else { return false };
        if kind == 6 {
            return true;
        }
        i += size;
    }
    false
}

/// An AVCDecoderConfigurationRecord (avcC) for the parameter sets, the first
/// SPS's fields in its head; lengths of 4 bytes.
pub fn avcc(sps: &[&[u8]], pps: &[&[u8]], info: &AvcSps) -> Vec<u8> {
    let s0 = sps.first().copied().unwrap_or(&[]);
    let at = |i: usize| s0.get(i).copied().unwrap_or(0);
    let mut v = vec![1, at(1), at(2), at(3), 0xff, 0xe0 | sps.len().min(31) as u8];
    for s in sps.iter().take(31) {
        v.extend_from_slice(&(s.len() as u16).to_be_bytes());
        v.extend_from_slice(s);
    }
    v.push(pps.len().min(255) as u8);
    for p in pps.iter().take(255) {
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend_from_slice(p);
    }
    if matches!(info.profile, 100 | 110 | 122 | 144) {
        v.push(0xfc | info.chroma_format as u8);
        v.push(0xf8 | info.bit_depth_luma.saturating_sub(8) as u8);
        v.push(0xf8 | info.bit_depth_chroma.saturating_sub(8) as u8);
        v.push(0);
    }
    v
}

/// An avcC for one SPS and one PPS (NAL units, each with its header byte),
/// with the chroma format and bit depths a High profile's record carries
/// after the sets; None when they are not an SPS and a PPS, or the SPS
/// can't be read.
pub fn avcc_record(sps: &[u8], pps: &[u8]) -> Option<Vec<u8>> {
    if sps.first()? & 0x1f != 7 || pps.first()? & 0x1f != 8 {
        return None;
    }
    let info = parse_avc_sps(sps)?;
    Some(avcc(&[sps], &[pps], &info))
}

/// What an HEVC sequence parameter set says that a container needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HevcSps {
    /// The general profile_tier_level: profile space, tier and profile; the
    /// compatibility flags; the constraint flags; the level.
    pub ptl: [u8; 12],
    pub max_sub_layers: u32,
    pub temporal_id_nesting: bool,
    pub chroma_format: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub width: u32,
    pub height: u32,
}

/// An HEVC SPS NAL unit (its two header bytes first).
pub fn parse_hevc_sps(nal: &[u8]) -> Option<HevcSps> {
    let r = rbsp(nal.get(2..)?);
    let mut b = Bits::new(&r);
    b.u(4)?; // sps_video_parameter_set_id
    let sub_layers_minus1 = b.u(3)?;
    let nesting = b.flag()?;
    let mut ptl = [0u8; 12];
    for p in &mut ptl {
        *p = b.u(8)? as u8;
    }
    let mut present = Vec::new();
    for _ in 0..sub_layers_minus1 {
        present.push((b.flag()?, b.flag()?));
    }
    if sub_layers_minus1 > 0 {
        for _ in sub_layers_minus1..8 {
            b.u(2)?;
        }
    }
    for (profile, level) in present {
        if profile {
            b.skip(88)?;
        }
        if level {
            b.skip(8)?;
        }
    }
    b.ue()?; // sps_seq_parameter_set_id
    let chroma = b.ue()?;
    if chroma == 3 {
        b.flag()?;
    }
    let w = b.ue()?;
    let h = b.ue()?;
    let mut crop = [0u32; 4];
    if b.flag()? {
        for c in &mut crop {
            *c = b.ue()?;
        }
    }
    let depth_luma = 8 + b.ue()?;
    let depth_chroma = 8 + b.ue()?;
    let (sub_w, sub_h) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    Some(HevcSps {
        ptl,
        max_sub_layers: sub_layers_minus1 + 1,
        temporal_id_nesting: nesting,
        chroma_format: chroma,
        bit_depth_luma: depth_luma,
        bit_depth_chroma: depth_chroma,
        width: w.saturating_sub(sub_w * (crop[0] + crop[1])),
        height: h.saturating_sub(sub_h * (crop[2] + crop[3])),
    })
}

/// An HEVCDecoderConfigurationRecord (hvcC) for the parameter sets, the
/// SPS's fields in its head; lengths of 4 bytes. The sets may come again
/// in the stream (`hev1`), so the arrays are not marked complete.
pub fn hvcc(vps: &[&[u8]], sps: &[&[u8]], pps: &[&[u8]], info: &HevcSps) -> Vec<u8> {
    let mut v = vec![1];
    v.extend_from_slice(&info.ptl);
    v.extend_from_slice(&[0xf0, 0x00, 0xfc]);
    v.push(0xfc | info.chroma_format as u8);
    v.push(0xf8 | info.bit_depth_luma.saturating_sub(8) as u8);
    v.push(0xf8 | info.bit_depth_chroma.saturating_sub(8) as u8);
    v.extend_from_slice(&[0, 0]);
    v.push(((info.max_sub_layers.min(7) as u8) << 3) | ((info.temporal_id_nesting as u8) << 2) | 3);
    let arrays: Vec<(u8, &[&[u8]])> = [(32u8, vps), (33, sps), (34, pps)].into_iter().filter(|(_, a)| !a.is_empty()).collect();
    v.push(arrays.len() as u8);
    for (kind, nals) in arrays {
        v.push(kind);
        v.extend_from_slice(&(nals.len() as u16).to_be_bytes());
        for n in nals {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention() {
        assert_eq!(rbsp(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), vec![0, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn avc_sps_fields() {
        // x264, High 4:2:0 8-bit, 1920x1080 (1088 coded, cropped by 8)
        let sps = [0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xc8, 0x3c, 0x60, 0xc6, 0x58];
        let s = parse_avc_sps(&sps).unwrap();
        assert_eq!((s.profile, s.width, s.height, s.chroma_format, s.bit_depth_luma, s.frame_mbs_only), (100, 1920, 1080, 1, 8, true));
        let rec = avcc(&[&sps], &[&[0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0]], &s);
        assert_eq!(&rec[..6], &[1, 0x64, 0x00, 0x28, 0xff, 0xe1]);
        assert_eq!(&rec[rec.len() - 4..], &[0xfd, 0xf8, 0xf8, 0]);
        assert_eq!(crate::codec::avc_codec(&rec).unwrap().codec, "avc1.640028");
        // interlaced (field pairs: 68 map units of 16 rows each field)
        let sps = [0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x04, 0x4f, 0xde, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x20, 0x00, 0x00, 0x06, 0x43, 0xe2, 0xc5, 0xb2, 0xc0];
        let s = parse_avc_sps(&sps).unwrap();
        assert_eq!((s.width, s.height, s.frame_mbs_only), (1920, 1080, false));
        // Constrained Baseline
        let sps = [0x67, 0x42, 0xc0, 0x28, 0xda, 0x01, 0xe0, 0x08, 0x9f, 0x97, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x2a];
        let s = parse_avc_sps(&sps).unwrap();
        assert_eq!((s.profile, s.width, s.height), (66, 1920, 1080));
        assert_eq!(avcc(&[&sps], &[&[0x68, 0xce, 0x0f, 0xc8]], &s).len(), 6 + 2 + sps.len() + 1 + 2 + 4);
    }

    #[test]
    fn avcc_from_one_sps_and_one_pps() {
        // x264, High 4:2:0 8-bit: the record ends with its chroma format and bit depths
        let sps = [0x67, 0x64, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78, 0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0xc8, 0x3c, 0x60, 0xc6, 0x58];
        let pps = [0x68, 0xeb, 0xe3, 0xcb, 0x22, 0xc0];
        let want = [&[1, 0x64, 0x00, 0x28, 0xff, 0xe1, 0, 27][..], &sps, &[1, 0, 6], &pps, &[0xfd, 0xf8, 0xf8, 0]].concat();
        assert_eq!(avcc_record(&sps, &pps), Some(want));
        // Constrained Baseline: no tail
        let sps = [0x67, 0x42, 0xc0, 0x28, 0xda, 0x01, 0xe0, 0x08, 0x9f, 0x97, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0x20, 0xf1, 0x83, 0x2a];
        let pps = [0x68, 0xce, 0x0f, 0xc8];
        assert_eq!(avcc_record(&sps, &pps), Some([&[1, 0x42, 0xc0, 0x28, 0xff, 0xe1, 0, 25][..], &sps, &[1, 0, 4], &pps].concat()));
        // the sets the wrong way round, an SPS cut short, nothing
        assert_eq!(avcc_record(&pps, &sps), None);
        assert_eq!(avcc_record(&sps[..3], &pps), None);
        assert_eq!(avcc_record(&[], &[]), None);
    }

    #[test]
    fn sei_recovery_point() {
        // SEI: recovery point (type 6, 1 byte), then trailing bits
        assert!(avc_sei_recovery_point(&[0x06, 0x06, 0x01, 0xc4, 0x80]));
        // user data unregistered only (type 5)
        assert!(!avc_sei_recovery_point(&[0x06, 0x05, 0x02, 0xaa, 0xbb, 0x80]));
    }
}
