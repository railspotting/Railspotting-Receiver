//! The speech side of transcription: the Transcriber with voice clips, detector templates and
//! contract v1.5 results, the detector CLI and the replay.
//!
//! The synthetic voice comes from a local generator (as in the detect crate's tests): passages
//! that sound like speech in kind, not recordings of it.

#![allow(clippy::type_complexity)]



use crate::common::*;
use railspotting_detect::detectors::{validate_detector, DetectorTemplates, ENGINE as DETECTOR_ENGINE};
use railspotting_detect::text::NUMBER_PROMPT;
use railspotting_dsp::sdr::{SdrSource, TranscriptDone};
use railspotting_engine::cli::{self, DetectorCmd};
use railspotting_engine::logging::Capture;
use railspotting_engine::transcribe::{Segment, SpeechModel, SpeechOptions, Transcriber, ENGINE};
use railspotting_engine::AgentTranscriber;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};

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
        let (lo, hi) = ((pos * SR as f64).ceil() as usize, (((pos + dur) * SR as f64).ceil() as usize).min(n));
        for (i, o) in out.iter_mut().enumerate().take(hi).skip(lo) {
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

struct Air {
    rng: Rng,
    opening: Vec<f32>,
    no_defects: Vec<f32>,
    wording: Vec<f32>,
    own_number: Vec<f32>,
}

impl Air {
    fn new(seed: u64) -> Self {
        Air { rng: Rng::new(seed), opening: speech(475, SPLIT_S), no_defects: speech(0, 4.5),
              wording: speech(100, 2.6), own_number: speech(4758, 2.6) }
    }
    fn message(&self, ending: &[f32]) -> Vec<f32> {
        cat(&[&self.opening, ending])
    }
    fn announce(&self, number: &[f32], ending: Option<&[f32]>) -> Vec<f32> {
        cat(&[&self.wording, number, ending.unwrap_or(&self.no_defects)])
    }
    /// As the receiver hears it: noise, and the call may start late.
    fn on_air_with(&mut self, clean: &[f32], snr_db: f64) -> Vec<f32> {
        let mut x = clean.to_vec();
        x.extend(std::iter::repeat_n(0.0, (0.3 * SR as f64) as usize));
        let p = clean.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / clean.len() as f64;
        let sd = (p / 10f64.powf(snr_db / 10.0)).sqrt();
        for v in x.iter_mut() {
            *v += self.rng.normal(sd) as f32;
        }
        let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        x.iter().map(|v| v / peak * 0.9).collect()
    }
    fn on_air(&mut self, clean: &[f32]) -> Vec<f32> {
        self.on_air_with(clean, 6.0)
    }
    fn recs(&mut self) -> Vec<Vec<f32>> {
        let m = self.message(&self.no_defects.clone());
        (0..5).map(|_| self.on_air(&m)).collect()
    }
    fn generic_recs(&mut self) -> Vec<Vec<f32>> {
        let m = self.announce(&self.own_number.clone(), None);
        (0..5).map(|_| self.on_air(&m)).collect()
    }
}

/// Stands in for the speech model: one segment with the given text (none for "").
#[derive(Clone, Default)]
struct Model {
    segments: Vec<Segment>,
    calls: Arc<Mutex<Vec<(Vec<f32>, SpeechOptions)>>>,
    /// what the milepost reader hears, in turn (the last one repeats); None: the same segments
    numbers: Option<Arc<Mutex<Vec<String>>>>,
    /// what the other calls hear, in turn (the last one repeats); None: the same segments
    script: Option<Arc<Mutex<Vec<String>>>>,
}

fn seg(text: &str, start: f64, end: f64, logprob: f64, no_speech: f64) -> Segment {
    Segment { text: text.into(), start, end, avg_logprob: logprob, no_speech_prob: no_speech }
}

impl Model {
    fn text(text: &str) -> Model {
        let segments = if text.is_empty() { vec![] } else { vec![seg(text, 0.0, 1.0, -0.5, 0.1)] };
        Model { segments, ..Default::default() }
    }
    fn segs(segments: Vec<Segment>) -> Model {
        Model { segments, ..Default::default() }
    }
    fn script(heard: &[&str]) -> Model {
        Model { script: Some(Arc::new(Mutex::new(heard.iter().map(|h| h.to_string()).collect()))), ..Default::default() }
    }
    fn factory(&self) -> railspotting_engine::transcribe::ModelFactory {
        let m = self.clone();
        Arc::new(move || Ok(Box::new(m.clone()) as Box<dyn SpeechModel>))
    }
    fn calls(&self) -> Vec<(Vec<f32>, SpeechOptions)> {
        self.calls.lock().unwrap().clone()
    }
}

impl SpeechModel for Model {
    fn transcribe(&mut self, audio: &[f32], opts: &SpeechOptions) -> Result<(Vec<Segment>, Option<String>), String> {
        self.calls.lock().unwrap().push((audio.to_vec(), opts.clone()));
        if let (Some(n), true) = (&self.numbers, opts.initial_prompt == NUMBER_PROMPT) {
            let mut n = n.lock().unwrap();
            let heard = if n.len() > 1 { n.remove(0) } else { n.first().cloned().unwrap_or_default() };
            return Ok((vec![seg(&heard, 0.0, 1.0, -0.5, 0.1)], Some("en".into())));
        }
        if let Some(n) = &self.script {
            let mut n = n.lock().unwrap();
            let heard = if n.len() > 1 { n.remove(0) } else { n.first().cloned().unwrap_or_default() };
            let segs = if heard.is_empty() { vec![] } else { vec![seg(&heard, 0.0, 1.0, -0.5, 0.1)] };
            return Ok((segs, Some("en".into())));
        }
        Ok((self.segments.clone(), Some("en".into())))
    }
}

/// The Norfolk Southern profile's prompt: its wording without the values.
const NS_PROMPT: &str = "Norfolk Southern, milepost, no defects. Repeat, no defects.";

fn tr_with(dir: &Path, model: &Model, templates: Option<&Path>, other_calls: bool) -> Arc<Transcriber> {
    railspotting_engine::logging::setup_for_tests();
    Transcriber::new(&dir.join("models"), None, 50, Some(model.factory()), templates, other_calls)
}

fn add_template(dir: &Path, recs: &[Vec<f32>], opening_end: Option<f64>) {
    DetectorTemplates::new(dir).add("ns-475.8", ALL_CLEAR, recs, Some(parsed()), Some(SPLIT_S), opening_end).unwrap();
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

// voice clips ------------------------------------------------------------------------------------

#[test]
fn transcribe_call_and_confidence() {
    let t = Tmp::new("tr1");
    let model = Model::segs(vec![seg(" CSX detector, milepost 12.5", 0.0, 3.0, 0.9f64.ln(), 0.05),
                                 seg(" track two, no defects", 3.0, 4.0, 0.6f64.ln(), 0.05)]);
    let tr = tr_with(t.path(), &model, None, true);
    let mut rng = Rng::new(0);
    let audio: Vec<f32> = (0..SR * 4).map(|_| (rng.normal(1.0) * 0.1) as f32).collect();
    let r = tr.transcribe(&audio, None, false).unwrap().unwrap();
    assert_eq!(r["text"], "CSX detector, milepost 12.5 track two, no defects");
    assert!((r["confidence"].as_f64().unwrap() - (3.0 * 0.9 + 0.6) / 4.0).abs() <= 0.01);
    assert!(r["engine"] == "faster-whisper-small.en" && r["duration_s"] == 4.0);
    assert!(r["parsed"]["railroad"] == "CSX" && r["parsed"]["track"] == 2);
    let (a, kw) = &model.calls()[0];
    assert!(kw.language == "en" && kw.beam_size == 3 && kw.vad_filter && !kw.condition_on_previous_text);
    assert!(!kw.initial_prompt.contains("NOAA") && !kw.initial_prompt.to_lowercase().contains("weather"));
    let peak = a.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    assert!((peak - 0.9).abs() < 1e-4);
}

#[test]
fn fillers_rejected() {
    let t = Tmp::new("tr2");
    for filler in railspotting_detect::text::FILLERS {
        let tr = tr_with(t.path(), &Model::text(filler), None, true);
        assert!(tr.transcribe(&vec![1.0; 32_000], None, false).unwrap().is_none());
        assert_eq!(tr.last_reject().unwrap()["reason"], "filler phrase", "{filler}");
    }
}

#[test]
fn rejection_reasons() {
    let t = Tmp::new("tr3");
    let tr = tr_with(t.path(), &Model::segs(vec![seg("hello", 0.0, 2.0, 0.3f64.ln(), 0.05)]), None, true);
    assert!(tr.transcribe(&vec![1.0; 8_000], None, false).unwrap().is_none());
    assert_eq!(tr.last_reject().unwrap()["reason"], "too short");
    assert!(tr.transcribe(&vec![1.0; 32_000], None, false).unwrap().is_none());
    let r = tr.last_reject().unwrap();
    assert!(r["reason"] == "low confidence" && r["text"] == "hello");
    let tr2 = tr_with(t.path(), &Model::segs(vec![seg("noise", 0.0, 2.0, -0.1, 0.8), seg("   ", 0.0, 2.0, -0.1, 0.05)]), None, true);
    assert!(tr2.transcribe(&vec![1.0; 32_000], None, false).unwrap().is_none());
    let r = tr2.last_reject().unwrap();
    assert!(r["reason"] == "no speech found" && r["skipped"].as_array().unwrap().len() == 2);
}

#[test]
fn worker_saves_clips_and_always_calls_done() {
    let t = Tmp::new("tr4");
    railspotting_engine::logging::setup_for_tests();
    let model = Model::text("Approach signal, over.");
    let clips = t.path().join("clips");
    let tr = Transcriber::new(&t.path().join("models"), Some(&clips), 2, Some(model.factory()), None, true);
    tr.start();
    let results: Arc<Mutex<Vec<Option<Map<String, Value>>>>> = Arc::default();
    for i in 0..3 {
        let r = results.clone();
        tr.submit(vec![0.1; 32_000], Box::new(move |x| r.lock().unwrap().push(x)), "voice", None);
        // clip names are stamped to the millisecond
        std::thread::sleep(std::time::Duration::from_millis(if i < 2 { 30 } else { 0 }));
    }
    for _ in 0..200 {
        if results.lock().unwrap().len() == 3 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    tr.stop();
    let res = results.lock().unwrap();
    assert!(res.len() == 3 && res.iter().all(|r| r.as_ref().unwrap()["text"] == "Approach signal, over."));
    let mut wavs: Vec<_> = std::fs::read_dir(&clips).unwrap().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav")).collect();
    wavs.sort();
    let jsons = std::fs::read_dir(&clips).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).count();
    assert!(wavs.len() == 2 && jsons == 2);
    let meta = read_json(&wavs[0].with_extension("json"));
    assert_eq!(meta["transcript"]["text"], "Approach signal, over.");
}

#[test]
fn queue_full_drops_and_calls_done() {
    let t = Tmp::new("tr5");
    let tr = tr_with(t.path(), &Model::default(), None, true);
    let got: Arc<Mutex<Vec<Option<Map<String, Value>>>>> = Arc::default();
    for _ in 0..21 {
        let g = got.clone();
        tr.submit(vec![0.0; 10], Box::new(move |x| g.lock().unwrap().push(x)), "voice", None);
    }
    assert_eq!(tr.dropped(), 1);
    assert_eq!(*got.lock().unwrap(), vec![None]);
}

#[test]
fn model_failure_still_calls_done() {
    let t = Tmp::new("tr6");
    railspotting_engine::logging::setup_for_tests();
    let boom: railspotting_engine::transcribe::ModelFactory = Arc::new(|| Err("no model".into()));
    let tr = Transcriber::new(&t.path().join("m"), None, 50, Some(boom), None, true);
    tr.start();
    let (tx, rx) = std::sync::mpsc::channel();
    tr.submit(vec![1.0; 32_000], Box::new(move |r| tx.send(r).unwrap()), "voice", None);
    assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(), None);
    tr.stop();
}

#[test]
fn transcript_from_speech_recognition_has_no_defects_field() {
    let t = Tmp::new("tr7");
    let model = Model::segs(vec![seg("Norfolk Southern detector, milepost 476.2, no defects.", 0.0, 3.0, -0.3, 0.1)]);
    let tr = tr_with(t.path(), &model, None, true);
    let mut rng = Rng::new(0);
    let audio: Vec<f32> = (0..48_000).map(|_| rng.normal(0.1) as f32).collect();
    let res = tr.transcribe(&audio, None, false).unwrap().unwrap();
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "NS", "milepost": 476.2}));
}

