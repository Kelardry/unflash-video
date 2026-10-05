//! The sync frame CRC (A/52 §7.10.1): generator x^16 + x^15 + x^2 + 1,
//! most significant bit first, registers starting at zero. A span of the
//! frame that ends with its CRC word leaves the registers at zero.

const fn table() -> [u16; 256] {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000 != 0 { (c << 1) ^ 0x8005 } else { c << 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static TABLE: [u16; 256] = table();

/// The CRC register after shifting `data` in, starting from zero.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc = (crc << 8) ^ TABLE[((crc >> 8) as u8 ^ b) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The register after a message followed by its own CRC is zero.
    #[test]
    fn appended_crc_checks_to_zero() {
        let msg = b"Digital Audio Compression";
        let c = crc16(msg);
        let mut framed = msg.to_vec();
        framed.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc16(&framed), 0);
        // a bit by bit shift register gives the same value
        let mut r = 0u16;
        for &b in msg.iter() {
            for k in (0..8).rev() {
                let bit = (b >> k) & 1;
                let top = (r >> 15) as u8 & 1;
                r <<= 1;
                if top ^ bit != 0 {
                    r ^= 0x8005;
                }
            }
        }
        assert_eq!(r, c);
    }
}
