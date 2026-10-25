//! An ITC dongle (role `itc`): its samples go to the ITC decoder (railspotting-itc) instead of
//! the voice and telemetry detectors.
//!
//! Channels: the national channel (102), where base stations send beacons, and the local channel
//! of every base whose beacon is heard, when it is inside the dongle's window. A fixed list can be
//! given instead (`channels` in the dongle's settings, ITC channel numbers).
//!
//! Decoded messages go to a daily log (`state/itc/YYYY-MM-DD.jsonl`, kept `keep_days`) and to a
//! local feed for a monitor (`serve`, 127.0.0.1:8221 by default; empty turns it off). They describe
//! railroad operations, so they are not sent to the headend. When the owner turns ITC upload on
//! (`/config` `itc.upload`, contract v1.18), wayside state changes and base stations are, as
//! records from `railspotting_itc::changes`; repeats never are.
//!
//! For the owner's own screens (the phone app), `units` gives each wayside unit's state as last
//! heard and the latest changes, by the same rules as the records. It is not part of `status`,
//! so the open status page never shows signal states.

use num_complex::Complex32;
use railspotting_itc::decode::{decode_burst, Decoder};
use railspotting_itc::dsp::{channel_hz, channel_number, channels_in};
use railspotting_itc::feed::Feed;
use railspotting_itc::changes::Tracker;
use railspotting_itc::survey::{beacon_of, NATIONAL_CH};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A burst opens this many dB above its channel's noise floor.
const OPEN_DB: f32 = 8.0;
pub const DEFAULT_SERVE: &str = "127.0.0.1:8221";
pub const DEFAULT_KEEP_DAYS: u64 = 7;
/// The heartbeat counts messages over this many seconds.
pub const SUMMARY_S: f64 = 60.0;
/// The heartbeat lists base stations heard within this many seconds.
pub const BASES_S: f64 = 300.0;
/// `units` forgets a wayside unit not heard for this long.
pub const UNITS_KEEP_S: f64 = 3600.0;
/// `units` lists at most this many units, and this many of the latest changes.
pub const UNITS_SHOWN: usize = 200;
pub const CHANGES_KEPT: usize = 50;

/// A wayside unit as this receiver last heard it.
#[derive(Clone, Debug, PartialEq)]
pub struct UnitView {
    pub codes: Vec<i64>,
    pub previous: Option<Vec<i64>>,
    /// When its state was first heard or last changed.
    pub changed: f64,
    pub heard: f64,
    pub channel: i64,
    pub level_db: f64,
}

/// Each code's signal aspect name where public material gives one, else null.
fn aspects(codes: &[i64]) -> Vec<Value> {
    codes.iter().map(|&c| json!(u8::try_from(c).ok().and_then(railspotting_itc::msg::signal_aspect))).collect()
}

/// Where ITC records go while upload is on: the agent's ITC outbox.
pub struct ItcUpload {
    /// Upload is on (`/config` `itc.upload`). Off, no records are made.
    pub on: AtomicBool,
    ns: uuid::Uuid,
    add: Box<dyn Fn(Value) + Send + Sync>,
}

impl ItcUpload {
    /// Record IDs are UUID v5 under `ns`; `add` takes each record.
    pub fn new(ns: uuid::Uuid, add: Box<dyn Fn(Value) + Send + Sync>) -> Arc<ItcUpload> {
        Arc::new(ItcUpload { on: AtomicBool::new(false), ns, add })
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }
}

