//! Scanner checks: radio check, scan setup, reception and detection tests, plan sync.
//!
//! Each takes the `Timing` it runs with (its clock and sleep); the
//! receiver passes `monitor.timing()`.

use crate::focus::FOCUS_CH;
use crate::monitor::ScannerMonitor;
use crate::jsonfmt;
use crate::scanner::{Scanner, ScannerError};
use crate::util::{finally, median, pct0, value_f64, value_i64, round_even, value_str, value_str_opt, round_int, truthy};
use crate::Timing;
use railspotting_common::nominal_noise;
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const MEASURE_S: f64 = 5.0;
pub const SAMPLE_S: f64 = 0.1;
pub const DETECT_TIMEOUT_S: f64 = 30.0;

fn io(e: std::io::Error) -> ScannerError {
    ScannerError::Io(e.to_string())
}

fn val(e: String) -> ScannerError {
    ScannerError::Value(e)
}

fn restore_path(state_dir: &Path) -> PathBuf {
    state_dir.join("scanner_restore.json")
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_json(path: &Path, v: &Value) -> Result<(), ScannerError> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(io)?;
    }
    std::fs::write(path, jsonfmt::dumps_indent(v, 2) + "\n").map_err(io)
}

/// Save original channel contents before borrowing them, so a crash can undo it.
fn remember(state_dir: &Path, s: &mut Scanner, channels: &[i64], scg: bool) -> Result<Value, ScannerError> {
    let path = restore_path(state_dir);
    let mut saved = read_json(&path).filter(|v| v.get("channels").is_some_and(Value::is_object))
        .unwrap_or_else(|| json!({"channels": {}, "scg": null}));
    for n in channels {
        let key = n.to_string();
        if saved["channels"].get(&key).is_none() {
            let mem = s.cin_read(*n)?.to_json();
            saved["channels"][key] = mem;
        }
    }
    if scg && saved.get("scg").is_none_or(Value::is_null) {
        saved["scg"] = json!(s.scg()?);
    }
    write_json(&path, &saved)?;
    Ok(saved)
}

fn write_back(s: &mut Scanner, n: i64, orig: &Value) -> Result<(), ScannerError> {
    if truthy(orig.get("freq")) {
        let delay_on = match orig.get("delay") {
            None => false,
            Some(Value::String(d)) => d != "0",
            Some(_) => true,
        };
        let f = value_f64(orig.get("freq")).map_err(val)?;
        s.cin_write(n, f, delay_on, truthy(orig.get("lockout")))
    } else {
        s.cin_clear(n)
    }
}

/// Put back any channel or bank setting a check borrowed (call inside program mode).
pub fn restore_borrowed(s: &mut Scanner, state_dir: &Path) -> Result<bool, ScannerError> {
    let path = restore_path(state_dir);
    let Some(saved) = read_json(&path) else { return Ok(false) };
    if let Some(chans) = saved.get("channels").and_then(Value::as_object) {
        for (n, orig) in chans {
            let n = crate::util::parse_int(n).map_err(val)?;
            write_back(s, n, orig)?;
        }
    }
    if let Some(scg) = saved.get("scg").filter(|v| truthy(Some(v))) {
        s.set_scg(&value_str(scg))?;
    }
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(io(e)),
        _ => {}
    }
    Ok(true)
}

/// On connect: restore channel 1 (and anything else) a check left borrowed.
pub fn recover_after_crash(monitor: &ScannerMonitor, state_dir: &Path) -> Result<bool, ScannerError> {
    if !restore_path(state_dir).exists() {
        return Ok(false);
    }
    monitor.borrow("restore", |s| {
        s.prg()?;
        let r = restore_borrowed(s, state_dir);
        finally(r, s.epg(true))
    })?;
    log::info!("restored scanner memory borrowed by an interrupted check");
    Ok(true)
}

/// Median RSSI and how often squelch was open over a listening window.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub rssi_median: Option<i64>,
    pub squelch_open: f64,
    pub samples: usize,
}