#[test]
fn read_number_uses_a_neutral_prompt() {
    let t = Tmp::new("tr8");
    let model = Model::text("4, 7, 5, .8.");
    let tr = tr_with(t.path(), &model, None, true);
    let mut rng = Rng::new(0);
    let audio: Vec<f32> = (0..32_000).map(|_| rng.normal(0.1) as f32).collect();
    assert_eq!(tr.read_number(&audio), Some(475.8));
    let (a, kw) = &model.calls()[0];
    assert!(!kw.initial_prompt.contains("475") && !kw.vad_filter && a.len() == 32_000);
}

// detector templates: the transcriber ------------------------------------------------------------

#[test]
fn all_clear_is_transcribed_whole_with_its_profile() {
    let t = Tmp::new("det1");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let model = Model::text(ALL_CLEAR);
    let tr = tr_with(t.path(), &model, Some(&t.path().join("t")), true);
    let x = air.on_air(&air.message(&air.no_defects.clone()));
    let res = tr.transcribe(&x, None, false).unwrap().unwrap();
    assert!(res["text"] == ALL_CLEAR && res["engine"] == DETECTOR_ENGINE && res["parsed"] == parsed());
    let keys: Vec<&str> = res.keys().map(String::as_str).filter(|k| *k != "_detector").collect();
    assert_eq!(keys, ["engine", "confidence", "language", "duration_s", "text", "parsed"]);
    // one call, for the whole announcement, prompted with the profile's wording (no milepost)
    let calls = model.calls();
    assert!(calls.len() == 1 && calls[0].1.initial_prompt == NS_PROMPT, "{:?}", calls.iter().map(|c| &c.1.initial_prompt).collect::<Vec<_>>());
    assert!(calls[0].0.len() > 8 * SR); // the whole message, not a slice
}

#[test]
fn defect_report_by_its_words() {
    let t = Tmp::new("det2");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let log = Capture::start();
    let report = "Norfolk Southern, milepost 475.8, defect detected. Hot box, axle 112, south rail.";
    let tr = tr_with(t.path(), &Model::text(report), Some(&t.path().join("t")), true);
    let x = air.on_air(&air.message(&speech(11, 7.0)));
    let res = tr.transcribe(&x, None, false).unwrap().unwrap();
    assert_eq!(res["text"], report);
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "NS", "milepost": 475.8, "defects": true}));
    assert!(log.text().contains("sent something other than its all-clear"));
}

