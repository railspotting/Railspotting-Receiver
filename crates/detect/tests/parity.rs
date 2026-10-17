//! Parity with the reference fixtures: fixtures/detect (recorded reference outputs), the real
//! clips in fixtures/local/clips and the v1.5 vectors in fixtures/api/vectors.json.

use railspotting_detect::*;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn expected() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixtures().join("detect/expected.json")).unwrap()).unwrap()
}

fn clips_dir() -> Option<PathBuf> {
    let d = fixtures().join("local/clips");
    if d.is_dir() {
        Some(d)
    } else {
        eprintln!("skipped: {} missing (git-ignored local fixtures)", d.display());
        None
    }
}

fn load_wav(path: &Path) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).unwrap();
    r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
}

/// `audio` scaled so its peak is 0.9 (a peak below 1e-9 counts as 1e-9), the factor computed in
/// float64 and applied in float32.
fn normalized(audio: &[f32]) -> Vec<f32> {
    let peak = audio.iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
    let k = (0.9 / peak.max(1e-9)) as f32;
    audio.iter().map(|v| v * k).collect()
}

fn template() -> Template {
    let mut t = DetectorTemplates::new(fixtures().join("detect/template"));
    t.reload();
    assert_eq!(t.items.len(), 1);
    t.items.remove(0)
}

fn max_diff(a: &[f32], b: &[f32]) -> f32 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

#[test]
fn template_files_round_trip_byte_for_byte() {
    let dir = fixtures().join("detect/template");
    let t = template();
    assert_eq!(t.name(), "ns-475.8");
    assert_eq!(t.split_frames(), Some(260));
    assert_eq!(t.opening_end_frames(), Some(131));
    assert_eq!(t.features.cols, 24);
    // the .npy and the indented JSON exactly as the reference files have them
    let npy_bytes = std::fs::read(dir.join("ns-475.8.npy")).unwrap();
    assert_eq!(npy::to_bytes(&[t.features.len(), 24], &t.features.data), npy_bytes);
    let json_text = std::fs::read_to_string(dir.join("ns-475.8.json")).unwrap();
    assert_eq!(jsonfmt::dumps_indent(&Value::Object(t.info.clone()), 2) + "\n", json_text);
    assert_eq!(all_clear_length(&t.info), Some(9.2));
}

