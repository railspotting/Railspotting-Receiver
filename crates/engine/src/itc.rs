//! `railspotting-receiver itc ...`: ITC (220 MHz PTC) from the command line, for setting up a
//! receiver and for working on recordings. The running receiver decodes ITC on its own when a
//! dongle has the `itc` role; these commands use a dongle directly, so stop the service first
//! (or use a dongle it does not hold).
//!
//! ```text
//! railspotting-receiver itc survey                  # at setup: what is heard here
//! railspotting-receiver itc decode --auto           # JSON lines on stdout
//! railspotting-receiver itc record --out a.cu8      # raw samples, to decode later
//! railspotting-receiver itc decode --input a.cu8
//! railspotting-receiver itc place                   # where the units heard are (reads the logs)
//! ```

use crate::device::Device;
use crate::rtl::RtlDevice;
use clap::{Args, Subcommand};
use crossbeam_channel::{bounded, RecvTimeoutError};
use num_complex::Complex32;
use railspotting_dsp::sdr::block_samples;
use railspotting_itc::decode::{burst_json, decode_burst, Decoder};
use railspotting_itc::dsp::{channel_hz, plan_capture};
use railspotting_itc::feed::Feed;
use railspotting_itc::place;
use railspotting_itc::recording::{File, Meta, Recorder, Source};
use railspotting_itc::survey::{self, beacon_of, Beacon, Survey, NATIONAL_CH};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// A live dongle that sends nothing for this long is reopened.
const STALL: Duration = Duration::from_secs(5);

#[derive(Subcommand, Debug)]
pub enum ItcCmd {
    /// at setup: listen for base station beacons, record the whole ITC band, decode every
    /// channel, and say what is heard here and which settings to use
    Survey {
        #[command(flatten)]
        src: ItcSrc,
        /// seconds to listen for beacons on the national channel first
        #[arg(long, default_value_t = 60.0)]
        probe_seconds: f64,
        /// keep the whole-band recording here (.cu8 and .json) instead of deleting it
        #[arg(long)]
        keep: Option<PathBuf>,
        /// the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// decoded messages, one JSON line each (fields: crates/itc/messages.md)
    Decode {
        #[command(flatten)]
        src: ItcSrc,
        /// only these channels (e.g. 220.1375M,220.4375M); default: every one in the window
        #[arg(long, value_delimiter = ',', value_parser = hz)]
        channels: Vec<f64>,
        /// find the channels first: beacons on the national channel name each base's channel
        #[arg(long, conflicts_with = "input")]
        auto: bool,
        #[arg(long, default_value_t = 60.0)]
        probe_seconds: f64,
        /// also frames that failed the CRC
        #[arg(long)]
        all: bool,
        /// also append the messages to this file
        #[arg(long)]
        log: Option<PathBuf>,
        /// serve the messages as a feed for a monitor (e.g. 127.0.0.1:8221)
        #[arg(long)]
        serve: Option<String>,
    },
    /// save raw samples (.cu8 and a .json sidecar) to decode later
    Record {
        #[command(flatten)]
        src: ItcSrc,
        #[arg(long)]
        out: PathBuf,
    },
    /// one JSON line per burst: channel, time, length, level
    Bursts {
        #[command(flatten)]
        src: ItcSrc,
        #[arg(long, value_delimiter = ',', value_parser = hz)]
        channels: Vec<f64>,
    },
    /// where the wayside units heard here are: their order from train timing, their distance
    /// from a defect detector, matched to OpenStreetMap signals; proposes positions to the
    /// headend (reads the running receiver's logs, not a dongle)
    Place {
        /// days of state changes to use (the receiver keeps 7)
        #[arg(long, default_value_t = 7.0)]
        days: f64,
        /// OpenStreetMap data from this file (an Overpass answer) instead of fetching it
        #[arg(long)]
        osm: Option<PathBuf>,
        /// a setting for this run, over receiver.json's "itc_place" (e.g. --set speed_mph=[30,60]
        /// or --set detector_milepost=123.4); repeat for more
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// queue the proposals for the headend; the running receiver uploads them while ITC
        /// upload is on
        #[arg(long)]
        send: bool,
    },
}

impl ItcCmd {
    /// Commands that read the running receiver's files, so need its settings.
    pub fn needs_settings(&self) -> bool {
        matches!(self, ItcCmd::Place { .. })
    }
}

#[derive(Args, Debug, Clone)]
pub struct ItcSrc {
    /// a recording (.cu8 with its .json) instead of a dongle
    #[arg(long)]
    input: Option<PathBuf>,
    /// the dongle's serial (see sdr-list); may be left out when only one is plugged in
    #[arg(long)]
    serial: Option<String>,
    /// centre frequency, e.g. 220.3M
    #[arg(long, default_value = "220.3M", value_parser = hz)]
    freq: f64,
    /// sample rate: 1.024M, 2.048M or 2.4M
    #[arg(long, default_value = "1.024M", value_parser = hz)]
    rate: f64,
    /// tuner gain in dB, or "auto"
    #[arg(long, default_value = "40")]
    gain: String,
    /// stop after this many seconds
    #[arg(long)]
    seconds: Option<f64>,
    /// a burst opens this many dB above its channel's noise floor
    #[arg(long, default_value_t = 8.0)]
    open_db: f32,
}

/// "220.1375M", "1.024M", "25k" or plain Hz.
fn hz(s: &str) -> Result<f64, String> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('M' | 'm') => (&s[..s.len() - 1], 1e6),
        Some('k' | 'K') => (&s[..s.len() - 1], 1e3),
        _ => (s, 1.0),
    };
    num.parse::<f64>().map(|v| (v * mul).round()).map_err(|e| format!("{s}: {e}"))
}

