//! Fakes for running without hardware: synthetic RF and fake RTL-SDR dongles (`--simulate`).
//!
//! The tests use the same fakes. The random numbers come from a small local generator, so the
//! noise is not the reference recordings' sample for sample; only its statistics match.

use num_complex::{Complex32, Complex64};
use railspotting_common::{nominal_noise, Clock};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::f64::consts::PI;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::eot;
use crate::sdr::DeviceControl;

pub const NOISE_SIGMA: f64 = 0.05;

/// xoshiro256** with Marsaglia's polar method for normals.
#[derive(Debug, Clone)]
pub struct Rng {
    s: [u64; 4],
    spare: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut z = seed;
        let mut next = || {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^ (x >> 31)
        };
        Rng { s: [next(), next(), next(), next()], spare: None }
    }

    pub fn next_u64(&mut self) -> u64 {
        let r = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }

    /// Uniform in [0, 1).
    pub fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.random()
    }

    /// Integer in [lo, hi).
    pub fn integers(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.random() * (hi - lo) as f64) as i64
    }

    pub fn standard_normal(&mut self) -> f64 {
        if let Some(s) = self.spare.take() {
            return s;
        }
        loop {
            let u = 2.0 * self.random() - 1.0;
            let v = 2.0 * self.random() - 1.0;
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let m = (-2.0 * s.ln() / s).sqrt();
                self.spare = Some(v * m);
                return u * m;
            }
        }
    }

    pub fn normal(&mut self, mean: f64, sigma: f64) -> f64 {
        mean + sigma * self.standard_normal()
    }

    pub fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.integers(0, items.len() as i64) as usize]
    }
}

/// (t_rel, rate) -> complex baseband.
pub type IqFn = Arc<dyn Fn(&[f64], f64) -> Vec<Complex64> + Send + Sync>;

#[derive(Clone)]
pub struct Emitter {
    pub freq: f64,
    pub snr_db: f64,
    pub start: f64,
    pub end: Option<f64>,
    pub kind: String,
    pub iq_fn: Option<IqFn>,
}

impl Emitter {
    pub fn new(freq: f64, snr_db: f64, start: f64, end: Option<f64>, kind: &str) -> Self {
        Emitter { freq, snr_db, start, end, kind: kind.into(), iq_fn: None }
    }

    pub fn active(&self, t0: f64, t1: f64) -> bool {
        self.start < t1 && self.end.is_none_or(|e| e > t0)
    }
}

type OscKey = (i64, u64, usize);

/// Everything on the air. Times are in the caller's clock.
pub struct World {
    emitters: Mutex<Vec<Arc<Emitter>>>,
    pub rng: Mutex<Rng>,
    osc_cache: Mutex<HashMap<OscKey, Arc<Vec<Complex32>>>>,
}

impl Default for World {
    fn default() -> Self {
        World::new(1)
    }
}

impl World {
    pub fn new(seed: u64) -> Self {
        World { emitters: Mutex::new(vec![]), rng: Mutex::new(Rng::new(seed)), osc_cache: Mutex::new(HashMap::new()) }
    }

    pub fn add(&self, em: Emitter) -> Arc<Emitter> {
        let em = Arc::new(em);
        if let Ok(mut g) = self.emitters.lock() {
            g.push(em.clone());
        }
        em
    }

    pub fn prune(&self, now: f64) {
        if let Ok(mut g) = self.emitters.lock() {
            g.retain(|e| e.end.is_none_or(|end| end > now - 5.0));
        }
    }

    pub fn emitters(&self) -> Vec<Arc<Emitter>> {
        self.emitters.lock().map(|g| g.clone()).unwrap_or_default()
    }