#[test]
fn longer_all_clear_in_noise_is_not_a_defect() {
    // 10-13 01:39 and 02:38: an all-clear of the reference detector with noise after it ran long, its ending missed by
    // sound, and was reported as a defect; its words say the all-clear
    let t = Tmp::new("det2b");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let log = Capture::start();
    let tr = tr_with(t.path(), &Model::text(ALL_CLEAR), Some(&t.path().join("t")), true);
    let x = air.on_air(&air.message(&speech(11, 7.0)));
    let res = tr.transcribe(&x, None, false).unwrap().unwrap();
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "NS", "milepost": 475.8, "defects": false}));
    assert!(log.text().contains("all clear by its words"));
}

#[test]
fn as_long_as_the_all_clear_stays_unconfirmed() {
    // the all-clear through noise (its ending not matched) must not become a defect report
    let t = Tmp::new("det3");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    // words that are neither the all-clear nor a defect
    let heard = "Norfolk Southern, milepost 475.8, [inaudible]";
    let tr = tr_with(t.path(), &Model::text(heard), Some(&t.path().join("t")), true);
    let rest_s = air.no_defects.len() as f64 / SR as f64;
    let x = air.on_air(&air.message(&speech(12, rest_s)));
    let res = tr.transcribe(&x, None, false).unwrap().unwrap();
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "NS", "milepost": 475.8}));
    assert_eq!(res["text"], heard);
}

#[test]
fn other_speech_goes_to_the_speech_model() {
    let t = Tmp::new("det4");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let model = Model::default();
    let tr = tr_with(t.path(), &model, Some(&t.path().join("t")), true);
    assert!(tr.transcribe(&air.on_air(&speech(21, 6.0)), None, false).unwrap().is_none());
    assert!(model.calls()[0].1.vad_filter);
    assert_eq!(tr.last_reject().unwrap()["reason"], "no speech found");
}

#[test]
fn repeat_is_linked_but_judged_on_its_own() {
    let t = Tmp::new("det6");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let log = Capture::start();
    let tr = tr_with(t.path(), &Model::default(), Some(&t.path().join("t")), true);
    let clear = air.message(&air.no_defects.clone());
    let ev = |id: &str, s: f64| json!({"event_id": id, "start_ts": s});
    let first = tr.transcribe(&air.on_air(&clear), Some(&ev("ev-1", 1000.0)), false).unwrap().unwrap();
    assert!(first["parsed"]["defects"] == json!(false) && first["parsed"].get("repeat").is_none());
    // 19 s later the detector repeats with a longer message: linked, and judged on its own
    let second = tr.transcribe(&air.on_air(&air.message(&speech(11, 7.0))), Some(&ev("ev-2", 1019.0)), false).unwrap().unwrap();
    assert!(second["parsed"]["repeat"] == json!(true) && second["parsed"]["repeat_of"] == "ev-1");
    assert_eq!(second["parsed"]["defects"], json!(true));
    assert!(log.text().contains("repeat of event ev-1 (19 s earlier)"));
    // the next train, a minute later: a new announcement, not a repeat
    let third = tr.transcribe(&air.on_air(&clear), Some(&ev("ev-3", 1080.0)), false).unwrap().unwrap();
    assert!(third["parsed"].get("repeat").is_none());
    let fourth = tr.transcribe(&air.on_air(&clear), Some(&ev("ev-4", 1101.0)), false).unwrap().unwrap();
    assert!(fourth["parsed"]["repeat_of"] == "ev-3" && fourth["parsed"]["defects"] == json!(false));
    // marking a repeat must not leak into later announcements (or the template)
    let fifth = tr.transcribe(&air.on_air(&clear), Some(&ev("ev-5", 1300.0)), false).unwrap().unwrap();
    assert!(fifth["parsed"].get("repeat").is_none() && fifth["parsed"].get("repeat_of").is_none());
    assert!(tr.template_list().unwrap()[0]["parsed"].get("repeat").is_none());
    // and without an event there is nothing to link
    let a = tr.transcribe(&air.on_air(&clear), None, false).unwrap().unwrap();
    assert!(a["parsed"].get("repeat").is_none());
}

#[test]
fn transcript_for_another_milepost_and_repeats_per_milepost() {
    let t = Tmp::new("det7");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.generic_recs(), Some(OPENING_END_S));
    let other_clear = "Norfolk Southern, milepost 462.2, no defects. Repeat, no defects.";
    let model = Model::script(&[other_clear, "Norfolk Southern, milepost 462.2, defect detected. Hot box, axle 31."]);
    let tr = tr_with(t.path(), &model, Some(&t.path().join("t")), true);
    let res = tr.transcribe(&air.on_air(&air.announce(&speech(1800, 1.8), None)), None, false).unwrap().unwrap();
    assert_eq!(res["text"], "Norfolk Southern, milepost 462.2, no defects. Repeat, no defects.");
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "NS", "milepost": 462.2, "defects": false}));
    let other = tr.transcribe(&air.on_air(&air.announce(&speech(1800, 1.8), Some(&speech(31, 7.0)))), None, false)
        .unwrap().unwrap();
    assert!(other["text"] == "Norfolk Southern, milepost 462.2, defect detected. Hot box, axle 31."
            && other["parsed"]["defects"] == json!(true), "{other:?}");

    let model = Model::script(&[ALL_CLEAR, other_clear, ALL_CLEAR]);
    let tr = tr_with(t.path(), &model, Some(&t.path().join("t")), true);
    let own = air.announce(&air.own_number.clone(), None);
    tr.transcribe(&air.on_air(&own), Some(&json!({"event_id": "a", "start_ts": 0.0})), false).unwrap();
    let b = tr.transcribe(&air.on_air(&air.announce(&speech(1800, 1.8), None)), Some(&json!({"event_id": "b", "start_ts": 15.0})), false)
        .unwrap().unwrap();
    assert!(b["parsed"].get("repeat").is_none()); // another milepost: not a repeat of a
    let c = tr.transcribe(&air.on_air(&own), Some(&json!({"event_id": "c", "start_ts": 20.0})), false).unwrap().unwrap();
    assert_eq!(c["parsed"]["repeat_of"], "a");
}

#[test]
fn other_calls_skip_speech_recognition() {
    // transcribe_other_calls off: only detector calls go further than the template check
    let t = Tmp::new("det8");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let model = Model::text("Clear signal at Greer, over.");
    let tr = tr_with(t.path(), &model, Some(&t.path().join("t")), false);
    assert!(tr.transcribe(&air.on_air(&speech(21, 6.0)), None, false).unwrap().is_none());
    assert!(model.calls().is_empty() && tr.last_reject().unwrap()["reason"].as_str().unwrap().contains("not a detector call"));
    let clear = air.message(&air.no_defects.clone());
    assert_eq!(tr.transcribe(&air.on_air(&clear), None, false).unwrap().unwrap()["text"], ALL_CLEAR); // detectors still
    assert!(tr.transcribe(&air.on_air(&speech(21, 6.0)), None, true).unwrap().is_some()); // the NOAA test
}

#[test]
fn without_speech_detectors_still_match() {
    // a build without the speech feature: speech is reported unavailable
    let t = Tmp::new("det9");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let tr = Transcriber::new(&t.path().join("m"), None, 50, None, Some(&t.path().join("t")), true);
    tr.set_model_factory(railspotting_engine::transcribe::no_speech("off"), false);
    let clear = air.message(&air.no_defects.clone());
    assert_eq!(tr.transcribe(&air.on_air(&clear), None, false).unwrap().unwrap()["text"], ALL_CLEAR);
    assert!(tr.transcribe(&air.on_air(&speech(21, 6.0)), None, false).unwrap().is_none());
    assert!(tr.last_reject().unwrap()["reason"].as_str().unwrap().contains("unavailable"));
    let other = tr.transcribe(&air.on_air(&air.message(&speech(11, 7.0))), None, false).unwrap().unwrap();
    assert!(other["text"].as_str().unwrap().ends_with(", defect") && other["parsed"]["defects"] == json!(true));
    assert!(!AgentTranscriber::other_calls(tr.as_ref()));
}

