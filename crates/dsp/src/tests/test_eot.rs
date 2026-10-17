//! The EOT encoder and decoder.

use super::*;
use crate::eot::{self, Payload};
use crate::sim::{Rng, World};
use num_complex::{Complex32, Complex64};

const GROUND_TRUTH: &str = "111100001000110111001001000110100000000111100011100000111011000";

fn bits(s: &str) -> Vec<u8> {
    s.bytes().map(|c| c - b'0').collect()
}

#[test]
fn ground_truth_passes_and_decodes() {
    let p = bits(GROUND_TRUTH);
    assert!(eot::check_ok(&p));
    let f = eot::fields(&p, 0);
    assert!(f["unit_id"] == 75618 && f["brake_pipe_psi"] == 88);
    assert!(f["motion"] == true && f["marker_light"] == false);
    assert!(f["crc_ok"] == true && f["corrected_bits"] == 0);
    assert_eq!(eot::build_payload(75618, 88), p);
}

#[test]
fn bit_12_flipped_is_corrected() {
    let mut p = bits(GROUND_TRUTH);
    p[12] ^= 1;
    assert!(!eot::check_ok(&p));
    let (fixed, idx) = eot::correct_one(&p).unwrap();
    assert!(idx == 12 && fixed == bits(GROUND_TRUTH));
}

#[test]
fn two_bit_errors_not_corrected() {
    let mut p = bits(GROUND_TRUTH);
    p[3] ^= 1;
    p[40] ^= 1;
    assert!(eot::correct_one(&p).is_none());
}

#[test]
fn plain_cyclic_check_without_key_fails_on_valid_packet() {
    let p = bits(GROUND_TRUTH);
    let keyless: Vec<u8> = eot::check_bits(&p[..45]).iter().zip(eot::KEY).map(|(a, b)| a ^ b).collect();
    assert_ne!(keyless, p[45..].to_vec());
}

fn synthetic(payloads: &[Vec<u8>], snr_noise: f64, seed: u64) -> Vec<Complex64> {
    let fs = 48_000.0;
    let mut b: Vec<u8> = [0u8, 1].repeat(10);
    for p in payloads {
        b.extend(eot::packet_bits(p));
        b.extend([1u8, 0].repeat(6));
    }
    let audio = eot::ffsk_audio(&b, fs);
    let iq = eot::fm_modulate(&audio, fs, 3_500.0, -350.0);
    let mut rng = Rng::new(seed);
    iq.into_iter().map(|v| v + Complex64::new(rng.standard_normal(), rng.standard_normal()) * snr_noise).collect()
}

#[test]
fn synthetic_packet_decodes_to_same_fields() {
    let p = Payload { motion: false, marker_light: true, battery_charge_raw: 17, ..Payload::new(12345, 90) }.bits();
    let pkts = eot::decode(&synthetic(&[p], 0.25, 5), 48_000.0);
    assert_eq!(pkts.len(), 1);
    let f = &pkts[0];
    assert!(f["unit_id"] == 12345 && f["brake_pipe_psi"] == 90 && f["motion"] == false);
    assert!(f["marker_light"] == true && f["battery_charge_raw"] == 17);
    assert_eq!(f["corrected_bits"], 0);
}

#[test]
fn two_packets_in_one_burst_in_order() {
    let a = eot::build_payload(75618, 87);
    let b = eot::build_payload(75618, 88);
    let pkts = eot::decode(&synthetic(&[a, b], 0.25, 5), 48_000.0);
    let psi: Vec<i64> = pkts.iter().map(|p| p["brake_pipe_psi"].as_i64().unwrap()).collect();
    assert_eq!(psi, vec![87, 88]);
    let tel = eot::telemetry_from(&pkts).unwrap();
    assert!(tel["packets"] == 2 && tel["brake_pipe_psi"] == 88 && !tel.contains_key("t_offset_s"));
}

#[test]
fn inverted_check_bits_rejected() {
    let p = eot::build_payload(75618, 88);
    let mut bad = p[..45].to_vec();
    bad.extend(p[45..].iter().map(|b| 1 - b));
    assert!(eot::decode(&synthetic(&[bad], 0.25, 5), 48_000.0).is_empty());
}

#[test]
fn noise_yields_nothing() {
    let mut rng = Rng::new(11);
    for _ in 0..3 {
        let n: Vec<Complex64> = (0..48_000).map(|_| Complex64::new(rng.standard_normal(), rng.standard_normal())).collect();
        assert!(eot::decode(&n, 48_000.0).is_empty());
    }
}

#[test]
fn eot_in_sdr_stream_produces_telemetry() {
    let tmp = TmpDir::new("eot-stream");
    let (mut src, events) = source(tmp.path(), &eot_dev());
    let w = World::default();
    w.eot_burst(457.9375, 18.0, 2.0, &[eot::build_payload(75618, 88)]);
    run(&mut src, &w, 0.0, 3.5);
    let c = closed(&events);
    assert_eq!(c.len(), 1);
    let tel = &c[0]["telemetry"];
    assert!(tel["unit_id"] == 75618 && tel["brake_pipe_psi"] == 88 && tel["packets"] == 1);
    assert!(tel["crc_ok"] == true && tel["decoder"] == "railspotting-eot-1");
    let json = std::fs::read_dir(tmp.path().join("bursts")).unwrap().map(|e| e.unwrap().path())
        .find(|p| p.extension().unwrap() == "json").unwrap();
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(json).unwrap()).unwrap();
    assert_eq!(meta["decoded"][0]["unit_id"], 75618);
    assert_eq!(src.last_eot.as_ref().unwrap()["unit_id"], 75618);
    assert!(logged("UHF SDR: EOT unit 75618, 88 psi, moving"));
}

