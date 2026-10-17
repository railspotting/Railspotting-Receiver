//! In-memory SR30C behind a serial-port-like interface.
//!
//! What the radio hears comes from an `RssiSource`: the receiver's simulated world in
//! `--simulate`, or the small `CarrierWorld` here in tests.

use crate::scanner::{freq_to_digits, Opener, SerialLink};
use railspotting_common::{nominal_noise, Clock};
use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

/// The scanner's RSSI reading (0 to 1023) for a frequency at a time.
pub trait RssiSource: Send + Sync {
    fn scanner_rssi(&self, freq: Option<f64>, now: f64) -> i64;
}

/// A steady carrier from `start` to `end` (None: forever).
#[derive(Debug, Clone)]
pub struct Carrier {
    pub freq: f64,
    pub snr_db: f64,
    pub start: f64,
    pub end: Option<f64>,
}

impl Carrier {
    pub fn active(&self, t0: f64, t1: f64) -> bool {
        self.start < t1 && self.end.is_none_or(|e| e > t0)
    }
}

/// Carriers on a noise floor that wobbles by -8..8.
pub struct CarrierWorld {
    carriers: Mutex<Vec<Carrier>>,
    rng: Mutex<u64>,
}

impl CarrierWorld {
    pub fn new(seed: u64) -> Self {
        CarrierWorld { carriers: Mutex::new(Vec::new()), rng: Mutex::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1) }
    }

    pub fn carrier(&self, freq: f64, snr_db: f64, start: f64, end: Option<f64>) {
        lock(&self.carriers).push(Carrier { freq, snr_db, start, end });
    }

    pub fn clear(&self) {
        lock(&self.carriers).clear();
    }

    /// Uniform integer in [lo, hi).
    fn integers(&self, lo: i64, hi: i64) -> i64 {
        let mut s = lock(&self.rng);
        // xorshift64*
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        let r = s.wrapping_mul(0x2545_F491_4F6C_DD1D);
        lo + (r % (hi - lo) as u64) as i64
    }
}

impl Default for CarrierWorld {
    fn default() -> Self {
        CarrierWorld::new(1)
    }
}

impl RssiSource for CarrierWorld {
    fn scanner_rssi(&self, freq: Option<f64>, now: f64) -> i64 {
        let Some(freq) = freq.filter(|f| *f != 0.0) else { return 0 };
        let base = nominal_noise(freq) + self.integers(-8, 8);
        let best = lock(&self.carriers)
            .iter()
            .filter(|c| (c.freq - freq).abs() < 0.003 && c.active(now, now + 0.01))
            .map(|c| c.snr_db)
            .fold(None, |a: Option<f64>, b| Some(a.map_or(b, |a| a.max(b))));
        match best {
            None => base,
            Some(b) => (1023f64).min(base as f64 + b * 2.5) as i64,
        }
    }
}

impl<T: RssiSource + ?Sized> RssiSource for Arc<T> {
    fn scanner_rssi(&self, freq: Option<f64>, now: f64) -> i64 {
        (**self).scanner_rssi(freq, now)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One memory channel: frequency in 100 Hz units, delay and lockout as the CIN reply has them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeChannel {
    pub freq: i64,
    pub delay: String,
    pub lockout: String,
}

impl FakeChannel {
    fn empty() -> Self {
        FakeChannel { freq: 0, delay: "0".into(), lockout: "1".into() }
    }
}

/// The radio's state, open for tests to read and change.
pub struct FakeState {
    pub channels: BTreeMap<i64, FakeChannel>,
    pub banks: String,
    pub volume: i64,
    pub sql_level: i64,
    pub program: bool,
    pub mode: String,
    pub parked_ch: i64,
    pub connected: bool,
    pub log: Vec<String>,
    pub writes: u32,
    reply: Vec<u8>,
    step: i64,
}

#[derive(Clone)]
pub struct FakeSR30C {
    inner: Arc<Mutex<FakeState>>,
    world: Arc<dyn RssiSource>,
    clock: Clock,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.into())
}

fn int(s: &str) -> io::Result<i64> {
    crate::util::parse_int(s).map_err(bad)
}

impl FakeSR30C {
    pub fn new(world: Arc<dyn RssiSource>, clock: Clock) -> Self {
        Self::with_banks(world, clock, "0111110110")
    }

