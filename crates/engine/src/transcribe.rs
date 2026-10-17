//! Local speech-to-text and the detector-template side of transcription. The Transcriber:
//! the queue and worker, the v1.5 detector result, milepost numbers, repeats and saved clips.
//! The speech model itself is whisper.cpp behind the cargo feature "speech"; without
//! it detector templates still match and speech recognition reports itself unavailable.

use crate::util::{fmt_g, json_dumps, local_stamp, list_text_opt, repr_text, truthy};
use crate::wav;
use crossbeam_channel::{bounded, Receiver, Sender};
use railspotting_detect::detectors::{self as det, Assessment, DetectorTemplates, Template, CLIP_LEAD_S, HOP,
                                     NUMBER_MAX_S};
use railspotting_detect::profile::{self, Profile, Reading};
use railspotting_detect::text::{self, FILLERS, MAX_NO_SPEECH, MIN_AUDIO_S, MIN_CONFIDENCE, MODEL, NUMBER_PROMPT,
                                PROMPT, REPEAT_MAX_S, REPEAT_MIN_S};
use railspotting_dsp::sdr::TranscriptDone;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;

pub use railspotting_detect::text::{DETECTOR_CLIP_KEEP, ENGINE};

/// The profiles a receiver can use. The headend sends them in /config (`voice_profiles`, and each
/// channel's `profile`); the built-in ones (the repository's profiles/ folder) fill in until it
/// does. A headend profile with a built-in profile's id replaces it.
pub struct Profiles {
    builtin: Vec<Profile>,
    headend: Vec<Profile>,
    /// channel (frequency key) to profile id, from /config channels[].profile
    assigned: Map<String, Value>,
    /// the headend sends voice profiles, so transcripts may say which one read them
    from_headend: bool,
}

impl Profiles {
    pub fn new() -> Profiles {
        Profiles { builtin: Profile::builtin(), headend: vec![], assigned: Map::new(), from_headend: false }
    }

    /// Take the profiles and channel assignments from a /config. Invalid profiles are left out
    /// (and logged); a channel naming an unknown profile uses its railroad's default.
    pub fn set_from_config(&mut self, profiles: Option<&Vec<Value>>, channels: &[Value]) {
        self.from_headend = profiles.is_some();
        self.headend = profiles.into_iter().flatten().filter_map(|v| {
            match serde_json::from_value::<Profile>(v.clone()) {
                Ok(p) if p.problems().is_empty() => Some(p),
                Ok(p) => {
                    log::warn!("voice profile {} from the headend ignored: {}", p.id, p.problems().join("; "));
                    None
                }
                Err(e) => {
                    log::warn!("a voice profile from the headend ignored: {e}");
                    None
                }
            }
        }).collect();
        self.assigned = channels.iter().filter_map(|c| {
            let f = c.get("freq").and_then(Value::as_f64)?;
            let id = c.get("profile").and_then(Value::as_str).filter(|s| !s.is_empty())?;
            Some((channel_key(f), json!(id)))
        }).collect();
        for (k, id) in &self.assigned {
            if id.as_str().is_some_and(|i| self.get(i).is_none()) {
                log::warn!("channel {k} MHz names voice profile {id}, which this receiver doesn't have; using the default");
            }
        }
    }

    /// Headend profiles first, then the built-in ones they don't replace.
    pub fn all(&self) -> Vec<&Profile> {
        self.headend.iter()
            .chain(self.builtin.iter().filter(|b| !self.headend.iter().any(|h| h.id == b.id)))
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.all().into_iter().find(|p| p.id == id)
    }

    pub fn is_headend(&self, id: &str) -> bool {
        self.headend.iter().any(|p| p.id == id)
    }

    /// The default for a railroad: its headend profile, else its built-in one, else the generic one.
    pub fn default_for(&self, railroad: &str) -> Option<String> {
        let all: Vec<Profile> = self.all().into_iter().cloned().collect();
        profile::default_for(&all, railroad)
    }

    /// The profile for announcements on a channel: the headend's assignment, else the default for
    /// the railroad.
    pub fn for_channel(&self, freq: Option<f64>, railroad: &str) -> Option<Profile> {
        freq.and_then(|f| self.assignment(f)).and_then(|id| self.get(&id))
            .or_else(|| self.default_for(railroad).and_then(|id| self.get(&id)))
            .cloned()
    }

    pub fn assignment(&self, freq: f64) -> Option<String> {
        self.assigned.get(&channel_key(freq)).and_then(Value::as_str).map(String::from)
    }
}

impl Default for Profiles {
    fn default() -> Self {
        Self::new()
    }
}

/// A channel's key in the choices: its frequency in MHz with four decimals.
fn channel_key(freq: f64) -> String {
    format!("{freq:.4}")
}

/// Whether this build has a speech model.
pub const SPEECH_AVAILABLE: bool = cfg!(feature = "speech");
pub const UNAVAILABLE: &str = "speech recognition unavailable (receiver built without the speech feature)";
pub const QUEUE_SIZE: usize = 20;
/// A milepost heard this near the template's is the template's detector, misheard (detectors are
/// at most about 10 miles apart).
pub const SAME_TEMPLATE_MI: f64 = 7.0;
/// Clips kept per watched channel folder (state/watch_clips), apart from the newest voice clips.
pub const WATCH_CLIP_KEEP: usize = 300;
/// Calls on a watched channel this long are transcribed even with transcribe_other_calls off, to
/// catch detector announcements that have no template yet (Try on CSX was 18.4 s).
pub const WATCH_MIN_S: f64 = 5.0;
pub const WATCH_MAX_S: f64 = 45.0;
/// Announcements kept per suggested detector in state/detector_candidates, to build its template.
pub const CANDIDATE_KEEP: usize = 50;
/// Speech confidence needed on a watched channel. Lower than MIN_CONFIDENCE because the text must
/// also read as a detector announcement; Try's first recording scored 0.39 yet read "milepost,
/// 6/8.1, no defects, no defects, total axle, 1/7/4, end of train".
pub const WATCH_MIN_CONFIDENCE: f64 = 0.25;
/// Share of a nearby detector's expected words (its `spoken` text) that makes a transcript its
/// announcement even when "detector" and "milepost" were misheard.
pub const WORDING_MIN: f64 = 0.5;
/// A watched-channel call that opened together with this many other channels is a burst of
/// interference, not a detector (one at 07:22 on 10-13 opened 8 channels and was heard as Try).
pub const BURST_CHANNELS: u64 = 2;
/// Detector prompts for watched channels: each railroad's own wording, without real mileposts
/// (listing them biased the number, once into "4568.9").
const WATCH_PROMPT: &str = "Defect detector, milepost, no defects, no defects, total axles, train speed.";

/// One recognized segment from the speech model.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub avg_logprob: f64,
    pub no_speech_prob: f64,
}

/// The decoding options passed to the speech model.
#[derive(Debug, Clone, PartialEq)]
pub struct SpeechOptions {
    pub language: String,
    pub beam_size: u32,
    pub vad_filter: bool,
    pub initial_prompt: String,
    pub condition_on_previous_text: bool,
    pub temperature: Option<f64>,
    pub max_new_tokens: Option<u32>,
    pub without_timestamps: bool,
}