impl Sample {
    pub fn to_json(&self) -> Value {
        json!({"rssi_median": self.rssi_median, "squelch_open": self.squelch_open, "samples": self.samples})
    }
}

pub fn sample(s: &mut Scanner, seconds: f64, t: &Timing) -> Result<Sample, ScannerError> {
    let (mut rssi, mut sql) = (Vec::new(), Vec::new());
    let end = t.now() + seconds;
    while t.now() < end {
        let g = s.glg()?;
        let (r, _) = s.pwr()?;
        rssi.push(r as f64);
        sql.push(if g.sql { 1.0 } else { 0.0 });
        t.sleep(SAMPLE_S);
    }
    if rssi.is_empty() {
        return Ok(Sample { rssi_median: None, squelch_open: 0.0, samples: 0 });
    }
    Ok(Sample {
        rssi_median: median(&rssi).map(|m| m.trunc() as i64),
        squelch_open: round_even(sql.iter().sum::<f64>() / sql.len() as f64, 2),
        samples: rssi.len(),
    })
}

/// Listen to one frequency by borrowing channel 1 (EPG parks there).
pub fn measure(monitor: &ScannerMonitor, state_dir: &Path, freq: f64, seconds: f64, t: &Timing, reason: &str)
    -> Result<Sample, ScannerError> {
    monitor.borrow(reason, |s| {
        s.prg()?;
        let r = (|| {
            remember(state_dir, s, &[1], false)?;
            s.cin_write(1, freq, false, false)?;
            s.epg(false)?; // parked on channel 1
            t.sleep(0.3);
            let result = sample(s, seconds, t)?;
            s.prg()?;
            restore_borrowed(s, state_dir)?;
            Ok(result)
        })();
        let f = (|| {
            if !s.in_program {
                s.prg()?;
            }
            s.epg(true)
        })();
        finally(r, f)
    })
}

/// One sample per frequency, in order (a frequency listed twice keeps its first place and
/// the last sample).
pub fn measure_many(monitor: &ScannerMonitor, state_dir: &Path, freqs: &[f64], seconds: f64, t: &Timing, reason: &str)
    -> Result<Vec<(f64, Sample)>, ScannerError> {
    monitor.borrow(reason, |s| {
        s.prg()?;
        let r = (|| {
            let mut out: Vec<(f64, Sample)> = Vec::new();
            remember(state_dir, s, &[1], false)?;
            for &f in freqs {
                if !s.in_program {
                    s.prg()?;
                }
                s.cin_write(1, f, false, false)?;
                s.epg(false)?;
                t.sleep(0.3);
                let m = sample(s, seconds, t)?;
                // a dict assignment: a repeated frequency keeps its first place
                match out.iter_mut().find(|(g, _)| *g == f) {
                    Some(slot) => slot.1 = m,
                    None => out.push((f, m)),
                }
            }
            if !s.in_program {
                s.prg()?;
            }
            restore_borrowed(s, state_dir)?;
            Ok(out)
        })();
        let f = (|| {
            if !s.in_program {
                s.prg()?;
            }
            s.epg(true)
        })();
        finally(r, f)
    })
}

fn lookup(m: &[(f64, Sample)], f: f64) -> Result<&Sample, ScannerError> {
    m.iter().find(|(g, _)| *g == f).map(|(_, s)| s).ok_or_else(|| val(crate::jsonfmt::float_repr(f)))
}

// radio check -----------------------------------------------------------------------------

/// A scanner error comes back as a failed check; an unreadable reply is an Err.
pub fn radio_check_scanner(monitor: &ScannerMonitor, state_dir: &Path, reference: &Value, t: &Timing)
    -> Result<Value, ScannerError> {
    radio_check_scanner_for(monitor, state_dir, reference, MEASURE_S, t)
}

