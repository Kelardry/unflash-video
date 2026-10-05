//! Print the parameter sets and slice headers of an Annex B HEVC stream.
//!
//!     cargo run --release -p unflash-hevc --example psdump -- stream.bit [max_nal_units]

use std::rc::Rc;

use unflash_hevc::bitreader::{unescape, BitReader};
use unflash_hevc::ps::{parse_pps, parse_sps, Pps, Sps};
use unflash_hevc::slice::{parse_slice_header, SliceHeader};

fn main() {
    let path = std::env::args().nth(1).expect("stream");
    let max: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let data = std::fs::read(path).expect("read");
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut spss: Vec<Option<Rc<Sps>>> = vec![None; 16];
    let mut ppss: Vec<Option<Rc<Pps>>> = vec![None; 64];
    let mut prev: Option<SliceHeader> = None;
    let mut rbsp = Vec::new();
    for (k, &s) in starts.iter().enumerate().take(max) {
        let e = starts.get(k + 1).map_or(data.len(), |&n| n - 3);
        let nal = &data[s..e];
        if nal.len() < 2 {
            continue;
        }
        let t = (nal[0] >> 1) & 0x3f;
        let layer = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        unescape(&nal[2..], &mut rbsp);
        match t {
            33 if layer == 0 => match parse_sps(&rbsp) {
                Ok(sps) => {
                    println!("{k}: SPS {sps:?}");
                    let id = sps.id as usize;
                    spss[id] = Some(Rc::new(sps));
                }
                Err(e) => println!("{k}: SPS error {e}"),
            },
            34 if layer == 0 => match parse_pps(&rbsp, &spss) {
                Ok(pps) => {
                    println!("{k}: PPS {pps:?}");
                    let id = pps.id as usize;
                    ppss[id] = Some(Rc::new(pps));
                }
                Err(e) => println!("{k}: PPS error {e}"),
            },
            0..=31 if layer == 0 => {
                let mut r = BitReader::new(&rbsp);
                let first = r.peek(1) == Some(1);
                match parse_slice_header(&mut r, t, &spss, &ppss, if first { None } else { prev.as_ref() }) {
                    Ok(h) => {
                        println!("{k}: slice type {t} {:?} poc_lsb {} addr {} dependent {} qp {} data at {}", h.slice_type, h.poc_lsb, h.segment_address, h.dependent, h.qp, h.data_offset);
                        if !h.dependent {
                            prev = Some(h);
                        }
                    }
                    Err(e) => println!("{k}: slice type {t} error {e}"),
                }
            }
            _ => println!("{k}: NAL type {t} layer {layer}, {} bytes", nal.len()),
        }
    }
}