#[test]
fn features_and_build_match_reference() {
    let Some(clips) = clips_dir() else { return };
    let exp = expected();
    let clip = exp["features_clip"].as_str().unwrap();
    let audio = normalized(&load_wav(&clips.join(format!("{clip}_voice.wav"))));
    let want = npy::load(&fixtures().join("detect/features_first_detector_clip.npy")).unwrap();
    let got = features(&audio);
    assert_eq!(vec![got.len(), got.cols], want.shape);
    let d = max_diff(&got.data, &want.data);
    eprintln!("features: max abs difference {d:e}");
    assert!(d < 1e-3);

    let six: Vec<Vec<f32>> = exp["build_clips"].as_array().unwrap().iter()
        .map(|c| load_wav(&clips.join(format!("{}_voice.wav", c.as_str().unwrap()))))
        .collect();
    let want = npy::load(&fixtures().join("detect/build_six.npy")).unwrap();
    let got = build(&six).unwrap();
    assert_eq!(vec![got.len(), got.cols], want.shape);
    let d = max_diff(&got.data, &want.data);
    eprintln!("build: max abs difference {d:e}");
    assert!(d < 1e-3);

    let profile = rhythm_profile(&six);
    let want: Vec<f64> = exp["rhythm_profile_six"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!(profile.len(), want.len());
    let d = profile.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
    eprintln!("rhythm profile: max abs difference {d:e}");
    assert!(d < 0.01);

    let q = quiet_frames(&six, 260);
    let want: Vec<i64> = exp["quiet_frames_six"].as_array().unwrap().iter().map(|v| v.as_i64().unwrap()).collect();
    assert_eq!(q, want);
}

fn compare_assessment(clip: &str, got: &Value, want: &Value, worst: &mut f64) {
    let (g, w) = (got.as_object().unwrap(), want.as_object().unwrap());
    let gk: Vec<&String> = g.keys().collect();
    let wk: Vec<&String> = w.keys().collect();
    assert_eq!(gk, wk, "{clip}: keys");
    for (k, wv) in w {
        let gv = &g[k];
        match k.as_str() {
            "score" | "rhythm" => {
                let d = (gv.as_f64().unwrap() - wv.as_f64().unwrap()).abs();
                *worst = worst.max(d);
                assert!(d <= 0.02 + 1e-9, "{clip}: {k} {gv} against {wv}");
            }
            "opening" | "ending" => {
                let (ga, wa) = (gv.as_array().unwrap(), wv.as_array().unwrap());
                assert_eq!(ga.len(), wa.len(), "{clip}: {k}");
                for (x, y) in ga.iter().zip(wa) {
                    assert_eq!(x.is_null(), y.is_null(), "{clip}: {k} {gv} against {wv}");
                    if let (Some(x), Some(y)) = (x.as_f64(), y.as_f64()) {
                        *worst = worst.max((x - y).abs());
                        assert!((x - y).abs() <= 0.02 + 1e-9, "{clip}: {k} {gv} against {wv}");
                    }
                }
            }
            _ => assert_eq!(gv, wv, "{clip}: {k}"),
        }
    }
}

#[test]
fn assessments_match_on_every_clip() {
    let Some(clips) = clips_dir() else { return };
    let exp = expected();
    let t = template();
    let (split, o_end) = (t.split_frames(), t.opening_end_frames());
    let mut worst = 0.0f64;
    let mut n = 0;
    for c in exp["clips"].as_array().unwrap() {
        let name = c["clip"].as_str().unwrap();
        let audio = normalized(&load_wav(&clips.join(format!("{name}_voice.wav"))));
        let f = features(&audio);
        let a = assess(t.features.view(), f.view(), split, o_end);
        compare_assessment(name, &a.to_json(), &c["assess"], &mut worst);
        let mut b = a.clone();
        if b.kind.is_some() {
            check_length_and_rhythm(&t, &mut b, &audio, split);
        }
        compare_assessment(name, &b.to_json(), &c["after_length_and_rhythm"], &mut worst);
        n += 1;
    }
    eprintln!("{n} clips: kinds exact, worst score difference {worst:.4}");
    assert_eq!(n, 53);
}

#[test]
fn text_and_objects_match_reference() {
    let exp = expected();
    for (s, want) in exp["milepost_from_text"].as_object().unwrap() {
        assert_eq!(milepost_from_text(s), want.as_f64(), "{s:?}");
    }
    for (s, want) in exp["parse"].as_object().unwrap() {
        let got = parse(s, true);
        assert_eq!(got.as_ref().unwrap_or(&Value::Null), want, "{s:?}");
        if let Some(g) = got {   // same keys in the same order
            assert_eq!(jsonfmt::dumps_compact(&g), jsonfmt::dumps_compact(want));
        }
    }
    for (s, want) in exp["parse_untrusted"].as_object().unwrap() {
        assert_eq!(parse(s, false).unwrap_or(Value::Null), *want, "{s:?}");
    }
    let a = Assessment { score: 0.81, opening: Some(vec![None, Some(0.8), Some(0.8), Some(0.8)]),
                         ending: Some(vec![Some(0.6), Some(0.55), Some(0.8)]), ..Default::default() };
    let obj = detector_object("ns-475.8", "ns", 475.8, "no_defects", "sound", &a, 9.2, 9.2, None, None, None);
    assert_eq!(jsonfmt::dumps_compact(&obj), jsonfmt::dumps_compact(&exp["detector_object_example"]));
    let a = Assessment { score: 0.79, opening: Some(vec![Some(0.8)]), ending: Some(vec![Some(0.16)]),
                         rhythm: Some(0.31), ..Default::default() };
    let obj = detector_object("ns-475.8", "NS", 475.8, "different_message", "opening", &a, 11.4, 9.2, None,
                              Some("defect"), None);
    assert_eq!(validate_detector(&obj), Vec::<String>::new());
    assert_eq!(exp["validate_examples"]["ok"], serde_json::json!([]));
}

#[test]
fn v1_5_vectors() {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(fixtures().join("api/vectors.json")).unwrap()).unwrap();
    let v15 = &v["v1_5"];
    for obj in v15["valid_detector_objects"].as_array().unwrap() {
        assert_eq!(validate_detector(obj), Vec::<String>::new(), "{obj}");
    }
    for row in v15["result_order"].as_array().unwrap() {
        let a = Assessment { kind: row["kind"].as_str().map(String::from), ..Default::default() };
        let (result, _) = contract_result(&a, row["length_s"].as_f64().unwrap(), row["all_clear_s"].as_f64().unwrap());
        assert_eq!(result, row["result"].as_str().unwrap(), "{row}");
    }
}

#[test]
fn add_writes_the_production_template_byte_for_byte() {
    let Some(clips) = clips_dir() else { return };
    let exp = expected();
    let six: Vec<Vec<f32>> = exp["build_clips"].as_array().unwrap().iter()
        .map(|c| load_wav(&clips.join(format!("{}_voice.wav", c.as_str().unwrap()))))
        .collect();
    let want = template();
    let dir = std::env::temp_dir().join(format!("rs-detect-add-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut t = DetectorTemplates::new(&dir);
    let text = want.info["text"].as_str().unwrap();
    let info = t.add("ns-475.8", text, &six, Some(want.info["parsed"].clone()), Some(5.2), Some(2.62)).unwrap();
    assert_eq!(info, want.info);
    let src = fixtures().join("detect/template");
    for f in ["ns-475.8.json", "ns-475.8.npy"] {
        assert!(std::fs::read(dir.join(f)).unwrap() == std::fs::read(src.join(f)).unwrap(), "{f} differs");
    }
    let _ = std::fs::remove_dir_all(dir);
}