pub fn radio_check_scanner_for(monitor: &ScannerMonitor, state_dir: &Path, reference: &Value, seconds: f64, t: &Timing)
    -> Result<Value, ScannerError> {
    let freq = value_f64(reference.get("freq")).map_err(val)?;
    let m = match measure(monitor, state_dir, freq, seconds, t, "radio check") {
        Ok(m) => m,
        Err(ScannerError::Io(e)) => {
            return Ok(json!({"freq": freq, "rssi": null, "squelch_open": null, "ok": false, "error": e}));
        }
        Err(e) => return Err(e),
    };
    let noise = nominal_noise(freq);
    let ok = m.squelch_open >= 0.5 || m.rssi_median.unwrap_or(0) >= noise + 35;
    let err = if ok {
        Value::Null
    } else {
        let name = reference.get("name").filter(|n| truthy(Some(n))).map(value_str)
            .unwrap_or_else(|| crate::jsonfmt::float_repr(freq));
        json!(format!("{name} weak: RSSI {}, squelch open {}",
                      m.rssi_median.map_or("None".to_string(), |r| r.to_string()), pct0(m.squelch_open)))
    };
    Ok(json!({"freq": freq, "rssi": m.rssi_median, "squelch_open": m.squelch_open, "ok": ok, "error": err}))
}

/// The radio check from the SDR's reference reading (`{"freq", "snr_db", ...}`).
pub fn radio_check_sdr(reference_state: &Value, name: &str) -> Value {
    let snr = reference_state.get("snr_db").and_then(Value::as_f64).unwrap_or(f64::NAN);
    let freq = reference_state.get("freq").and_then(Value::as_f64).unwrap_or(0.0);
    let ok = snr >= 10.0;
    json!({"freq": reference_state.get("freq"), "rssi": round_int(nominal_noise(freq) as f64 + snr),
           "squelch_open": if ok { 1.0 } else { 0.0 }, "ok": ok,
           "error": if ok { Value::Null } else { json!(format!("{name} only {:+.0} dB on the SDR", snr)) }})
}

// scan setup ---------------------------------------------------------------------------------

fn bank(ch: i64) -> i64 {
    (ch - 1).div_euclid(50)
}

/// Plan channels that live in scanner memory (1 to 500).
fn memory_channels(cfg: &Value) -> Result<Vec<(i64, &Value)>, ScannerError> {
    let mut out = Vec::new();
    for c in cfg.get("channels").and_then(Value::as_array).into_iter().flatten() {
        let n = value_i64(c.get("ch")).map_err(val)?;
        if (1..=500).contains(&n) {
            out.push((n, c));
        }
    }
    Ok(out)
}

/// Make sure every bank holding plan channels is scanned (SCG: 0 = scanned).
pub fn scan_setup_check(monitor: &ScannerMonitor, cfg: &Value) -> Result<Value, ScannerError> {
    let chans = memory_channels(cfg)?;
    let (mut fixed, mut problems): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let banks = monitor.borrow("scan setup", |s| {
        s.prg()?;
        let r = (|| {
            let mut banks = s.scg()?;
            let mut want: Vec<char> = banks.chars().collect();
            for (n, _) in &chans {
                let b = bank(*n) as usize;
                let slot = want.get_mut(b).ok_or_else(|| val("list index out of range".into()))?;
                if *slot != '0' {
                    *slot = '0';
                }
            }
            let new: String = want.into_iter().collect();
            if new != banks {
                s.set_scg(&new)?;
                fixed.push(format!("banks {banks} -> {new} (bank 1 = first digit, 0 = scanned)"));
                banks = new;
            }
            for (n, c) in &chans {
                let mem = s.cin_read(*n)?;
                let f = value_f64(c.get("freq")).map_err(val)?;
                if mem.freq.is_none_or(|m| (m - f).abs() > 1e-4) {
                    let holds = mem.freq.map_or("nothing".to_string(), crate::jsonfmt::float_repr);
                    problems.push(format!("CH {} holds {holds}, plan says {:.4}", value_str_opt(c.get("ch")), f));
                } else if mem.lockout {
                    problems.push(format!("CH {} ({:.4}) is locked out", value_str_opt(c.get("ch")), f));
                }
            }
            Ok(banks)
        })();
        finally(r, s.epg(true))
    })?;
    Ok(json!({"banks": banks, "fixed": fixed, "problems": problems}))
}

