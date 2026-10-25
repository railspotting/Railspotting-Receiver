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

/// `railspotting-receiver itc ...`; the exit code.
pub fn cmd_itc(action: &ItcCmd, out: &mut dyn Write) -> i32 {
    let r = match action {
        ItcCmd::Survey { src, probe_seconds, keep, json } => run_survey(src, *probe_seconds, keep.as_ref(), *json, out),
        ItcCmd::Decode { src, channels, auto, probe_seconds, all, log, serve } => {
            decode(src, channels, *auto, *probe_seconds, *all, log.as_ref(), serve.as_deref(), out)
        }
        ItcCmd::Record { src, out: path } => record(src, path),
        ItcCmd::Bursts { src, channels } => bursts(src, channels, out),
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