fn gain_value(g: &str) -> Value {
    if g == "auto" { json!("auto") } else { g.parse::<f64>().map(|x| json!(x)).unwrap_or(json!(g)) }
}

/// The dongle to use: the one named, or the only one plugged in.
fn pick_serial(given: &Option<String>) -> Result<String, String> {
    if let Some(s) = given {
        return Ok(s.clone());
    }
    let devs = crate::rtl::list_devices().map_err(|e| format!("cannot load the RTL-SDR driver: {e}"))?;
    let serials: Vec<String> = devs.iter().filter_map(|d| d.get("serial").and_then(Value::as_str).map(String::from)).collect();
    match serials.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err("no RTL-SDR dongle is plugged in".into()),
        many => Err(format!("more than one dongle is plugged in ({}): choose one with --serial", many.join(", "))),
    }
}

/// Blocks of samples from the source, each with the time of its first sample, until it ends,
/// `seconds` pass or `f` fails. A live dongle that stalls is reopened.
fn each_block(src: &ItcSrc, freq: f64, rate: f64, seconds: Option<f64>, mut f: impl FnMut(&[Complex32], f64) -> Result<(), String>) -> Result<Meta, String> {
    if let Some(p) = &src.input {
        let mut file = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let meta = file.meta().clone();
        let block = (meta.rate * 0.05) as usize;
        let limit = seconds.map(|s| (s * meta.rate) as u64);
        let mut done = 0u64;
        let mut buf = Vec::with_capacity(block);
        loop {
            buf.clear();
            let n = file.read(&mut buf, block).map_err(|e| e.to_string())?;
            if n == 0 || limit.is_some_and(|l| done >= l) {
                return Ok(meta);
            }
            f(&buf, meta.start + done as f64 / meta.rate)?;
            done += n as u64;
        }
    }
    let serial = pick_serial(&src.serial)?;
    let gain = gain_value(&src.gain);
    let meta = Meta { center_hz: freq, rate, start: railspotting_common::now(), format: "cu8".into(), gain_db: gain.as_f64() };
    let end = seconds.map(|s| meta.start + s);
    let mut done = 0u64;
    loop {
        let dev = Arc::new(RtlDevice::open(&serial)?);
        dev.configure(rate, freq, &gain)?;
        let (tx, rx) = bounded::<Vec<Complex32>>(64);
        let reader = dev.clone();
        let block = block_samples(rate);
        let t = std::thread::spawn(move || reader.read_samples_async(&mut |s| { let _ = tx.try_send(s); }, block));
        let outcome = loop {
            match rx.recv_timeout(STALL) {
                Ok(b) => {
                    let t0 = meta.start + done as f64 / rate;
                    done += b.len() as u64;
                    if let Err(e) = f(&b, t0) {
                        break Err(e);
                    }
                    if end.is_some_and(|e| railspotting_common::now() >= e) {
                        break Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("itc: no samples for {} s from {serial}; reopening it", STALL.as_secs());
                    break Err(String::new());
                }
                Err(RecvTimeoutError::Disconnected) => break Err(String::new()),
            }
        };
        dev.cancel_read_async();
        let _ = t.join();
        dev.close();
        match outcome {
            Ok(()) => return Ok(meta),
            Err(e) if !e.is_empty() => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_secs(5)),
        }
    }
}

