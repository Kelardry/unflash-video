//! WebCodecs codec strings and decoder descriptions from sample entries.

use crate::reader::{find_box, Reader};
use crate::Error;

/// What WebCodecs needs to configure a decoder for a track.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodecInfo {
    /// e.g. `avc1.640028`, `hvc1.1.6.L93.B0`, `av01.0.04M.08`, `vp09.00.10.08`,
    /// `mp4a.40.2`, `opus`.
    pub codec: String,
    /// `description` for the decoder config (avcC / hvcC / av1C payload,
    /// AudioSpecificConfig), when the codec needs one.
    pub description: Option<Vec<u8>>,
}

/// `avc1.PPCCLL` from an AVCDecoderConfigurationRecord.
pub fn avc_codec(avcc: &[u8]) -> Result<CodecInfo, Error> {
    if avcc.len() < 4 {
        return Err("avcC too short".into());
    }
    Ok(CodecInfo {
        codec: format!("avc1.{:02X}{:02X}{:02X}", avcc[1], avcc[2], avcc[3]),
        description: Some(avcc.to_vec()),
    })
}

/// `hvc1.<profile>.<compat>.<tier><level>.<constraints>` per ISO/IEC
/// 14496-15 Annex E.
pub fn hevc_codec(hvcc: &[u8], fourcc: &str) -> Result<CodecInfo, Error> {
    if hvcc.len() < 13 {
        return Err("hvcC too short".into());
    }
    let b1 = hvcc[1];
    let profile_space = b1 >> 6;
    let tier = (b1 >> 5) & 1;
    let profile_idc = b1 & 0x1f;
    let compat = u32::from_be_bytes([hvcc[2], hvcc[3], hvcc[4], hvcc[5]]);
    let constraints = &hvcc[6..12];
    let level_idc = hvcc[12];
    let mut s = String::from(fourcc);
    s.push('.');
    if profile_space > 0 {
        s.push((b'A' + profile_space - 1) as char);
    }
    s.push_str(&profile_idc.to_string());
    s.push('.');
    s.push_str(&format!("{:X}", compat.reverse_bits()));
    s.push('.');
    s.push(if tier == 1 { 'H' } else { 'L' });
    s.push_str(&level_idc.to_string());
    let mut last = 0;
    for (i, &c) in constraints.iter().enumerate() {
        if c != 0 {
            last = i + 1;
        }
    }
    for &c in &constraints[..last] {
        s.push('.');
        s.push_str(&format!("{c:X}"));
    }
    Ok(CodecInfo { codec: s, description: Some(hvcc.to_vec()) })
}

/// `av01.P.LLT.DD` from an AV1CodecConfigurationRecord.
pub fn av1_codec(av1c: &[u8]) -> Result<CodecInfo, Error> {
    if av1c.len() < 4 {
        return Err("av1C too short".into());
    }
    let profile = av1c[1] >> 5;
    let level = av1c[1] & 0x1f;
    let tier = (av1c[2] >> 7) & 1;
    let high = (av1c[2] >> 6) & 1;
    let twelve = (av1c[2] >> 5) & 1;
    let depth = if high == 1 {
        if twelve == 1 {
            12
        } else {
            10
        }
    } else {
        8
    };
    Ok(CodecInfo {
        codec: format!("av01.{profile}.{level:02}{}.{depth:02}", if tier == 1 { 'H' } else { 'M' }),
        description: Some(av1c.to_vec()),
    })
}

/// `vp09.PP.LL.DD` from a VPCodecConfigurationRecord (full box payload).
pub fn vp9_codec(vpcc: &[u8]) -> Result<CodecInfo, Error> {
    if vpcc.len() < 7 {
        return Err("vpcC too short".into());
    }
    let profile = vpcc[4];
    let level = vpcc[5];
    let depth = vpcc[6] >> 4;
    Ok(CodecInfo { codec: format!("vp09.{profile:02}.{level:02}.{depth:02}"), description: None })
}