    // convenience emitters -------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn carrier(&self, freq: f64, snr_db: f64, start: f64, end: Option<f64>, tone_hz: f64, deviation: f64,
                   kind: &str) -> Arc<Emitter> {
        let f: IqFn = Arc::new(move |t: &[f64], _rate: f64| {
            if tone_hz <= 0.0 {
                return vec![Complex64::new(1.0, 0.0); t.len()];
            }
            t.iter().map(|&t| {
                let phase = deviation / tone_hz * (2.0 * PI * tone_hz * t).sin();
                Complex64::new(phase.cos(), phase.sin())
            }).collect()
        });
        self.add(Emitter { iq_fn: Some(f), ..Emitter::new(freq, snr_db, start, end, kind) })
    }

    /// A voice carrier with the usual test settings (1 kHz tone, 2.5 kHz deviation).
    pub fn tone(&self, freq: f64, snr_db: f64, start: f64, end: Option<f64>, tone_hz: f64) -> Arc<Emitter> {
        self.carrier(freq, snr_db, start, end, tone_hz, 2500.0, "voice")
    }

    /// FM carrier with a few wandering tones, a stand-in for a voice.
    pub fn speechlike(&self, freq: f64, snr_db: f64, start: f64, end: Option<f64>, deviation: f64,
                      kind: &str) -> Arc<Emitter> {
        let f: IqFn = Arc::new(move |t: &[f64], rate: f64| {
            // integrate audio for FM: approximate by cumulative sum on this block's grid
            let mut acc = 0.0f64;
            t.iter().map(|&t| {
                let audio = (2.0 * PI * 440.0 * t + 3.0 * (2.0 * PI * 3.0 * t).sin()).sin()
                    + 0.6 * (2.0 * PI * 1210.0 * t + 2.0 * (2.0 * PI * 2.1 * t).sin()).sin()
                    + 0.3 * (2.0 * PI * 2300.0 * t).sin();
                let env = 0.6 + 0.4 * (2.0 * PI * 4.0 * t).sin().powi(2);
                acc += audio * env;
                let phase = 2.0 * PI * deviation * acc / rate / 2.0;
                Complex64::new(phase.cos(), phase.sin())
            }).collect()
        });
        self.add(Emitter { iq_fn: Some(f), ..Emitter::new(freq, snr_db, start, end, kind) })
    }

    /// One burst carrying one or more EOT packets back to back.
    pub fn eot_burst(&self, freq: f64, snr_db: f64, start: f64, payloads: &[Vec<u8>]) -> Arc<Emitter> {
        self.eot_burst_with(freq, snr_db, start, payloads, 3500.0, -300.0)
    }

    pub fn eot_burst_with(&self, freq: f64, snr_db: f64, start: f64, payloads: &[Vec<u8>], deviation: f64,
                          tune_err_hz: f64) -> Arc<Emitter> {
        let mut bits: Vec<u8> = [0u8, 1].repeat(8);
        for p in payloads {
            bits.extend(eot::packet_bits(p));
            bits.extend([1u8, 0].repeat(4));
        }
        let duration = bits.len() as f64 / eot::BAUD;
        let cache: Mutex<HashMap<u64, Arc<Vec<Complex64>>>> = Mutex::new(HashMap::new());
        let f: IqFn = Arc::new(move |t: &[f64], rate: f64| {
            let sig = {
                let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                c.entry(rate.to_bits()).or_insert_with(|| {
                    let audio = eot::ffsk_audio(&bits, rate);
                    let mut acc = 0.0;
                    Arc::new(audio.iter().enumerate().map(|(k, a)| {
                        acc += a;
                        let phase = 2.0 * PI * deviation * acc / rate;
                        let p = phase + 2.0 * PI * tune_err_hz * k as f64 / rate;
                        Complex64::new(p.cos(), p.sin())
                    }).collect())
                }).clone()
            };
            let last = sig.len() as i64 - 1;
            t.iter().map(|&t| sig[((t * rate) as i64).clamp(0, last) as usize]).collect()
        });
        self.add(Emitter { iq_fn: Some(f), ..Emitter::new(freq, snr_db, start, Some(start + duration), "telemetry") })
    }

    // rendering -------------------------------------------------------------------------

    pub fn iq(&self, center_mhz: f64, rate: f64, t0: f64, n: usize) -> Vec<Complex32> {
        self.iq_sigma(center_mhz, rate, t0, n, NOISE_SIGMA)
    }

    /// Noise plus every active emitter, as complex64 at `rate`.
    ///
    /// Emitters are rendered at rate/10 and linearly interpolated up, which
    /// keeps images about 70 dB down for narrowband signals and runs in real
    /// time on a small CPU.
    pub fn iq_sigma(&self, center_mhz: f64, rate: f64, t0: f64, n: usize, sigma: f64) -> Vec<Complex32> {
        let scale = (sigma / 2f64.sqrt()) as f32;
        let mut x: Vec<Complex32> = {
            let mut rng = self.rng.lock().unwrap_or_else(|e| e.into_inner());
            (0..n).map(|_| {
                let re = rng.standard_normal() as f32;
                let im = rng.standard_normal() as f32;
                Complex32::new(re * scale, im * scale)
            }).collect()
        };
        let t1 = t0 + n as f64 / rate;
        let ems: Vec<Arc<Emitter>> = self.emitters().into_iter().filter(|e| e.active(t0, t1)).collect();
        let half = rate * 0.5 / 1e6;
        let up = 10usize;
        let n_low = n / up + 2;
        for e in ems {
            if (e.freq - center_mhz).abs() > half {
                continue;
            }
            let end = e.end.unwrap_or(f64::INFINITY);
            let inside = |k: usize| {
                let t = t0 + k as f64 / rate;
                t >= e.start && t < end
            };
            let Some(first) = (0..n).find(|&k| inside(k)) else { continue };
            if e.kind == "noise" {
                let g = 10f64.powf(e.snr_db / 20.0) as f32;
                for (k, v) in x.iter_mut().enumerate().skip(first) {
                    if inside(k) {
                        *v *= g;
                    }
                }
                continue;
            }
            let amp = (sigma * (10f64.powf(e.snr_db / 10.0) * 10_000.0 / rate).sqrt()) as f32;
            let low: Option<Vec<Complex32>> = e.iq_fn.as_ref().map(|f| {
                let rel: Vec<f64> = (0..n_low).map(|i| t0 - e.start + (i * up) as f64 / rate).collect();
                f(&rel, rate / up as f64).into_iter().map(|c| Complex32::new(c.re as f32, c.im as f32)).collect()
            });
            let off = (e.freq - center_mhz) * 1e6;
            let osc = self.osc(off, rate, n);
            let a = 2.0 * PI * off * t0;
            let rot = Complex32::new(a.cos() as f32, a.sin() as f32);
            for k in first..n {
                if !inside(k) {
                    continue;
                }
                let base = match &low {
                    Some(low) => {
                        let (i, j) = (k / up, k % up);
                        let frac = j as f32 / up as f32;
                        low[i] + (low[i + 1] - low[i]) * frac
                    }
                    None => Complex32::new(1.0, 0.0),
                };
                x[k] += base * amp * (osc[k] * rot);
            }
        }
        x
    }

    fn osc(&self, off_hz: f64, rate: f64, n: usize) -> Arc<Vec<Complex32>> {
        let key = ((off_hz * 1000.0).round_ties_even() as i64, rate.to_bits(), n);
        let mut cache = self.osc_cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = cache.get(&key) {
            return t.clone();
        }
        if cache.len() > 64 {
            cache.clear();
        }
        let t: Arc<Vec<Complex32>> = Arc::new((0..n).map(|k| {
            let a = 2.0 * PI * off_hz * k as f64 / rate;
            Complex32::new(a.cos() as f32, a.sin() as f32)
        }).collect());
        cache.insert(key, t.clone());
        t
    }

    pub fn scanner_rssi(&self, freq: Option<f64>, now: f64) -> i64 {
        let Some(freq) = freq.filter(|f| *f != 0.0) else { return 0 };
        let base = nominal_noise(freq) + self.rng.lock().map(|mut r| r.integers(-8, 8)).unwrap_or(0);
        let best = self.emitters().iter()
            .filter(|e| e.kind != "noise" && (e.freq - freq).abs() < 0.003 && e.active(now, now + 0.01))
            .map(|e| e.snr_db).fold(None, |m: Option<f64>, s| Some(m.map_or(s, |m| m.max(s))));
        match best {
            None => base,
            Some(b) => (1023.0f64).min(base as f64 + b * 2.5) as i64,
        }
    }
}

