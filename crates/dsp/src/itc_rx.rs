//! An ITC dongle (role `itc`): its samples go to the ITC decoder (railspotting-itc) instead of
//! the voice and telemetry detectors.
//!
//! Channels: the national channel (102), where base stations send beacons, and the local channel
//! of every base whose beacon is heard, when it is inside the dongle's window. A fixed list can be
//! given instead (`channels` in the dongle's settings, ITC channel numbers).
//!
//! Decoded messages go to a daily log (`state/itc/YYYY-MM-DD.jsonl`, kept `keep_days`) and to a
//! local feed for a monitor (`serve`, 127.0.0.1:8221 by default; empty turns it off). They are not
//! sent to the headend: they describe railroad operations.

use num_complex::Complex32;
use railspotting_itc::decode::{decode_burst, Decoder};
use railspotting_itc::dsp::{channel_hz, channel_number, channels_in};
use railspotting_itc::feed::Feed;
use railspotting_itc::survey::{beacon_of, NATIONAL_CH};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A burst opens this many dB above its channel's noise floor.
const OPEN_DB: f32 = 8.0;
pub const DEFAULT_SERVE: &str = "127.0.0.1:8221";
pub const DEFAULT_KEEP_DAYS: u64 = 7;

pub struct ItcRx {
    /// Channels given in the settings (ITC numbers); empty: found from beacons.
    fixed: Vec<i64>,
    watched: Vec<i64>,
    decoder: Option<Decoder>,
    /// The window the decoder was made for (centre MHz, rate).
    made_for: (f64, f64),
    dir: PathBuf,
    keep_days: u64,
    log: Option<(String, std::io::BufWriter<std::fs::File>)>,
    feed: Option<Feed>,
    pub serve: String,
    pub messages: u64,
    pub by_type: BTreeMap<String, u64>,
    /// Base radio -> (local channel, level dB, last heard).
    pub bases: BTreeMap<String, (i64, f64, f64)>,
    pub last_t: f64,
    pub last_error: Option<String>,
}