/// A base station heard here.
#[derive(Clone, Debug, PartialEq)]
pub struct Base {
    pub local_channel: i64,
    /// Its beacon's peak, dB relative to full scale.
    pub level_db: f64,
    /// The beacon's utilisation byte.
    pub util: Option<i64>,
    pub last: f64,
}

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
    pub bases: BTreeMap<String, Base>,
    pub last_t: f64,
    pub last_error: Option<String>,
    /// Set by the agent: where records go when upload is on.
    pub upload: Option<Arc<ItcUpload>>,
    /// Made when upload turns on, so every unit starts with a starting state; dropped when off.
    tracker: Option<Tracker>,
    /// The last minute's messages (time, type, wayside unit) and failed frames, for the heartbeat.
    recent: VecDeque<(f64, String, Option<String>)>,
    crc_fails: VecDeque<f64>,
    /// Always on, for `units`: the same rules as the upload records.
    view: Tracker,
    units: BTreeMap<String, UnitView>,
    changes: VecDeque<Value>,
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
            upload: None,
            tracker: None,
            recent: VecDeque::new(),
            crc_fails: VecDeque::new(),
            view: Tracker::new(uuid::Uuid::nil()),
            units: BTreeMap::new(),
            changes: VecDeque::new(),
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
            // with the failed frames too, only to count them (a weak antenna shows in the heartbeat)
            for m in decode_burst(&b, true) {
                if m["crc_ok"] == true {
                    msgs.push(m);
                } else {
                    self.crc_fails.push_back(m["t"].as_f64().unwrap_or(t0));
                }
            }
        }
        self.handle(&msgs, t0, center_mhz, rate);
    }

    /// Decoded messages (`messages.md`) from a window at `center_mhz` and `rate`: logged, fed,
    /// counted, and turned into records while upload is on.
    pub fn handle(&mut self, msgs: &[Value], t0: f64, center_mhz: f64, rate: f64) {
        let upload = self.upload.clone().filter(|u| u.is_on());
        match &upload {
            Some(u) if self.tracker.is_none() => self.tracker = Some(Tracker::new(u.ns)),
            None => self.tracker = None,
            _ => {}
        }
        let mut add = vec![];
        for m in msgs {
            self.messages += 1;
            let kind = m["type"].as_str().unwrap_or("OTHER").to_string();
            *self.by_type.entry(kind.clone()).or_default() += 1;
            let t = m["t"].as_f64().unwrap_or(t0);
            self.last_t = self.last_t.max(t);
            self.recent.push_back((t, kind, m["wiu"].as_str().map(str::to_ascii_lowercase)));
            self.see(m, t);
            if let (Some(tr), Some(u)) = (self.tracker.as_mut(), &upload) {
                if let Some(r) = tr.add(m) {
                    (u.add)(r);
                }
            }
            if let Some((radio, b)) = beacon_of(m) {
                if !self.bases.contains_key(&radio) {
                    log::info!("ITC: base station {radio} heard, local channel {}", b.local);
                }
                let level_db = b.level + m["floor_db"].as_f64().unwrap_or(0.0);
                self.bases.insert(radio, Base { local_channel: b.local, level_db, util: m["util"].as_i64(), last: t });
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
        let since = self.last_t - SUMMARY_S;
        while self.recent.front().is_some_and(|r| r.0 < since) {
            self.recent.pop_front();
        }
        while self.crc_fails.front().is_some_and(|t| *t < since) {
            self.crc_fails.pop_front();
        }
    }

    /// Keep `units` up to date with one message heard at `t`.
    fn see(&mut self, m: &Value, t: f64) {
        if let Some(u) = m["wiu"].as_str().and_then(|w| self.units.get_mut(&w.to_ascii_lowercase())) {
            u.heard = u.heard.max(t);
        }
        let Some(r) = self.view.add(m).filter(|r| r["type"] == "wayside") else { return };
        let Some(wiu) = r["wiu"].as_str().map(str::to_string) else { return };
        let codes: Vec<i64> = r["codes"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
        let previous: Option<Vec<i64>> = r["previous_codes"].as_array().map(|a| a.iter().filter_map(Value::as_i64).collect());
        let rt = r["t"].as_f64().unwrap_or(t);
        if previous.is_some() {
            self.changes.push_front(json!({"wiu": wiu, "t": rt, "codes": codes, "aspects": aspects(&codes),
                                           "previous_codes": previous, "channel": r["channel"]}));
            self.changes.truncate(CHANGES_KEPT);
        }
        self.units.insert(wiu, UnitView { codes, previous, changed: rt, heard: rt, channel: r["channel"].as_i64().unwrap_or(0),
                                          level_db: r["level_db"].as_f64().unwrap_or(0.0) });
        self.units.retain(|_, u| rt - u.heard <= UNITS_KEEP_S);
    }

    /// Each wayside unit's state as last heard (latest change first) and the latest changes, at
    /// `now`. For the owner's own screens only; see the module notes.
    pub fn units(&self, now: f64) -> Value {
        let mut units: Vec<(&String, &UnitView)> = self.units.iter().filter(|(_, u)| now - u.heard <= UNITS_KEEP_S).collect();
        units.sort_by(|a, b| b.1.changed.total_cmp(&a.1.changed).then(a.0.cmp(b.0)));
        let ago = |t: f64| (now - t).max(0.0).round();
        json!({
            "units": units.iter().take(UNITS_SHOWN).map(|(w, u)| json!({
                "wiu": w, "codes": u.codes, "aspects": aspects(&u.codes), "previous_codes": u.previous,
                "changed_s": ago(u.changed), "heard_s": ago(u.heard), "channel": u.channel, "level_db": u.level_db,
            })).collect::<Vec<_>>(),
            "changes": self.changes.iter().map(|c| {
                let mut c = c.clone();
                c["ago_s"] = json!(ago(c["t"].as_f64().unwrap_or(now)));
                c
            }).collect::<Vec<_>>(),
        })
    }

    /// The heartbeat's `itc` summary at `now` (contract v1.18, section 5), without `outbox`.
    pub fn summary(&self, now: f64) -> Value {
        let since = now - SUMMARY_S;
        let recent: Vec<&(f64, String, Option<String>)> = self.recent.iter().filter(|r| r.0 >= since).collect();
        let mut messages: BTreeMap<&str, u64> = ["WIU", "WIU_REPLY", "BEACON", "TIME", "CNTL", "other"].iter().map(|k| (*k, 0)).collect();
        for (_, kind, _) in &recent {
            let k = if messages.contains_key(kind.as_str()) && kind != "other" { kind.as_str() } else { "other" };
            *messages.entry(k).or_default() += 1;
        }
        let units: BTreeSet<&String> = recent.iter().filter_map(|r| r.2.as_ref()).collect();
        let bases: Vec<Value> = self.bases.iter().filter(|(_, b)| now - b.last <= BASES_S).map(|(r, b)| json!({
            "base": r.to_ascii_lowercase(), "channel": b.local_channel,
            // the byte is read as a percentage; larger values are not one
            "utilisation_pct": b.util.filter(|u| (0..=100).contains(u)),
            "level_db": (b.level_db.clamp(-150.0, 20.0) * 10.0).round() / 10.0,
            "last_s": (now - b.last).max(0.0).round(),
        })).collect();
        json!({
            "channels": self.watched,
            "bases": bases,
            "units_heard": units.len(),
            "messages": messages,
            "crc_fail": self.crc_fails.iter().filter(|t| **t >= since).count(),
        })
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
        let bases: Vec<Value> = self.bases.iter().map(|(r, b)| json!({ "radio": r, "local_channel": b.local_channel, "level_db": b.level_db, "last": b.last })).collect();
        json!({
            "channels": self.watched,
            "messages": self.messages,
            "by_type": self.by_type,
            "bases": bases,
            "last_message": if self.last_t > 0.0 { json!(self.last_t) } else { Value::Null },
            "feed": if self.feed.is_some() { json!(self.serve) } else { Value::Null },
            "log_dir": self.dir.display().to_string(),
            "error": self.last_error,
            "upload": self.upload.as_ref().is_some_and(|u| u.is_on()),
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
        assert_eq!(r.bases.get("123456").map(|b| b.local_channel), Some(114));
        assert_eq!(r.watched, vec![NATIONAL_CH, 114]);
        let logged = std::fs::read_to_string(dir.join("itc").join("2026-10-23.jsonl")).unwrap();
        assert!(logged.contains("\"type\":\"BEACON\""), "{logged}");
        assert_eq!(r.status()["channels"], json!([102, 114]));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn wiu(t: f64, codes: &[i64]) -> Value {
        json!({"type": "WIU", "wiu": "1234560ff0", "t": t, "ch": 114, "codes": codes, "crc_ok": true, "level_db": 20.0, "floor_db": -80.0})
    }

    #[test]
    fn units_show_each_signal_as_last_heard_without_upload() {
        let dir = std::env::temp_dir().join(format!("rs-itc-units-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut r = ItcRx::new(&json!({"serve": ""}), &dir);
        let reply = |t: f64, codes: &[i64]| {
            let mut m = wiu(t, codes);
            m["type"] = json!("WIU_REPLY");
            m
        };
        r.handle(&[wiu(1000.0, &[3, 15]), wiu(1004.0, &[3, 15]), wiu(1008.0, &[8, 15]), reply(1009.0, &[3, 15]), wiu(1012.0, &[8, 15])],
                 1000.0, 220.3, 1.024e6);
        let v = r.units(1020.0);
        let u = &v["units"][0];
        assert_eq!(u["wiu"], "1234560ff0");
        assert_eq!(u["codes"], json!([8, 15]), "a late reply does not flip it back");
        assert_eq!(u["aspects"], json!(["Approach", null]));
        assert_eq!(u["previous_codes"], json!([3, 15]));
        assert_eq!((u["changed_s"].as_f64(), u["heard_s"].as_f64()), (Some(12.0), Some(8.0)));
        assert_eq!(u["channel"], 114);
        assert_eq!(v["changes"].as_array().unwrap().len(), 1, "the starting state is not a change");
        assert_eq!(v["changes"][0]["codes"], json!([8, 15]));
        assert_eq!(v["changes"][0]["ago_s"], 12.0);
        // not in the status, which the open status page shows
        assert!(r.status().get("units").is_none());
        // a unit not heard for an hour is forgotten
        assert!(r.units(1012.0 + UNITS_KEEP_S + 1.0)["units"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_are_made_only_while_upload_is_on() {
        let dir = std::env::temp_dir().join(format!("rs-itc-upload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut r = ItcRx::new(&json!({"serve": ""}), &dir);
        let got = Arc::new(std::sync::Mutex::new(vec![]));
        let sink = got.clone();
        let up = ItcUpload::new(uuid::Uuid::nil(), Box::new(move |v| sink.lock().unwrap().push(v)));
        r.upload = Some(up.clone());
        r.handle(&[wiu(1000.0, &[3])], 1000.0, 220.3, 1.024e6);
        assert!(got.lock().unwrap().is_empty(), "off: nothing");
        up.on.store(true, Ordering::SeqCst);
        // on: a starting state for a unit already heard, then only changes
        r.handle(&[wiu(1004.0, &[3]), wiu(1008.0, &[3]), wiu(1012.0, &[8])], 1004.0, 220.3, 1.024e6);
        let recs = got.lock().unwrap().clone();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0]["first"], true);
        assert_eq!(recs[1]["previous_codes"], json!([3]));
        assert_eq!(r.status()["upload"], true);
        // off and on again: a starting state again
        up.on.store(false, Ordering::SeqCst);
        r.handle(&[wiu(1016.0, &[8])], 1016.0, 220.3, 1.024e6);
        up.on.store(true, Ordering::SeqCst);
        r.handle(&[wiu(1020.0, &[8])], 1020.0, 220.3, 1.024e6);
        let recs = got.lock().unwrap().clone();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[2]["first"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_counts_the_last_minute() {
        let dir = std::env::temp_dir().join(format!("rs-itc-summary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut r = ItcRx::new(&json!({"serve": ""}), &dir);
        let beacon = json!({"type": "BEACON", "radio": "123456", "t": 1000.0, "ch": 102, "channel": 114, "channels": [],
                            "util": 37, "level_db": 18.8, "floor_db": -80.0, "crc_ok": true});
        let time = json!({"type": "TIME", "radio": "123456", "t": 1001.0, "ch": 102, "crc_ok": true});
        let ack = json!({"type": "ACK", "radio": "123456", "t": 1002.0, "ch": 114, "crc_ok": true});
        r.handle(&[beacon, time, ack, wiu(1003.0, &[3]), wiu(1004.0, &[3])], 1000.0, 220.3, 1.024e6);
        let s = r.summary(1010.0);
        assert_eq!(s["units_heard"], 1);
        assert_eq!(s["messages"], json!({"BEACON": 1, "CNTL": 0, "TIME": 1, "WIU": 2, "WIU_REPLY": 0, "other": 1}));
        assert_eq!(s["bases"], json!([{"base": "123456", "channel": 114, "utilisation_pct": 37, "level_db": -61.2, "last_s": 10.0}]));
        assert_eq!(s["crc_fail"], 0);
        // a minute later the counts are empty; the base stays listed for 5 minutes
        let later = r.summary(1070.0);
        assert_eq!(later["units_heard"], 0);
        assert_eq!(later["messages"]["WIU"], 0);
        assert_eq!(later["bases"].as_array().unwrap().len(), 1);
        assert!(r.summary(1400.0)["bases"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
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