impl SpeechOptions {
    fn new(beam_size: u32, vad_filter: bool, prompt: &str) -> Self {
        SpeechOptions { language: "en".into(), beam_size, vad_filter, initial_prompt: prompt.into(),
                        condition_on_previous_text: false, temperature: None, max_new_tokens: None,
                        without_timestamps: false }
    }
}

/// A speech model: segments and the language it heard.
pub trait SpeechModel: Send {
    fn transcribe(&mut self, audio: &[f32], opts: &SpeechOptions) -> Result<(Vec<Segment>, Option<String>), String>;
}

pub type ModelFactory = Arc<dyn Fn() -> Result<Box<dyn SpeechModel>, String> + Send + Sync>;

/// The model this build has: whisper.cpp with the "speech" feature, otherwise none.
pub fn default_factory(models_dir: &Path) -> ModelFactory {
    #[cfg(feature = "speech")]
    {
        let dir = models_dir.to_path_buf();
        Arc::new(move || crate::whisper::load(&dir))
    }
    #[cfg(not(feature = "speech"))]
    {
        let _ = models_dir;
        Arc::new(|| Err(UNAVAILABLE.to_string()))
    }
}

/// A factory that never has a model (speech off).
pub fn no_speech(reason: &'static str) -> ModelFactory {
    Arc::new(move || Err(reason.to_string()))
}

struct Job {
    audio: Vec<f32>,
    done: TranscriptDone,
    tag: String,
    event: Option<Value>,
}

#[derive(Debug, Clone)]
struct LastCall {
    start_ts: f64,
    event_id: Value,
    repeat: bool,
}

pub struct Transcriber {
    me: Weak<Transcriber>,
    pub models_dir: PathBuf,
    pub clips_dir: Option<PathBuf>,
    pub clip_keep: usize,
    model_factory: Mutex<ModelFactory>,
    /// Whether the factory can give a model at all (false: speech is reported unavailable).
    speech: AtomicBool,
    pub rate: usize,
    pub templates: Option<Mutex<DetectorTemplates>>,
    // Speech recognition for calls that aren't a known detector. Off by default in the
    // receiver: since the new antenna it gave no usable transcript in hundreds of crew calls,
    // and it was the receiver's heaviest work.
    other_calls: AtomicBool,
    last_detector_call: Mutex<HashMap<String, LastCall>>,
    known_detectors: Mutex<Option<Vec<Value>>>,
    /// the headend's nearby_detectors (contract v1.6): channel and wording of each detector near
    /// this receiver
    nearby: Mutex<Vec<Value>>,
    /// detector voice profiles: built in, plus the owner's custom ones and choices
    profiles: Mutex<Profiles>,
    /// the channel plan's channels (from /config), for choosing profiles per channel
    channels: Mutex<Vec<Value>>,
    /// the headend accepts `method: "words"` in v1.5 detector objects (contract v1.9)
    words_method: AtomicBool,
    /// channels (MHz) whose every clip is kept in watch_clips, for detectors without a template
    watch_freqs: Mutex<Vec<f64>>,
    model: Mutex<Option<Box<dyn SpeechModel>>>,
    tx: Sender<Job>,
    rx: Receiver<Job>,
    dropped: AtomicU64,
    last_reject: Mutex<Option<Value>>,
    last_result: Mutex<Option<Value>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stop: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Round to `n` decimals, half to even.
fn round(x: f64, n: usize) -> f64 {
    railspotting_scanner::util::round_even(x, n)
}

/// Scale to a peak of 0.9: the factor is worked out in f64, then applied in f32.
fn normalize(audio: &[f32]) -> Vec<f32> {
    let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs())) as f64;
    if peak > 0.0 {
        let k = (0.9 / peak) as f32;
        audio.iter().map(|x| x * k).collect()
    } else {
        audio.to_vec()
    }
}

/// a[lo..hi] with both bounds clamped to the slice, empty when hi <= lo.
fn slice(a: &[f32], lo: usize, hi: usize) -> &[f32] {
    let lo = lo.min(a.len());
    &a[lo..hi.clamp(lo, a.len())]
}

/// Whether a transcript repeats a phrase of three or more words three or more times, the usual
/// sign of speech recognition looping on noise ("...detector, milepost 69.1. No defects. Total
/// axle All equipment defect detector, milepost 69.1. No defects. ..."). A detector says its
/// "no defects" twice, which this allows.
fn repeats_itself(text: &str) -> bool {
    let words: Vec<String> = text.to_lowercase().split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string()).filter(|w| !w.is_empty()).collect();
    for n in 3..=6 {
        let mut counts: HashMap<&[String], usize> = HashMap::new();
        for g in words.windows(n) {
            let c = counts.entry(g).or_default();
            *c += 1;
            if *c >= 3 {
                return true;
            }
        }
    }
    false
}

fn weighted_confidence(kept: &[&Segment]) -> f64 {
    let weights: Vec<f64> = kept.iter().map(|s| (s.end - s.start).max(0.01)).collect();
    let num: f64 = weights.iter().zip(kept).map(|(w, s)| w * s.avg_logprob.exp()).sum();
    num / weights.iter().sum::<f64>()
}

impl Transcriber {
    pub fn new(models_dir: &Path, clips_dir: Option<&Path>, clip_keep: usize, model_factory: Option<ModelFactory>,
               templates_dir: Option<&Path>, other_calls: bool) -> Arc<Transcriber> {
        let speech = model_factory.is_some() || SPEECH_AVAILABLE;
        let factory = model_factory.unwrap_or_else(|| default_factory(models_dir));
        let (tx, rx) = bounded(QUEUE_SIZE);
        Arc::new_cyclic(|me| Transcriber {
            me: me.clone(),
            models_dir: models_dir.to_path_buf(),
            clips_dir: clips_dir.map(Path::to_path_buf),
            clip_keep,
            model_factory: Mutex::new(factory),
            speech: AtomicBool::new(speech),
            rate: 16_000,
            templates: templates_dir.map(|d| Mutex::new(DetectorTemplates::new(d))),
            other_calls: AtomicBool::new(other_calls),
            last_detector_call: Mutex::new(HashMap::new()),
            known_detectors: Mutex::new(None),
            nearby: Mutex::new(vec![]),
            profiles: Mutex::new(Profiles::new()),
            channels: Mutex::new(vec![]),
            words_method: AtomicBool::new(false),
            watch_freqs: Mutex::new(vec![]),
            model: Mutex::new(None),
            tx, rx,
            dropped: AtomicU64::new(0),
            last_reject: Mutex::new(None),
            last_result: Mutex::new(None),
            thread: Mutex::new(None),
            stop: AtomicBool::new(false),
        })
    }

    /// Swap the speech model (tests, or a build with none).
    pub fn set_model_factory(&self, f: ModelFactory, available: bool) {
        *lock(&self.model_factory) = f;
        *lock(&self.model) = None;
        self.speech.store(available, Ordering::SeqCst);
    }