// reception test ------------------------------------------------------------------------------

pub fn reception_test(monitor: &ScannerMonitor, state_dir: &Path, cfg: &Value, seconds: f64, t: &Timing)
    -> Result<Value, ScannerError> {
    let reference = cfg.get("reference").filter(|r| truthy(Some(r)));
    let voice: Vec<&Value> = cfg.get("channels").and_then(Value::as_array).into_iter().flatten()
        .filter(|c| c.get("kind").and_then(Value::as_str) == Some("voice")).collect();
    let mut freqs = Vec::new();
    if let Some(r) = reference {
        freqs.push(value_f64(r.get("freq")).map_err(val)?);
    }
    for c in &voice {
        freqs.push(value_f64(c.get("freq")).map_err(val)?);
    }
    let m = measure_many(monitor, state_dir, &freqs, seconds, t, "reception test")?;
    let mut channels = Map::new();
    let mut quiet = Vec::new();
    for c in &voice {
        let f = value_f64(c.get("freq")).map_err(val)?;
        let r = lookup(&m, f)?;
        let busy = r.squelch_open > 0.2;
        if !busy {
            if let Some(x) = r.rssi_median {
                quiet.push(x as f64);
            }
        }
        channels.insert(format!("{:.4}", f), json!({
            "ch": c.get("ch"), "name": c.get("label"), "freq": f,
            "rssi_median": r.rssi_median, "squelch_open": r.squelch_open, "busy": busy,
            "noise": if busy { None } else { r.rssi_median }}));
    }
    let vhf_noise = median(&quiet).map(|x| x.trunc() as i64);
    let mut ref_out = Value::Null;
    let (mut verdict, mut detail) = ("poor", "no reference station configured".to_string());
    if let Some(rc) = reference {
        let rf = value_f64(rc.get("freq")).map_err(val)?;
        let r = lookup(&m, rf)?;
        let margin = match (vhf_noise.filter(|n| *n != 0), r.rssi_median.filter(|x| *x != 0)) {
            (Some(n), Some(x)) => Some(x - n),
            _ => None,
        };
        let name = value_str_opt(rc.get("name"));
        ref_out = json!({"name": rc.get("name"), "freq": rf, "rssi_median": r.rssi_median,
                         "squelch_open": r.squelch_open, "margin": margin, "vhf_noise": vhf_noise});
        (verdict, detail) = match margin {
            None => ("poor", "could not measure the noise floor".to_string()),
            Some(mg) if mg >= 50 => ("good", format!("{name} {mg} above the noise")),
            Some(mg) if mg >= 20 => ("fair", format!("{name} only {mg} above the noise")),
            Some(mg) => ("poor", format!("{name} barely above the noise ({mg})")),
        };
    }
    Ok(json!({"verdict": verdict, "detail": detail, "reference": ref_out, "channels": channels}))
}

// detection test -----------------------------------------------------------------------------