pub fn read_cf32(p: &Path) -> Vec<Complex32> {
    std::fs::read(p).unwrap().chunks_exact(8).map(|b| Complex32::new(
        f32::from_le_bytes([b[0], b[1], b[2], b[3]]), f32::from_le_bytes([b[4], b[5], b[6], b[7]]))).collect()
}

/// A deterministic recording made by the burst recorder (tests/data).
#[test]
fn recording_fixture_decodes() {
    let d = fixtures().join("eot");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(d.join("eot_burst.json")).unwrap()).unwrap();
    let iq = read_cf32(&d.join("eot_burst.cf32"));
    assert_eq!(iq.len() as u64, meta["samples"].as_u64().unwrap());
    let pkts = eot::decode32(&iq, meta["sample_rate"].as_f64().unwrap());
    let got: Vec<(Value, Value, Value)> = pkts.iter()
        .map(|p| (p["unit_id"].clone(), p["brake_pipe_psi"].clone(), p["motion"].clone())).collect();
    let want: Vec<(Value, Value, Value)> = meta["decoded"].as_array().unwrap().iter()
        .map(|p| (p["unit_id"].clone(), p["brake_pipe_psi"].clone(), p["motion"].clone())).collect();
    assert_eq!(got, want);
    assert!(!pkts.is_empty() && pkts.iter().all(|p| p["crc_ok"] == true));
}

#[test]
fn describe_wording() {
    let mut tel = eot::telemetry_from(&[eot::fields(&bits(GROUND_TRUTH), 1)]).unwrap();
    assert_eq!(eot::describe(&tel), "EOT unit 75618, 88 psi, moving (1 bit corrected)");
    tel.insert("motion".into(), Value::Bool(false));
    tel.insert("corrected_bits".into(), 0.into());
    assert_eq!(eot::describe(&tel), "EOT unit 75618, 88 psi, stopped");
}

#[test]
fn two_bit_errors_corrected_by_correct_two() {
    let mut p = bits(GROUND_TRUTH);
    p[3] ^= 1;
    p[40] ^= 1;
    assert_eq!(eot::correct_two(&p), Some(bits(GROUND_TRUTH)));
    // three errors are beyond it: no packet within two flips
    p[50] ^= 1;
    assert_eq!(eot::correct_two(&p), None);
}

#[test]
fn burst_with_two_bit_errors_decodes_as_corrected() {
    let mut p = eot::build_payload(70110, 88);
    p[5] ^= 1;
    p[30] ^= 1;
    let pkts = eot::decode(&synthetic(&[p], 0.25, 5), 48_000.0);
    assert_eq!(pkts.len(), 1);
    assert!(pkts[0]["unit_id"] == 70110 && pkts[0]["brake_pipe_psi"] == 88);
    assert_eq!(pkts[0]["corrected_bits"], 2);
    let tel = eot::telemetry_from(&pkts).unwrap();
    assert_eq!(eot::describe(&tel), "EOT unit 70110, 88 psi, moving (2 bits corrected)");
}

#[test]
fn longer_noise_yields_nothing() {
    // two-flip candidates turn up in noise, but two timing offsets never agree on one
    let mut rng = Rng::new(23);
    for _ in 0..30 {
        let n: Vec<Complex64> = (0..48_000).map(|_| Complex64::new(rng.standard_normal(), rng.standard_normal())).collect();
        assert!(eot::decode(&n, 48_000.0).is_empty());
    }
}

#[test]
fn telemetry_prefers_a_packet_the_headend_takes() {
    let a = eot::fields(&eot::build_payload(70110, 87), 1);
    let b = eot::fields(&eot::build_payload(70110, 88), 2);
    let tel = eot::telemetry_from(&[a, b.clone()]).unwrap();
    assert!(tel["brake_pipe_psi"] == 87 && tel["corrected_bits"] == 1 && tel["packets"] == 2);
    // only a two-bit packet: it is the telemetry
    let tel = eot::telemetry_from(&[b]).unwrap();
    assert_eq!(tel["corrected_bits"], 2);
}

#[test]
fn telemetry_over_the_headend_limit_is_left_off() {
    let tel = |c: i64| eot::telemetry_from(&[eot::fields(&eot::build_payload(70110, 88), c)]).unwrap();
    let event = |c: i64| {
        let mut e = serde_json::Map::new();
        e.insert("event_id".into(), "e1".into());
        e.insert("telemetry".into(), serde_json::Value::Object(tel(c)));
        e
    };
    assert_eq!(eot::corrected_max(&serde_json::json!({})), 1);
    assert_eq!(eot::corrected_max(&serde_json::json!({"eot_corrected_bits_max": 2})), 2);
    let mut e = event(2);
    assert!(eot::fit_for_headend(&mut e, 1));
    assert!(!e.contains_key("telemetry") && e["event_id"] == "e1");
    let mut e = event(2);
    assert!(!eot::fit_for_headend(&mut e, 2));
    assert!(e.contains_key("telemetry"));
    let mut e = event(1);
    assert!(!eot::fit_for_headend(&mut e, 1));
    assert!(e.contains_key("telemetry"));
}
