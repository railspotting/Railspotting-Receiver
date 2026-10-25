//! Numbers for the status page's tabs, worked out from the event history (kept 7 days) so an
//! owner can look back and tune: activity over time, per channel and per unit or detector, and
//! for EOT and HOT how often bursts decode at each signal level.

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// The tab an event belongs to: `eot`, `hot`, `detector`, `voice` or `other`.
pub fn feature_of(ev: &Value) -> &'static str {
    let label = ev["label"].as_str().unwrap_or("");
    match ev["kind"].as_str() {
        Some("telemetry") => {
            let hot_freq = ev["freq"].as_f64().is_some_and(|f| (f - 452.9375).abs() < 0.01);
            if label.contains("HOT") || (hot_freq && !label.contains("EOT")) { "hot" } else { "eot" }
        }
        Some("voice") => {
            let parsed_detector = ev.pointer("/transcript/parsed/type").and_then(Value::as_str) == Some("defect_detector");
            if ev["detector"].is_object() || parsed_detector { "detector" } else { "voice" }
        }
        _ => "other",
    }
}

/// Whether a log line belongs on a tab. `all` takes every line; a line can be on several tabs.
pub fn log_on(topic: &str, line: &str) -> bool {
    let lower = line.to_lowercase();
    match topic {
        "eot" => line.contains("EOT"),
        "hot" => line.contains("HOT"),
        "itc" => line.contains("ITC") || line.contains("220 MHz"),
        "detector" => ["detector", "milepost", "all clear", "all-clear", "defect"].iter().any(|w| lower.contains(w)),
        "voice" => line.contains("VHF") || line.contains("(voice)") || lower.contains("said:") || lower.contains("transcri")
            || lower.contains("scanner"),
        _ => true,
    }
}

/// How a detector announcement ended: `all_clear`, `defect` or `unconfirmed`.
pub fn detector_outcome(ev: &Value) -> &'static str {
    let defects = ev.pointer("/transcript/parsed/defects").and_then(Value::as_bool);
    let result = ev.pointer("/detector/result").and_then(Value::as_str);
    match (defects, result) {
        (Some(true), _) => "defect",
        (Some(false), _) | (_, Some("no_defects")) => "all_clear",
        _ => "unconfirmed",
    }
}