    pub fn with_banks(world: Arc<dyn RssiSource>, clock: Clock, banks: &str) -> Self {
        let channels = (1..=500).map(|n| (n, FakeChannel::empty())).collect();
        let st = FakeState {
            channels, banks: banks.into(), volume: 9, sql_level: 2, program: false, mode: "scanning".into(),
            parked_ch: 1, connected: true, log: Vec::new(), writes: 0, reply: Vec::new(), step: -1,
        };
        FakeSR30C { inner: Arc::new(Mutex::new(st)), world, clock }
    }

    pub fn lock(&self) -> MutexGuard<'_, FakeState> {
        lock(&self.inner)
    }

    pub fn set_connected(&self, on: bool) {
        self.lock().connected = on;
    }

    pub fn program_channel(&self, n: i64, freq: f64, delay: bool, lockout: bool) {
        self.lock().channels.insert(n, FakeChannel {
            freq: (freq * 10_000.0).round_ties_even() as i64,
            delay: if delay { "1" } else { "0" }.into(),
            lockout: if lockout { "1" } else { "0" }.into(),
        });
    }

    /// A serial link to this radio.
    pub fn link(&self) -> Box<dyn SerialLink> {
        Box::new(FakeLink(self.clone()))
    }

    /// An opener for the monitor; fails like a missing port while disconnected.
    pub fn opener(&self) -> Opener {
        let me = self.clone();
        Arc::new(move |port: &str, _baud: u32| {
            if !me.lock().connected {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("could not open port {port}")));
            }
            Ok(me.link())
        })
    }

    fn scannable(st: &FakeState) -> Vec<i64> {
        let banks: Vec<char> = st.banks.chars().collect();
        st.channels
            .iter()
            .filter(|(n, c)| {
                let bank = ((**n - 1) / 50) as usize;
                c.freq != 0 && c.lockout == "0" && banks.get(bank) == Some(&'0')
            })
            .map(|(n, _)| *n)
            .collect()
    }

    /// (frequency, channel) the radio is on. While scanning with nothing heard it
    /// steps through the scannable channels with squelch closed.
    fn current(&self, st: &mut FakeState) -> (Option<f64>, Option<i64>) {
        let now = (self.clock)();
        if st.program {
            return (None, None);
        }
        if st.mode == "parked" {
            let f = st.channels.get(&st.parked_ch).map_or(0, |c| c.freq) as f64 / 10_000.0;
            return (if f != 0.0 { Some(f) } else { None }, Some(st.parked_ch));
        }
        let chans = Self::scannable(st);
        for &n in &chans {
            let f = st.channels[&n].freq as f64 / 10_000.0;
            if self.world.scanner_rssi(Some(f), now) >= nominal_noise(f) + 40 {
                return (Some(f), Some(n));
            }
        }
        if chans.is_empty() {
            return (None, None);
        }
        st.step = (st.step + 1).rem_euclid(chans.len() as i64);
        let n = chans[st.step as usize];
        (Some(st.channels[&n].freq as f64 / 10_000.0), Some(n))
    }

    pub fn handle(&self, text: &str) -> io::Result<String> {
        let mut st = self.lock();
        self.handle_locked(&mut st, text)
    }

    fn handle_locked(&self, st: &mut FakeState, text: &str) -> io::Result<String> {
        st.log.push(text.to_string());
        let parts: Vec<&str> = text.split(',').collect();
        let c = parts[0].to_uppercase();
        Ok(match c.as_str() {
            "MDL" => "MDL,SR30C".into(),
            "VER" => "VER,Version 1.01.04".into(),
            "GLG" => {
                let (f, _) = self.current(st);
                match f {
                    None => "GLG,0.0000,,,,,,,0,0,,,".into(),
                    Some(f) => {
                        let rssi = self.world.scanner_rssi(Some(f), (self.clock)());
                        let sql = if rssi >= nominal_noise(f) + 40 + st.sql_level { "1" } else { "0" };
                        format!("GLG,{:.4},NFM,0,0,,,,{sql},0,,,", f)
                    }
                }
            }
            "PWR" => {
                let (f, _) = self.current(st);
                match f {
                    None => "PWR,0,00000000".into(),
                    Some(f) => format!("PWR,{},{}", self.world.scanner_rssi(Some(f), (self.clock)()), freq_to_digits(f)),
                }
            }
            "VOL" => {
                if parts.len() == 1 {
                    format!("VOL,{}", st.volume)
                } else {
                    st.volume = int(parts[1])?;
                    "VOL,OK".into()
                }
            }
            "SQL" => {
                if parts.len() == 1 {
                    format!("SQL,{}", st.sql_level)
                } else {
                    st.sql_level = int(parts[1])?;
                    "SQL,OK".into()
                }
            }
            "KEY" => {
                let k = parts.get(1).copied().unwrap_or("");
                let digit = !k.is_empty() && k.chars().all(|c| c.is_ascii_digit());
                if k == "H" && st.mode == "parked" {
                    st.mode = "scanning".into();
                } else if digit && st.mode == "scanning" {
                    let i = if k != "0" { int(k)? - 1 } else { 9 } as usize;
                    let mut b: Vec<char> = st.banks.chars().collect();
                    let cur = *b.get(i).ok_or_else(|| bad("string index out of range"))?;
                    b[i] = if cur == '0' { '1' } else { '0' };
                    st.banks = b.into_iter().collect();
                } else if k == "<" || k == ">" {
                    st.volume = (st.volume + if k == ">" { 1 } else { -1 }).clamp(0, 15);
                }
                "KEY,OK".into()
            }
            "PRG" => {
                st.program = true;
                "PRG,OK".into()
            }
            "EPG" => {
                st.program = false;
                st.mode = "parked".into();
                st.parked_ch = 1;
                "EPG,OK".into()
            }
            "CIN" => {
                if !st.program {
                    return Ok("ERR".into());
                }
                let n = int(parts.get(1).copied().unwrap_or(""))?;
                let ch = st.channels.get(&n).cloned().ok_or_else(|| bad(n.to_string()))?;
                if parts.len() == 2 {
                    format!("CIN,{n},,{:08},,,{},{},0", ch.freq, ch.delay, ch.lockout)
                } else {
                    let field = |i: usize| parts.get(i).copied().ok_or_else(|| bad("list index out of range"));
                    let freq = int(field(3)?)?;
                    st.writes += 1;
                    let new = if freq == 0 {
                        FakeChannel::empty()
                    } else {
                        FakeChannel { freq, delay: if field(6)? != "0" { "1" } else { "0" }.into(), lockout: field(7)?.into() }
                    };
                    st.channels.insert(n, new);
                    "CIN,OK".into()
                }
            }
            "SCG" => {
                if parts.len() == 1 {
                    format!("SCG,{}", st.banks)
                } else if !st.program {
                    "ERR".into()
                } else {
                    st.banks = parts[1].into();
                    st.writes += 1;
                    "SCG,OK".into()
                }
            }
            _ => "ERR".into(), // CLR, ESN, BSV, WXS, ...
        })
    }
}

struct FakeLink(FakeSR30C);

impl SerialLink for FakeLink {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let fake = &self.0;
        let mut st = fake.lock();
        if !st.connected {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "device disconnected"));
        }
        let text = String::from_utf8_lossy(data);
        let text = text.trim_matches('\r');
        st.reply = if text.is_empty() {
            Vec::new()
        } else {
            let mut r = fake.handle_locked(&mut st, text)?.into_bytes();
            r.push(b'\r');
            r
        };
        Ok(())
    }

    fn read_until(&mut self, _term: u8) -> io::Result<Vec<u8>> {
        let mut st = self.0.lock();
        if !st.connected {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "device disconnected"));
        }
        Ok(std::mem::take(&mut st.reply))
    }

    fn reset_input_buffer(&mut self) -> io::Result<()> {
        self.0.lock().reply.clear();
        Ok(())
    }
}