// fake dongle ---------------------------------------------------------------------------

struct SimTuning {
    rate: f64,
    center_mhz: f64,
    gain: Value,
}

/// Stands in for a real dongle: renders World IQ in real time.
pub struct SimDevice {
    pub serial: String,
    pub world: Arc<World>,
    pub clock: Clock,
    pub realtime: bool,
    pub info: Map<String, Value>,
    tuning: Mutex<SimTuning>,
    cancel: Mutex<bool>,
    wake: Condvar,
    pub stall: AtomicBool,
}

impl SimDevice {
    pub fn new(serial: &str, world: Arc<World>, clock: Clock, realtime: bool) -> Self {
        Self::with_names(serial, world, clock, realtime, "Nooelec", "NESDR SMArt v5")
    }

    pub fn with_names(serial: &str, world: Arc<World>, clock: Clock, realtime: bool, vendor: &str,
                      product: &str) -> Self {
        let info = json!({"vendor": vendor, "product": product, "chipset": "RTL2832U", "tuner": "R820T/R820T2"});
        SimDevice {
            serial: serial.into(), world, clock, realtime, info: info.as_object().cloned().unwrap_or_default(),
            tuning: Mutex::new(SimTuning { rate: 2_400_000.0, center_mhz: 0.0, gain: Value::Null }),
            cancel: Mutex::new(false), wake: Condvar::new(), stall: AtomicBool::new(false),
        }
    }

