//! Filters, resampling and burst recordings.

use super::*;
use crate::bursts::*;
use crate::sim::{Rng, World};
use num_complex::Complex32;
use serde_json::json;

#[test]
fn fir_decimator_is_block_size_independent() {
    let mut rng = Rng::new(3);
    let x: Vec<Complex32> = (0..10_000)
        .map(|_| Complex32::new(rng.standard_normal() as f32, rng.standard_normal() as f32)).collect();
    let h = lowpass(63, 0.1, 1.0);
    let whole = FirDecimator::new(&h, 7).process(&x);
    let mut f = FirDecimator::new(&h, 7);
    let mut parts = vec![];
    let mut i = 0;
    for n in [1, 13, 500, 7, 2000, 3, 7476] {
        parts.extend(f.process(&x[i..i + n]));
        i += n;
    }
    assert_eq!(parts.len(), whole.len());
    for (a, b) in parts.iter().zip(&whole) {
        assert!((a - b).norm() <= 1e-5);
    }
}

#[test]
fn shift_decimate_rates() {
    for (rate, decim) in [(2_400_000.0, 5), (2_880_000.0, 6)] {
        let mut sd = ShiftDecimator::new(rate, 200_000.0);
        assert!(sd.out_rate == 48_000.0 && sd.stage2.decim == decim);
        let n = (rate * 0.1) as usize;
        let x: Vec<Complex32> = (0..n).map(|k| {
            let a = 2.0 * std::f64::consts::PI * (200_000.0 + 1_000.0) * (k as f64 / rate);
            Complex32::new(a.cos() as f32, a.sin() as f32)
        }).collect();
        let y = sd.process(&x);
        assert!((y.len() as i64 - 4800).abs() <= 2);
        // peak bin of a plain FFT
        let mut buf: Vec<rustfft::num_complex::Complex<f64>> =
            y[200..].iter().map(|c| rustfft::num_complex::Complex::new(c.re as f64, c.im as f64)).collect();
        let m = buf.len();
        rustfft::FftPlanner::new().plan_fft_forward(m).process(&mut buf);
        let k = (0..m).max_by(|&a, &b| buf[a].norm().total_cmp(&buf[b].norm())).unwrap();
        let f = if k <= (m - 1) / 2 { k as f64 } else { k as f64 - m as f64 } * 48_000.0 / m as f64;
        assert!((f - 1_000.0).abs() <= 15.0, "{f}");
    }
}

fn fm_burst_world(tone: f64) -> World {
    let w = World::default();
    w.carrier(457.9375, 25.0, 2.0, Some(2.4), tone, 3_000.0, "telemetry");
    w
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).map(|r| r.map(|e| e.unwrap().path()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn read_cf32(p: &Path) -> Vec<Complex32> {
    std::fs::read(p).unwrap().chunks_exact(8).map(|b| Complex32::new(
        f32::from_le_bytes([b[0], b[1], b[2], b[3]]), f32::from_le_bytes([b[4], b[5], b[6], b[7]]))).collect()
}

#[test]
fn burst_saved_as_cf32_and_json() {
    let tmp = TmpDir::new("burst-save");
    let (mut src, events) = source(tmp.path(), &eot_dev());
    run(&mut src, &fm_burst_world(1_200.0), 0.0, 3.2);
    let c = closed(&events);
    assert_eq!(c.len(), 1);
    let fs = files(&tmp.path().join("bursts"));
    let ext: Vec<String> = fs.iter().map(|f| f.extension().unwrap().to_string_lossy().into()).collect();
    assert_eq!(ext, vec!["cf32", "json"]);
    assert!(fs[0].file_name().unwrap().to_string_lossy().ends_with("_457.9375.cf32"));
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(&fs[1]).unwrap()).unwrap();
    assert!(meta["sample_rate"] == 48_000.0 && meta["freq_mhz"] == 457.9375);
    assert_eq!(meta["event_id"], c[0]["event_id"]);
    assert_eq!(meta["decoded"], json!([]));
    assert!((meta["sdr_center_mhz"].as_f64().unwrap() - 458.1385).abs() < 1e-9);
    let iq = read_cf32(&fs[0]);
    assert_eq!(iq.len() as u64, meta["samples"].as_u64().unwrap());
    let d = meta["duration_s"].as_f64().unwrap();
    let secs = iq.len() as f64 / 48_000.0;
    assert!(d + 0.45 <= secs && secs <= d + 0.6, "{d} {secs}");
    // indented JSON, two spaces, keys in the reference order
    let text = std::fs::read_to_string(&fs[1]).unwrap();
    assert!(text.starts_with("{\n  \"freq_mhz\": 457.9375,\n  \"label\": \"EOT (end of train)\",\n  \"ch\": 9,\n"));
    assert!(text.ends_with("\"decoded\": []\n}\n"));
}

#[test]
fn saved_burst_demodulates_to_tone() {
    let tmp = TmpDir::new("burst-tone");
    let (mut src, _) = source(tmp.path(), &eot_dev());
    run(&mut src, &fm_burst_world(1_200.0), 0.0, 3.2);
    let fs = files(&tmp.path().join("bursts"));
    let iq = read_cf32(&fs[0]);
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(&fs[1]).unwrap()).unwrap();
    let i0 = ((meta["burst_start_ts"].as_f64().unwrap() - meta["t_first_sample"].as_f64().unwrap()) * 48_000.0) as usize + 2400;
    let seg = &iq[i0..i0 + 9600];
    let mut audio: Vec<f64> = seg.windows(2).map(|w| (w[1] * w[0].conj()).arg() as f64).collect();
    let mean = audio.iter().sum::<f64>() / audio.len() as f64;
    audio.iter_mut().for_each(|a| *a -= mean);
    let f = peak_hz_real(&audio, 48_000.0);
    assert!((f - 1_200.0).abs() <= 10.0, "{f}");
}