    pub fn speech_available(&self) -> bool {
        self.speech.load(Ordering::SeqCst)
    }

    pub fn other_calls(&self) -> bool {
        self.other_calls.load(Ordering::SeqCst)
    }

    pub fn set_other_calls(&self, on: bool) {
        self.other_calls.store(on, Ordering::SeqCst)
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::SeqCst)
    }

    pub fn last_reject(&self) -> Option<Value> {
        lock(&self.last_reject).clone()
    }

    pub fn last_result(&self) -> Option<Value> {
        lock(&self.last_result).clone()
    }

    pub fn known_detectors(&self) -> Option<Vec<Value>> {
        lock(&self.known_detectors).clone()
    }

    pub fn set_watch_freqs(&self, freqs: Vec<f64>) {
        *lock(&self.watch_freqs) = freqs;
    }

    /// Contract v1.9: the headend accepts `method: "words"`, so an all-clear proven by its words
    /// alone can be a detector result.
    pub fn set_words_method(&self, on: bool) {
        self.words_method.store(on, Ordering::SeqCst);
    }

    /// The channel plan and voice profiles from /config: `channels` (with each channel's
    /// `profile`) and `voice_profiles`.
    pub fn set_profile_config(&self, cfg: &Value) {
        let channels = cfg.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
        lock(&self.profiles).set_from_config(cfg.get("voice_profiles").and_then(Value::as_array), &channels);
        *lock(&self.channels) = channels;
    }

    /// The profile for announcements on a channel (None: the railroad's default).
    pub fn profile_for(&self, freq: Option<f64>, railroad: &str) -> Option<Profile> {
        lock(&self.profiles).for_channel(freq, railroad)
    }

    pub fn find_profile(&self, id: &str) -> Option<Profile> {
        lock(&self.profiles).get(id).cloned()
    }

    /// The profiles, the plan's voice channels with the profile each uses and the detectors heard
    /// on it, for the status page.
    pub fn profiles_json(&self) -> Value {
        let reg = lock(&self.profiles);
        let list: Vec<Value> = reg.all().iter().map(|p| {
            let mut v = p.to_value(!reg.is_headend(&p.id));
            v["source"] = json!(if reg.is_headend(&p.id) { "headend" } else { "built in" });
            v
        }).collect();
        let nearby = lock(&self.nearby).clone();
        let channels: Vec<Value> = lock(&self.channels).iter()
            .filter(|c| c.get("kind").and_then(Value::as_str).unwrap_or("voice") == "voice")
            .filter_map(|c| {
                let f = c.get("freq").and_then(Value::as_f64)?;
                let rr = c.get("railroad").and_then(Value::as_str).unwrap_or("");
                let detectors: Vec<Value> = nearby.iter()
                    .filter(|d| d.pointer("/channel/freq").and_then(Value::as_f64).is_some_and(|x| (x - f).abs() < 0.00125))
                    .map(|d| json!({"detector_id": d.get("detector_id"), "name": d.get("name"), "milepost": d.get("milepost")}))
                    .collect();
                Some(json!({"freq": f, "ch": c.get("ch"), "label": c.get("label"), "railroad": rr,
                            "profile": reg.for_channel(Some(f), rr).map(|p| p.id), "assigned": reg.assignment(f),
                            "default": reg.default_for(rr), "detectors": detectors}))
            }).collect();
        json!({"profiles": list, "channels": channels, "from_headend": reg.from_headend})
    }

    /// Which profile read an announcement, for its transcript: only once the headend sends voice
    /// profiles, so a headend that doesn't know the field never sees it.
    fn profile_note(&self, p: Option<&Profile>) -> Option<Value> {
        let reg = lock(&self.profiles);
        p.filter(|_| reg.from_headend).map(|p| json!(p.id))
    }

    /// Speech recognition of a whole detector announcement with a profile: its audio filter and
    /// its wording as the prompt (values left out). None without speech, or when nothing was heard.
    fn transcribe_announcement(&self, audio: &[f32], p: &Profile) -> Result<Option<(String, f64)>, String> {
        if !self.speech_available() {
            return Ok(None);
        }
        let x = if p.audio.filter == "voice" { normalize(&det::clean_voice(audio)) } else { normalize(audio) };
        let opts = SpeechOptions::new(3, false, &p.prompt());
        let (segments, _) = self.with_model(|m| m.transcribe(&x, &opts))?;
        let kept: Vec<&Segment> = segments.iter()
            .filter(|s| !s.text.trim().is_empty() && s.no_speech_prob <= MAX_NO_SPEECH).collect();
        if kept.is_empty() {
            return Ok(None);
        }
        let text = kept.iter().map(|s| s.text.trim()).collect::<Vec<_>>().join(" ");
        // the voice's known mishearings put right ("so defects" for "no defects")
        Ok(Some((p.correct(&text), weighted_confidence(&kept))))
    }

    /// The status page's test: a saved recording read with a profile, and how long it took.
    pub fn test_profile(&self, audio: &[f32], p: &Profile) -> Result<Value, String> {
        let problems = p.problems();
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        if !self.speech_available() {
            return Err(UNAVAILABLE.into());
        }
        let t0 = std::time::Instant::now();
        let heard = self.transcribe_announcement(audio, p)?;
        let seconds = t0.elapsed().as_secs_f64();
        let Some((text, conf)) = heard else {
            return Ok(json!({"profile": p.id, "prompt": p.prompt(), "text": null, "seconds": round(seconds, 1),
                             "result": "nothing heard"}));
        };
        let r = p.read(&text);
        let looping = repeats_itself(&text);
        let result = if looping { "rejected: repeating transcript" } else if r.all_clear(p) { "all clear" }
                     else if r.defect() { "defect" } else { "not confirmed" };
        Ok(json!({"profile": p.id, "prompt": p.prompt(), "text": text, "confidence": round(conf, 2),
                  "seconds": round(seconds, 1), "wording": round(r.wording, 2), "fields": r.fields,
                  "out_of_range": r.out_of_range, "all_clear_said": r.all_clear_said,
                  "defect_words": r.defect_words, "result": result}))
    }

    pub fn set_nearby_detectors(&self, d: Option<Vec<Value>>) {
        *lock(&self.nearby) = d.unwrap_or_default();
    }

    /// Nearby detectors announcing on this frequency (on a channel of this receiver's plan).
    fn nearby_on(&self, freq: f64) -> Vec<Value> {
        lock(&self.nearby).iter()
            .filter(|d| d.pointer("/channel/ch").is_some_and(|c| !c.is_null()))
            .filter(|d| d.pointer("/channel/freq").and_then(Value::as_f64).is_some_and(|f| (f - freq).abs() < 0.00125))
            .cloned().collect()
    }

    /// Railroads that have a sound template: their announcements are caught by sound, so their
    /// channels need no speech watch (NS Misco sounds like NS Taylors).
    fn template_railroads(&self) -> Vec<String> {
        self.template_list().unwrap_or_default().iter()
            .filter_map(|t| t.get("parsed").and_then(|p| p.get("railroad")).and_then(Value::as_str))
            .map(str::to_uppercase).collect()
    }

    /// A channel is watched for detector announcements when the settings name it, or when a
    /// nearby detector of a railroad without a template announces on it.
    fn watched(&self, freq: f64) -> bool {
        if lock(&self.watch_freqs).iter().any(|w| (w - freq).abs() < 0.00125) {
            return true;
        }
        let near = self.nearby_on(freq);
        if near.is_empty() {
            return false;
        }
        let covered = self.template_railroads();
        near.iter().any(|d| d.get("railroad").and_then(Value::as_str)
            .is_some_and(|r| !covered.contains(&r.to_uppercase())))
    }

    /// The voice profile for a watched channel: the owner's choice for the channel, else the
    /// default for its railroad (the channel's, or its nearby detector's).
    fn channel_profile(&self, freq: f64, railroad: Option<&str>) -> Option<Profile> {
        let near = self.nearby_on(freq);
        let rr = railroad.filter(|r| !r.is_empty())
            .or_else(|| near.first().and_then(|d| d.get("railroad")).and_then(Value::as_str)).unwrap_or("");
        self.profile_for(Some(freq), rr)
    }

    pub fn set_known_detectors(&self, d: Option<Vec<Value>>) {
        *lock(&self.known_detectors) = d;
    }

    /// The (name@milepost) -> first call memory that pairs repeats.
    pub fn last_detector_call(&self, name: &str) -> Option<Value> {
        lock(&self.last_detector_call).get(name)
            .map(|c| json!({"start_ts": c.start_ts, "event_id": c.event_id, "repeat": c.repeat}))
    }

    pub fn template_list(&self) -> Option<Vec<Map<String, Value>>> {
        self.templates.as_ref().map(|t| lock(t).list())
    }

    // worker -------------------------------------------------------------------

    pub fn start(&self) {
        let mut t = lock(&self.thread);
        if let (None, Some(me)) = (t.as_ref(), self.me.upgrade()) {
            self.stop.store(false, Ordering::SeqCst);
            *t = std::thread::Builder::new().name("transcriber".into()).spawn(move || me.run()).ok();
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Queue a clip. `done(result_or_None)` is always called exactly once. `event` (event_id,
    /// start_ts) lets a detector's repeated announcement point at the first one.
    pub fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>) {
        if let Err(e) = self.tx.try_send(Job { audio, done, tag: tag.into(), event }) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            log::warn!("transcription queue full; clip dropped");
            (e.into_inner().done)(None);
        }
    }

    fn run(self: Arc<Self>) {
        while !self.stop.load(Ordering::SeqCst) {
            let Ok(job) = self.rx.recv_timeout(std::time::Duration::from_millis(500)) else { continue };
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let result = self.transcribe(&job.audio, job.event.as_ref(), job.tag == "test")?;
                self.save_clip(&job.audio, result.as_ref(), &job.tag, job.event.as_ref());
                Ok::<_, String>(result)
            }));
            let result = match r {
                Ok(Ok(res)) => res,
                Ok(Err(e)) => {
                    log::error!("transcription failed: {e}");
                    None
                }
                Err(_) => {
                    log::error!("transcription failed");
                    None
                }
            };
            let done = job.done;
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| done(result))).is_err() {
                log::error!("transcription callback failed");
            }
        }
    }

    // transcription ------------------------------------------------------------

    fn with_model<T>(&self, f: impl FnOnce(&mut dyn SpeechModel) -> Result<T, String>) -> Result<T, String> {
        let mut m = lock(&self.model);
        if m.is_none() {
            let t0 = std::time::Instant::now();
            let factory = lock(&self.model_factory).clone();
            *m = Some(factory()?);
            log::info!("speech model {} loaded in {:.1} s", MODEL, t0.elapsed().as_secs_f64());
        }
        match m.as_mut() {
            Some(model) => f(model.as_mut()),
            None => Err("no speech model".into()),
        }
    }

    fn reject(&self, reason: &str, detail: Vec<(&str, Value)>) {
        let mut r = Map::new();
        r.insert("reason".into(), json!(reason));
        for (k, v) in detail {
            r.insert(k.into(), v);
        }
        let r = Value::Object(r);
        log::info!("no transcript: {}", json_dumps(&r));
        *lock(&self.last_reject) = Some(r);
    }

    /// A transcript (the event's `transcript` object, with `_detector` for a v1.5 detector call),
    /// None when there is none, Err when it failed.
    pub fn transcribe(&self, audio: &[f32], event: Option<&Value>, force: bool) -> Result<Option<Map<String, Value>>, String> {
        let duration = audio.len() as f64 / self.rate as f64;
        if duration < MIN_AUDIO_S {
            self.reject("too short", vec![("duration_s", json!(round(duration, 2)))]);
            return Ok(None);
        }
        let audio = normalize(audio);
        let known = self.templates.as_ref().and_then(|t| lock(t).match_audio(&audio));
        if let Some((tmpl, a)) = known {
            let result = self.detector_result(&audio, duration, &tmpl, &a, event)?;
            *lock(&self.last_result) = Some(Value::Object(result.clone()));
            return Ok(Some(result));
        }
        let freq = event.and_then(|e| e.get("freq")).and_then(Value::as_f64);
        let watched = freq.is_some_and(|f| self.watched(f));
        let channel_railroad = event.and_then(|e| e.get("railroad")).and_then(Value::as_str);
        // on a watched channel, a call as long as the channel's detector speaks is heard for one,
        // read with that detector's voice profile
        let watch_profile = if watched { freq.and_then(|f| self.channel_profile(f, channel_railroad)) } else { None };
        let (min_s, max_s, max_others) = watch_profile.as_ref()
            .map_or((WATCH_MIN_S, WATCH_MAX_S, BURST_CHANNELS - 1), |p| (p.trigger.min_s, p.trigger.max_s, p.trigger.max_other_channels));
        let mut watch_detector = watched && !self.other_calls() && !force && (min_s..=max_s).contains(&duration);
        let simultaneous = event.and_then(|e| e.get("simultaneous")).and_then(Value::as_u64).unwrap_or(0);
        if watch_detector && simultaneous > max_others {
            self.reject("interference: opened with other channels", vec![("channels", json!(simultaneous + 1))]);
            watch_detector = false;
            if !self.other_calls() {
                return Ok(None);
            }
        }
        if !(self.other_calls() || force || watch_detector) {
            *lock(&self.last_reject) = Some(json!({"reason": "not a detector call (transcribe_other_calls is off)"}));
            log::debug!("no transcript: not a detector call");
            return Ok(None);
        }
        if !self.speech_available() {
            self.reject(UNAVAILABLE, vec![]);
            return Ok(None);
        }
        let prompt = match (&watch_profile, watch_detector) {
            (Some(p), true) => p.prompt(),
            (None, true) => WATCH_PROMPT.to_string(),
            _ => PROMPT.to_string(),
        };
        let opts = SpeechOptions::new(3, true, &prompt);
        // detectors on watched channels are often far and weak: bring the voice out first
        let filter = watch_detector && watch_profile.as_ref().is_none_or(|p| p.audio.filter == "voice");
        // the original is kept for candidate recordings: templates and tests filter it themselves
        let original = audio.clone();
        let audio = if filter { normalize(&det::clean_voice(&audio)) } else { audio };
        let (segments, language) = self.with_model(|m| m.transcribe(&audio, &opts))?;
        let (mut kept, mut skipped) = (vec![], vec![]);
        for seg in &segments {
            let t = seg.text.trim();
            if t.is_empty() || seg.no_speech_prob > MAX_NO_SPEECH {
                skipped.push(json!({"text": t, "no_speech_prob": round(seg.no_speech_prob, 2)}));
                continue;
            }
            kept.push(seg);
        }
        if kept.is_empty() {
            self.reject("no speech found", vec![("skipped", Value::Array(skipped))]);
            return Ok(None);
        }
        let conf = weighted_confidence(&kept);
        let text = kept.iter().map(|s| s.text.trim()).collect::<Vec<_>>().join(" ").trim().to_string();
        let text = match (&watch_profile, watch_detector) {
            (Some(p), true) => p.correct(&text),
            _ => text,
        };
        if conf < if watch_detector { WATCH_MIN_CONFIDENCE } else { MIN_CONFIDENCE } {
            self.reject("low confidence", vec![("text", json!(text)), ("confidence", json!(round(conf, 2)))]);
            return Ok(None);
        }
        if watch_detector && repeats_itself(&text) {
            // speech recognition inventing text from noise loops on the prompt's wording
            self.reject("repeating transcript", vec![("text", json!(text))]);
            return Ok(None);
        }
        if FILLERS.contains(&text.to_lowercase().as_str()) {
            self.reject("filler phrase", vec![("text", json!(text))]);
            return Ok(None);
        }
        let mut result = Map::new();
        result.insert("text".into(), json!(text));
        result.insert("engine".into(), json!(ENGINE));
        result.insert("confidence".into(), json!(round(conf, 2)));
        result.insert("language".into(), json!(language.filter(|l| !l.is_empty()).unwrap_or_else(|| "en".into())));
        result.insert("duration_s".into(), json!(round(duration, 1)));
        let mut parsed = text::parse(&text, false);
        if watch_detector && parsed.is_none() {
            // the milepost in words alone gives the parser no field, but it is still an announcement
            let lower = text.to_lowercase();
            // or its words match enough of a nearby detector's expected wording
            let wording = freq.map(|f| self.nearby_on(f)).unwrap_or_default().into_iter()
                .filter_map(|d| d.pointer("/transcript/spoken").and_then(Value::as_str)
                    .map(|sp| (text::spoken_similarity(&text, sp), d.get("railroad").cloned())))
                .max_by(|a, b| a.0.total_cmp(&b.0)).filter(|(sim, _)| *sim >= WORDING_MIN);
            if lower.contains("detector") || lower.contains("milepost") || lower.contains("mile post") {
                parsed = Some(json!({"type": "defect_detector"}));
            } else if let Some((_, rr)) = wording {
                let mut p = json!({"type": "defect_detector"});
                if let Some(rr) = rr.filter(|r| r.is_string()) {
                    p["railroad"] = rr;
                }
                // the parser reads counts only from text it took for an announcement
                if let Some(n) = text::spoken_count_after(&text, &["total axles", "total axle", "axle count"]) {
                    p["axles"] = json!(n);
                }
                if let Some(n) = text::spoken_count_after(&text, &["train speed", "speed"]) {
                    p["speed_mph"] = json!(n);
                }
                parsed = Some(p);
            }
        }
        if watch_detector {
            let Some(Value::Object(parsed)) = parsed.filter(|p| p["type"] == "defect_detector") else {
                self.reject("watched channel call, not a detector announcement", vec![("text", json!(text))]);
                return Ok(None);
            };
            let parsed = self.watched_announcement(&original, &text, parsed, event, watch_profile.as_ref());
            result.insert("parsed".into(), Value::Object(parsed));
            if let Some(n) = self.profile_note(watch_profile.as_ref()) {
                result.insert("profile".into(), n);
            }
        } else if let Some(parsed) = parsed {
            // speech recognition only
            result.insert("parsed".into(), parsed);
        }
        *lock(&self.last_result) = Some(Value::Object(result.clone()));
        Ok(Some(result))
    }

    /// A detector announcement heard by speech on a watched channel, from a detector that has no
    /// template yet. The milepost is matched to the headend's list (a misheard number is suggested
    /// as the railroad's likely detector), and the recording is kept as a candidate for that
    /// detector's template. It is reported as a transcript only: without a sound match, speech
    /// alone can't be trusted for a v1.5 detector result.
    fn watched_announcement(&self, audio: &[f32], text: &str, mut parsed: Map<String, Value>, event: Option<&Value>,
                            prof: Option<&Profile>)
        -> Map<String, Value> {
        // the railroad as heard, else the channel's own (speech often loses a short "CSX")
        let railroad = parsed.get("railroad").and_then(Value::as_str).filter(|r| !r.is_empty())
            .or_else(|| event.and_then(|e| e.get("railroad")).and_then(Value::as_str))
            .unwrap_or("").to_uppercase();
        if !railroad.is_empty() {
            parsed.insert("railroad".into(), json!(railroad));
        }
        let heard = text::spoken_milepost(text).or_else(|| text::spoken_decimal(text));
        // the channel's nearby detectors first (v1.6), then the headend's wider list (v1.5)
        let freq = event.and_then(|e| e.get("freq")).and_then(Value::as_f64);
        let near = freq.map(|f| self.nearby_on(f)).unwrap_or_default();
        let listed = det::suggest_detector(Some(&near), &railroad, heard).map(|(d, m)| (d.clone(), m))
            .or_else(|| {
                let kd = lock(&self.known_detectors);
                det::suggest_detector(kd.as_deref(), &railroad, heard).map(|(d, m)| (d.clone(), m))
            });
        // the profile reads the values and decides the all-clear; without one, "no defects" said
        // twice is the detector's own repeat (a single one may be a mishearing)
        match prof {
            Some(p) => {
                let r = p.read(text);
                for (name, key) in [("axles", "axles"), ("speed", "speed_mph"), ("track", "track"), ("temperature", "temperature_f")] {
                    if let Some(v) = r.fields.get(name) {
                        parsed.insert(key.into(), v.clone());
                    }
                }
                if r.all_clear(p) {
                    parsed.insert("defects".into(), json!(false));
                } else if r.defect() {
                    parsed.insert("defects".into(), json!(true));
                }
            }
            None if text.to_lowercase().matches("no defect").count() >= 2 => {
                parsed.insert("defects".into(), json!(false));
            }
            None => {}
        }
        let heard_s = heard.map_or("not heard".to_string(), |m| format!("{} heard", fmt_g(m)));
        let Some((d, how)) = listed else {
            if let Some(m) = heard {
                parsed.insert("milepost".into(), json!(m));
            }
            log::info!("detector announcement on a watched channel ({} milepost {}), not in the headend's list: \"{}\"",
                       if railroad.is_empty() { "unknown railroad" } else { &railroad }, heard_s, text);
            return parsed;
        };
        let mp = d.get("milepost").and_then(det::value_f64).unwrap_or(0.0);
        parsed.insert("milepost".into(), json!(round(mp, 1)));
        let id = d.get("detector_id").and_then(Value::as_str).map(String::from)
            .unwrap_or_else(|| format!("{}-{}", railroad.to_lowercase(), fmt_g(round(mp, 1))));
        let how_s = match how { det::DetectorMatch::Heard => "heard", det::DetectorMatch::Suggested => "suggested" };
        log::info!("detector announcement on a watched channel: {} ({}), milepost {}, match {}{}{}: \"{}\"",
                   d.get("name").and_then(Value::as_str).unwrap_or(&id), id, heard_s, how_s,
                   parsed.get("axles").map_or(String::new(), |a| format!(", {a} axles")),
                   parsed.get("speed_mph").map_or(String::new(), |v| format!(", {v} mph")), text);
        let meta = json!({"detector_id": id, "detector": d, "match": how_s, "milepost_heard": heard, "text": text,
                          "parsed": parsed, "event": event});
        if let Err(e) = self.save_candidate(&id, audio, &meta) {
            log::warn!("could not keep the candidate recording: {e}");
        }
        parsed
    }

    /// Keep a detector announcement in state/detector_candidates/<id>/ for building its template.
    fn save_candidate(&self, id: &str, audio: &[f32], meta: &Value) -> std::io::Result<()> {
        let Some(clips) = &self.clips_dir else { return Ok(()) };
        let dir = clips.parent().unwrap_or(Path::new(".")).join("detector_candidates").join(id);
        std::fs::create_dir_all(&dir)?;
        let now = railspotting_common::now();
        let stamp = format!("{}.{:03}", local_stamp(now, "%Y%m%d-%H%M%S"), ((now * 1000.0) as i64).rem_euclid(1000));
        wav::write_pcm16(&dir.join(format!("{stamp}_voice.wav")), audio, self.rate as u32)?;
        std::fs::write(dir.join(format!("{stamp}_voice.json")), json_dumps(meta) + "\n")?;
        prune(&dir, CANDIDATE_KEEP);
        Ok(())
    }

    /// A message from a known kind of detector (API contract v1.5).
    ///
    /// The transcript follows section 3; the `detector` object (section 1) goes in
    /// result["_detector"] for the SDR to put on the event. The milepost always comes from speech
    /// recognition of the number, judged against the template's: within SAME_DETECTOR_MI (475.9
    /// for 475.8) it is the template's detector and its milepost; further off (462.2) it is a
    /// different detector, looked up in the headend's list (/config). A number heard but not
    /// listed gets a transcript but no `detector` object, so a misheard number can't add a
    /// detector on the headend. Only when no number could be read does the sound match alone
    /// decide, and only if it heard the template's own number.
    fn detector_result(&self, audio: &[f32], duration: f64, tmpl: &Template, a: &Assessment, event: Option<&Value>)
        -> Result<Map<String, Value>, String> {
        let info = &tmpl.info;
        let tparsed = info.get("parsed").filter(|p| truthy(p)).cloned().unwrap_or_else(|| json!({}));
        let railroad = tparsed.get("railroad").filter(|v| truthy(v)).map(crate::util::value_str).unwrap_or_default();
        let ev_f = |k: &str| event.and_then(|e| e.get(k)).filter(|v| !v.is_null());
        let length_s = match (ev_f("start_ts"), ev_f("end_ts")) {
            (Some(s), Some(e)) => e.as_f64().unwrap_or(0.0) - s.as_f64().unwrap_or(0.0),
            _ => (duration - CLIP_LEAD_S).max(0.0),
        };
        let mut all_clear_s = det::all_clear_length(info).filter(|v| *v != 0.0).unwrap_or(length_s);

        let sound_known = matches!(a.milepost.as_deref(), None | Some("known"));
        let template_mp = tparsed.get("milepost").and_then(Value::as_f64);
        // the whole announcement, read with the detector's voice profile: its filter, its wording
        // as the prompt (values left out), then the milepost and train details from the words
        let freq = event.and_then(|e| e.get("freq")).and_then(Value::as_f64);
        let prof = self.profile_for(freq, &railroad);
        let words: Option<(String, Reading)> = match &prof {
            Some(p) => match self.transcribe_announcement(audio, p) {
                Ok(Some((t, _))) if repeats_itself(&t) => {
                    log::warn!("detector {}: transcript ignored, it repeats itself: \"{}\"", tmpl.name(), t);
                    None
                }
                Ok(Some((t, _))) => {
                    let r = p.read(&t);
                    log::info!("detector {} heard with profile {}: \"{}\" (wording {:.2})", tmpl.name(), p.id, t, r.wording);
                    Some((t, r))
                }
                Ok(None) => None,
                Err(e) => {
                    log::error!("transcribing detector {} failed: {e}", tmpl.name());
                    None
                }
            },
            None => None,
        };
        let heard_mp = match &words {
            Some((_, r)) => r.fields.get("milepost").and_then(Value::as_f64),
            // no speech model: the old number reading (which needs one too) is all there is
            None => self.recognize_number(audio, a, info),
        };
        let known = match (heard_mp, template_mp) {
            (Some(h), Some(t)) => {
                // detectors are at most about 10 miles apart: a number this near is a mishearing
                // of the template's; anything further is another detector
                let same = (h - t).abs() <= SAME_TEMPLATE_MI;
                if same {
                    log::info!("detector {}: milepost {} heard, the template's {}", tmpl.name(), fmt_g(h), fmt_g(t));
                } else {
                    log::warn!("detector {}: milepost {} heard, not the template's {}: a different detector",
                               tmpl.name(), fmt_g(h), fmt_g(t));
                }
                same
            }
            (None, Some(t)) => {
                if sound_known {
                    log::info!("detector {}: milepost number not read; the sound match heard the template's {}",
                               tmpl.name(), fmt_g(t));
                }
                sound_known
            }
            _ => sound_known,
        };
        let (mut milepost, mut configured): (Option<Value>, bool) = if known {
            (tparsed.get("milepost").filter(|v| !v.is_null()).cloned(), true)
        } else {
            (None, false)
        };
        let mut detector_id = tmpl.name().to_string();
        if !known {
            milepost = heard_mp.map(|v| json!(v));
            let listed = {
                let kd = lock(&self.known_detectors);
                det::headend_detector(kd.as_deref(), &railroad, milepost.as_ref().and_then(Value::as_f64)).cloned()
            };
            if let Some(listed) = listed {
                let m = round(listed.get("milepost").and_then(Value::as_f64).unwrap_or(0.0), 1);
                milepost = Some(json!(m));
                configured = true;
                detector_id = format!("{}-{}", railroad.to_lowercase(), fmt_g(m));
                // its all-clear is longer or shorter than the template's by its number's length
                let o_end = DetectorTemplates::opening_end_frames(info);
                let split = DetectorTemplates::split_frames(info).unwrap_or(0);
                if let (Some(ef), Some(of), Some(oe)) = (a.ending_frame, a.opening_frame, o_end.filter(|v| *v != 0)) {
                    let said = ef - (of + oe);
                    all_clear_s += (said - (split - oe)) as f64 * HOP as f64 / 16_000.0;
                }
            }
        }
        let milepost_f = milepost.as_ref().and_then(Value::as_f64);
        let (mut result_kind, mut method) = det::contract_result(a, length_s, all_clear_s);
        let words_all_clear = matches!((&words, &prof), (Some((_, r)), Some(p)) if r.all_clear(p));
        let words_defect = words.as_ref().is_some_and(|(_, r)| r.defect());
        if result_kind != "no_defects" && words_all_clear {
            if self.words_method.load(Ordering::SeqCst) {
                (result_kind, method) = ("no_defects", "words");
            } else {
                // v1.5 has no method for an all-clear proven by its words: a transcript only
                configured = false;
            }
        }
        if a.opening_frame.is_none() && result_kind != "no_defects" {
            configured = false; // the ending alone can only confirm an all-clear
        }

        let s = |k: &str| info.get(k).filter(|v| truthy(v)).and_then(Value::as_str);
        let wording = s("wording_text").or(s("opening_text")).unwrap_or("").to_string();
        let where_ = if known {
            crate::util::value_str_opt(info.get("opening_text"))
        } else if let Some(m) = milepost_f {
            format!("{wording} {}", fmt_g(m))
        } else {
            format!("{wording} [number not recognized]")
        };
        let mut parsed = Map::new();
        parsed.insert("type".into(), json!("defect_detector"));
        parsed.insert("railroad".into(), json!(railroad));
        if let Some(m) = &milepost {
            parsed.insert("milepost".into(), m.clone());
        }
        let mut result = Map::new();
        result.insert("engine".into(), json!(det::ENGINE));
        result.insert("confidence".into(), json!(round(a.score.clamp(0.0, 1.0), 2)));
        result.insert("language".into(), json!("en"));
        result.insert("duration_s".into(), json!(round(duration, 1)));
        let mut heard: Option<String> = None;
        if let Some((_, r)) = &words {
            for (name, key) in [("axles", "axles"), ("speed", "speed_mph"), ("track", "track"), ("temperature", "temperature_f")] {
                if let Some(v) = r.fields.get(name) {
                    parsed.insert(key.into(), v.clone());
                }
            }
        }
        if result_kind == "no_defects" {
            let text = if known {
                crate::util::value_str_opt(info.get("text"))
            } else {
                format!("{where_}, {}", s("ending_text").unwrap_or("no defects"))
            };
            result.insert("text".into(), json!(text));
            parsed.insert("defects".into(), json!(false)); // the ending is proven by sound
            if words_defect {
                log::warn!("detector {}: its words mention a defect but the sound matched the all-clear; reported as the all-clear",
                           tmpl.name());
            }
            let how = if method == "sound" { String::new() } else { format!(", by {}", method.replace('_', " ")) };
            log::info!("known detector announcement {}: all clear at {} (match {:.2}{})", tmpl.name(), where_, a.score, how);
        } else {
            // Not the all-clear. The rest of the message isn't transcribed: speech recognition
            // heard this synthetic "no" as "so" in most recordings, so its words can't be trusted
            // and would cost minutes of CPU. A message longer than the all-clear is a defect
            // report (it adds the axle and rail). One as long as the all-clear whose ending didn't
            // match is usually the all-clear through noise (10-12 11:51: ending 0.32, heard as "No
            // defects. Repeated. No defects."), so it stays unconfirmed; a cut-off one too.
            let lead = if known { where_.clone() } else { wording.clone() };
            let heard_text = words.as_ref().map(|(t, _)| t.clone());
            let text = if words_defect {
                // a defect report, by its words
                parsed.insert("defects".into(), json!(true));
                heard = heard_text.clone().map(|t| t.chars().take(300).collect());
                heard_text.clone().unwrap_or_else(|| format!("{lead}, defect"))
            } else if words_all_clear {
                parsed.insert("defects".into(), json!(false));
                log::info!("detector {}: all clear by its words; the sound didn't confirm it ({}){}", tmpl.name(), result_kind,
                           if configured { "" } else { ", sent as a transcript only" });
                heard_text.clone().unwrap_or_default()
            } else if let Some(t) = heard_text.clone() {
                t
            // without speech: the all-clear's own ending heard, only running long, is never a defect
            } else if result_kind == "different_message" && length_s > all_clear_s + det::NOT_LONGER_S
                && !a.is("all_clear") {
                parsed.insert("defects".into(), json!(true));
                format!("{lead}, defect")
            } else if result_kind == "different_message" {
                format!("{lead}, [all-clear not confirmed]")
            } else {
                format!("{lead}, [the message was cut off]")
            };
            log::warn!("detector {} at {} sent something other than its all-clear ({}, {:.1} s against {:.1} s) \
                        (opening {}, ending {})", tmpl.name(), where_, result_kind, length_s, all_clear_s,
                       list_text_opt(a.opening.as_deref()), list_text_opt(a.ending.as_deref()));
            result.insert("text".into(), json!(text));
        }
        result.insert("parsed".into(), Value::Object(parsed));
        if let Some(n) = self.profile_note(prof.as_ref()) {
            result.insert("profile".into(), n);
        }

        // repeats pair up per detector and milepost
        let key = format!("{}@{}", tmpl.name(), crate::util::value_str_opt(milepost.as_ref()));
        self.link_repeat(&key, &mut result, event);
        let Some(m) = milepost_f.filter(|_| configured && !railroad.is_empty()) else {
            if let (Some(m), false) = (milepost_f, known) {
                log::warn!("detector {} heard at milepost {}, which the headend doesn't list; sent as a transcript only",
                           tmpl.name(), fmt_g(m));
            }
            return Ok(result);
        };
        let repeat_of = result.get("parsed").and_then(|p| p.get("repeat_of")).and_then(Value::as_str).map(String::from);
        let timestamp = ev_f("timestamp").and_then(Value::as_f64);
        let obj = det::detector_object(&detector_id, &railroad, m, result_kind, method, a, length_s, all_clear_s,
                                       repeat_of.as_deref(), heard.as_deref(), timestamp);
        let problems = det::validate_detector(&obj);
        if problems.is_empty() {
            result.insert("_detector".into(), obj);
        } else {
            log::error!("detector report for {} not sent, invalid {}: {}", tmpl.name(), problems.join(", "), json_dumps(&obj));
        }
        Ok(result)
    }

    /// The milepost number said between the wording and the ending (unverified). The assessment
    /// gives its frames when the sound didn't match the template's number; when it did, they are
    /// from the end of the template's opening to the start of the ending.
    fn recognize_number(&self, audio: &[f32], a: &Assessment, info: &Map<String, Value>) -> Option<f64> {
        let (lo, hi) = a.number_frames.unwrap_or_else(|| {
            let o_end = DetectorTemplates::opening_end_frames(info).filter(|v| *v != 0);
            (a.opening_frame.zip(o_end).map(|(of, oe)| of + oe), a.ending_frame)
        });
        let span = (NUMBER_MAX_S * 50.0) as i64;
        let (lo, hi) = match (lo, hi) {
            (None, None) => return None,
            (None, Some(h)) => ((h - span).max(0), h),
            (Some(l), None) => (l, l + span),
            (Some(l), Some(h)) => (l, h),
        };
        let at = |f: i64| (f.max(0) as usize) * HOP;
        self.read_number(slice(audio, at(lo), at(hi)))
    }

    /// Speech recognition of a milepost number alone: cut exactly between the wording and the
    /// ending (a margin let the start of "no defects" add a digit), a neutral prompt (listing
    /// nearby mileposts biased it, once into "4568.9"), and digit-by-digit speech joined.
    /// Matching the result against the headend's list is the caller's job.
    pub fn read_number(&self, audio: &[f32]) -> Option<f64> {
        if (audio.len() as f64) < 0.3 * self.rate as f64 {
            return None;
        }
        if !self.speech_available() {
            log::debug!("milepost number not read: {UNAVAILABLE}");
            return None;
        }
        let x = normalize(audio);
        let mut opts = SpeechOptions::new(5, false, NUMBER_PROMPT);
        opts.temperature = Some(0.0);
        opts.max_new_tokens = Some(16);
        opts.without_timestamps = true;
        let text = match self.with_model(|m| m.transcribe(&x, &opts)) {
            Ok((segs, _)) => segs.iter().filter(|s| !s.text.trim().is_empty() && s.no_speech_prob <= MAX_NO_SPEECH)
                .map(|s| s.text.trim()).collect::<Vec<_>>().join(" "),
            Err(e) => {
                log::error!("reading a milepost number failed: {e}");
                return None;
            }
        };
        let value = text::milepost_from_text(&text);
        let short: String = text.chars().take(40).collect();
        log::info!("milepost number heard: {} -> {} ({:.1} s of audio)", repr_text(&short),
                   value.map_or("None".into(), railspotting_proto::jsonfmt::float_repr),
                   audio.len() as f64 / self.rate as f64);
        value
    }

    /// Mark a detector's second transmission of the same train. On the reference detector every all-clear
    /// is sent twice, 19 to 23 s apart (start to start). Only a link for the headend to count
    /// one train: each transmission is judged on its own, because tests showed neither
    /// comparing the two nor averaging them can tell a repeat from a different message.
    fn link_repeat(&self, name: &str, result: &mut Map<String, Value>, event: Option<&Value>) {
        let Some(event) = event else { return };
        let Some(start) = event.get("start_ts").filter(|v| !v.is_null()) else { return };
        let start = start.as_f64().unwrap_or(0.0);
        let mut calls = lock(&self.last_detector_call);
        if let Some(prev) = calls.get(name).cloned() {
            let gap = start - prev.start_ts;
            if (REPEAT_MIN_S..=REPEAT_MAX_S).contains(&gap) && !prev.repeat {
                let parsed = result.entry("parsed").or_insert_with(|| json!({}));
                if let Some(p) = parsed.as_object_mut() {
                    p.insert("repeat".into(), json!(true));
                    p.insert("repeat_of".into(), prev.event_id.clone());
                }
                log::info!("detector {}: repeat of event {} ({:.0} s earlier)", name, crate::util::value_str(&prev.event_id), gap);
                calls.insert(name.into(), LastCall { repeat: true, ..prev });
                return;
            }
        }
        calls.insert(name.into(), LastCall {
            start_ts: start, event_id: event.get("event_id").cloned().unwrap_or(Value::Null), repeat: false,
        });
    }

    // clips --------------------------------------------------------------------

    fn save_clip(&self, audio: &[f32], result: Option<&Map<String, Value>>, tag: &str, event: Option<&Value>) {
        let Some(dir) = &self.clips_dir else { return };
        if let Err(e) = self.try_save_clip(dir, audio, result, tag, event) {
            log::warn!("could not save the voice clip: {e}");
        }
    }

    fn try_save_clip(&self, dir: &Path, audio: &[f32], result: Option<&Map<String, Value>>, tag: &str,
                     event: Option<&Value>) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let now = railspotting_common::now();
        let stamp = format!("{}.{:03}", local_stamp(now, "%Y%m%d-%H%M%S"), ((now * 1000.0) as i64).rem_euclid(1000));
        let pcm: Vec<f32> = normalize(&audio.iter().map(|x| x.clamp(-1.0, 1.0)).collect::<Vec<_>>());
        let wav_path = dir.join(format!("{stamp}_{tag}.wav"));
        let json_path = dir.join(format!("{stamp}_{tag}.json"));
        wav::write_pcm16(&wav_path, &pcm, self.rate as u32)?;
        let rejected = if result.is_some() { Value::Null } else { self.last_reject().unwrap_or(Value::Null) };
        let mut meta = json!({"transcript": result.cloned().map(Value::Object).unwrap_or(Value::Null), "rejected": rejected});
        let freq = event.and_then(|e| e.get("freq")).and_then(Value::as_f64);
        if let Some(f) = freq {
            meta["freq"] = json!(f);
        }
        std::fs::write(&json_path, railspotting_store::jsonfmt::dumps_indent(&meta, 2) + "\n")?;
        prune(dir, self.clip_keep);
        if result.and_then(|r| r.get("engine")).and_then(Value::as_str) == Some(det::ENGINE) {
            // detector calls are kept longer, for replays and new templates
            let keep = dir.parent().unwrap_or(Path::new(".")).join("detector_clips");
            std::fs::create_dir_all(&keep)?;
            for p in [&wav_path, &json_path] {
                if let Some(name) = p.file_name() {
                    std::fs::copy(p, keep.join(name))?;
                }
            }
            prune(&keep, DETECTOR_CLIP_KEEP);
        }
        if let Some(f) = freq.filter(|f| lock(&self.watch_freqs).iter().any(|w| (w - f).abs() < 0.00125)) {
            let keep = dir.parent().unwrap_or(Path::new(".")).join("watch_clips").join(format!("{f:.4}"));
            std::fs::create_dir_all(&keep)?;
            for p in [&wav_path, &json_path] {
                if let Some(name) = p.file_name() {
                    std::fs::copy(p, keep.join(name))?;
                }
            }
            prune(&keep, WATCH_CLIP_KEEP);
        }
        Ok(())
    }
}

/// Keep the newest `keep` clips (by name) in a folder.
pub fn prune(folder: &Path, keep: usize) {
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(folder).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav")).collect();
    wavs.sort();
    let n = wavs.len().saturating_sub(keep);
    for old in &wavs[..n] {
        let _ = std::fs::remove_file(old);
        let name = old.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let _ = std::fs::remove_file(old.with_file_name(format!("{}.json", &name[..name.len().saturating_sub(4)])));
    }
}

impl railspotting_dsp::sdr::Transcriber for Transcriber {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>) {
        Transcriber::submit(self, audio, done, tag, event)
    }
    /// For the heartbeat's `transcription` feature: only when calls are really transcribed.
    fn other_calls(&self) -> bool {
        Transcriber::other_calls(self) && self.speech_available()
    }
}