// contract v1.5: the transcriber builds sections 1 and 3 ------------------------------------------

fn v15(t: &Tmp, air: &mut Air, text: &str) -> Arc<Transcriber> {
    v15_model(t, air, Model::text(text))
}

fn v15_model(t: &Tmp, air: &mut Air, model: Model) -> Arc<Transcriber> {
    add_template(&t.path().join("t"), &air.generic_recs(), Some(OPENING_END_S));
    tr_with(t.path(), &model, Some(&t.path().join("t")), true)
}

fn ev(start: f64, length: f64) -> Value {
    json!({"event_id": uuid::Uuid::new_v4().to_string(), "start_ts": start, "end_ts": start + length})
}

#[test]
fn v15_all_clear_event_different_message_and_repeat() {
    let t = Tmp::new("v1");
    let mut air = Air::new(3);
    let report = "Norfolk Southern, milepost 475.8, defect detected. Hot box, axle 112.";
    // what speech recognition hears for each call below, in order
    let tr = v15_model(&t, &mut air, Model::script(&[ALL_CLEAR, ALL_CLEAR, report, ALL_CLEAR, "Norfolk Southern, milepost 475.8,",
                                                     ALL_CLEAR]));
    let own = air.announce(&air.own_number.clone(), None);
    let e = ev(1000.0, 9.7 - 0.48);
    let res = tr.transcribe(&air.on_air(&own), Some(&e), false).unwrap().unwrap();
    let d = &res["_detector"];
    assert!(d["result"] == "no_defects" && d["method"] == "sound" && d["milepost"] == 475.8, "{d}");
    assert!((d["length_s"].as_f64().unwrap() - (9.7 - 0.48)).abs() <= 0.01);
    assert!((d["all_clear_length_s"].as_f64().unwrap() - (10.0 - 0.48)).abs() <= 0.05); // the template's clips, as radio
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "NS", "milepost": 475.8, "defects": false}));
    assert!(res["confidence"] == d["scores"]["opening"] && res["text"] == ALL_CLEAR);
    assert!(d["heard_text"].is_null() && validate_detector(d).is_empty());

    let first = tr.transcribe(&air.on_air(&own), Some(&ev(2000.0, 9.7 - 0.48)), false).unwrap().unwrap();
    let other = tr.transcribe(&air.on_air(&air.announce(&air.own_number.clone(), Some(&speech(31, 6.5)))),
                              Some(&ev(2019.0, 11.4)), false).unwrap().unwrap();
    let d = &other["_detector"];
    assert!(d["result"] == "different_message" && d["method"] == "opening", "{d}");
    // the defect report's words go along as heard_text
    assert!(d["heard_text"] == report && other["parsed"]["defects"] == json!(true), "{other:?}");
    assert!(validate_detector(d).is_empty());
    let first_id = tr.last_detector_call("ns-475.8@475.8").unwrap()["event_id"].clone();
    assert!(d["repeat_of"] == first_id && other["parsed"]["repeat_of"] == first_id && other["parsed"]["repeat"] == json!(true));
    assert!(first["_detector"]["repeat_of"].is_null());

    // longer than the all-clear is never no_defects by sound; its words say the all-clear, which
    // v1.5 has no method for, so it goes as a transcript only
    let usual = tr.template_list().unwrap()[0]["all_clear_length_s"].as_f64().unwrap();
    let res = tr.transcribe(&air.on_air(&own), Some(&ev(3000.0, usual + 0.5)), false).unwrap().unwrap();
    assert!(res.get("_detector").is_none(), "{res:?}");
    assert_eq!(res["parsed"]["defects"], json!(false));

    // incomplete, and words that say nothing more
    let cut = air.announce(&air.own_number.clone(), Some(&air.no_defects[..SR]));
    let res = tr.transcribe(&air.on_air(&cut), Some(&ev(4000.0, 5.6)), false).unwrap().unwrap();
    assert_eq!(res["_detector"]["result"], "incomplete");
    assert!(res["parsed"].get("defects").is_none());

    // once the headend accepts it (contract v1.9), the words' all-clear is a detector result
    tr.set_words_method(true);
    let res = tr.transcribe(&air.on_air(&own), Some(&ev(5000.0, usual + 0.5)), false).unwrap().unwrap();
    let d = &res["_detector"];
    assert!(d["result"] == "no_defects" && d["method"] == "words" && d["heard_text"].is_null(), "{d}");
    assert!(validate_detector(d).is_empty());
}

#[test]
fn heard_milepost_is_judged_against_the_template() {
    let t = Tmp::new("v4");
    let mut air = Air::new(3);
    // the milepost now comes from the whole announcement's transcript
    let say = |n: &str| format!("Norfolk Southern, milepost {n}, no defects. Repeat, no defects.");
    let lines = [say("475.9"), say("462.2"), say("4-7-5, only take"), say("475"), "Norfolk Southern, no defects. Repeat, no defects.".to_string()];
    let model = Model::script(&lines.iter().map(String::as_str).collect::<Vec<_>>());
    let tr = v15_model(&t, &mut air, model);
    let own = air.announce(&air.own_number.clone(), None);
    let log = Capture::start();
    // 475.9 is a mishearing of the template's 475.8: the same detector
    let res = tr.transcribe(&air.on_air(&own), Some(&ev(1000.0, 9.7 - 0.48)), false).unwrap().unwrap();
    let d = &res["_detector"];
    assert!(d["milepost"] == 475.8 && d["detector_id"] == "ns-475.8" && d["result"] == "no_defects", "{d}");
    assert!(log.text().contains("milepost 475.9 heard, the template's 475.8"));
    // 462.2 sounds like the template but is said to be another detector (not on the headend's list)
    let res = tr.transcribe(&air.on_air(&own), Some(&ev(2000.0, 9.7 - 0.48)), false).unwrap().unwrap();
    assert!(res.get("_detector").is_none() && res["parsed"]["milepost"] == 462.2, "{res:?}");
    assert!(log.text().contains("milepost 462.2 heard, not the template's 475.8: a different detector"));
    // the whole miles with the decimal lost (a real clip: "4-7-5, only take...") is still 475.8
    for start in [2500.0, 2700.0] {
        let res = tr.transcribe(&air.on_air(&own), Some(&ev(start, 9.7 - 0.48)), false).unwrap().unwrap();
        assert_eq!(res["_detector"]["milepost"], 475.8, "{res:?}");
    }
    // nothing read: only then does the sound match decide
    let res = tr.transcribe(&air.on_air(&own), Some(&ev(3000.0, 9.7 - 0.48)), false).unwrap().unwrap();
    assert_eq!(res["_detector"]["milepost"], 475.8);
    assert!(log.text().contains("milepost number not read; the sound match heard the template's 475.8"));
}

#[test]
fn v15_unlisted_and_listed_mileposts() {
    let t = Tmp::new("v2");
    let mut air = Air::new(3);
    let tr = v15(&t, &mut air, "462.2");
    let log = Capture::start();
    let x = air.announce(&speech(1800, 1.8), None);
    let res = tr.transcribe(&air.on_air(&x), Some(&ev(4000.0, 8.4)), false).unwrap().unwrap();
    assert!(res.get("_detector").is_none() && res["parsed"]["milepost"] == 462.2);
    assert!(log.text().contains("which the headend doesn't list"));

    tr.set_known_detectors(Some(vec![json!({"railroad": "NS", "subdivision": "Charlotte", "milepost": 462.0, "name": ""})]));
    let res = tr.transcribe(&air.on_air(&x), Some(&ev(5000.0, 9.7 - 0.8 - 0.48)), false).unwrap().unwrap();
    let d = &res["_detector"];
    assert!(d["milepost"] == 462.0 && d["detector_id"] == "ns-462" && d["result"] == "no_defects", "{d}");
    assert!(d["all_clear_length_s"].as_f64().unwrap() < 9.4); // its shorter number makes a shorter all-clear
}

