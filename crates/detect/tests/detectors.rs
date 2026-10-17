//! Known defect detector announcements: the opening identifies the detector, the ending says
//! whether it is the all-clear. Covers everything below the speech model, and the v1.5
//! detector objects.
//!
//! The synthetic voice is `speech()` below, driven by a small local random generator, so the
//! passages differ in detail from the reference recordings but not in kind.

use railspotting_detect::*;
use serde_json::{json, Value};
use std::path::PathBuf;

const SR: usize = 16_000;
const ALL_CLEAR: &str = "Norfolk Southern, milepost 475.8, no defects. Repeat, no defects.";
const SPLIT_S: f64 = 5.2;
const OPENING_END_S: f64 = 2.6;

fn parsed() -> Value {
    json!({"type": "defect_detector", "railroad": "NS", "milepost": 475.8, "defects": false})
}

/// splitmix64 with uniform and Box-Muller normal draws.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * ((self.next() >> 11) as f64 / (1u64 << 53) as f64)
    }
    fn normal(&mut self, sd: f64) -> f64 {
        let u1 = self.uniform(f64::MIN_POSITIVE, 1.0);
        let u2 = self.uniform(0.0, 1.0);
        sd * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// A fixed 'synthetic voice' passage: the same syllable sweeps every time for one seed.
fn speech(seed: u64, seconds: f64) -> Vec<f32> {
    let mut rng = Rng::new(seed);
    let n = (seconds * SR as f64) as usize;
    let mut out = vec![0.0f64; n];
    let mut pos = 0.05;
    while pos < seconds - 0.3 {
        let dur = rng.uniform(0.12, 0.35);
        let (f0, f1) = (rng.uniform(300.0, 900.0), rng.uniform(300.0, 900.0));
        let mut ph = 0.0;
        for (i, o) in out.iter_mut().enumerate() {
            let t = i as f64 / SR as f64;
            if t >= pos && t < pos + dur {
                ph += 2.0 * std::f64::consts::PI * (f0 + (f1 - f0) * (t - pos) / dur) / SR as f64;
                for (h, a) in [(1.0, 1.0), (2.0, 0.5), (3.0, 0.3), (4.0, 0.2)] {
                    *o += a * (h * ph).sin();
                }
            }
        }
        pos += dur + rng.uniform(0.03, 0.25);
    }
    out.into_iter().map(|v| v as f32).collect()
}

fn cat(parts: &[&[f32]]) -> Vec<f32> {
    parts.concat()
}

struct World {
    rng: Rng,
    opening: Vec<f32>,    // "Norfolk Southern, milepost 475.8,"
    no_defects: Vec<f32>, // "no defects. Repeat, no defects."
    wording: Vec<f32>,    // "Norfolk Southern, milepost"
    own_number: Vec<f32>, // "475.8,"
}

impl World {
    fn new() -> Self {
        World { rng: Rng::new(1), opening: speech(475, SPLIT_S), no_defects: speech(0, 4.5),
                wording: speech(100, 2.6), own_number: speech(4758, 2.6) }
    }
    fn message(&self, ending: &[f32]) -> Vec<f32> {
        cat(&[&self.opening, ending])
    }
    fn announce(&self, number: &[f32], ending: Option<&[f32]>) -> Vec<f32> {
        cat(&[&self.wording, number, ending.unwrap_or(&self.no_defects)])
    }
    /// As the receiver hears it: noise, and the call may start late.
    fn on_air_with(&mut self, clean: &[f32], snr_db: f64, lead_s: f64, tail_s: f64) -> Vec<f32> {
        let mut x = clean[((lead_s * SR as f64) as usize).min(clean.len())..].to_vec();
        x.extend(std::iter::repeat_n(0.0, (tail_s * SR as f64) as usize));
        let p = clean.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / clean.len() as f64;
        let sd = (p / 10f64.powf(snr_db / 10.0)).sqrt();
        for v in x.iter_mut() {
            *v += self.rng.normal(sd) as f32;
        }
        let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        x.iter().map(|v| v / peak * 0.9).collect()
    }
    fn on_air(&mut self, clean: &[f32]) -> Vec<f32> {
        self.on_air_with(clean, 6.0, 0.0, 0.3)
    }
    fn recs(&mut self) -> Vec<Vec<f32>> {
        let m = self.message(&self.no_defects.clone());
        (0..5).map(|_| self.on_air(&m)).collect()
    }
}

fn split_frames() -> i64 {
    (SPLIT_S * SR as f64 / 320.0).round() as i64
}

fn kind(tmpl: &Feats, audio: &[f32], split: Option<i64>) -> Option<String> {
    assess(tmpl.view(), features(audio).view(), Some(split.unwrap_or_else(split_frames)), None).kind
}

fn tmp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rs-detect-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn all_clear_other_message_and_other_speech() {
    let mut w = World::new();
    let tmpl = build(&w.recs()).unwrap();
    let nd = w.no_defects.clone();
    let clear = w.on_air(&w.message(&nd));
    assert_eq!(kind(&tmpl, &clear, None).as_deref(), Some("all_clear"));
    let late = w.on_air_with(&w.message(&nd), 6.0, 0.6, 0.3); // caught late
    assert_eq!(kind(&tmpl, &late, None).as_deref(), Some("all_clear"));
    for seed in [11, 12, 13] {
        // same detector, a different message
        let x = w.on_air(&w.message(&speech(seed, 7.0)));
        assert_eq!(kind(&tmpl, &x, None).as_deref(), Some("other"), "seed {seed}");
    }
    for seed in [21, 22, 23] {
        // someone else talking
        let x = w.on_air(&speech(seed, 10.0));
        assert_eq!(kind(&tmpl, &x, None), None, "seed {seed}");
    }
    let x = w.on_air(&speech(9, 3.0));
    assert_eq!(kind(&tmpl, &x, None), None);
}

/// A defect report starts like the all-clear and then differs. A real one differs right after
/// the milepost; the last part of a real ending is mostly the silent tail after "no defects", so
/// a change only there isn't tested.
#[test]
fn changed_ending_is_never_the_all_clear() {
    for keep in [0.0, 0.1, 0.3, 0.5, 0.7] {
        for extra_s in [0.0, 4.0] {
            let mut w = World::new();
            let tmpl = build(&w.recs()).unwrap();
            let nd = &w.no_defects;
            let cut = (nd.len() as f64 * keep) as usize;
            let other = speech(31, 20.0);
            let ending = cat(&[&nd[..cut], &other[..nd.len() - cut + (extra_s * SR as f64) as usize]]);
            let x = w.on_air(&w.message(&ending));
            assert_eq!(kind(&tmpl, &x, None).as_deref(), Some("other"), "keep {keep} extra {extra_s}");
        }
    }
}

#[test]
fn cut_off_ending_is_flagged_but_not_all_clear() {
    let mut w = World::new();
    let tmpl = build(&w.recs()).unwrap();
    let full = w.message(&w.no_defects);
    let x = w.on_air_with(&full[..(full.len() as f64 * 0.8) as usize], 6.0, 0.0, 0.0);
    assert_eq!(kind(&tmpl, &x, None).as_deref(), Some("other"));
}

#[test]
fn template_without_split_checks_the_whole_announcement() {
    let mut w = World::new();
    let tmpl = build(&w.recs()).unwrap();
    let x = w.on_air(&w.message(&w.no_defects));
    assert_eq!(kind(&tmpl, &x, Some(0)).as_deref(), Some("all_clear"));
    let x = w.on_air(&w.message(&speech(11, 7.0)));
    assert_eq!(kind(&tmpl, &x, Some(0)), None);
}

#[test]
fn announcement_text() {
    assert_eq!(opening_text(ALL_CLEAR), "Norfolk Southern, milepost 475.8");
    assert_eq!(opening_text("CSX detector milepost 12.3 no defects"), "CSX detector milepost 12.3");
    assert_eq!(wording_text(ALL_CLEAR), "Norfolk Southern, milepost");
    assert_eq!(ending_text(ALL_CLEAR), "no defects. Repeat, no defects.");
    assert_eq!(ending_text("Norfolk Southern, milepost 475.8"), "");
    assert_eq!(opening_text("Norfolk Southern milepost 475.8. NO DEFECTS"), "Norfolk Southern milepost 475.8");
}

#[test]
fn add_checks() {
    let mut w = World::new();
    let recs = w.recs();
    let dir = tmp_dir("checks");
    let mut t = DetectorTemplates::new(&dir);
    assert!(t.add("../evil", ALL_CLEAR, &recs, None, None, None).is_err());
    let e = t.add("x", ALL_CLEAR, &recs[..3], None, Some(9.5), None).unwrap_err(); // past the end
    assert_eq!(e, "split must be between 1 s and 9.0 s");
    let e = t.add("x", ALL_CLEAR, &recs[..3], None, Some(SPLIT_S), Some(5.0)).unwrap_err();
    assert_eq!(e, "opening end needs a split, and must be between 0.5 s and the split");
    assert!(t.add("x", ALL_CLEAR, &recs[..0], None, None, None).is_err());
    assert!(t.list().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// The opening's gaps measure this transmission's hiss; removing it from the rest cuts the
/// hiss without removing the speech.
#[test]
fn noise_filter_trained_on_the_opening() {
    let mut w = World::new();
    let tmpl = build(&w.recs()).unwrap();
    let clean_opening = w.opening.clone();
    let mut hiss = |n: usize| -> Vec<f32> { (0..n).map(|_| w.rng.normal(0.05) as f32).collect() };
    let rest_speech: Vec<f32> = speech(41, 4.0).iter().map(|v| v * 0.3).collect();
    let h = hiss(5 * SR);
    let rest: Vec<f32> = cat(&[&vec![0.0; SR], &rest_speech]).iter().zip(&h).map(|(a, b)| a + b).collect();
    let h2 = hiss(clean_opening.len());
    let opening: Vec<f32> = clean_opening.iter().zip(&h2).map(|(a, b)| a * 0.3 + b).collect();
    let call = cat(&[&opening, &rest]);
    let a = assess(tmpl.view(), features(&call).view(), Some(split_frames()), None);
    let noise = opening_noise_features(&call, tmpl.view(), split_frames(), a.opening_frame.unwrap()).unwrap();
    let out = remove_noise(&rest, &noise);
    assert_eq!(out.len(), rest.len());
    let db = |x: &[f32]| 10.0 * (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).log10();
    assert!(db(&out[..SR]) < db(&rest[..SR]) - 8.0); // the silent second
    assert!(db(&out[SR..]) > db(&rest[SR..]) - 3.0); // the speech
}

#[test]
fn longer_than_the_all_clear_is_never_it() {
    // "...no defects, repeat, no defects" followed by more words
    let mut w = World::new();
    let recs = w.recs();
    let dir = tmp_dir("longer");
    let mut t = DetectorTemplates::new(&dir);
    t.add("ns-475.8", ALL_CLEAR, &recs, Some(parsed()), Some(SPLIT_S), None).unwrap();
    let longer = w.on_air(&cat(&[&w.message(&w.no_defects), &speech(51, 2.0)]));
    let (_, a) = t.match_audio(&longer).unwrap();
    let length = t.list()[0]["length_s"].as_f64().unwrap();
    assert!(a.is("other") && a.length_s.unwrap() > length + 1.5);
    let (tm, a) = t.match_audio(&w.on_air(&w.message(&w.no_defects))).unwrap();
    assert!(a.is("all_clear") && a.method.as_deref() == Some("sound"));
    assert_eq!(tm.name(), "ns-475.8");
    // the files: metadata in the reference key order, features as a .npy file
    let info = &t.list()[0];
    let keys: Vec<&str> = info.keys().map(String::as_str).collect();
    assert_eq!(keys, ["name", "text", "opening_text", "parsed", "split_s", "opening_end_s", "wording_text",
                      "ending_text", "recordings", "seconds", "quiet_frames", "length_s", "all_clear_length_s",
                      "rhythm"]);
    assert_eq!(info["opening_text"], "Norfolk Southern, milepost 475.8");
    assert_eq!(info["recordings"], 5);
    assert_eq!(info["length_s"], 10.0);
    assert_eq!(info["all_clear_length_s"], 9.52);
    assert_eq!(all_clear_length(info), Some(9.52));
    let text = std::fs::read_to_string(dir.join("ns-475.8.json")).unwrap();
    assert!(text.starts_with("{\n  \"name\": \"ns-475.8\",\n") && text.ends_with("]\n}\n"));
    let arr = npy::load(&dir.join("ns-475.8.npy")).unwrap();
    assert_eq!(arr.data, t.items[0].features.data);
    assert!(t.remove("ns-475.8"));
    assert!(t.list().is_empty() && !t.remove("ns-475.8"));
    let _ = std::fs::remove_dir_all(dir);
}

/// For the template's own detector: when the ending's detailed check missed, the rhythm and
/// length can confirm a near miss, never a different message of the same length.
#[test]
fn rhythm_and_length_rescue_near_misses_only() {
    let mut w = World::new();
    let recs = w.recs();
    let dir = tmp_dir("rhythm");
    let mut t = DetectorTemplates::new(&dir);
    t.add("ns-475.8", ALL_CLEAR, &recs, Some(parsed()), Some(SPLIT_S), None).unwrap();
    t.reload();
    let tmpl = t.items[0].clone();
    let split = split_frames();
    let judge = |audio: &[f32]| {
        let mut a = Assessment { kind: Some("other".into()), milepost: Some("known".into()), opening_frame: Some(0),
                                 rest_frame: Some(split), score: 0.5, ..Default::default() };
        check_length_and_rhythm(&tmpl, &mut a, audio, Some(split));
        a
    };
    let a = judge(&w.on_air(&w.message(&w.no_defects)));
    assert!(a.is("all_clear") && a.method.as_deref() == Some("rhythm and length") && a.rhythm.unwrap() >= 0.9,
            "{a:?}");
    let a = judge(&w.on_air(&w.message(&speech(31, w.no_defects.len() as f64 / SR as f64))));
    assert!(a.is("other") && a.rhythm.unwrap() < 0.9, "{a:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn older_template_without_length_still_refuses_more_after_the_ending() {
    let mut w = World::new();
    let recs = w.recs();
    let dir = tmp_dir("older");
    DetectorTemplates::new(&dir).add("ns-475.8", ALL_CLEAR, &recs, Some(parsed()), Some(SPLIT_S), None).unwrap();
    let meta = dir.join("ns-475.8.json");
    let mut info: serde_json::Map<String, Value> = serde_json::from_str(&std::fs::read_to_string(&meta).unwrap()).unwrap();
    info.shift_remove("length_s");
    info.shift_remove("rhythm");
    std::fs::write(&meta, serde_json::to_string(&info).unwrap()).unwrap();
    let mut t2 = DetectorTemplates::new(&dir);
    let longer = w.on_air(&cat(&[&w.message(&w.no_defects), &speech(51, 2.0)]));
    assert!(t2.match_audio(&longer).unwrap().1.is("other")); // the ending, then more message
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unreadable_template_is_skipped() {
    let dir = tmp_dir("unreadable");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bad.json"), "{not json").unwrap();
    std::fs::write(dir.join("nonpy.json"), "{\"name\": \"nonpy\"}").unwrap();
    let mut t = DetectorTemplates::new(&dir);
    assert!(t.list().is_empty());
    assert!(t.match_audio(&vec![0.1; SR]).is_none());
    assert!(DetectorTemplates::new(dir.join("missing")).list().is_empty());
    // features that are not 24 bands per frame
    npy::save(&dir.join("narrow.npy"), &[4, 10], &[0.0; 40]).unwrap();
    std::fs::write(dir.join("narrow.json"), "{\"name\": \"narrow\"}").unwrap();
    assert!(DetectorTemplates::new(&dir).list().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

/// A template file may hold a bare NaN for a NaN rhythm value; it is read back as null.
#[test]
fn template_with_nan_is_read() {
    let dir = tmp_dir("nan");
    std::fs::create_dir_all(&dir).unwrap();
    npy::save(&dir.join("t.npy"), &[4, 24], &[0.0; 96]).unwrap();
    std::fs::write(dir.join("t.json"), "{\"name\": \"t\", \"text\": \"NaN Infinity\", \"rhythm\": [0.5, NaN, -Infinity]}")
        .unwrap();
    let mut t = DetectorTemplates::new(&dir);
    let list = t.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["text"], "NaN Infinity");
    assert_eq!(list[0]["rhythm"], json!([0.5, null, null]));
    assert!(parse_json_lenient("{\"a\": Nope}").is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn noise_of_the_wrong_length_leaves_the_audio() {
    let audio: Vec<f32> = (0..4000).map(|i| (i as f32 * 0.01).sin()).collect();
    assert_eq!(remove_noise(&audio, &[0.0; 10]), audio);
    assert_eq!(remove_noise(&audio, &[0.0; 257]).len(), audio.len());
    assert_eq!(remove_noise(&audio, &[0.0]).len(), audio.len()); // one value broadcasts
}

// the milepost left out of matching (opening_end) ------------------------------------------------

fn generic(w: &mut World, tag: &str) -> (DetectorTemplates, PathBuf) {
    let clean = w.announce(&w.own_number, None);
    let recs: Vec<Vec<f32>> = (0..5).map(|_| w.on_air(&clean)).collect();
    let dir = tmp_dir(tag);
    let mut t = DetectorTemplates::new(&dir);
    t.add("ns-475.8", ALL_CLEAR, &recs, Some(parsed()), Some(SPLIT_S), Some(OPENING_END_S)).unwrap();
    (t, dir)
}

#[test]
fn generic_template() {
    let mut w = World::new();
    let (mut g, dir) = generic(&mut w, "generic");

    // own milepost is known
    let (_, a) = g.match_audio(&w.on_air(&w.announce(&w.own_number, None))).unwrap();
    assert!(a.is("all_clear") && a.milepost.as_deref() == Some("known") && a.method.as_deref() == Some("sound"),
            "{a:?}");

    // another detector of the same kind: a different number, of a different length
    for seconds in [1.2, 2.0, 3.5] {
        let x = w.on_air(&w.announce(&speech((seconds * 1000.0) as u64, seconds), None));
        let (_, a) = g.match_audio(&x).unwrap();
        assert!(a.is("all_clear") && a.milepost.as_deref() == Some("other"), "{seconds}: {a:?}");
        assert!(a.number_frames.unwrap().0.is_some());
    }
    for seconds in [1.2, 3.5] {
        let x = w.announce(&speech((seconds * 1000.0) as u64, seconds), Some(&speech(31, 7.0)));
        let (_, a) = g.match_audio(&w.on_air(&x)).unwrap();
        assert!(a.is("other") && a.milepost.as_deref() == Some("other"), "{seconds}: {a:?}");
    }

    // the ending followed by more is not the all-clear
    let x = cat(&[&w.announce(&speech(1500, 1.5), None), &speech(51, 2.0)]);
    assert!(g.match_audio(&w.on_air(&x)).unwrap().1.is("other"));

    // late start, and the ending alone
    let x = w.on_air_with(&w.announce(&speech(1500, 1.5), None), 6.0, 0.6, 0.3);
    assert!(g.match_audio(&x).unwrap().1.is("all_clear"));
    let garbled = cat(&[&speech(999, 2.6), &speech(1500, 1.5), &w.no_defects]); // wording unheard
    let (_, a) = g.match_audio(&w.on_air(&garbled)).unwrap();
    assert!(a.is("all_clear") && a.method.as_deref() == Some("ending"), "{a:?}");

    // cut off after the number: incomplete
    let x = w.announce(&w.own_number, Some(&w.no_defects[..SR])); // stops 1 s into the ending
    assert!(g.match_audio(&w.on_air(&x)).unwrap().1.is("incomplete"));

    // other speech is no detector
    for seed in [21, 22, 23] {
        assert!(g.match_audio(&w.on_air(&speech(seed, 10.0))).is_none());
    }
    let _ = std::fs::remove_dir_all(dir);
}

// API contract v1.5: the `detector` object ---------------------------------------------------------

fn vectors() -> Vec<Value> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/api/vectors.json")).unwrap()).unwrap();
    v["v1_5"]["valid_detector_objects"].as_array().unwrap().clone()
}

#[test]
fn invalid_objects_are_caught() {
    let base = &vectors()[3];
    let cases = [
        ("detector_id", json!("NS 475.8")), ("railroad", json!("ns")), ("railroad", json!("TOOLONGRR")),
        ("milepost", json!(10001)), ("result", json!("defect")), ("method", json!("ending")),
        ("scores", json!({"opening": 1.2, "ending": null, "rhythm": null})), ("scores", json!({"opening": 0.5})),
        ("length_s", json!(121)), ("repeat_of", json!("not-a-uuid")), ("heard_text", json!("x".repeat(301))),
        ("timestamp", json!("yesterday")),
    ];
    for (field, bad) in cases {
        let mut v = base.clone();
        v[field] = bad;
        let errs = validate_detector(&v);
        assert!(errs.join(" ").contains(field), "{field}: {errs:?}");
    }
    let mut v = vectors()[0].clone();
    v["heard_text"] = json!("no defects");
    assert_eq!(validate_detector(&v), vec!["heard_text on no_defects"]);
    assert_eq!(validate_detector(&json!("nope")).len(), 8);
}

#[test]
fn section_1_result_order() {
    let cases = [("all_clear", 9.2, "no_defects"), ("all_clear", 9.5, "no_defects"), ("all_clear", 9.6, "different_message"),
                 ("other", 9.2, "different_message"), ("other", 11.4, "different_message"),
                 ("other", 8.3, "different_message"), ("other", 8.1, "incomplete"), ("incomplete", 6.0, "incomplete"),
                 ("all_clear", 7.0, "no_defects")];
    for (k, length, want) in cases {
        let a = Assessment { kind: Some(k.into()), ..Default::default() };
        assert_eq!(contract_result(&a, length, 9.2).0, want, "{k} {length}");
    }
    let a = Assessment { kind: Some("all_clear".into()), method: Some("rhythm and length".into()), ..Default::default() };
    assert_eq!(contract_result(&a, 9.2, 9.2), ("no_defects", "rhythm_and_length"));
}

#[test]
fn object_shape_matches_the_vector() {
    let a = Assessment { score: 0.81, opening: Some(vec![None, Some(0.8), Some(0.8), Some(0.8)]),
                         ending: Some(vec![Some(0.6), Some(0.55), Some(0.8)]), ..Default::default() };
    let obj = detector_object("ns-475.8", "ns", 475.8, "no_defects", "sound", &a, 9.2, 9.2, None, None, None);
    assert_eq!(obj, vectors()[0]);
    let with_ts = detector_object("ns-475.8", "NS", 475.8, "no_defects", "sound", &a, 9.2, 9.2, None, None,
                                  Some(1791728100.0));
    assert_eq!(with_ts["timestamp"], 1791728100.0);
    assert!(validate_detector(&with_ts).is_empty());
    // heard text only on other results, cut to 300 characters
    let long = "é".repeat(400);
    let o = detector_object("ns-475.8", "NS", 475.8, "different_message", "opening", &a, 11.0, 9.2, None, Some(&long), None);
    assert_eq!(o["heard_text"].as_str().unwrap().chars().count(), 300);
    let o = detector_object("ns-475.8", "NS", 475.8, "no_defects", "sound", &a, 9.2, 9.2, None, Some("x"), None);
    assert!(o["heard_text"].is_null());
    // scores clipped to 0..1, no opening score without opening pieces
    let b = Assessment { score: 1.3, ending: Some(vec![Some(-0.2), None]), rhythm: Some(0.954), ..Default::default() };
    let o = detector_object("ns-475.8", "NS", 475.8, "no_defects", "sound", &b, 9.2, 9.2, None, None, None);
    assert_eq!(o["scores"], json!({"opening": null, "ending": 0.0, "rhythm": 0.95}));
}

#[test]
fn headend_detector_lookup() {
    let list = vec![json!({"railroad": "CSX", "milepost": 482.0}), json!({"railroad": "NS", "milepost": "bad"}),
                    json!({"railroad": "NS"}), json!({"railroad": "ns", "subdivision": "Charlotte", "milepost": 482.0})];
    assert_eq!(headend_detector(Some(&list), "NS", Some(482.1)), Some(&list[3]));
    assert_eq!(headend_detector(Some(&list), "NS", Some(482.3)), None);
    assert_eq!(headend_detector(Some(&list), "NS", None), None);
    assert_eq!(headend_detector(Some(&[]), "NS", Some(482.0)), None);
    assert_eq!(headend_detector(None, "NS", Some(482.0)), None);
    assert!(headend_detector(Some(&[json!({"railroad": "NS", "milepost": "482.1"})]), "ns", Some(482.0)).is_some());
}

#[test]
fn suggested_detectors() {
    use railspotting_detect::detectors::{suggest_detector, DetectorMatch};
    let list = vec![
        json!({"railroad": "CSX", "subdivision": "Belton", "milepost": 68.1, "name": "Try"}),
        json!({"railroad": "NS", "subdivision": "Charlotte", "milepost": 462, "name": "Wellford"}),
        json!({"railroad": "NS", "subdivision": "Charlotte", "milepost": 475.8, "name": "Taylors"}),
        json!({"railroad": "NS", "subdivision": "Charlotte", "milepost": 481.7, "name": "Misco"}),
    ];
    let name = |r: Option<(&serde_json::Value, DetectorMatch)>| r.map(|(d, m)| (d["name"].as_str().unwrap().to_string(), m));
    // heard right
    assert_eq!(name(suggest_detector(Some(&list), "CSX", Some(68.1))), Some(("Try".into(), DetectorMatch::Heard)));
    assert_eq!(name(suggest_detector(Some(&list), "NS", Some(481.7))), Some(("Misco".into(), DetectorMatch::Heard)));
    // misheard or unheard: the railroad's only listed detector
    assert_eq!(name(suggest_detector(Some(&list), "CSX", Some(16.1))), Some(("Try".into(), DetectorMatch::Suggested)));
    assert_eq!(name(suggest_detector(Some(&list), "csx", None)), Some(("Try".into(), DetectorMatch::Suggested)));
    // several on the railroad: the closest by digits, if clearly closest
    assert_eq!(name(suggest_detector(Some(&list), "NS", Some(475.9))), Some(("Taylors".into(), DetectorMatch::Heard)));
    assert_eq!(name(suggest_detector(Some(&list), "NS", Some(457.8))), Some(("Taylors".into(), DetectorMatch::Suggested)));
    assert_eq!(suggest_detector(Some(&list), "NS", None), None);
    assert_eq!(suggest_detector(Some(&list), "NS", Some(123.4)), None);
    assert_eq!(suggest_detector(Some(&list), "BNSF", Some(68.1)), None);
    assert_eq!(suggest_detector(None, "CSX", Some(68.1)), None);
}