/// Put the reference station in a spare bank 1 slot and time how long the scan
/// takes to stop on it. It transmits continuously, so a working scan finds it.
pub fn detection_test(monitor: &ScannerMonitor, state_dir: &Path, cfg: &Value, timeout: f64, t: &Timing)
    -> Result<Value, ScannerError> {
    let Some(rc) = cfg.get("reference").filter(|r| truthy(Some(r))) else {
        return Ok(json!({"ok": false, "freq": null, "channel": null, "detect_s": null, "peak_rssi": null,
                         "error": "no reference station configured"}));
    };
    let freq = value_f64(rc.get("freq")).map_err(val)?;
    let mut used = BTreeSet::new();
    for c in cfg.get("channels").and_then(Value::as_array).into_iter().flatten() {
        used.insert(value_i64(c.get("ch")).map_err(val)?);
    }
    let slot = (2..=50).rev().find(|n| !used.contains(n));
    let mut result = json!({"ok": false, "freq": freq, "channel": slot, "detect_s": null, "peak_rssi": null,
                            "error": null});
    monitor.borrow("detection test", |s| {
        s.prg()?;
        let r = (|| {
            let slot = slot.ok_or_else(|| ScannerError::Io("bad CIN reply 'ERR'".into()))?;
            remember(state_dir, s, &[slot], true)?;
            s.cin_write(slot, freq, true, false)?;
            s.set_scg("0111111111")?;
            s.epg(true)?;
            let t0 = t.now();
            let mut peak = 0;
            while t.now() - t0 < timeout {
                let g = s.glg()?;
                if g.sql && g.freq.is_some_and(|gf| (gf - freq).abs() < 1e-4) {
                    peak = peak.max(s.pwr()?.0);
                    result["ok"] = json!(true);
                    result["detect_s"] = json!(round_even(t.now() - t0, 2));
                    result["peak_rssi"] = json!(peak);
                    break;
                }
                t.sleep(0.03);
            }
            if result["ok"] != json!(true) {
                result["error"] = json!(format!("scan never stopped on {:.4} MHz in {:.0} s", freq, timeout));
            }
            s.prg()?;
            restore_borrowed(s, state_dir)?;
            Ok(())
        })();
        let f = (|| {
            if !s.in_program {
                s.prg()?;
            }
            s.epg(true)
        })();
        finally(r, f)
    })?;
    Ok(result)
}

pub fn channel_memory(monitor: &ScannerMonitor) -> Result<Value, ScannerError> {
    let out = monitor.borrow("channel memory", |s| {
        s.prg()?;
        let r = (|| {
            let mut out = Vec::new();
            for n in 1..=50 {
                let c = s.cin_read(n)?;
                out.push(json!({"ch": n, "freq": c.freq, "lockout": c.lockout}));
            }
            Ok(out)
        })();
        finally(r, s.epg(true))
    })?;
    Ok(json!({"channels": out}))
}

// plan sync -----------------------------------------------------------------------------------

/// Make scanner memory match the plan, writing only what differs.
/// Returns a scan_setup report body when anything changed.
pub fn plan_sync(monitor: &ScannerMonitor, cfg: &Value, state_dir: &Path) -> Result<Option<Value>, ScannerError> {
    let path = state_dir.join("scanner_plan.json");
    let prev = read_json(&path).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if crate::util::scalar_eq(prev.get("config_version"), cfg.get("config_version")) {
        return Ok(None);
    }
    let chans: Vec<(i64, &Value)> = memory_channels(cfg)?.into_iter().filter(|(n, _)| *n != FOCUS_CH).collect();
    let mut fixed: Vec<String> = Vec::new();
    monitor.borrow("plan sync", |s| {
        s.prg()?;
        let r = (|| {
            for (n, c) in &chans {
                let f = value_f64(c.get("freq")).map_err(val)?;
                let delay_on = c.get("kind").and_then(Value::as_str) != Some("telemetry");
                let mem = s.cin_read(*n)?;
                let same = mem.freq.is_some_and(|m| (m - f).abs() < 1e-4) && !mem.lockout && (mem.delay != "0") == delay_on;
                if !same {
                    s.cin_write(*n, f, delay_on, false)?;
                    fixed.push(format!("CH {n} set to {:.4} MHz", f));
                }
            }
            let keep: BTreeSet<i64> = chans.iter().map(|(n, _)| *n).collect();
            for v in prev.get("managed").and_then(Value::as_array).into_iter().flatten() {
                let n = value_i64(Some(v)).map_err(val)?;
                if !keep.contains(&n) {
                    s.cin_clear(n)?;
                    fixed.push(format!("CH {} cleared", value_str(v)));
                }
            }
            Ok(())
        })();
        finally(r, s.epg(true))
    })?;
    let managed: BTreeSet<i64> = chans.iter().map(|(n, _)| *n).collect();
    write_json(&path, &json!({"config_version": cfg.get("config_version"), "managed": managed}))?;
    if !fixed.is_empty() {
        log::info!("scanner memory updated to plan {}: {}", value_str_opt(cfg.get("config_version")), fixed.join("; "));
        return Ok(Some(json!({"banks": null, "fixed": fixed, "problems": []})));
    }
    Ok(None)
}