#[test]
fn sdr_moves_the_object_onto_the_event() {
    let t = Tmp::new("v3");
    let jobs: Arc<Mutex<Vec<(TranscriptDone, Option<Value>)>>> = Arc::default();
    struct Tr(Arc<Mutex<Vec<(TranscriptDone, Option<Value>)>>>);
    impl railspotting_dsp::sdr::Transcriber for Tr {
        fn submit(&self, _a: Vec<f32>, done: TranscriptDone, _tag: &str, event: Option<Value>) {
            self.0.lock().unwrap().push((done, event));
        }
    }
    let (events, emit) = sink();
    let mut src = SdrSource::new(&vhf(), &json!({}), t.path(), emit, Some(Arc::new(Tr(jobs.clone()))),
                                 railspotting_common::system_clock());
    let p = plan("7");
    src.set_plan(&channels(&p), Some(&p["reference"]));
    let w = railspotting_dsp::sim::World::default();
    w.carrier(160.95, 25.0, 2.0, Some(4.0), 700.0, 2500.0, "voice");
    run(&mut src, &w, 0.0, 5.0);
    let (done, meta) = jobs.lock().unwrap().remove(0);
    let keys: Vec<String> = meta.unwrap().as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["event_id", "start_ts", "end_ts", "freq", "railroad", "simultaneous"]);
    let vector = json!({"detector_id": "ns-475.8", "railroad": "NS", "milepost": 475.8, "result": "no_defects", "method": "sound",
                        "scores": {"opening": 0.81, "ending": 0.55, "rhythm": null}, "length_s": 9.2, "all_clear_length_s": 9.2,
                        "repeat_of": null, "heard_text": null});
    done(Some(obj(json!({"text": "x", "engine": "e", "confidence": 0.8, "language": "en", "duration_s": 2.0,
                         "parsed": {"type": "defect_detector"}, "_detector": vector}))));
    let ev = events.lock().unwrap();
    let closed = ev.iter().find(|e| e["state"] == "closed").unwrap();
    assert_eq!(closed["detector"], vector);
    assert!(closed["transcript"].get("_detector").is_none());
}

#[test]
fn heartbeat_flag_and_config_warning() {
    let (c, t) = (clock(), Tmp::new("v4"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    assert_eq!(agent.capabilities()["features"]["detector_reports"], json!(false));
    let mut air = Air::new(3);
    let tdir = agent.state_dir.join("detector_templates");
    let tr = Transcriber::new(&t.path().join("m"), None, 50, None, Some(&tdir), true);
    add_template(&tdir, &air.generic_recs(), Some(OPENING_END_S));
    agent.set_transcriber(Some(tr.clone()));
    assert_eq!(agent.capabilities()["features"]["detector_reports"], json!(true));
    let log = Capture::start();
    assert!(agent.check_detectors(&json!({"detectors": [{"railroad": "NS", "milepost": 475.9}]})).is_empty());
    assert_eq!(tr.known_detectors().unwrap()[0]["milepost"], 475.9);
    assert_eq!(agent.check_detectors(&json!({"detectors": [{"railroad": "NS", "milepost": 476.2}]})), ["ns-475.8"]);
    assert!(log.text().contains("matches none of the headend's detectors"));
    assert!(agent.check_detectors(&json!({})).is_empty() && tr.known_detectors().is_none());
}

#[test]
fn transcription_feature_follows_the_setting() {
    let (c, t) = (clock(), Tmp::new("v5"));
    let h = headend(&c);
    let tr = tr_with(t.path(), &Model::default(), None, false);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [vhf()]}}), true,
                                railspotting_engine::AgentOptions { transcriber: Some(tr.clone()), ..Default::default() });
    let v = agent.sdrs()[0].clone();
    v.with(|s| s.connected = true);
    assert_eq!(agent.capabilities()["features"]["transcription"], json!(false));
    tr.set_other_calls(true);
    assert_eq!(agent.capabilities()["features"]["transcription"], json!(true));
}

// the detector CLI and the replay ----------------------------------------------------------------

fn save_wav(p: &Path, x: &[f32]) {
    railspotting_engine::wav::write_pcm16(p, &x.iter().map(|v| v.clamp(-1.0, 1.0)).collect::<Vec<_>>(), 16_000).unwrap();
}

#[test]
fn cli_add_list_test_remove() {
    let t = Tmp::new("cli-det");
    railspotting_engine::logging::setup_for_tests();
    let st = settings(t.path(), None, json!({}));
    let clips = st.state_dir().join("voice_clips");
    std::fs::create_dir_all(&clips).unwrap();
    let mut air = Air::new(1);
    let mut ids = vec![];
    for (i, x) in air.recs().iter().enumerate() {
        ids.push(format!("20261011-01000{i}.000"));
        save_wav(&clips.join(format!("{}_voice.wav", ids[i])), x);
    }
    save_wav(&clips.join("20261011-020000.000_voice.wav"), &air.on_air(&speech(8, 9.0)));
    let add = |clips: Vec<String>| DetectorCmd::Add { name: "ns-475.8".into(), text: ALL_CLEAR.into(), split: Some(SPLIT_S),
                                                      opening_end: None, clips };
    let mut out = vec![];
    let mut with_other = ids[..3].to_vec();
    with_other.push("20261011-020000.000".into()); // a clip of something else
    assert_eq!(cli::cmd_detector(&st, &add(with_other), &mut out), 1);
    assert!(String::from_utf8_lossy(&out).contains("don't match the others"));
    let mut out = vec![];
    assert_eq!(cli::cmd_detector(&st, &add(ids.clone()), &mut out), 0);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("added ns-475.8") && text.contains("\"defects\": false"), "{text}");
    assert!(text.contains("opening \"Norfolk Southern, milepost 475.8\" ends at 5.2 s"), "{text}");
    let saved = read_json(&st.state_dir().join("detector_templates/ns-475.8.json"));
    assert!(saved["split_s"] == SPLIT_S && saved["opening_text"] == "Norfolk Southern, milepost 475.8");

    save_wav(&t.path().join("clear.wav"), &air.on_air(&air.message(&air.no_defects.clone())));
    save_wav(&t.path().join("other.wav"), &air.on_air(&air.message(&speech(13, 7.0))));
    let mut out = vec![];
    cli::cmd_detector(&st, &DetectorCmd::Test { clips: vec!["20261011-020000.000".into(),
        t.path().join("clear.wav").to_string_lossy().into(), t.path().join("other.wav").to_string_lossy().into()] }, &mut out);
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[0].ends_with("no match"), "{text}");
    assert!(lines[1].ends_with("-> ns-475.8 (all-clear)") && lines[2].ends_with("-> ns-475.8 (other)"), "{text}");

    let mut out = vec![];
    cli::cmd_detector(&st, &DetectorCmd::List, &mut out);
    assert!(String::from_utf8_lossy(&out).contains("opening 5.2 s"));
    assert_eq!(cli::cmd_detector(&st, &DetectorCmd::Remove { name: "ns-475.8".into() }, &mut out), 0);
    assert!(DetectorTemplates::new(st.state_dir().join("detector_templates")).list().is_empty());
}

