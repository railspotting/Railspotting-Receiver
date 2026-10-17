//! Parity with the reference fixtures: fixtures/eot/expected.json exactly, and fixtures/dsp/*.json
//! fed from fixtures/local/dsp/*.cu8 (u8 I/Q pairs, b / 127.5 - 1, one block per call).

use super::test_eot::read_cf32;
use super::*;
use crate::eot::{self, Payload};
use crate::sdr::SdrSource;
use railspotting_common::FakeClock;
use serde_json::json;
use std::io::Read;

fn load(p: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

fn bits_of(v: &Value) -> Vec<u8> {
    v.as_array().unwrap().iter().map(|b| b.as_u64().unwrap() as u8).collect()
}

fn as_value(pkts: &[Map<String, Value>]) -> Value {
    Value::Array(pkts.iter().cloned().map(Value::Object).collect())
}

#[test]
fn eot_expected_exactly() {
    let d = fixtures().join("eot");
    let exp = load(&d.join("expected.json"));
    let real = eot::decode32(&read_cf32(&d.join(exp["real"]["file"].as_str().unwrap())), 48_000.0);
    assert_eq!(as_value(&real), exp["real"]["decoded"]);
    let noise = eot::decode32(&read_cf32(&d.join(exp["noise"]["file"].as_str().unwrap())), 48_000.0);
    assert_eq!(as_value(&noise), exp["noise"]["decoded"]);

    // the payloads the reference fixtures were built from
    let payloads = [
        Payload::new(75618, 88),
        Payload { motion: false, ..Payload::new(4157, 0) },
        Payload { marker_light: true, chaining: 0, message_type: 7, ..Payload::new(131071, 127) },
        Payload { valve_circuit_ok: false, turbine: false, confirmation: false, ..Payload::new(1, 45) },
    ];
    let cases = exp["synthetic"].as_array().unwrap();
    assert_eq!(cases.len(), payloads.len());
    for (case, p) in cases.iter().zip(&payloads) {
        let bits = p.bits();
        assert_eq!(bits, bits_of(&case["payload_bits"]));
        assert_eq!(eot::packet_bits(&bits), bits_of(&case["packet_bits"]));
        assert_eq!(eot::check_bits(&bits[..45]), bits_of(&case["check_bits"]));
        assert_eq!(json!(eot::check_ok(&bits)), case["check_ok"]);
        assert_eq!(Value::Object(eot::fields(&bits, 0)), case["fields"]);
        let mut flipped = bits.clone();
        flipped[12] ^= 1;
        let (fixed, idx) = eot::correct_one(&flipped).unwrap();
        assert_eq!(json!([fixed, idx]), case["flipped12_corrected"]);
        let iq = read_cf32(&d.join(case["file"].as_str().unwrap()));
        assert_eq!(as_value(&eot::decode32(&iq, 48_000.0)), case["decoded"], "{}", case["file"]);
    }
}

struct Diff {
    start: f64,
    end: f64,
    rssi: i64,
    floor: f64,
    reference: f64,
}

fn run_dsp_fixture(name: &str) -> Option<Diff> {
    let exp = load(&fixtures().join("dsp").join(format!("{name}.json")));
    let cu8 = fixtures().join("local/dsp").join(format!("{name}.cu8"));
    if !cu8.exists() {
        eprintln!("skip: {} missing (fixtures/local is not committed)", cu8.display());
        return None;
    }
    let tmp = TmpDir::new(name);
    let (events, emit) = events_sink();
    let clock = FakeClock::new(0.0);
    let mut src = SdrSource::new(&exp["device"], &exp["sdr_settings"], tmp.path(), emit, None, clock.clock());
    let chans = channels(&exp["plan"]);
    src.set_plan(&chans, Some(&exp["plan"]["reference"]));
    assert_eq!(src.block as u64, exp["block_samples"].as_u64().unwrap());
    assert_eq!(json!(src.center), exp["center_mhz"]);
    assert_eq!(json!(src.plan.window), exp["window_mhz"]);

    let b = src.block;
    let dur = b as f64 / src.rate;
    let mut f = std::io::BufReader::new(std::fs::File::open(&cu8).unwrap());
    let mut raw = vec![0u8; 2 * b];
    let mut iq = Vec::with_capacity(b);
    let mut refs: Vec<(u64, Value)> = vec![];
    let (mut t, mut k) = (0.0f64, 0u64);
    while f.read_exact(&mut raw).is_ok() {
        iq.clear();
        crate::cu8_to_iq(&raw, &mut iq);
        src.process_block(&iq, t + dur);
        if let Some(r) = &src.reference {
            refs.push((k, r.clone()));
        }
        t += dur;
        k += 1;
    }
    assert_eq!(k, exp["blocks"].as_u64().unwrap());

    let got = all(&events);
    let want = exp["events"].as_array().unwrap();
    let key = |e: &Map<String, Value>| (e["kind"].clone(), e["ch"].clone(), e["freq"].clone(), e["state"].clone());
    let gk: Vec<_> = got.iter().map(key).collect();
    let wk: Vec<_> = want.iter().map(|e| key(e.as_object().unwrap())).collect();
    assert_eq!(gk, wk, "{name}: events differ\n got {got:?}");
    let mut d = Diff { start: 0.0, end: 0.0, rssi: 0, floor: 0.0, reference: 0.0 };
    for (g, w) in got.iter().zip(want) {
        let df = |k: &str| (g[k].as_f64().unwrap_or(0.0) - w[k].as_f64().unwrap_or(0.0)).abs();
        d.start = d.start.max(df("start_ts"));
        d.end = d.end.max(df("end_ts"));
        for k in ["peak_rssi", "avg_rssi"] {
            d.rssi = d.rssi.max((g[k].as_i64().unwrap() - w[k].as_i64().unwrap()).abs());
        }
        assert_eq!(g.get("truncated"), w.get("truncated"), "{name}");
        assert_eq!(g["samples"], w["samples"], "{name}: windows above the floor");
        assert_eq!(g.get("telemetry"), w.get("telemetry"), "{name}: telemetry");
    }
    assert!(d.start <= 0.05 && d.end <= 0.05, "{name}: start/end off by {} / {}", d.start, d.end);
    assert!(d.rssi as f64 <= 1.5, "{name}: rssi off by {}", d.rssi);

    let st = src.status();
    let fin = &exp["final"];
    assert_eq!(st["blocks"], fin["blocks"]);
    assert_eq!(st["clipping"].as_f64(), fin["clipping"].as_f64());
    let (gc, wc) = (st["channels"].as_array().unwrap(), fin["channels"].as_array().unwrap());
    assert_eq!(gc.len(), wc.len());
    for (g, w) in gc.iter().zip(wc) {
        assert_eq!((&g["freq"], &g["ch"], &g["kind"], &g["receiving"]), (&w["freq"], &w["ch"], &w["kind"], &w["receiving"]));
        d.floor = d.floor.max((g["floor_db"].as_f64().unwrap() - w["floor_db"].as_f64().unwrap()).abs());
    }
    assert!(d.floor <= 1.0, "{name}: floors off by {}", d.floor);
    match (&st["reference"], &fin["reference"]) {
        (Value::Null, Value::Null) => {}
        (g, w) => {
            assert_eq!((&g["name"], &g["freq"]), (&w["name"], &w["freq"]));
            d.reference = (g["snr_db"].as_f64().unwrap() - w["snr_db"].as_f64().unwrap()).abs();
            assert!((g["ts"].as_f64().unwrap() - w["ts"].as_f64().unwrap()).abs() < 1e-9);
        }
    }
    for w in exp["reference_samples"].as_array().unwrap() {
        let (_, g) = refs.iter().find(|(k, _)| *k == w["block"].as_u64().unwrap()).expect("reference sample");
        d.reference = d.reference.max((g["snr_db"].as_f64().unwrap() - w["snr_db"].as_f64().unwrap()).abs());
    }
    assert!(d.reference <= 1.0, "{name}: reference off by {}", d.reference);
    match (&src.last_eot, &exp["last_eot"]) {
        (None, Value::Null) => {}
        (Some(g), w) => assert_eq!(g, w, "{name}: last_eot"),
        (None, w) => panic!("{name}: no last_eot, want {w}"),
    }
    eprintln!("{name}: {} events match; max |start| {:.3} s, |end| {:.3} s, rssi {} dB, floors {:.2} dB, \
               reference {:.2} dB", got.len(), d.start, d.end, d.rssi, d.floor, d.reference);
    Some(d)
}

#[test]
fn dsp_vhf_voice() {
    run_dsp_fixture("vhf_voice");
}

#[test]
fn dsp_eot_bursts() {
    run_dsp_fixture("eot_bursts");
}

#[test]
fn dsp_eot_noise_rise() {
    run_dsp_fixture("eot_noise_rise");
}
