//! Print the parameter sets among the first NAL units of an Annex B stream:
//!
//!     cargo run --release -p unflash-h264 --example spsdump -- stream.264

use unflash_h264::bitreader::unescape;
use unflash_h264::decoder::annexb_nal_units;
use unflash_h264::ps::{parse_pps, parse_sps};

fn main() {
    let data = std::fs::read(std::env::args().nth(1).expect("an Annex B .264 file")).unwrap();
    for nal in annexb_nal_units(&data).into_iter().take(42) {
        match nal[0] & 0x1f {
            7 => println!("SPS: {:?}", parse_sps(&unescape(&nal[1..]))),
            8 => println!("PPS: {:?}", parse_pps(&unescape(&nal[1..])).map(|p| (p.id, p.sps_id, p.entropy_coding_mode, p.num_ref_idx_default_active, p.weighted_pred, p.weighted_bipred_idc, p.transform_8x8_mode))),
            _ => {}
        }
    }
}