fn utc_date(t: f64) -> String {
    // days from 1970-01-01 to a civil date (proleptic Gregorian)
    let z = (t / 86400.0).floor() as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

impl ItcRx {
    /// From the dongle's settings: `channels` (ITC numbers), `serve`, `keep_days`.
    pub fn new(dev_cfg: &Value, state_dir: &Path) -> ItcRx {
        let fixed: Vec<i64> = dev_cfg.get("channels").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
        let serve = match dev_cfg.get("serve") {
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Null) | None => DEFAULT_SERVE.to_string(),
            Some(_) => String::new(),
        };
        let keep_days = dev_cfg.get("keep_days").and_then(Value::as_u64).unwrap_or(DEFAULT_KEEP_DAYS).max(1);
        ItcRx {
            watched: if fixed.is_empty() { vec![NATIONAL_CH] } else { fixed.clone() },
            fixed,
            decoder: None,
            made_for: (0.0, 0.0),
            dir: state_dir.join("itc"),
            keep_days,
            log: None,
            feed: None,
            serve,
            messages: 0,
            by_type: BTreeMap::new(),
            bases: BTreeMap::new(),
            last_t: 0.0,
            last_error: None,
        }
    }

    /// Start the feed (once the dongle streams, so a receiver without an ITC dongle opens no port).
    fn ensure_feed(&mut self) {
        if self.feed.is_some() || self.serve.is_empty() {
            return;
        }
        match Feed::serve(&self.serve) {
            Ok(f) => {
                log::info!("ITC: messages feed on {}", f.addr);
                self.feed = Some(f);
            }
            Err(e) => {
                log::warn!("ITC: cannot serve the feed on {}: {e}", self.serve);
                self.serve.clear();
            }
        }
    }

    /// One block of samples from a window at `center_mhz` and `rate`, the first taken at `t0`.
    pub fn process(&mut self, samples: &[Complex32], t0: f64, center_mhz: f64, rate: f64) {
        self.ensure_feed();
        if self.decoder.is_none() || self.made_for != (center_mhz, rate) {
            let freqs: Vec<f64> = self.watched.iter().map(|&n| channel_hz(n)).collect();
            match Decoder::new(center_mhz * 1e6, rate, t0, &freqs, OPEN_DB) {
                Ok(d) => {
                    log::info!("ITC: decoding channels {:?} at {:.4} MHz", self.watched, center_mhz);
                    self.decoder = Some(d);
                    self.made_for = (center_mhz, rate);
                    self.last_error = None;
                }
                Err(e) => {
                    if self.last_error.as_deref() != Some(e.as_str()) {
                        log::warn!("ITC: {e}");
                    }
                    self.last_error = Some(e);
                    return;
                }
            }
        }
        let Some(dec) = self.decoder.as_mut() else { return };
        let mut msgs = vec![];
        for b in dec.bursts(samples) {
            msgs.extend(decode_burst(&b, false));
        }
        let mut add = vec![];
        for m in &msgs {
            self.messages += 1;
            *self.by_type.entry(m["type"].as_str().unwrap_or("OTHER").to_string()).or_default() += 1;
            let t = m["t"].as_f64().unwrap_or(t0);
            self.last_t = self.last_t.max(t);
            if let Some((radio, b)) = beacon_of(m) {
                if !self.bases.contains_key(&radio) {
                    log::info!("ITC: base station {radio} heard, local channel {}", b.local);
                }
                self.bases.insert(radio, (b.local, b.level, t));
                if self.fixed.is_empty() && !self.watched.contains(&b.local) && !add.contains(&b.local) {
                    let heard = channels_in(center_mhz * 1e6, rate).iter().any(|&f| channel_number(f) == b.local);
                    if heard {
                        add.push(b.local);
                    } else {
                        log::warn!("ITC: base channel {} is outside this dongle's window; set center_mhz to reach it", b.local);
                    }
                }
            }
            let line = m.to_string();
            self.write_log(t, &line);
            if let Some(f) = &self.feed {
                f.send(&line);
            }
        }
        if !add.is_empty() {
            self.watched.extend(add);
            self.watched.sort();
            self.decoder = None; // remade for the new channels on the next block
        }
    }

    fn write_log(&mut self, t: f64, line: &str) {
        let day = utc_date(t);
        if self.log.as_ref().is_none_or(|(d, _)| *d != day) {
            if let Err(e) = std::fs::create_dir_all(&self.dir) {
                log::warn!("ITC: {}: {e}", self.dir.display());
                return;
            }
            self.prune(&day);
            let path = self.dir.join(format!("{day}.jsonl"));
            match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(f) => self.log = Some((day, std::io::BufWriter::new(f))),
                Err(e) => {
                    log::warn!("ITC: {}: {e}", path.display());
                    return;
                }
            }
        }
        if let Some((_, w)) = self.log.as_mut() {
            if writeln!(w, "{line}").and_then(|_| w.flush()).is_err() {
                self.log = None;
            }
        }
    }

    /// Delete daily logs older than `keep_days` (file names sort by date).
    fn prune(&self, today: &str) {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        let mut days: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
        days.sort();
        let keep = self.keep_days as usize;
        let old: Vec<&PathBuf> = days.iter().filter(|p| p.file_stem().is_some_and(|s| s.to_string_lossy().as_ref() < today)).collect();
        if old.len() + 1 > keep {
            for p in &old[..old.len() + 1 - keep] {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// For the status page and `status`.
    pub fn status(&self) -> Map<String, Value> {
        let bases: Vec<Value> = self.bases.iter().map(|(r, (ch, db, t))| json!({ "radio": r, "local_channel": ch, "level_db": db, "last": t })).collect();
        json!({
            "channels": self.watched,
            "messages": self.messages,
            "by_type": self.by_type,
            "bases": bases,
            "last_message": if self.last_t > 0.0 { json!(self.last_t) } else { Value::Null },
            "feed": if self.feed.is_some() { json!(self.serve) } else { Value::Null },
            "log_dir": self.dir.display().to_string(),
            "error": self.last_error,
        })
        .as_object()
        .cloned()
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_settings() {
        assert_eq!(utc_date(0.0), "1970-01-01");
        assert_eq!(utc_date(1_792_894_800.0), "2026-10-25");
        assert_eq!(utc_date(951_782_400.0), "2000-02-29");
        let dir = std::env::temp_dir().join(format!("rs-itc-rx-{}", std::process::id()));
        let r = ItcRx::new(&json!({}), &dir);
        assert_eq!(r.serve, DEFAULT_SERVE);
        assert_eq!(r.watched, vec![NATIONAL_CH]);
        let fixed = ItcRx::new(&json!({"channels": [102, 114], "serve": "", "keep_days": 3}), &dir);
        assert_eq!(fixed.watched, vec![102, 114]);
        assert!(fixed.serve.is_empty());
        assert_eq!(fixed.keep_days, 3);
    }

    /// What a dongle at `center` would give: noise, one ITC frame on `channel`, noise.
    fn capture(center: f64, rate: f64, channel: f64, bits: &[u8]) -> Vec<Complex32> {
        use std::f32::consts::PI;
        let sps = (rate / 8_000.0) as usize; // the national channel runs at half rate
        let shift = 2.0 * PI * (channel - center) as f32 / rate as f32;
        let mut seed = 777u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 16) as f32 / 32768.0 - 1.0) * 0.02
        };
        let mut clean = vec![Complex32::new(0.0, 0.0); (1.5 * rate) as usize];
        let mut phase = 0.0f32;
        for d in bits.chunks(2) {
            phase += match (d[0], d[1]) {
                (0, 0) => PI / 4.0,
                (0, 1) => 3.0 * PI / 4.0,
                (1, 1) => -3.0 * PI / 4.0,
                _ => -PI / 4.0,
            };
            clean.extend(std::iter::repeat_n(Complex32::from_polar(0.4, phase), sps));
        }
        clean.extend(vec![Complex32::new(0.0, 0.0); (0.5 * rate) as usize]);
        clean.iter().enumerate().map(|(n, v)| v * Complex32::from_polar(1.0, shift * n as f32) + Complex32::new(noise(), noise())).collect()
    }

    #[test]
    fn a_beacon_is_logged_and_its_base_channel_watched() {
        let dir = std::env::temp_dir().join(format!("rs-itc-beacon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut r = ItcRx::new(&json!({"serve": ""}), &dir);
        // a made-up base 123456 at 40.5, -90.25, local channel 114, neighbours 101 and 127
        let mut msg = vec![0x88, 0x12, 0x34, 0x56];
        msg.extend(((40.5f64 * 600000.0) as i32).to_be_bytes());
        msg.extend(((-90.25f64 * 600000.0) as i32).to_be_bytes());
        msg.extend([114, 1, 20, 0x30, 101, 127, 0, 0, 0, 0]);
        let mut bits = vec![1u8; 14];
        bits.extend(railspotting_itc::link::encode_frame(0x02, &msg));
        let samples = capture(220.3e6, 1.024e6, channel_hz(NATIONAL_CH), &bits);
        let t0 = 1_792_713_600.0;
        for (k, block) in samples.chunks(51_200).enumerate() {
            r.process(block, t0 + k as f64 * 0.05, 220.3, 1.024e6);
        }
        assert_eq!(r.messages, 1);
        assert_eq!(r.by_type.get("BEACON"), Some(&1));
        assert_eq!(r.bases.get("123456").map(|b| b.0), Some(114));
        assert_eq!(r.watched, vec![NATIONAL_CH, 114]);
        let logged = std::fs::read_to_string(dir.join("itc").join("2026-10-23.jsonl")).unwrap();
        assert!(logged.contains("\"type\":\"BEACON\""), "{logged}");
        assert_eq!(r.status()["channels"], json!([102, 114]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_logs_are_pruned() {
        let dir = std::env::temp_dir().join(format!("rs-itc-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut r = ItcRx::new(&json!({"serve": "", "keep_days": 2}), &dir);
        std::fs::create_dir_all(dir.join("itc")).unwrap();
        for d in ["2026-10-20", "2026-10-21", "2026-10-22"] {
            std::fs::write(dir.join("itc").join(format!("{d}.jsonl")), "{}\n").unwrap();
        }
        // a message on the 23rd: today plus one earlier day are kept
        r.write_log(1_792_713_600.0 + 3600.0, "{\"type\":\"TIME\"}");
        let mut left: Vec<String> = std::fs::read_dir(dir.join("itc")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
        left.sort();
        assert_eq!(left, vec!["2026-10-22.jsonl", "2026-10-23.jsonl"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