#[test]
fn add_checks() {
    let t = Tmp::new("cli-add");
    let st = settings(t.path(), None, json!({}));
    let vc = st.state_dir().join("voice_clips");
    std::fs::create_dir_all(&vc).unwrap();
    let mut air = Air::new(1);
    let recs = air.recs();
    for (i, r) in recs.iter().take(3).enumerate() {
        save_wav(&vc.join(format!("c{i}_voice.wav")), r);
    }
    let add = |clips: &[&str], split: Option<f64>| DetectorCmd::Add {
        name: "x".into(), text: ALL_CLEAR.into(), split, opening_end: None, clips: clips.iter().map(|s| s.to_string()).collect() };
    let run = |a: DetectorCmd| {
        let mut out = vec![];
        let code = cli::cmd_detector(&st, &a, &mut out);
        (code, String::from_utf8(out).unwrap())
    };
    let (code, out) = run(add(&["c0", "c1"], None));
    assert!(code == 1 && out.contains("at least 3"));
    let (code, out) = run(add(&["c0", "c1", "c2"], Some(9.5))); // past the end of the announcement
    assert!(code == 1 && out.contains("split must be between"), "{out}");
    let (code, out) = run(add(&["/no/such/dir/*.wav", "c1", "c2"], None));
    assert!(code == 1 && out.contains("no such file: /no/such/dir/*.wav"));
    assert!(DetectorTemplates::new(t.path()).add("../evil", ALL_CLEAR, &recs, None, None, None).is_err());
}

#[test]
fn replay_queues_v15_events_with_new_ids() {
    let t = Tmp::new("replay");
    railspotting_engine::logging::setup_for_tests();
    let st = settings(t.path(), None, json!({"transcription": false}));
    let state = st.state_dir();
    std::fs::create_dir_all(state.join("voice_clips")).unwrap();
    let mut air = Air::new(3);
    add_template(&state.join("detector_templates"), &air.generic_recs(), Some(OPENING_END_S));
    let now = railspotting_common::now();
    let db = rusqlite::Connection::open(state.join("history.db")).unwrap();
    db.execute("create table events (event_id text primary key, start_ts real, kind text, payload text)", []).unwrap();
    let mut originals = vec![];
    let own = air.announce(&air.own_number.clone(), None);
    for (i, (start, audio)) in [(now - 3600.0, own.clone()), (now - 3581.0, own.clone()), (now - 1800.0, speech(77, 4.0))]
        .into_iter().enumerate() {
        let clip = air.on_air(&audio);
        let length = clip.len() as f64 / SR as f64 - 0.48;
        let e = json!({"event_id": uuid::Uuid::new_v4().to_string(), "state": "closed", "start_ts": start, "end_ts": start + length,
                       "freq": 160.95, "ch": 5, "kind": "voice", "peak_rssi": 415, "avg_rssi": 400, "samples": 300});
        db.execute("insert into events values (?, ?, ?, ?)",
                   rusqlite::params![e["event_id"].as_str().unwrap(), start, "voice", e.to_string()]).unwrap();
        let f = state.join("voice_clips").join(format!("c{i}_voice.wav"));
        save_wav(&f, &clip);
        let mt = std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(e["end_ts"].as_f64().unwrap() + 3.0);
        std::fs::File::options().write(true).open(&f).unwrap().set_modified(mt).unwrap();
        originals.push(e);
    }
    drop(db);

    let mut out = vec![];
    assert_eq!(cli::cmd_detector_replay(&st, 12.0, false, Some(now), &mut out), 0);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("2 detector events"), "{text}");
    let mut out = vec![];
    assert_eq!(cli::cmd_detector_replay(&st, 12.0, true, Some(now), &mut out), 0);
    let ob = railspotting_store::outbox::Outbox::open(state.join("outbox.db")).unwrap();
    let queued: Vec<Value> = ob.next_events(10).unwrap().into_iter().map(|(_, e)| e).collect();
    assert_eq!(queued.len(), 2);
    let (first, second) = (&queued[0], &queued[1]);
    let want = uuid::Uuid::new_v5(&cli::REPLAY_NAMESPACE, originals[0]["event_id"].as_str().unwrap().as_bytes()).to_string();
    assert_eq!(first["event_id"], json!(want));
    assert_ne!(first["event_id"], originals[0]["event_id"]);
    assert!((first["detector"]["timestamp"].as_f64().unwrap() - originals[0]["start_ts"].as_f64().unwrap()).abs() <= 0.01);
    assert_eq!(second["detector"]["repeat_of"], first["event_id"]);
    assert!(first["start_ts"] == originals[0]["start_ts"] && first["ch"] == 5);
    assert!(queued.iter().all(|e| validate_detector(&e["detector"]).is_empty() && e["transcript"].get("_detector").is_none()));
    let _ = ENGINE;
}

#[test]
fn watched_channel_clips_are_kept_apart() {
    let t = Tmp::new("watch");
    railspotting_engine::logging::setup_for_tests();
    let clips = t.path().join("state").join("voice_clips");
    let tr = Transcriber::new(&t.path().join("models"), Some(&clips), 1, Some(Model::default().factory()), None, false);
    tr.set_watch_freqs(vec![161.1]);
    tr.start();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut air = Air::new(1);
    for (i, freq) in [161.1, 160.95, 161.1].iter().enumerate() {
        let tx = tx.clone();
        let done: TranscriptDone = Box::new(move |_| { let _ = tx.send(()); });
        let ev = json!({"event_id": format!("e{i}"), "start_ts": 1000.0 + i as f64, "freq": freq});
        tr.submit(air.on_air(&speech(40 + i as u64, 3.0)), done, "voice", Some(ev));
        rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5)); // distinct clip names
    }
    tr.stop();
    let count = |d: &Path| std::fs::read_dir(d).map(|r| r.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "wav")).count()).unwrap_or(0);
    assert_eq!(count(&clips), 1); // the newest only
    let watched = t.path().join("state").join("watch_clips").join("161.1000");
    assert_eq!(count(&watched), 2); // both 161.100 calls, not the 160.950 one
}

