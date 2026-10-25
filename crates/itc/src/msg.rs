//! Fields of the message types that are understood, as JSON. Anything not understood is kept
//! as hex so nothing is lost.
//!
//! Decoded messages describe railroad operations (base station positions, wayside signal and
//! switch states). Keep them private.

use serde_json::{json, Value};

pub fn type_name(kind: u8) -> &'static str {
    match kind {
        0x01 => "QSTAT",
        0x02 => "BEACON",
        0x03 => "ACK",
        0x04 => "ACQ",
        0x05 => "CNTL",
        0x06 => "TIME",
        0x07 => "T07",
        0x33 => "WIU",
        0x34 => "WIU_REPLY",
        0x70 => "UNICAST",
        _ => "OTHER",
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn id24(b: &[u8]) -> Option<String> {
    (b.len() >= 3).then(|| hex(&b[..3]))
}

fn i32_be(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Indicator bits as 5-bit device codes, each read last bit first, and the bits left over.
///
/// On recordings every change of a wayside unit's indicators changes whole 5-bit fields, and
/// each field rests on one code and leaves it for a while (a signal cleared for a train, then
/// back to stop). What each code means is set per railroad and is not in the message.
pub fn device_codes(bits: &str) -> (Vec<u8>, String) {
    let b = bits.as_bytes();
    let n = b.len() / 5;
    let codes = (0..n)
        .map(|i| b[5 * i..5 * i + 5].iter().rev().fold(0u8, |v, &c| (v << 1) | (c == b'1') as u8))
        .collect();
    (codes, bits[5 * n..].to_string())
}

/// Signal aspect names for the status codes documented in public material (a railroad's wayside
/// message tester manual, filed with the FRA). Each wayside unit has its own device status
/// table, so other codes are left unnamed rather than guessed.
pub fn signal_aspect(code: u8) -> Option<&'static str> {
    match code {
        2 => Some("Medium Clear"),
        3 => Some("Clear"),
        6 => Some("Approach Medium"),
        8 => Some("Approach"),
        _ => None,
    }
}

/// What a payload says, by type. Always has "type" and "hex"; adds fields where known.
pub fn describe(kind: u8, p: &[u8]) -> Value {
    let mut o = serde_json::Map::new();
    o.insert("type".into(), json!(type_name(kind)));
    o.insert("hex".into(), json!(hex(p)));
    match kind {
        // flags, base radio, latitude and longitude in 1/600000 degree, local channel, rate,
        // utilisation, a flag, then the channels of nearby bases
        0x02 if p.len() >= 16 => {
            o.insert("radio".into(), json!(id24(&p[1..])));
            o.insert("flags".into(), json!(p[0]));
            o.insert("lat".into(), json!((i32_be(&p[4..]) as f64 / 600000.0 * 1e6).round() / 1e6));
            o.insert("lon".into(), json!((i32_be(&p[8..]) as f64 / 600000.0 * 1e6).round() / 1e6));
            o.insert("channel".into(), json!(p[12]));
            o.insert("beacon_rate".into(), json!(p[13]));
            o.insert("util".into(), json!(p[14]));
            let chans: Vec<u8> = p[16..].iter().take(6).copied().take_while(|c| (80..=220).contains(c)).collect();
            o.insert("channels".into(), json!(chans));
        }
        // base radio, then the UTC date and time
        0x06 if p.len() >= 9 => {
            o.insert("radio".into(), json!(id24(p)));
            let t = format!("20{:02}-{:02}-{:02}T{:02}:{:02}:{:02}Z", p[3], p[4], p[5], p[6], p[7], p[8]);
            o.insert("utc".into(), json!(t));
        }
        // base radio, a sequence number, then slot allocation data
        0x05 if p.len() >= 4 => {
            o.insert("radio".into(), json!(id24(p)));
            o.insert("seq".into(), json!(p[3]));
            o.insert("data".into(), json!(hex(&p[4..])));
        }
        // Wayside status, as seen on recordings: a 3-byte group shared by many units, the unit's
        // own 2 bytes, a label byte, a 2-byte counter that changes every message, then the
        // indicator bytes (switches, signals; how many depends on the unit), packed MSB first
        0x33 | 0x34 if p.len() >= 9 => {
            o.insert("wiu".into(), json!(hex(&p[..5])));
            o.insert("group".into(), json!(hex(&p[..3])));
            o.insert("label".into(), json!(p[5]));
            o.insert("seq".into(), json!(u16::from_be_bytes([p[6], p[7]])));
            o.insert("indicators".into(), json!(hex(&p[8..])));
            let bits: String = p[8..].iter().map(|b| format!("{b:08b}")).collect();
            let (codes, spare) = device_codes(&bits);
            let aspects: Vec<Option<&str>> = codes.iter().map(|c| signal_aspect(*c)).collect();
            o.insert("aspects".into(), json!(aspects));
            o.insert("codes".into(), json!(codes));
            o.insert("spare".into(), json!(spare));
            o.insert("bits".into(), json!(bits));
        }
        _ => {
            if let Some(id) = id24(p) {
                o.insert("radio".into(), json!(id));
            }
        }
    }
    Value::Object(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_of_made_up_messages() {
        // a made-up beacon: radio 123456 at 40.5, -90.25, channel 114, nearby 101 and 127
        let mut b = vec![0x88, 0x12, 0x34, 0x56];
        b.extend(((40.5f64 * 600000.0) as i32).to_be_bytes());
        b.extend(((-90.25f64 * 600000.0) as i32).to_be_bytes());
        b.extend([114, 1, 20, 0x30, 101, 127, 0, 0]);
        let v = describe(0x02, &b);
        assert_eq!(v["radio"], "123456");
        assert_eq!(v["lat"], 40.5);
        assert_eq!(v["lon"], -90.25);
        assert_eq!(v["channels"], json!([101, 127]));

        let t = describe(0x06, &[0x12, 0x34, 0x56, 26, 1, 2, 3, 4, 5]);
        assert_eq!(t["utc"], "2026-01-02T03:04:05Z");

        let w = describe(0x34, &[0xab, 0xcd, 0xef, 0x12, 0x34, 0x82, 0x01, 0x02, 0xbd, 0xef]);
        assert_eq!(w["wiu"], "abcdef1234");
        assert_eq!(w["seq"], 0x0102);
        assert_eq!(w["indicators"], "bdef");
        assert_eq!(w["bits"], "1011110111101111");
        // 10111 10111 10111 1 -> codes read last bit first: 29, 29, 29, spare "1"
        assert_eq!(w["codes"], json!([29, 29, 29]));
        assert_eq!(w["spare"], "1");
        // the documented example: 11110 read last bit first is 15
        assert_eq!(device_codes("1111011000").0, vec![15, 3]);
        let w = describe(0x33, &[0xab, 0xcd, 0xef, 0x12, 0x34, 0x82, 0x01, 0x02, 0xf6, 0x3c, 0xc0]);
        assert_eq!(w["codes"], json!([15, 3, 15, 6]));
        assert_eq!(w["aspects"], json!([null, "Clear", null, "Approach Medium"]));
    }
}
