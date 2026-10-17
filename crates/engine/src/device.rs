//! One dongle: a device thread streaming USB blocks and a processing thread. It starts and stops
//! dsp's SdrSource, takes its samples and runs both loops.

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use num_complex::Complex32;
use railspotting_common::Clock;
use railspotting_dsp::sdr::{DeviceControl, SdrSource, QUEUE_BLOCKS, REOPEN_DELAY_S};
use railspotting_store::Signal;
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

/// What the device thread needs from a dongle (RtlDevice, or the simulator's).
pub trait Device: Send + Sync {
    fn info(&self) -> Map<String, Value>;
    fn configure(&self, rate: f64, center_hz: f64, gain: &Value) -> Result<(), String>;
    fn set_center(&self, center_hz: f64);
    /// Blocks, calling back with each block of samples, until cancelled or the stream fails.
    fn read_samples_async(&self, callback: &mut dyn FnMut(Vec<Complex32>), num_samples: usize) -> Result<(), String>;
    fn cancel_read_async(&self);
    fn close(&self);
}

/// Opens a dongle by serial.
pub type DeviceFactory = Arc<dyn Fn(&str) -> Result<Arc<dyn Device>, String> + Send + Sync>;

/// The real dongles through librtlsdr.
pub fn rtl_factory() -> DeviceFactory {
    Arc::new(|serial: &str| crate::rtl::RtlDevice::open(serial).map(|d| Arc::new(d) as Arc<dyn Device>))
}

struct Ctl(Arc<dyn Device>);

impl DeviceControl for Ctl {
    fn cancel_read_async(&self) {
        self.0.cancel_read_async()
    }
    fn set_center(&self, center_hz: f64) {
        self.0.set_center(center_hz)
    }
}

/// The simulator's dongle.
pub struct SimDev(pub railspotting_dsp::sim::SimDevice);

impl Device for SimDev {
    fn info(&self) -> Map<String, Value> {
        self.0.info.clone()
    }
    fn configure(&self, rate: f64, center_hz: f64, gain: &Value) -> Result<(), String> {
        self.0.configure(rate, center_hz, gain.clone());
        Ok(())
    }
    fn set_center(&self, center_hz: f64) {
        DeviceControl::set_center(&self.0, center_hz)
    }
    fn read_samples_async(&self, callback: &mut dyn FnMut(Vec<Complex32>), num_samples: usize) -> Result<(), String> {
        self.0.read_samples_async(callback, num_samples);
        Ok(())
    }
    fn cancel_read_async(&self) {
        self.0.cancel_read_async()
    }
    fn close(&self) {
        self.0.close()
    }
}

/// What other threads may read without waiting for the processing thread.
#[derive(Debug, Clone, Default)]
pub struct Cover {
    pub connected: bool,
    pub voice: bool,
    pub eot: bool,
    pub hot: bool,
}

/// An SdrSource with its threads.
pub struct SdrHandle {
    src: Mutex<SdrSource>,
    tx: Sender<(Vec<Complex32>, f64)>,
    rx: Receiver<(Vec<Complex32>, f64)>,
    overruns: AtomicU64,
    factory: Option<DeviceFactory>,
    clock: Clock,
    stop: Signal,
    cover: Mutex<Cover>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// Seconds between reopen attempts (REOPEN_DELAY_S; shorter in tests).
    pub reopen_delay_s: Mutex<f64>,
}

impl SdrHandle {
    pub fn new(src: SdrSource, factory: Option<DeviceFactory>) -> Arc<SdrHandle> {
        let (tx, rx) = bounded(QUEUE_BLOCKS);
        let clock = src.clock.clone();
        let h = SdrHandle {
            src: Mutex::new(src), tx, rx, overruns: AtomicU64::new(0), factory, clock,
            stop: Signal::default(), cover: Mutex::new(Cover::default()), threads: Mutex::new(vec![]),
            reopen_delay_s: Mutex::new(REOPEN_DELAY_S),
        };
        h.with(|_| ());
        Arc::new(h)
    }