    pub fn configure(&self, rate: f64, center_hz: f64, gain: Value) {
        if let Ok(mut t) = self.tuning.lock() {
            *t = SimTuning { rate, center_mhz: center_hz / 1e6, gain };
        }
    }

    pub fn center_mhz(&self) -> f64 {
        self.tuning.lock().map(|t| t.center_mhz).unwrap_or(0.0)
    }

    pub fn gain(&self) -> Value {
        self.tuning.lock().map(|t| t.gain.clone()).unwrap_or(Value::Null)
    }

    fn cancelled(&self) -> bool {
        self.cancel.lock().map(|c| *c).unwrap_or(true)
    }

    /// Calls `callback` with blocks of `num_samples` until cancelled.
    pub fn read_samples_async(&self, callback: &mut dyn FnMut(Vec<Complex32>), num_samples: usize) {
        if let Ok(mut c) = self.cancel.lock() {
            *c = false;
        }
        let mut t = (self.clock)();
        while !self.cancelled() {
            let (rate, center) = self.tuning.lock().map(|g| (g.rate, g.center_mhz)).unwrap_or((2_400_000.0, 0.0));
            if !self.stall.load(Ordering::SeqCst) {
                callback(self.world.iq(center, rate, t, num_samples));
            }
            t += num_samples as f64 / rate;
            if self.realtime {
                let delay = t - (self.clock)();
                if delay > 0.0 {
                    if let Ok(g) = self.cancel.lock() {
                        let _ = self.wake.wait_timeout_while(g, Duration::from_secs_f64(delay), |c| !*c);
                    }
                }
            }
        }
    }

    pub fn cancel_read_async(&self) {
        if let Ok(mut c) = self.cancel.lock() {
            *c = true;
        }
        self.wake.notify_all();
    }

    pub fn close(&self) {
        self.cancel_read_async();
    }
}

impl DeviceControl for SimDevice {
    fn cancel_read_async(&self) {
        SimDevice::cancel_read_async(self);
    }

    fn set_center(&self, center_hz: f64) {
        if let Ok(mut t) = self.tuning.lock() {
            t.center_mhz = center_hz / 1e6;
        }
    }
}

