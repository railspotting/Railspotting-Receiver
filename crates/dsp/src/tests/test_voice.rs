//! The dsp side of voice handling: AudioTap and the held voice events.

use super::*;
use crate::sdr::{Transcriber, TranscriptDone};
use crate::sim::World;
use crate::voice::AudioTap;
use num_complex::Complex32;
use serde_json::json;

#[test]
fn audiotap_1khz_tone() {
    let (rate, center, f) = (2_880_000.0, 161.5452, 161.10);
    let w = World::default();
    w.carrier(f, 25.0, 0.0, None, 1000.0, 2500.0, "voice");
    let mut tap = AudioTap::new(rate, (f - center) * 1e6);
    let n = 114_688;
    let mut t = 0.0;
    while t < 2.0 {
        tap.feed(&w.iq(center, rate, t, n));
        t += n as f64 / rate;
    }
    let a = tap.audio();
    assert_eq!(tap.rate, 16_000.0);
    assert!((tap.duration_s() - t).abs() <= 0.01);
    let seg: Vec<f64> = a[4000..].iter().map(|v| *v as f64).collect();
    let pk = peak_hz_real(&seg, 16_000.0);
    assert!((pk - 1000.0).abs() <= 3.0, "{pk}");
}

#[test]
fn audiotap_caps_at_max() {
    let mut tap = AudioTap::with_max(2_400_000.0, 100_000.0, 0.5);
    let ones = vec![Complex32::new(1.0, 0.0); 98_304];
    for _ in 0..30 {
        tap.feed(&ones);
    }
    assert!((tap.duration_s() - 0.5).abs() <= 0.001);
}

type Job = (Vec<f32>, Option<TranscriptDone>, Option<Value>);

/// Fake: keeps each clip until the test releases it.
#[derive(Default)]
struct HoldingTranscriber {
    jobs: Mutex<Vec<Job>>,
}

impl Transcriber for HoldingTranscriber {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, _tag: &str, event: Option<Value>) {
        self.jobs.lock().unwrap().push((audio, Some(done), event));
    }
}

impl HoldingTranscriber {
    fn release(&self, i: usize, result: Option<Map<String, Value>>) {
        let done = self.jobs.lock().unwrap()[i].1.take().unwrap();
        done(result);
    }
}

fn vhf_with(tr: &Arc<HoldingTranscriber>, tmp: &TmpDir) -> (SdrSource, Events) {
    source_with(tmp.path(), &vhf(), Some(tr.clone() as Arc<dyn Transcriber>), json!({}))
}

#[test]
fn voice_closed_waits_for_transcript_and_has_preroll() {
    let tmp = TmpDir::new("voice-held");
    let tr = Arc::new(HoldingTranscriber::default());
    let (mut src, events) = vhf_with(&tr, &tmp);
    let w = World::default();
    w.tone(161.10, 25.0, 2.0, Some(5.0), 800.0);
    run(&mut src, &w, 0.0, 6.0);
    assert_eq!(states(&events), vec!["open"]); // closed is held
    assert_eq!(src.held(), 1);
    assert_eq!(tr.jobs.lock().unwrap().len(), 1);
    let (dur, event) = {
        let j = tr.jobs.lock().unwrap();
        (j[0].0.len() as f64 / 16_000.0, j[0].2.clone().unwrap())
    };
    assert!(dur >= 3.0 + 0.25, "{dur}"); // pre-roll before the detector confirmed
    assert_eq!(event["event_id"], all(&events)[0]["event_id"]);
    let text = "Norfolk Southern detector, milepost 476.2, track one, no defects";
    let result = json!({"text": text, "engine": "faster-whisper-small.en", "confidence": 0.91, "language": "en",
                        "duration_s": 3.6, "parsed": {"type": "defect_detector", "railroad": "NS", "milepost": 476.2,
                                                       "track": 1, "defects": false},
                        "_detector": {"detector_id": "ns-476.2"}});
    tr.release(0, result.as_object().cloned());
    let c = closed(&events);
    assert!(c.len() == 1 && c[0]["transcript"]["parsed"]["milepost"] == 476.2);
    assert!(!c[0]["transcript"].as_object().unwrap().contains_key("_detector"));
    assert_eq!(c[0]["detector"]["detector_id"], "ns-476.2");
    assert!(c[0]["event_id"] == all(&events)[0]["event_id"] && src.held() == 0);
    let lt = src.last_transcript().unwrap();
    assert!(lt["text"].as_str().unwrap().starts_with("Norfolk") && lt["freq"] == 161.1);
    assert!(logged("VHF SDR: 161.1000 MHz Belton Sub Road said: \"Norfolk Southern detector, milepost 476.2, track one, no defects\" (91%)"));
}

#[test]
fn voice_none_result_closes_without_transcript() {
    let tmp = TmpDir::new("voice-none");
    let tr = Arc::new(HoldingTranscriber::default());
    let (mut src, events) = vhf_with(&tr, &tmp);
    let w = World::default();
    w.tone(161.10, 25.0, 2.0, Some(4.0), 800.0);
    run(&mut src, &w, 0.0, 5.0);
    tr.release(0, None);
    let c = closed(&events);
    assert!(c.len() == 1 && !c[0].contains_key("transcript"));
}

#[test]
fn short_voice_closes_immediately() {
    let tmp = TmpDir::new("voice-short");
    let tr = Arc::new(HoldingTranscriber::default());
    let (mut src, events) = vhf_with(&tr, &tmp);
    let w = World::default();
    w.tone(161.10, 25.0, 2.0, Some(2.3), 800.0);
    run(&mut src, &w, 0.0, 3.5);
    assert!(tr.jobs.lock().unwrap().is_empty());
    let c = closed(&events);
    assert!(c.len() == 1 && !c[0].contains_key("transcript"));
}

#[test]
fn telemetry_never_carries_transcript() {
    let tmp = TmpDir::new("voice-eot");
    let tr: Arc<dyn Transcriber> = Arc::new(HoldingTranscriber::default());
    let (src, _) = source_with(tmp.path(), &eot_dev(), Some(tr), json!({}));
    assert!(src.transcriber.is_none()); // only the VHF dongle transcribes
}

#[test]
fn reference_tap_delivers_audio_once() {
    let tmp = TmpDir::new("ref-tap");
    let (mut src, _) = source(tmp.path(), &vhf());
    let got: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![]));
    assert!(!src.record_reference(0.5, Box::new(|_| {}))); // not streaming yet
    src.streaming = true;
    let g = got.clone();
    assert!(src.record_reference(0.5, Box::new(move |a| g.lock().unwrap().push(a.len()))));
    let w = World::default();
    w.tone(162.55, 30.0, 0.0, None, 1000.0);
    run(&mut src, &w, 0.0, 1.0);
    let got = got.lock().unwrap();
    assert_eq!(got.len(), 1);
    assert!(got[0] >= 8000);
}