    fn guard(&self) -> MutexGuard<'_, SdrSource> {
        let mut g = self.src.lock().unwrap_or_else(|p| p.into_inner());
        g.overruns = self.overruns.load(Ordering::SeqCst);
        g
    }

    fn refresh(&self, s: &SdrSource) {
        let c = Cover {
            connected: s.connected,
            voice: s.covers_kind("voice", None),
            eot: s.covers_kind("telemetry", Some("EOT")),
            hot: s.covers_kind("telemetry", Some("HOT")),
        };
        *self.cover.lock().unwrap_or_else(|p| p.into_inner()) = c;
    }

    /// Run `f` on the source under its lock.
    pub fn with<T>(&self, f: impl FnOnce(&mut SdrSource) -> T) -> T {
        let mut g = self.guard();
        let out = f(&mut g);
        // tests and the agent may change counters directly
        self.overruns.store(g.overruns, Ordering::SeqCst);
        self.refresh(&g);
        out
    }

    /// Coverage without waiting: the live source when it is free, else the last snapshot
    /// (the processing thread holds the lock while it emits).
    pub fn covers(&self, kind: &str, word: Option<&str>) -> bool {
        if let Ok(mut g) = self.src.try_lock() {
            g.overruns = self.overruns.load(Ordering::SeqCst);
            let r = g.covers_kind(kind, word);
            self.refresh(&g);
            return r;
        }
        let c = self.cover.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match (kind, word.map(str::to_ascii_uppercase).as_deref()) {
            ("voice", None) => c.voice,
            ("telemetry", Some("EOT")) => c.eot,
            ("telemetry", Some("HOT")) => c.hot,
            _ => false,
        }
    }

    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::SeqCst)
    }

    /// USB callback: copy, stamp, hand off. Never blocks.
    pub fn on_samples(&self, samples: Vec<Complex32>) {
        match self.tx.try_send((samples, (self.clock)())) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.overruns.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    pub fn start(self: &Arc<Self>) {
        self.stop.clear();
        let role = self.with(|s| s.role.clone());
        let mut threads = self.threads.lock().unwrap_or_else(|p| p.into_inner());
        if self.factory.is_some() {
            let me = self.clone();
            if let Ok(t) = std::thread::Builder::new().name(format!("sdr-{role}-dev")).spawn(move || me.device_loop()) {
                threads.push(t);
            }
        }
        let me = self.clone();
        if let Ok(t) = std::thread::Builder::new().name(format!("sdr-{role}-proc")).spawn(move || me.process_loop()) {
            threads.push(t);
        }
    }

    pub fn stop(&self) {
        self.stop.set();
        let dev = self.with(|s| if s.streaming { s.dev.clone() } else { None });
        if let Some(d) = dev {
            d.cancel_read_async();
        }
    }

    pub fn stopped(&self) -> bool {
        self.stop.is_set()
    }

    /// Wait for the threads to end (after stop). A cancel that came just before the stream
    /// started is lost, so it is repeated until the device thread is out.
    pub fn join(&self) {
        let threads: Vec<_> = self.threads.lock().unwrap_or_else(|p| p.into_inner()).drain(..).collect();
        for t in threads {
            while !t.is_finished() {
                if let Some(d) = self.with(|s| s.dev.clone()) {
                    d.cancel_read_async();
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = t.join();
        }
    }

    fn reopen_delay(&self) -> f64 {
        *self.reopen_delay_s.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn device_loop(self: Arc<Self>) {
        let Some(factory) = self.factory.clone() else { return };
        while !self.stop.is_set() {
            let (serial, rate, gain) = self.with(|s| (s.serial.clone(), s.rate, s.gain.clone()));
            if serial.is_empty() {
                // automatic roles: no dongle for this one yet
                self.stop.wait(self.reopen_delay());
                continue;
            }
            let mut opened: Option<Arc<dyn Device>> = None;
            let result = (|| -> Result<(), String> {
                let dev = factory(&serial)?;
                opened = Some(dev.clone());
                let info = dev.info();
                let center = self.with(|s| {
                    if !info.is_empty() {
                        s.info = info.clone();
                    }
                    s.open_center()
                });
                dev.configure(rate, center * 1e6, &gain)?;
                self.with(|s| s.stream_started(Arc::new(Ctl(dev.clone())), None));
                let me = self.clone();
                dev.read_samples_async(&mut |samples| me.on_samples(samples), self.with(|s| s.block))?;
                if self.stop.is_set() {
                    return Ok(());
                }
                Err(self.with(|s| s.stream_end_reason().to_string()))
            })();
            match result {
                Err(msg) if !self.stop.is_set() => self.with(|s| s.stream_ended(Some(&msg))),
                _ => self.with(|s| s.stream_ended(None)),
            }
            if let Some(dev) = opened {
                dev.close();
            }
            if self.stop.is_set() {
                break;
            }
            self.stop.wait(self.reopen_delay());
        }
    }

    fn process_loop(self: Arc<Self>) {
        while !self.stop.is_set() {
            match self.rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Err(RecvTimeoutError::Timeout) => {
                    let now = (self.clock)();
                    self.with(|s| s.check_stall(now));
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Ok((samples, ts)) => {
                    let now = (self.clock)();
                    self.with(|s| {
                        s.last_block_rx = now;
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.process_block(&samples, ts)));
                        if r.is_err() {
                            log::error!("{}: processing failed", s.name);
                        }
                    });
                }
            }
        }
    }
}
