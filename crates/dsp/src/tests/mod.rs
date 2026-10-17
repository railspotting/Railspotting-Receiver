//! Tests of the channelizer, bursts, EOT, corrupt input, voice and the simulator, the reference
//! fixture parity checks and the benchmark. Shared helpers live here.

mod bench;
mod parity;
mod test_bursts;
mod test_corrupt;
mod test_eot;
mod test_sdr;
mod test_sim;
mod test_voice;

use crate::sdr::{Emit, SdrSource, Transcriber};
use crate::sim::World;
use railspotting_common::{builtin_config, FakeClock};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
pub use num_complex::Complex32;

pub fn vhf() -> Value {
    json!({"serial": "30002222", "role": "vhf", "sample_rate": 2_880_000.0, "gain": 20.7, "name": "VHF SDR"})
}

pub fn eot_dev() -> Value {
    json!({"serial": "30001111", "role": "eot", "sample_rate": 2_400_000.0, "gain": 40.2, "name": "UHF SDR"})
}

pub fn plan(version: &str) -> Value {
    let mut cfg = builtin_config();
    cfg["config_version"] = json!(version);
    cfg
}

pub fn channels(cfg: &Value) -> Vec<Value> {
    cfg["channels"].as_array().cloned().unwrap_or_default()
}

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// A scratch directory removed when dropped.
pub struct TmpDir(pub PathBuf);

impl TmpDir {
    pub fn new(name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!("rs-dsp-{}-{}-{}", name, std::process::id(),
                                                  N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("tmp dir");
        TmpDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub type Events = Arc<Mutex<Vec<Map<String, Value>>>>;

pub fn events_sink() -> (Events, Emit) {
    let ev: Events = Arc::new(Mutex::new(vec![]));
    let e2 = ev.clone();
    (ev, Arc::new(move |m| e2.lock().unwrap().push(m)))
}

pub fn source_with(tmp: &Path, dev: &Value, transcriber: Option<Arc<dyn Transcriber>>, sdr: Value)
                   -> (SdrSource, Events) {
    log_lines(); // capture from the start
    let cfg = plan("7");
    let (events, emit) = events_sink();
    let mut settings = json!({"record_keep": 500});
    for (k, v) in sdr.as_object().cloned().unwrap_or_default() {
        settings[k] = v;
    }
    let mut s = SdrSource::new(dev, &settings, tmp, emit, transcriber, FakeClock::new(0.0).clock());
    s.set_plan(&channels(&cfg), Some(&cfg["reference"]));
    (s, events)
}

pub fn source(tmp: &Path, dev: &Value) -> (SdrSource, Events) {
    source_with(tmp, dev, None, json!({}))
}

/// Feed blocks of world IQ from t_start to t_end (simulated seconds).
pub fn run(src: &mut SdrSource, world: &World, t_start: f64, t_end: f64) -> f64 {
    let mut t = t_start;
    let dur = src.block as f64 / src.rate;
    while t < t_end {
        let iq = world.iq(src.center.unwrap(), src.rate, t, src.block);
        src.process_block(&iq, t + dur);
        t += dur;
    }
    t
}

pub fn all(events: &Events) -> Vec<Map<String, Value>> {
    events.lock().unwrap().clone()
}

pub fn closed(events: &Events) -> Vec<Map<String, Value>> {
    all(events).into_iter().filter(|e| e["state"] == "closed").collect()
}

pub fn states(events: &Events) -> Vec<String> {
    all(events).iter().map(|e| e["state"].as_str().unwrap().to_string()).collect()
}

/// Log lines captured for the tests.
pub fn log_lines() -> &'static Mutex<Vec<String>> {
    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    struct Cap;
    impl log::Log for Cap {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            log_lines().lock().unwrap().push(format!("{} {}", r.level(), r.args()));
        }
        fn flush(&self) {}
    }
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = log::set_boxed_logger(Box::new(Cap)).map(|()| log::set_max_level(log::LevelFilter::Info));
    });
    LINES.get_or_init(|| Mutex::new(vec![]))
}

pub fn logged(text: &str) -> bool {
    log_lines().lock().unwrap().iter().any(|l| l.contains(text))
}

/// Peak frequency of a real signal with a Hann window (real FFT, bin to Hz).
pub fn peak_hz_real(x: &[f64], fs: f64) -> f64 {
    use rustfft::num_complex::Complex64;
    let n = x.len();
    let w = crate::bursts::hanning(n);
    let mut buf: Vec<Complex64> = x.iter().zip(&w).map(|(a, b)| Complex64::new(a * b, 0.0)).collect();
    rustfft::FftPlanner::new().plan_fft_forward(n).process(&mut buf);
    let k = (0..=n / 2).max_by(|&a, &b| buf[a].norm().total_cmp(&buf[b].norm())).unwrap();
    k as f64 * fs / n as f64
}