/// Listen to the national channel for `seconds` and collect the beacons heard.
fn probe(src: &ItcSrc, seconds: f64) -> Result<BTreeMap<String, Beacon>, String> {
    let (c, r) = plan_capture(&[NATIONAL_CH]).ok_or("cannot tune to the national channel")?;
    eprintln!("itc: listening for base station beacons on channel {NATIONAL_CH} for {seconds:.0} s");
    let mut dec: Option<Decoder> = None;
    let mut bases = BTreeMap::new();
    each_block(src, c, r, Some(seconds), |b, t0| {
        let d = match dec.as_mut() {
            Some(d) => d,
            None => dec.insert(Decoder::new(c, r, t0, &[channel_hz(NATIONAL_CH)], src.open_db)?),
        };
        for burst in d.bursts(b) {
            for m in decode_burst(&burst, false) {
                if let Some((radio, beacon)) = beacon_of(&m) {
                    bases.insert(radio, beacon);
                }
            }
        }
        Ok(())
    })?;
    Ok(bases)
}

#[allow(clippy::too_many_arguments)]
fn decode(src: &ItcSrc, channels: &[f64], auto: bool, probe_seconds: f64, all: bool, log: Option<&PathBuf>, serve: Option<&str>, out: &mut dyn Write) -> Result<(), String> {
    let (mut freq, mut rate, mut chans) = (src.freq, src.rate, channels.to_vec());
    // a recording was tuned where its sidecar says
    if let Some(p) = &src.input {
        let m = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?.meta().clone();
        (freq, rate) = (m.center_hz, m.rate);
    }
    if auto {
        let bases = probe(src, probe_seconds)?;
        let mut want = vec![NATIONAL_CH];
        for (radio, b) in &bases {
            eprintln!("itc: base {radio}: local channel {} ({:.4} MHz), {:.0} dB; its neighbours: {:?}", b.local, channel_hz(b.local) / 1e6, b.level, b.near);
            if !want.contains(&b.local) {
                want.push(b.local);
            }
        }
        want.sort();
        match plan_capture(&want) {
            Some((c, r)) if !bases.is_empty() => {
                eprintln!("itc: decoding channels {want:?} at {:.4} MHz, {:.3} MS/s", c / 1e6, r / 1e6);
                (freq, rate, chans) = (c, r, want.iter().map(|&n| channel_hz(n)).collect());
            }
            _ => eprintln!("itc: no beacon heard (or the channels are too far apart); keeping {:.4} MHz", freq / 1e6),
        }
    }
    let mut logf = match log {
        Some(p) => Some(std::io::BufWriter::new(std::fs::OpenOptions::new().create(true).append(true).open(p).map_err(|e| format!("{}: {e}", p.display()))?)),
        None => None,
    };
    let feed = match serve {
        Some(a) => Some(Feed::serve(a).map_err(|e| format!("{a}: {e}"))?),
        None => None,
    };
    if let Some(f) = &feed {
        eprintln!("itc: feed on {}", f.addr);
    }
    let mut dec: Option<Decoder> = None;
    each_block(src, freq, rate, src.seconds, |b, t0| {
        let d = match dec.as_mut() {
            Some(d) => d,
            None => {
                let d = dec.insert(Decoder::new(freq, rate, t0, &chans, src.open_db)?);
                eprintln!("itc: channels {:?} MHz", d.channels_mhz());
                d
            }
        };
        for burst in d.bursts(b) {
            for m in decode_burst(&burst, all) {
                let line = m.to_string();
                writeln!(out, "{line}").map_err(|e| e.to_string())?;
                if let Some(f) = logf.as_mut() {
                    writeln!(f, "{line}").and_then(|_| f.flush()).map_err(|e| e.to_string())?;
                }
                if let Some(f) = &feed {
                    f.send(&line);
                }
            }
        }
        out.flush().map_err(|e| e.to_string())
    })?;
    Ok(())
}