/// Bars for a time range: 10 minutes up to 6 hours, an hour up to 2 days, 3 hours beyond.
pub fn bucket_s(t0: f64, t1: f64) -> f64 {
    match t1 - t0 {
        s if s <= 6.0 * 3600.0 => 600.0,
        s if s <= 48.0 * 3600.0 => 3600.0,
        _ => 3.0 * 3600.0,
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn length(ev: &Value) -> f64 {
    match (ev["start_ts"].as_f64(), ev["end_ts"].as_f64()) {
        (Some(a), Some(b)) if b >= a => b - a,
        _ => 0.0,
    }
}

/// A tab's numbers from the events between `t0` and `t1` (any kind; those of other tabs are
/// left out). `recent` newest events are included whole.
pub fn feature_stats(name: &str, events: &[Value], t0: f64, t1: f64, recent: usize) -> Value {
    let evs: Vec<&Value> = events.iter()
        .filter(|e| feature_of(e) == name && e["start_ts"].as_f64().is_some_and(|t| t >= t0 && t <= t1))
        .collect();
    let step = bucket_s(t0, t1);
    let first_bar = (t0 / step).floor() * step;
    let bars = (((t1 - first_bar) / step).ceil() as usize).max(1);
    let mut buckets: Vec<Map<String, Value>> = (0..bars).map(|i| {
        let mut m = Map::new();
        m.insert("t".into(), json!(first_bar + i as f64 * step));
        m.insert("n".into(), json!(0));
        m
    }).collect();
    let bump = |m: &mut Map<String, Value>, k: &str| {
        let n = m.get(k).and_then(Value::as_u64).unwrap_or(0) + 1;
        m.insert(k.into(), json!(n));
    };

    // per channel: (freq, label) -> (n, airtime, peak sum, peak max, peaks)
    let mut chans: BTreeMap<String, (Value, Value, u64, f64, f64, f64, u64)> = BTreeMap::new();
    let mut airtime = 0.0;
    for e in &evs {
        let t = e["start_ts"].as_f64().unwrap_or(t0);
        let i = (((t - first_bar) / step) as usize).min(bars - 1);
        bump(&mut buckets[i], "n");
        match name {
            "eot" | "hot" if e["telemetry"].is_object() => bump(&mut buckets[i], "decoded"),
            "detector" => bump(&mut buckets[i], detector_outcome(e)),
            "voice" if e["transcript"].is_object() => bump(&mut buckets[i], "transcribed"),
            _ => {}
        }
        let len = length(e);
        airtime += len;
        let key = format!("{:.4} {}", e["freq"].as_f64().unwrap_or(0.0), e["label"].as_str().unwrap_or(""));
        let c = chans.entry(key).or_insert((e["freq"].clone(), e["label"].clone(), 0, 0.0, 0.0, f64::MIN, 0));
        c.2 += 1;
        c.3 += len;
        if let Some(p) = e["peak_rssi"].as_f64() {
            c.4 += p;
            c.5 = c.5.max(p);
            c.6 += 1;
        }
    }
    let channels: Vec<Value> = chans.into_values().map(|(freq, label, n, air, psum, pmax, pn)| json!({
        "freq": freq, "label": label, "n": n, "airtime_s": round1(air),
        "peak_avg": if pn > 0 { json!(round1(psum / pn as f64)) } else { Value::Null },
        "peak_max": if pn > 0 { json!(pmax) } else { Value::Null },
    })).collect();

    let mut out = json!({
        "name": name, "t0": t0, "t1": t1, "bucket_s": step, "total": evs.len(), "airtime_s": round1(airtime),
        "first": evs.first().map(|e| e["start_ts"].clone()), "last": evs.last().map(|e| e["start_ts"].clone()),
        "buckets": buckets, "channels": channels,
        "recent": evs.iter().rev().take(recent).collect::<Vec<_>>(),
    });
    match name {
        "eot" | "hot" => telemetry_stats(&evs, &mut out),
        "detector" => detector_stats(&evs, &mut out),
        "voice" => {
            out["transcribed"] = json!(evs.iter().filter(|e| e["transcript"].is_object()).count());
            let mut by_source: BTreeMap<String, u64> = BTreeMap::new();
            for e in &evs {
                *by_source.entry(e["source"].as_str().unwrap_or("sdr").to_string()).or_default() += 1;
            }
            out["by_source"] = json!(by_source);
        }
        _ => {}
    }
    out
}

/// EOT or HOT: decoding by signal level, corrected bits, and each unit heard.
fn telemetry_stats(evs: &[&Value], out: &mut Value) {
    let decoded: Vec<&&Value> = evs.iter().filter(|e| e["telemetry"].is_object()).collect();
    out["decoded"] = json!(decoded.len());
    // bursts and decodes per 10 of peak level: where decoding starts to fail
    let mut levels: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    for e in evs {
        if let Some(p) = e["peak_rssi"].as_f64() {
            let l = levels.entry((p / 10.0).floor() as i64 * 10).or_default();
            l.0 += 1;
            if e["telemetry"].is_object() {
                l.1 += 1;
            }
        }
    }
    out["levels"] = json!(levels.into_iter().map(|(lo, (n, d))| json!({"lo": lo, "n": n, "decoded": d})).collect::<Vec<_>>());
    let mut corrected: BTreeMap<i64, u64> = BTreeMap::new();
    for e in &decoded {
        *corrected.entry(e.pointer("/telemetry/corrected_bits").and_then(Value::as_i64).unwrap_or(0)).or_default() += 1;
    }
    out["corrected_bits"] = json!(corrected.into_iter().map(|(b, n)| json!({"bits": b, "n": n})).collect::<Vec<_>>());
    let mut units: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for e in &decoded {
        let tel = &e["telemetry"];
        let id = match &tel["unit_id"] {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            v => v.to_string(),
        };
        let t = e["start_ts"].clone();
        let u = units.entry(id.clone()).or_insert_with(|| {
            let mut m = Map::new();
            m.insert("unit_id".into(), tel["unit_id"].clone());
            m.insert("first".into(), t.clone());
            m.insert("packets".into(), json!(0));
            m
        });
        let n = u["packets"].as_u64().unwrap_or(0) + 1;
        u.insert("packets".into(), json!(n));
        u.insert("last".into(), t);
        if let Some(psi) = tel["brake_pipe_psi"].as_f64() {
            u.insert("psi".into(), json!(psi));
            let lo = u.get("psi_min").and_then(Value::as_f64).map_or(psi, |m| m.min(psi));
            let hi = u.get("psi_max").and_then(Value::as_f64).map_or(psi, |m| m.max(psi));
            u.insert("psi_min".into(), json!(lo));
            u.insert("psi_max".into(), json!(hi));
        }
        for k in ["motion", "marker_light", "marker_battery_weak", "battery_condition", "valve_circuit_ok", "turbine"] {
            if !tel[k].is_null() {
                u.insert(k.into(), tel[k].clone());
            }
        }
        if let Some(p) = e["peak_rssi"].as_f64() {
            u.insert("peak".into(), json!(p));
        }
    }
    let mut list: Vec<Value> = units.into_values().map(Value::Object).collect();
    list.sort_by(|a, b| b["last"].as_f64().unwrap_or(0.0).total_cmp(&a["last"].as_f64().unwrap_or(0.0)));
    out["units"] = json!(list);
}

/// Detector announcements: outcomes, and each detector heard.
fn detector_stats(evs: &[&Value], out: &mut Value) {
    let mut outcomes: BTreeMap<&str, u64> = BTreeMap::new();
    let mut dets: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for e in evs {
        let o = detector_outcome(e);
        *outcomes.entry(o).or_default() += 1;
        let det = &e["detector"];
        let parsed = e.pointer("/transcript/parsed").cloned().unwrap_or(Value::Null);
        let railroad = det["railroad"].as_str().or(parsed["railroad"].as_str()).unwrap_or("").to_string();
        let milepost = if det["milepost"].is_null() { parsed["milepost"].clone() } else { det["milepost"].clone() };
        let id = det["detector_id"].as_str().or(parsed["detector_id"].as_str()).map(String::from)
            .unwrap_or_else(|| if milepost.is_null() { e["label"].as_str().unwrap_or("unknown").to_string() }
                                else { format!("{railroad} MP {milepost}").trim().to_string() });
        let d = dets.entry(id.clone()).or_insert_with(|| {
            let mut m = Map::new();
            m.insert("id".into(), json!(id));
            m.insert("n".into(), json!(0));
            m
        });
        for (k, v) in [("railroad", json!(railroad)), ("milepost", milepost), ("label", e["label"].clone()), ("freq", e["freq"].clone())] {
            if !v.is_null() && v != "" {
                d.insert(k.into(), v);
            }
        }
        let n = d["n"].as_u64().unwrap_or(0) + 1;
        d.insert("n".into(), json!(n));
        let c = d.get(o).and_then(Value::as_u64).unwrap_or(0) + 1;
        d.insert(o.into(), json!(c));
        d.insert("last".into(), e["start_ts"].clone());
        d.insert("last_outcome".into(), json!(o));
        if let Some(t) = e.pointer("/transcript/text") {
            d.insert("last_text".into(), t.clone());
        }
        if let Some(m) = det["method"].as_str() {
            d.insert("method".into(), json!(m));
        }
    }
    out["outcomes"] = json!(outcomes);
    let mut list: Vec<Value> = dets.into_values().map(Value::Object).collect();
    list.sort_by(|a, b| b["last"].as_f64().unwrap_or(0.0).total_cmp(&a["last"].as_f64().unwrap_or(0.0)));
    out["detectors"] = json!(list);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tel(id: &str, t: f64, peak: i64, unit: Option<i64>, label: &str, freq: f64) -> Value {
        let mut e = json!({"event_id": id, "kind": "telemetry", "start_ts": t, "end_ts": t + 0.2, "freq": freq,
                           "label": label, "peak_rssi": peak});
        if let Some(u) = unit {
            e["telemetry"] = json!({"unit_id": u, "brake_pipe_psi": 88, "motion": true, "corrected_bits": 0});
        }
        e
    }

    #[test]
    fn events_are_sorted_into_tabs() {
        assert_eq!(feature_of(&tel("a", 0.0, 1, None, "EOT (end of train)", 457.9375)), "eot");
        assert_eq!(feature_of(&tel("a", 0.0, 1, None, "HOT (head of train)", 452.9375)), "hot");
        assert_eq!(feature_of(&tel("a", 0.0, 1, None, "", 452.9375)), "hot");
        assert_eq!(feature_of(&json!({"kind": "voice", "label": "Road"})), "voice");
        assert_eq!(feature_of(&json!({"kind": "voice", "detector": {"result": "no_defects"}})), "detector");
        assert_eq!(feature_of(&json!({"kind": "voice", "transcript": {"parsed": {"type": "defect_detector"}}})), "detector");
        assert_eq!(feature_of(&json!({"kind": "unknown"})), "other");
    }

    #[test]
    fn eot_decoding_by_level_and_units() {
        let t0 = 1_000_000.0;
        let evs = vec![
            tel("a", t0 + 10.0, 231, Some(12345), "EOT (end of train)", 457.9375),
            tel("b", t0 + 20.0, 236, None, "EOT (end of train)", 457.9375),
            tel("c", t0 + 4000.0, 252, Some(12345), "EOT (end of train)", 457.9375),
            tel("d", t0 + 30.0, 240, Some(999), "HOT (head of train)", 452.9375),
        ];
        let s = feature_stats("eot", &evs, t0, t0 + 86400.0, 10);
        assert_eq!(s["total"], 3);
        assert_eq!(s["decoded"], 2);
        assert_eq!(s["bucket_s"], 3600.0);
        assert_eq!(s["buckets"][0]["n"], 2);
        assert_eq!(s["buckets"][0]["decoded"], 1);
        assert_eq!(s["buckets"][1]["decoded"], 1);
        assert_eq!(s["levels"], json!([{"lo": 230, "n": 2, "decoded": 1}, {"lo": 250, "n": 1, "decoded": 1}]));
        assert_eq!(s["units"][0]["unit_id"], 12345);
        assert_eq!(s["units"][0]["packets"], 2);
        assert_eq!(s["recent"][0]["event_id"], "c", "newest first");
        assert_eq!(s["channels"][0]["n"], 3);
        assert_eq!(feature_stats("hot", &evs, t0, t0 + 86400.0, 10)["units"][0]["unit_id"], 999);
    }

    #[test]
    fn detector_outcomes_and_detectors() {
        let t0 = 1_000_000.0;
        let ann = |t: f64, det: Value, parsed: Value| json!({"kind": "voice", "start_ts": t, "end_ts": t + 20.0, "freq": 160.95,
            "label": "Road", "detector": det, "transcript": {"text": "words", "parsed": parsed}});
        let evs = vec![
            ann(t0 + 1.0, json!({"detector_id": "ns-101", "railroad": "NS", "milepost": 101.2, "result": "no_defects", "method": "sound"}),
                json!({"type": "defect_detector"})),
            ann(t0 + 2.0, json!({"detector_id": "ns-101", "railroad": "NS", "milepost": 101.2, "result": "different_message"}),
                json!({"type": "defect_detector", "defects": true})),
            ann(t0 + 3.0, Value::Null, json!({"type": "defect_detector", "railroad": "CSX", "milepost": 55.0})),
        ];
        let s = feature_stats("detector", &evs, t0, t0 + 3600.0, 10);
        assert_eq!(s["outcomes"], json!({"all_clear": 1, "defect": 1, "unconfirmed": 1}));
        assert_eq!(s["bucket_s"], 600.0);
        let dets = s["detectors"].as_array().unwrap();
        assert_eq!(dets[0]["id"], "CSX MP 55.0");
        assert_eq!(dets[1]["id"], "ns-101");
        assert_eq!(dets[1]["n"], 2);
        assert_eq!(dets[1]["last_outcome"], "defect");
    }

    #[test]
    fn log_lines_go_to_their_tabs() {
        assert!(log_on("eot", "UHF SDR (EOT): EOT unit 12345, 88 psi, moving"));
        assert!(!log_on("hot", "UHF SDR (EOT): EOT unit 12345, 88 psi, moving"));
        assert!(log_on("itc", "ITC: base station heard, local channel 114"));
        assert!(log_on("detector", "detector NS 101.2: all clear by its words"));
        assert!(log_on("voice", "VHF SDR (voice): 160.9500 MHz Road said: \"hello\" (90%)"));
        assert!(log_on("all", "anything"));
    }
}