#[test]
fn watched_channel_detector_without_a_template() {
    let t = Tmp::new("csx");
    railspotting_engine::logging::setup_for_tests();
    let log = Capture::start();
    let state = t.path().join("state");
    let heard = "CSX detector, milepost? Sixteen point one. No defects. No defects. Total axle one, seven, four, \
                 and one train speed.";
    let model = Model::text(heard);
    let tr = Transcriber::new(&t.path().join("models"), Some(&state.join("voice_clips")), 50, Some(model.factory()),
                              None, false);
    tr.set_watch_freqs(vec![161.1]);
    tr.set_known_detectors(Some(vec![
        json!({"railroad": "CSX", "subdivision": "Belton", "milepost": 68.1, "name": "Try"}),
        json!({"railroad": "NS", "subdivision": "Charlotte", "milepost": 475.8, "name": "Taylors"}),
    ]));
    let ev = |freq: f64| json!({"event_id": "e1", "start_ts": 1000.0, "end_ts": 1018.4, "freq": freq, "railroad": "CSX"});
    let x = speech(51, 12.0);
    let res = tr.transcribe(&x, Some(&ev(161.1)), false).unwrap().unwrap();
    // matched to the railroad's listed detector despite the misheard number, with the train details
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "CSX", "milepost": 68.1, "axles": 174,
                                     "defects": false}), "{res:?}");
    assert!(res.get("_detector").is_none()); // a transcript only until Try has a template
    assert!(log.text().contains("Try (csx-68.1), milepost 16.1 heard, match suggested, 174 axles"), "{}", log.text());
    // the channel's railroad picks its detectors' voice profile, whose wording is the prompt
    assert_eq!(model.calls()[0].1.initial_prompt, "CSX equipment defect detector, milepost. No defects. No defects. Total axle. End of transmission.");
    let kept: Vec<_> = std::fs::read_dir(state.join("detector_candidates/csx-68.1")).unwrap().flatten().collect();
    assert_eq!(kept.len(), 2); // the recording and what was heard
    // the recording is kept as heard, not filtered (templates and tests filter it themselves)
    let wav_path = kept.iter().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "wav")).unwrap();
    let kept_audio = railspotting_engine::wav::read(&wav_path).unwrap().samples();
    let expected: Vec<f32> = { let pk = x.iter().fold(0f32, |m, v| m.max(v.abs())); x.iter().map(|v| v * 0.9 / pk).collect() };
    let diff = kept_audio.iter().zip(&expected).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
    assert!(kept_audio.len() == x.len() && diff < 0.01, "kept audio differs from the original by up to {diff}");
    let meta_path = kept.iter().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "json")).unwrap();
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(meta_path).unwrap()).unwrap();
    assert!(meta["match"] == "suggested" && meta["milepost_heard"] == 16.1 && meta["detector"]["name"] == "Try");

    // the channel's railroad stands in when speech loses the "CSX"
    let tr2 = Transcriber::new(&t.path().join("m2"), None, 50,
                               Some(Model::text("Detector, milepost, six, eight points, one, no defects").factory()), None, false);
    tr2.set_watch_freqs(vec![161.1]);
    tr2.set_known_detectors(Some(vec![json!({"railroad": "CSX", "milepost": 68.1, "name": "Try"})]));
    let res = tr2.transcribe(&x, Some(&ev(161.1)), false).unwrap().unwrap();
    assert!(res["parsed"]["railroad"] == "CSX" && res["parsed"]["milepost"] == 68.1, "{res:?}");
    assert!(log.text().contains("milepost 68.1 heard, match heard"));

    // crew talk on the watched channel is not reported, and short calls or other channels are not transcribed
    let crew = Model::text("CSX 4512 to the dispatcher, we are by the signal at Greer, over.");
    let tr3 = Transcriber::new(&t.path().join("m3"), None, 50, Some(crew.factory()), None, false);
    tr3.set_watch_freqs(vec![161.1]);
    assert!(tr3.transcribe(&x, Some(&ev(161.1)), false).unwrap().is_none());
    assert_eq!(tr3.last_reject().unwrap()["reason"], "watched channel call, not a detector announcement");
    assert!(tr3.transcribe(&speech(52, 3.0), Some(&ev(161.1)), false).unwrap().is_none());
    assert!(tr3.transcribe(&x, Some(&ev(160.95)), false).unwrap().is_none());
    assert_eq!(crew.calls().len(), 1);
}

fn nearby(try_confirmed: bool) -> Vec<Value> {
    vec![
        json!({"detector_id": "ns-475.8", "railroad": "NS", "subdivision": "Charlotte", "milepost": 475.8, "name": "Taylors",
               "status": "approved", "distance_mi": 1,
               "channel": {"freq": 160.95, "ch": 5, "label": "Charlotte Dist. Road", "source": "heard"},
               "transcript": {"source": "railroad", "confirmed": true,
                              "template": "Norfolk Southern, milepost {milepost}, no defects. Repeat, no defects.",
                              "text": "Norfolk Southern, milepost 475.8, no defects. Repeat, no defects.",
                              "spoken": "norfolk southern milepost four seven five point eight no defects repeat no defects",
                              "variables": []}}),
        json!({"detector_id": "csx-68.1", "railroad": "CSX", "subdivision": "Belton", "milepost": 68.1, "name": "Try",
               "status": "listed", "distance_mi": 3,
               "channel": {"freq": 161.1, "ch": 7, "label": "Belton Sub Road", "source": "road_channel"},
               "transcript": {"source": if try_confirmed { "detector" } else { "railroad" }, "confirmed": try_confirmed,
                              "template": "CSX equipment detector, milepost {milepost}, track {track}, no defects, total axles {axles}, temperature {temperature} degrees, detector out.",
                              "text": "CSX equipment detector, milepost 68.1, track {track}, no defects, total axles {axles}, temperature {temperature} degrees, detector out.",
                              "spoken": "c s x equipment detector milepost six eight point one track {track} no defects total axles {axles} temperature {temperature} degrees detector out",
                              "variables": ["track", "axles", "temperature"]}}),
        // a detector whose channel isn't in this receiver's plan
        json!({"detector_id": "ns-492.5", "railroad": "NS", "subdivision": "", "milepost": 492.5, "name": "Lathem",
               "status": "listed", "distance_mi": 9.8,
               "channel": {"freq": 160.2, "ch": null, "label": "", "source": "road_channel"}, "transcript": null}),
    ]
}