fn run_survey(src: &ItcSrc, probe_seconds: f64, keep: Option<&PathBuf>, as_json: bool, out: &mut dyn Write) -> Result<(), String> {
    let live = src.input.is_none();
    let seconds = src.seconds.unwrap_or(survey::DEFAULT_SECONDS);
    let mut tally = Survey::default();
    if live {
        tally.bases = probe(src, probe_seconds)?;
    }
    let temp = std::env::temp_dir().join(format!("itc-survey-{}.cu8", std::process::id()));
    let path = match (&src.input, keep) {
        (Some(p), _) => p.clone(),
        (None, Some(k)) => k.clone(),
        (None, None) => temp.clone(),
    };
    if live {
        eprintln!("itc: recording the whole band for {seconds:.0} s");
        let mut rec: Option<Recorder> = None;
        let meta = each_block(src, survey::BAND_CENTER_HZ, survey::BAND_RATE, Some(seconds), |b, _| {
            let r = match rec.as_mut() {
                Some(r) => r,
                None => {
                    let m = Meta { center_hz: survey::BAND_CENTER_HZ, rate: survey::BAND_RATE, start: railspotting_common::now(), format: "cu8".into(), gain_db: gain_value(&src.gain).as_f64() };
                    rec.insert(Recorder::create(&path, &m).map_err(|e| format!("{}: {e}", path.display()))?)
                }
            };
            r.write(b).map_err(|e| e.to_string())
        })?;
        let _ = meta;
        if let Some(r) = rec {
            r.finish().map_err(|e| e.to_string())?;
        }
    }
    let clip = survey::clip_fraction(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    eprintln!("itc: decoding every channel (this takes a few times as long as the recording)");
    let replay = ItcSrc { input: Some(path.clone()), seconds: None, ..src.clone() };
    let mut dec: Option<Decoder> = None;
    let meta = each_block(&replay, 0.0, 0.0, None, |b, t0| {
        let d = match dec.as_mut() {
            Some(d) => d,
            None => {
                let m = File::open(&path).map_err(|e| e.to_string())?.meta().clone();
                dec.insert(Decoder::new(m.center_hz, m.rate, t0, &[], src.open_db)?)
            }
        };
        for burst in d.bursts(b) {
            let msgs = decode_burst(&burst, false);
            tally.add(&burst, &msgs);
        }
        Ok(())
    });
    if live && keep.is_none() {
        let _ = std::fs::remove_file(&temp);
        let _ = std::fs::remove_file(railspotting_itc::recording::sidecar(&temp));
    }
    let meta = meta?;
    let report = tally.report(clip, meta.gain_db, seconds, live);
    if as_json {
        writeln!(out, "{report}").map_err(|e| e.to_string())
    } else {
        survey::write_text(&report, out).map_err(|e| e.to_string())
    }
}

fn record(src: &ItcSrc, path: &std::path::Path) -> Result<(), String> {
    let mut rec: Option<Recorder> = None;
    let secs = src.seconds.or(Some(60.0));
    each_block(src, src.freq, src.rate, secs, |b, t0| {
        let r = match rec.as_mut() {
            Some(r) => r,
            None => {
                let m = Meta { center_hz: src.freq, rate: src.rate, start: t0, format: "cu8".into(), gain_db: gain_value(&src.gain).as_f64() };
                rec.insert(Recorder::create(path, &m).map_err(|e| format!("{}: {e}", path.display()))?)
            }
        };
        r.write(b).map_err(|e| e.to_string())
    })?;
    match rec {
        Some(r) => r.finish().map_err(|e| e.to_string()),
        None => Err("no samples recorded".into()),
    }
}

fn bursts(src: &ItcSrc, channels: &[f64], out: &mut dyn Write) -> Result<(), String> {
    let (mut freq, mut rate) = (src.freq, src.rate);
    if let Some(p) = &src.input {
        let m = File::open(p).map_err(|e| format!("{}: {e}", p.display()))?.meta().clone();
        (freq, rate) = (m.center_hz, m.rate);
    }
    let mut dec: Option<Decoder> = None;
    each_block(src, freq, rate, src.seconds, |b, t0| {
        let d = match dec.as_mut() {
            Some(d) => d,
            None => dec.insert(Decoder::new(freq, rate, t0, channels, src.open_db)?),
        };
        for burst in d.bursts(b) {
            writeln!(out, "{}", burst_json(&burst)).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .map(|_| ())
}

// placing wayside units -----------------------------------------------------------------------

/// Overpass, for OpenStreetMap signals and milepost markers (as the headend's signal fetch).
const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";
/// Mapped signals are fetched again when the saved copy is older than this.
const OSM_KEEP_S: f64 = 7.0 * 86400.0;
/// This many units starting again within this many seconds is a restart: no reception before.
const RESTART_UNITS: usize = 3;
const RESTART_S: f64 = 10.0;

/// The state changes kept in `state/itc/changes/`, from `since` on, and the spans with no
/// reception before each restart of the receiver (many units starting again at once).
fn read_changes(dir: &std::path::Path, since: f64) -> (Vec<place::Change>, Vec<(f64, f64)>) {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
    files.sort();
    let (mut out, mut firsts) = (vec![], vec![]);
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in text.lines() {
            // a line cut short by a stop is skipped
            let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
            let (Some(t), Some(w)) = (r["t"].as_f64(), r["wiu"].as_str()) else { continue };
            if t < since || r["type"] != "wayside" {
                continue;
            }
            if r["first"] == true {
                firsts.push(t);
            }
            let codes = r["codes"].as_array().into_iter().flatten().filter_map(Value::as_i64).collect();
            out.push(place::Change { t, wiu: w.to_string(), codes });
        }
    }
    firsts.sort_by(f64::total_cmp);
    let mut gaps: Vec<(f64, f64)> = vec![];
    for w in firsts.windows(RESTART_UNITS) {
        if w[RESTART_UNITS - 1] - w[0] <= RESTART_S && gaps.last().is_none_or(|g| w[0] - g.1 > 60.0) {
            gaps.push((w[0] - 60.0, w[0]));
        }
    }
    (out, gaps)
}

/// Defect detector announcements in the event history since `since`, repeats left out.
fn read_passes(history: &std::path::Path, since: f64) -> Result<Vec<place::Pass>, String> {
    let db = rusqlite::Connection::open_with_flags(history, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", history.display()))?;
    let mut q = db.prepare("select start_ts, payload from events where kind = 'voice' and start_ts >= ?").map_err(|e| e.to_string())?;
    let rows = q.query_map([since], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
    let mut out = vec![];
    for (t, text) in rows.flatten() {
        let Ok(ev) = serde_json::from_str::<Value>(&text) else { continue };
        let d = &ev["detector"];
        if let Some(mp) = d["milepost"].as_f64().filter(|_| d["repeat_of"].is_null()) {
            out.push(place::Pass { t, milepost: mp, railroad: d["railroad"].as_str().map(String::from) });
        }
    }
    Ok(out)
}

/// Where the base stations heard are, from their beacons in the newest daily logs.
fn read_bases(dir: &std::path::Path) -> Vec<(String, f64, f64)> {
    use std::io::BufRead;
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect();
    files.sort();
    let mut bases: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for f in files.iter().rev().take(2) {
        let Ok(file) = std::fs::File::open(f) else { continue };
        // beacons come every few seconds: the start of a day's log names every base
        for line in std::io::BufReader::new(file).lines().take(200_000).map_while(Result::ok) {
            if !line.contains("\"BEACON\"") {
                continue;
            }
            let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
            if let (Some(r), Some(lat), Some(lon)) = (m["radio"].as_str(), m["lat"].as_f64(), m["lon"].as_f64()) {
                bases.insert(r.to_string(), (lat, lon));
            }
        }
    }
    bases.into_iter().map(|(r, (a, b))| (r, a, b)).collect()
}

/// An Overpass answer to `query`.
fn fetch_osm(query: &str) -> Result<Value, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(concat!("railspotting-receiver/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(180)))
        .build()
        .into();
    let mut resp = agent.post(OVERPASS_URL).send_form([("data", query)]).map_err(|e| format!("{OVERPASS_URL}: {e}"))?;
    resp.body_mut().with_config().limit(64 * 1024 * 1024).read_json::<Value>().map_err(|e| format!("{OVERPASS_URL}: {e}"))
}

/// The placement settings: receiver.json's `itc_place`, then each `--set key=value`.
fn place_params(settings: &railspotting_common::Settings, set: &[String]) -> Result<place::Params, String> {
    let mut p = place::Params::from_json(settings.get("itc_place"))?;
    for kv in set {
        let (k, v) = kv.split_once('=').ok_or_else(|| format!("--set {kv}: KEY=VALUE"))?;
        // a value that isn't JSON is a string: --set railroad=NS
        let v: Value = serde_json::from_str(v.trim()).unwrap_or_else(|_| json!(v.trim()));
        p.set(k.trim(), &v)?;
    }
    Ok(p)
}

fn cmd_place(settings: &railspotting_common::Settings, days: f64, osm: Option<&PathBuf>, set: &[String], send: bool,
             out: &mut dyn Write) -> Result<(), String> {
    let p = place_params(settings, set)?;
    let state = settings.state_dir();
    let itc_dir = state.join("itc");
    let now = railspotting_common::now();
    let since = now - days * 86400.0;
    let changed = p.changed();
    if changed.as_object().is_some_and(|m| !m.is_empty()) {
        let _ = writeln!(out, "settings: {changed}");
    }
    let (changes, gaps) = read_changes(&itc_dir.join("changes"), since);
    if changes.is_empty() {
        return Err(format!("no wayside state changes in {} (does a dongle have the itc role?)", itc_dir.join("changes").display()));
    }
    let passes = read_passes(&state.join("history.db"), since)?;
    let (mut lay, drops) = place::layout(&changes, &gaps, &p);
    place::anchor(&mut lay, &drops, &passes, &p);
    let units = lay.locations.iter().map(|l| l.units.len()).sum::<usize>() + lay.unplaced.len();
    let det_name = |mp: f64, rr: &Option<String>| format!("{} milepost {mp}", rr.as_deref().unwrap_or("detector at"));
    let det = match &lay.detector {
        Some((mp, rr, n)) => format!("; measured from {} ({n} trains)", det_name(*mp, rr)),
        None => "; no detector heard".into(),
    };
    let _ = writeln!(out, "{units} units, {} train passes, {} places along the line{det}", lay.trains, lay.locations.len());
    if !lay.other_detectors.is_empty() {
        let others: Vec<String> = lay.other_detectors.iter().map(|(mp, rr, n)| format!("{} ({n})", det_name(*mp, rr))).collect();
        let _ = writeln!(out, "  other detectors heard: {} (choose one with --set detector_milepost=...)", others.join(", "));
    }
    // timed places by their distance from the detector, which is steadier than the order
    // trains give; then the rest in that order
    let mut shown: Vec<usize> = (0..lay.locations.len()).collect();
    shown.sort_by(|&a, &b| match (lay.locations[a].offset_s, lay.locations[b].offset_s) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.cmp(&b),
    });
    for i in shown {
        let l = &lay.locations[i];
        let off = l.offset_s.map(|o| format!("{o:+.0} s from the detector")).unwrap_or_else(|| "not timed both ways".into());
        let _ = writeln!(out, "  #{:<3} {:<23} {:<13} {off}", i + 1, l.units.join(" "),
                         if l.control_point { "control point" } else { "intermediate" });
    }
    if !lay.unplaced.is_empty() {
        let _ = writeln!(out, "  no train seen passing: {}", lay.unplaced.join(" "));
    }

    let osm_json: Value = match osm {
        Some(f) => serde_json::from_str(&std::fs::read_to_string(f).map_err(|e| format!("{}: {e}", f.display()))?)
            .map_err(|e| format!("{}: {e}", f.display()))?,
        None => {
            let bases = read_bases(&itc_dir);
            if bases.is_empty() {
                return Err("no base station beacon with a position in the ITC logs, so nowhere to look for signals".into());
            }
            let query = place::osm_query(&bases.iter().map(|b| (b.1, b.2)).collect::<Vec<_>>(), p.radius_km * 1000.0, p.track);
            // kept a week with the query it answers, so running again doesn't ask Overpass again
            let saved = itc_dir.join("osm_signals.json");
            let fresh = std::fs::metadata(&saved).and_then(|m| m.modified()).ok()
                .and_then(|t| t.elapsed().ok()).is_some_and(|age| age.as_secs_f64() < OSM_KEEP_S);
            let cached = if fresh {
                std::fs::read_to_string(&saved).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .filter(|v| v["query"] == query.as_str()).map(|v| v["data"].clone())
            } else {
                None
            };
            match cached {
                Some(v) => v,
                None => {
                    let _ = writeln!(out, "fetching OpenStreetMap signals{} within {} km of {} base station{}", if p.track { " and track" } else { "" },
                                     p.radius_km, bases.len(), if bases.len() == 1 { "" } else { "s" });
                    let v = fetch_osm(&query)?;
                    let _ = std::fs::write(&saved, json!({"query": query, "data": v}).to_string());
                    v
                }
            }
        }
    };
    let osm = place::read_osm(&osm_json, &p.milepost_unit);
    let _ = writeln!(out, "OpenStreetMap: {} signals, {} milepost markers, {} stretches of track", osm.signals.len(), osm.stones.len(),
                     osm.ways.len());
    let pl = place::place(&lay, &osm, &p);
    if !pl.note.is_empty() {
        let _ = writeln!(out, "{}", pl.note);
    }
    if pl.proposals.is_empty() {
        let _ = writeln!(out, "nothing to propose");
        return Ok(());
    }
    let ties = if pl.ties > 1 { format!(" ({} fits as good)", pl.ties) } else { String::new() };
    let how = if pl.along_track { "along the mapped track" } else { "along a straight line" };
    let held = if pl.mileposts { "the detector held to its milepost by mapped markers" } else { "no milepost markers fit" };
    let _ = writeln!(out, "best fit: {} places on mapped signals at about {:.0} mph, {how}, {held}{ties}", pl.matched, pl.speed * 2.23694);
    for pr in &pl.proposals {
        let name = pr.name.as_ref().map(|n| format!("  \"{n}\"")).unwrap_or_default();
        let _ = writeln!(out, "  #{:<3} {}  {:.5}, {:.5}  OSM {}{name}", pr.location + 1, pr.wiu, pr.lat, pr.lon,
                         pr.signals.iter().map(i64::to_string).collect::<Vec<_>>().join(","));
    }
    if !send {
        let _ = writeln!(out, "{} proposals; add --send to queue them for the headend", pl.proposals.len());
        return Ok(());
    }
    let cfg = railspotting_common::load_cached_config(&state);
    if cfg.get("itc").and_then(|i| i.get("upload")) != Some(&Value::Bool(true)) {
        return Err("ITC upload is off for this receiver (the owner turns it on on the receiver's page); nothing queued".into());
    }
    let outbox = railspotting_store::ItcOutbox::open(state.join("itc_outbox.db")).map_err(|e| format!("ITC outbox: {e:?}"))?;
    for pr in &pl.proposals {
        outbox.add(&place::record(&crate::agent::ITC_NAMESPACE, pr, now)).map_err(|e| format!("ITC outbox: {e:?}"))?;
    }
    let _ = writeln!(out, "queued {} proposals; the running receiver uploads them (watch: journalctl -u railspotting-receiver -f)",
                     pl.proposals.len());
    Ok(())
}

/// `railspotting-receiver itc ...` for the commands that read the receiver's files.
pub fn cmd_itc_settings(settings: &railspotting_common::Settings, action: &ItcCmd, out: &mut dyn Write) -> i32 {
    let r = match action {
        ItcCmd::Place { days, osm, set, send } => cmd_place(settings, *days, osm.as_ref(), set, *send, out),
        _ => return cmd_itc(action, out),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(out, "itc: {e}");
            1
        }
    }
}

/// `railspotting-receiver itc ...`; the exit code.
pub fn cmd_itc(action: &ItcCmd, out: &mut dyn Write) -> i32 {
    let r = match action {
        ItcCmd::Survey { src, probe_seconds, keep, json } => run_survey(src, *probe_seconds, keep.as_ref(), *json, out),
        ItcCmd::Decode { src, channels, auto, probe_seconds, all, log, serve } => {
            decode(src, channels, *auto, *probe_seconds, *all, log.as_ref(), serve.as_deref(), out)
        }
        ItcCmd::Record { src, out: path } => record(src, path),
        ItcCmd::Bursts { src, channels } => bursts(src, channels, out),
        ItcCmd::Place { .. } => Err("needs the receiver's settings".into()),
    };
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("itc: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequencies_and_gains() {
        assert_eq!(hz("220.1375M"), Ok(220_137_500.0));
        assert_eq!(hz("25k"), Ok(25_000.0));
        assert!(hz("fast").is_err());
        assert_eq!(gain_value("auto"), json!("auto"));
        assert_eq!(gain_value("40.2"), json!(40.2));
    }
}