#[test]
fn ring_buffer_keeps_record_keep() {
    let tmp = TmpDir::new("ring");
    let rec = BurstRecorder::new(2_400_000.0, 0.0, 457.9375, "EOT", json!(9), Some(tmp.path()), 3, 4.0, None, true);
    let x = vec![Complex32::new(1.0, 0.0); 1000];
    for i in 0..6 {
        let t = 100.0 + i as f64;
        rec.save(&x, t, &json!({"start_ts": t, "end_ts": t + 0.1, "event_id": i.to_string()}), 10.0, &[]).unwrap();
    }
    let fs = files(tmp.path());
    assert_eq!(fs.iter().filter(|p| p.extension().unwrap() == "cf32").count(), 3);
    assert_eq!(fs.iter().filter(|p| p.extension().unwrap() == "json").count(), 3);
}

#[test]
fn extract_clips_to_buffer() {
    let mut rec = BurstRecorder::new(2_400_000.0, 0.0, 457.9375, "EOT", json!(9), None, 3, 4.0, None, true);
    assert_eq!(rec.extract(1.0, 2.0), (vec![], 1.0));
    rec.feed(&vec![Complex32::new(1.0, 0.0); 240_000], 0.1); // 0.1 s: 4800 samples at 48 kS/s
    assert_eq!(rec.buf.len(), 4800);
    let (s, t_first) = rec.extract(-5.0, 0.05);
    assert!((t_first - (0.1 - 4799.0 / 48_000.0)).abs() < 1e-12);
    assert_eq!(s.len(), ((0.05 - t_first) * 48_000.0).round() as usize + 1);
}

#[test]
fn filters_match_reference_values() {
    // Blackman and Hann windows of 5 points, lowpass(5, 0.1, 1.0) and boxcar3(10), reference values
    let b = blackman(5);
    for (a, e) in b.iter().zip([-1.3877787807814457e-17, 0.34, 1.0, 0.34, -1.3877787807814457e-17]) {
        assert!((a - e).abs() < 1e-15);
    }
    assert_eq!(hanning(5), vec![0.0, 0.5, 1.0, 0.5, 0.0]);
    let lp = lowpass(5, 0.1, 1.0);
    assert!((lp.iter().sum::<f64>() - 1.0).abs() < 1e-12);
    let bx = boxcar3(10);
    assert_eq!(bx.len(), 28);
    assert!((bx.iter().sum::<f64>() - 1.0).abs() < 1e-12);
}

#[test]
fn burst_json_matches_reference_text() {
    // float text checked against the reference JSON writer
    for (x, want) in [(0.0, "0.0"), (-0.0, "-0.0"), (1e-05, "1e-05"), (0.0001, "0.0001"), (1e16, "1e+16"),
                      (9999999999999998.0, "9999999999999998.0"), (1.5e16, "1.5e+16"), (123.456, "123.456"),
                      (1760000000.123, "1760000000.123"), (48000.0, "48000.0"), (-2.5e-07, "-2.5e-07"),
                      (1e100, "1e+100"), (0.1, "0.1")] {
        assert_eq!(value_f64(x), want);
    }
    let tmp = TmpDir::new("burst-json");
    let rec = BurstRecorder::new(2_400_000.0, 0.0, 457.9375, "caf\u{e9} \u{1F600}", json!(9), Some(tmp.path()),
                                 3, 4.0, Some(458.1385), true);
    let pkt = {
        let mut p = crate::eot::fields(&crate::eot::build_payload(75618, 88), 0);
        p.insert("t_offset_s".into(), json!(0.3346));
        p
    };
    let x = vec![Complex32::new(1.0, 0.0); 10];
    rec.save(&x, 100.0, &json!({"start_ts": 100.5, "end_ts": 101.0, "event_id": "e"}), 12.34, &[pkt]).unwrap();
    let text = std::fs::read_to_string(files(tmp.path()).into_iter().find(|p| p.extension().unwrap() == "json")
        .unwrap()).unwrap();
    assert!(text.contains("\"label\": \"caf\\u00e9 \\ud83d\\ude00\",\n"), "{text}");
    // the reference key order inside each decoded packet
    let want = "  \"decoded\": [\n    {\n      \"protocol\": \"eot\",\n      \"chaining\": 3,\n      \
                \"battery_condition\": 3,\n      \"message_type\": 0,\n      \"unit_id\": 75618,\n      \
                \"brake_pipe_psi\": 88,\n      \"battery_charge_raw\": 0,\n      \"valve_circuit_ok\": true,\n      \
                \"confirmation\": true,\n      \"turbine\": true,\n      \"motion\": true,\n      \
                \"marker_battery_weak\": false,\n      \"marker_light\": false,\n      \
                \"decoder\": \"railspotting-eot-1\",\n      \"crc_ok\": true,\n      \"corrected_bits\": 0,\n      \
                \"t_offset_s\": 0.3346\n    }\n  ]\n}\n";
    assert!(text.ends_with(want), "{text}");
    assert!(text.contains("\"duration_s\": 0.5,\n  \"snr_peak_db\": 12.3,\n"), "{text}");
}