// traffic ----------------------------------------------------------------------------------

/// Adds synthetic transmissions to the World while --simulate runs.
pub struct Traffic {
    pub world: Arc<World>,
    pub plan: Value,
    pub clock: Clock,
    rnd: Rng,
    next_voice: f64,
    next_eot: f64,
    unit: i64,
    psi: i64,
    voice: Vec<Value>,
    eotch: Vec<Value>,
}

/// A running Traffic thread.
pub struct TrafficHandle {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl TrafficHandle {
    pub fn stop(&mut self) {
        let (m, cv) = &*self.stop;
        if let Ok(mut s) = m.lock() {
            *s = true;
        }
        cv.notify_all();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TrafficHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Traffic {
    pub fn new(world: Arc<World>, plan: &Value, clock: Clock) -> Self {
        Self::with_seed(world, plan, clock, 7)
    }

    pub fn with_seed(world: Arc<World>, plan: &Value, clock: Clock, seed: u64) -> Self {
        let chans: Vec<Value> = plan.get("channels").and_then(Value::as_array).cloned().unwrap_or_default();
        let voice = chans.iter().filter(|c| c.get("kind").and_then(Value::as_str) == Some("voice")).cloned().collect();
        let eotch = chans.iter()
            .filter(|c| c.get("label").and_then(Value::as_str).unwrap_or("").to_uppercase().contains("EOT"))
            .cloned().collect();
        Traffic { world, plan: plan.clone(), clock, rnd: Rng::new(seed), next_voice: 0.0, next_eot: 0.0,
                  unit: 75618, psi: 88, voice, eotch }
    }

    /// The reference station, on for good, and the first transmission times.
    pub fn begin(&mut self) {
        let now = (self.clock)();
        if let Some(r) = self.plan.get("reference").filter(|r| r.as_object().is_some_and(|o| !o.is_empty())) {
            self.world.speechlike(crate::sdr::freq_of(r), 36.0, now - 1.0, None, 2500.0, "noaa");
        }
        self.next_voice = now + 20.0;
        self.next_eot = now + 15.0;
    }

    /// One pass of the loop (every 0.5 s).
    pub fn tick(&mut self) {
        let now = (self.clock)();
        self.world.prune(now);
        if !self.voice.is_empty() && now >= self.next_voice {
            let ch = self.rnd.choice(&self.voice).clone();
            let dur = self.rnd.uniform(3.0, 10.0);
            let snr = self.rnd.uniform(10.0, 40.0);
            let f = crate::sdr::freq_of(&ch);
            self.world.speechlike(f, snr, now, Some(now + dur), 2500.0, "voice");
            log::info!("simulate: voice on {:.4} MHz for {:.1} s at +{:.0} dB", f, dur, snr);
            self.next_voice = now + self.rnd.uniform(40.0, 90.0);
        }
        if !self.eotch.is_empty() && now >= self.next_eot {
            self.psi = (self.psi + *self.rnd.choice(&[-1i64, 0, 1])).clamp(80, 92);
            let p = eot::build_payload(self.unit, self.psi);
            let snr = self.rnd.uniform(10.0, 40.0);
            self.world.eot_burst(crate::sdr::freq_of(&self.eotch[0]), snr, now, &[p.clone(), p]);
            self.next_eot = now + self.rnd.uniform(20.0, 60.0);
        }
    }

    pub fn spawn(mut self) -> TrafficHandle {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let s2 = stop.clone();
        let thread = std::thread::Builder::new().name("sim-traffic".into()).spawn(move || {
            self.begin();
            loop {
                let (m, cv) = &*s2;
                let Ok(g) = m.lock() else { return };
                let (g, _) = match cv.wait_timeout_while(g, Duration::from_millis(500), |s| !*s) {
                    Ok(r) => r,
                    Err(_) => return,
                };
                if *g {
                    return;
                }
                drop(g);
                self.tick();
            }
        }).ok();
        TrafficHandle { stop, thread }
    }
}