/// Parse an `esds` box payload (full box) into (objectTypeIndication,
/// DecoderSpecificInfo).
pub fn parse_esds(esds: &[u8]) -> Result<(u8, Option<Vec<u8>>), Error> {
    let mut r = Reader::new(esds);
    r.version_flags()?;
    fn tag_len(r: &mut Reader) -> Result<(u8, usize), Error> {
        let tag = r.u8()?;
        let mut len = 0usize;
        for _ in 0..4 {
            let b = r.u8()?;
            len = (len << 7) | (b & 0x7f) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        Ok((tag, len))
    }
    let (tag, _) = tag_len(&mut r)?;
    if tag != 0x03 {
        return Err(format!("esds: expected ES_Descriptor, got tag {tag:#x}"));
    }
    r.u16()?; // ES_ID
    let flags = r.u8()?;
    if flags & 0x80 != 0 {
        r.u16()?; // dependsOn_ES_ID
    }
    if flags & 0x40 != 0 {
        let n = r.u8()? as usize;
        r.skip(n)?; // URL
    }
    if flags & 0x20 != 0 {
        r.u16()?; // OCR ES id
    }
    let (tag, _) = tag_len(&mut r)?;
    if tag != 0x04 {
        return Err(format!("esds: expected DecoderConfigDescriptor, got tag {tag:#x}"));
    }
    let oti = r.u8()?;
    r.skip(1 + 3 + 4 + 4)?; // streamType, bufferSizeDB, maxBitrate, avgBitrate
    let dsi = if r.remaining() >= 2 {
        let (tag, len) = tag_len(&mut r)?;
        if tag == 0x05 {
            Some(r.bytes(len.min(r.remaining()))?.to_vec())
        } else {
            None
        }
    } else {
        None
    };
    Ok((oti, dsi))
}

/// AAC (and friends) from an `esds`.
pub fn mp4a_codec(esds: &[u8]) -> Result<CodecInfo, Error> {
    let (oti, dsi) = parse_esds(esds)?;
    let codec = match oti {
        0x40 | 0x66 | 0x67 | 0x68 => {
            let aot = dsi
                .as_ref()
                .filter(|d| !d.is_empty())
                .map(|d| {
                    let a = d[0] >> 3;
                    if a == 31 && d.len() >= 2 {
                        32 + (((d[0] & 7) << 3) | (d[1] >> 5))
                    } else {
                        a
                    }
                })
                .unwrap_or(2);
            format!("mp4a.{oti:02X}.{aot}")
        }
        0x69 | 0x6B => "mp3".to_string(),
        // DTS (as ffmpeg writes it): named as its sample entries name it
        0xA9 => "dtsc".to_string(),
        0xAA => "dtsh".to_string(),
        0xAB => "dtsl".to_string(),
        0xAC => "dtse".to_string(),
        other => format!("mp4a.{other:02X}"),
    };
    Ok(CodecInfo { codec, description: dsi })
}

/// Codec info from a sample entry (the child boxes after the fixed fields).
pub fn from_sample_entry(fourcc: &[u8; 4], children: &[u8]) -> Result<CodecInfo, Error> {
    match fourcc {
        b"avc1" | b"avc3" | b"avc2" | b"avc4" => {
            let avcc = find_box(children, b"avcC").ok_or("avc1 without avcC")?;
            avc_codec(avcc)
        }
        b"hvc1" | b"hev1" => {
            let hvcc = find_box(children, b"hvcC").ok_or("hevc without hvcC")?;
            hevc_codec(hvcc, std::str::from_utf8(fourcc).unwrap_or("hvc1"))
        }
        b"av01" => {
            let av1c = find_box(children, b"av1C").ok_or("av01 without av1C")?;
            av1_codec(av1c)
        }
        b"vp09" => {
            let vpcc = find_box(children, b"vpcC").ok_or("vp09 without vpcC")?;
            vp9_codec(vpcc)
        }
        b"vp08" => Ok(CodecInfo { codec: "vp8".into(), description: None }),
        b"mp4a" => {
            let esds = find_box(children, b"esds").ok_or("mp4a without esds")?;
            mp4a_codec(esds)
        }
        b"Opus" => Ok(CodecInfo { codec: "opus".into(), description: None }),
        b"fLaC" => Ok(CodecInfo { codec: "flac".into(), description: find_box(children, b"dfLa").map(|d| d.to_vec()) }),
        b"ac-3" => Ok(CodecInfo { codec: "ac-3".into(), description: None }),
        b"ec-3" => Ok(CodecInfo { codec: "ec-3".into(), description: None }),
        b".mp3" => Ok(CodecInfo { codec: "mp3".into(), description: None }),
        other => Ok(CodecInfo { codec: crate::reader::fourcc_str(other), description: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avc_string() {
        let avcc = [1u8, 0x64, 0x00, 0x28, 0xff, 0xe1];
        assert_eq!(avc_codec(&avcc).unwrap().codec, "avc1.640028");
    }

    #[test]
    fn hevc_string() {
        // Main profile, compat flags 0x60000000, level 93, constraints B0 00 ..
        let mut hvcc = vec![1u8, 0x01, 0x60, 0, 0, 0, 0xB0, 0, 0, 0, 0, 0, 93];
        hvcc.extend_from_slice(&[0; 10]);
        assert_eq!(hevc_codec(&hvcc, "hvc1").unwrap().codec, "hvc1.1.6.L93.B0");
    }

    #[test]
    fn av1_and_vp9_strings() {
        let av1c = [0x81u8, 0x04, 0x0c, 0x00];
        assert_eq!(av1_codec(&av1c).unwrap().codec, "av01.0.04M.08");
        let vpcc = [1u8, 0, 0, 0, 0, 10, 0x80 | 0x02, 1, 1, 1, 0, 0];
        assert_eq!(vp9_codec(&vpcc).unwrap().codec, "vp09.00.10.08");
    }

    #[test]
    fn esds_aac() {
        // full box header + ES_Descriptor(3) { ES_ID, flags, DCD(4) { oti 0x40, ..., DSI(5) { 12 10 } } }
        let esds = [
            0u8, 0, 0, 0, 0x03, 0x19, 0, 1, 0, 0x04, 0x11, 0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05, 0x02, 0x12, 0x10, 0x06, 0x01, 0x02,
        ];
        let c = mp4a_codec(&esds).unwrap();
        assert_eq!(c.codec, "mp4a.40.2");
        assert_eq!(c.description, Some(vec![0x12, 0x10]));
    }
}