#[test]
fn nearby_detectors_v16() {
    let t = Tmp::new("v16");
    railspotting_engine::logging::setup_for_tests();
    let log = Capture::start();
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None); // a Norfolk Southern template
    let state = t.path().join("state");
    let heard = "Equipment defect, six eight point one, no defects, no defects, total axles one seven four, end of train.";
    let model = Model::text(heard);
    let tr = Transcriber::new(&t.path().join("models"), Some(&state.join("voice_clips")), 50, Some(model.factory()),
                              Some(&t.path().join("t")), false);
    tr.set_nearby_detectors(Some(nearby(false)));
    let ev = |freq: f64, rr: &str| json!({"event_id": "e", "start_ts": 1000.0, "end_ts": 1012.0, "freq": freq, "railroad": rr});
    let x = speech(61, 12.0);

    // NS has a template, so its road channel is not watched for speech
    assert!(tr.transcribe(&x, Some(&ev(160.95, "NS")), false).unwrap().is_none());
    assert!(model.calls().is_empty());

    // Try's channel is watched with no watch_clips setting, and its wording alone identifies it
    let res = tr.transcribe(&x, Some(&ev(161.1, "CSX")), false).unwrap().unwrap();
    assert_eq!(res["parsed"], json!({"type": "defect_detector", "railroad": "CSX", "milepost": 68.1, "axles": 174,
                                     "defects": false}), "{res:?}");
    assert!(log.text().contains("Try (csx-68.1), milepost 68.1 heard, match heard"), "{}", log.text());
    assert!(state.join("detector_candidates/csx-68.1").is_dir());
    // Try uses the CSX profile by default, whose wording is the prompt
    assert_eq!(model.calls()[0].1.initial_prompt, "CSX equipment defect detector, milepost. No defects. No defects. Total axle. End of transmission.");

    // the headend assigns a profile to Try's channel, and can send its own profiles
    let model2 = Model::text(heard);
    let tr2 = Transcriber::new(&t.path().join("m2"), None, 50, Some(model2.factory()), Some(&t.path().join("t")), false);
    tr2.set_nearby_detectors(Some(nearby(true)));
    let mut heard_wording = tr2.find_profile("csx-edd-v1").unwrap();
    heard_wording.id = "csx-try-heard-v1".into();
    heard_wording.name = "CSX as heard".into();
    heard_wording.template = "CSX equipment defect detector, milepost {milepost}. No defects, no defects. Total axles {axles}. End of train.".into();
    let mut broken = heard_wording.clone();
    broken.id = "Bad Id".into();
    tr2.set_profile_config(&json!({
        "channels": [{"ch": 7, "freq": 161.1, "railroad": "CSX", "label": "Belton Sub Road", "kind": "voice", "profile": "csx-try-heard-v1"},
                     {"ch": 8, "freq": 161.37, "railroad": "CSX", "label": "Dispatcher", "kind": "voice", "profile": "no-such-profile"},
                     {"ch": 9, "freq": 457.9375, "railroad": null, "label": "EOT", "kind": "telemetry"}],
        "voice_profiles": [serde_json::to_value(&heard_wording).unwrap(), serde_json::to_value(&broken).unwrap()]}));
    let res = tr2.transcribe(&x, Some(&ev(161.1, "CSX")), false).unwrap().unwrap();
    assert_eq!(model2.calls()[0].1.initial_prompt,
               "CSX equipment defect detector, milepost. No defects, no defects. Total axles. End of train.");
    // the headend sends profiles, so the transcript says which one read it
    assert_eq!(res["profile"], "csx-try-heard-v1");
    let listed = tr2.profiles_json();
    let ids: Vec<&str> = listed["profiles"].as_array().unwrap().iter().map(|q| q["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"csx-try-heard-v1") && ids.contains(&"csx-edd-v1") && !ids.contains(&"Bad Id"), "{ids:?}");
    let chans = listed["channels"].as_array().unwrap();
    assert_eq!(chans.len(), 2, "voice channels only: {chans:?}");
    assert!(chans[0]["profile"] == "csx-try-heard-v1" && chans[0]["assigned"] == "csx-try-heard-v1"
            && chans[0]["detectors"][0]["detector_id"] == "csx-68.1", "{chans:?}");
    // an unknown profile on a channel falls back to the railroad's default, which is now the
    // headend's CSX profile (the headend's own profiles come before the built-in ones)
    assert!(chans[1]["profile"] == "csx-try-heard-v1" && chans[1]["assigned"] == "no-such-profile"
            && chans[1]["default"] == "csx-try-heard-v1", "{chans:?}");
    // a headend profile with a built-in id replaces the built-in one
    let mut ns = tr2.find_profile("ns-synthetic-v1").unwrap();
    ns.name = "NS from the headend".into();
    tr2.set_profile_config(&json!({"channels": [], "voice_profiles": [serde_json::to_value(&ns).unwrap()]}));
    assert_eq!(tr2.find_profile("ns-synthetic-v1").unwrap().name, "NS from the headend");
    assert_eq!(tr2.profiles_json()["profiles"].as_array().unwrap().iter().filter(|q| q["id"] == "ns-synthetic-v1").count(), 1);
    // without voice_profiles in the config: built-in profiles, and no profile field in transcripts
    tr2.set_profile_config(&json!({"channels": []}));
    let res = tr2.transcribe(&x, Some(&ev(161.1, "CSX")), false).unwrap().unwrap();
    assert!(res.get("profile").is_none());
    assert_eq!(tr2.profile_for(Some(161.1), "CSX").unwrap().id, "csx-edd-v1");

    // a nearby detector on a channel outside the plan is not watched
    assert!(tr2.transcribe(&x, Some(&ev(160.2, "NS")), false).unwrap().is_none());
}

#[test]
fn interference_is_not_a_detector() {
    let t = Tmp::new("burst");
    railspotting_engine::logging::setup_for_tests();
    // what speech recognition made of the 07:22 burst on 10-13
    let looping = "Total axle All equipment defect detector, milepost 69.1. No defects. Total axle All equipment \
                   defect detector, milepost 68.1. No defects. Total axle All equipment defect detector, milepost 69.1. \
                   No defects.";
    let model = Model::text(looping);
    let tr = Transcriber::new(&t.path().join("models"), None, 50, Some(model.factory()), None, false);
    tr.set_watch_freqs(vec![161.1]);
    tr.set_known_detectors(Some(vec![json!({"railroad": "CSX", "milepost": 68.1, "name": "Try"})]));
    let ev = |n: u64| json!({"event_id": "e", "start_ts": 1000.0, "end_ts": 1006.4, "freq": 161.1, "railroad": "CSX",
                             "simultaneous": n});
    let x = speech(71, 6.5);
    // opened with seven other channels: not transcribed at all
    assert!(tr.transcribe(&x, Some(&ev(7)), false).unwrap().is_none());
    assert_eq!(tr.last_reject().unwrap()["reason"], "interference: opened with other channels");
    assert!(model.calls().is_empty());
    // alone on its channel but looping: rejected after speech recognition
    assert!(tr.transcribe(&x, Some(&ev(1)), false).unwrap().is_none());
    assert_eq!(tr.last_reject().unwrap()["reason"], "repeating transcript");
    // a real announcement says "no defects" twice and passes
    let real = Model::text("CSX equipment defect detector, milepost 68.1. No defects, no defects. Total axles 1-7-4. End of train.");
    let tr2 = Transcriber::new(&t.path().join("m2"), None, 50, Some(real.factory()), None, false);
    tr2.set_watch_freqs(vec![161.1]);
    tr2.set_known_detectors(Some(vec![json!({"railroad": "CSX", "milepost": 68.1, "name": "Try"})]));
    assert!(tr2.transcribe(&x, Some(&ev(0)), false).unwrap().is_some());
}

#[test]
fn sdr_counts_channels_opening_together() {
    let t = Tmp::new("simul");
    let jobs: Arc<Mutex<Vec<(TranscriptDone, Option<Value>)>>> = Arc::default();
    struct Tr(Arc<Mutex<Vec<(TranscriptDone, Option<Value>)>>>);
    impl railspotting_dsp::sdr::Transcriber for Tr {
        fn submit(&self, _a: Vec<f32>, done: TranscriptDone, _tag: &str, event: Option<Value>) {
            self.0.lock().unwrap().push((done, event));
        }
    }
    let (_events, emit) = sink();
    let mut src = SdrSource::new(&vhf(), &json!({}), t.path(), emit, Some(Arc::new(Tr(jobs.clone()))),
                                 railspotting_common::system_clock());
    let p = plan("7");
    src.set_plan(&channels(&p), Some(&p["reference"]));
    let w = railspotting_dsp::sim::World::default();
    // a burst on three channels at once, and later one call alone
    for f in [160.95, 161.1, 161.31] {
        w.carrier(f, 25.0, 1.0, Some(2.5), 700.0, 2500.0, "voice");
    }
    w.carrier(161.1, 25.0, 4.0, Some(5.5), 700.0, 2500.0, "voice");
    run(&mut src, &w, 0.0, 6.5);
    let got: Vec<(f64, f64, u64)> = jobs.lock().unwrap().iter().filter_map(|(_, e)| e.as_ref()).map(|e| {
        (e["freq"].as_f64().unwrap(), e["start_ts"].as_f64().unwrap(), e["simultaneous"].as_u64().unwrap())
    }).collect();
    assert!(got.iter().filter(|(_, s, _)| *s < 3.0).all(|(_, _, n)| *n == 2), "{got:?}");
    assert!(got.iter().any(|(f, s, n)| (*f - 161.1).abs() < 0.001 && *s > 3.0 && *n == 0), "{got:?}");
}

#[test]
fn a_channels_profile_reads_its_sound_matched_announcements() {
    let t = Tmp::new("chanprof");
    let mut air = Air::new(1);
    add_template(&t.path().join("t"), &air.recs(), None);
    let model = Model::text(ALL_CLEAR);
    let tr = tr_with(t.path(), &model, Some(&t.path().join("t")), true);
    let x = air.on_air(&air.message(&air.no_defects.clone()));
    let ev = |f: f64| json!({"event_id": "e", "start_ts": 1000.0, "end_ts": 1009.2, "freq": f});
    // by default the railroad's profile
    tr.transcribe(&x, Some(&ev(160.95)), false).unwrap();
    assert_eq!(model.calls()[0].1.initial_prompt, NS_PROMPT);
    // the headend's profile for that channel; another channel keeps the default
    tr.set_profile_config(&json!({"channels": [{"freq": 160.95, "railroad": "NS", "kind": "voice", "profile": "generic-v1"},
                                               {"freq": 160.905, "railroad": "NS", "kind": "voice"}]}));
    tr.transcribe(&x, Some(&ev(160.95)), false).unwrap();
    tr.transcribe(&x, Some(&ev(160.905)), false).unwrap();
    let prompts: Vec<String> = model.calls().iter().map(|c| c.1.initial_prompt.clone()).collect();
    assert!(prompts[1].starts_with("Defect detector, milepost") && prompts[2] == NS_PROMPT, "{prompts:?}");
}
