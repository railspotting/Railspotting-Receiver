//! The survey (`railspotting-receiver itc survey`): what ITC traffic can be heard here, for
//! setting up a receiver. The receiver does the listening and recording; this keeps the tally
//! and writes the report.
//!
//! 1. Listens to the national channel for base station beacons (each names its local channel and
//!    its neighbours').
//! 2. Records the whole ITC band for a while and measures clipping (the gain).
//! 3. Decodes every channel of that recording: bursts, messages, types, quality, wayside units.
//! 4. Prints what was found and the `itc decode` settings to use here.
//!
//! Decoding the whole band takes longer than recording it (a few times as long), so the survey
//! records first and decodes after. Given a recording, it surveys that instead.

use crate::dsp::{channel_hz, channel_number, demod_burst, plan_capture, Burst, BAND_HI_HZ, BAND_LO_HZ};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::Path;

/// The national common channel, where every base station sends its beacon.
pub const NATIONAL_CH: i64 = 102;

/// A base station's beacon, as heard: its local channel, its neighbours' channels and its level.
#[derive(Debug, Clone)]
pub struct Beacon {
    pub local: i64,
    pub near: Vec<i64>,
    pub level: f64,
}

/// A beacon's base radio and fields, from a decoded message (None for other messages).
pub fn beacon_of(m: &Value) -> Option<(String, Beacon)> {
    if m["type"] != "BEACON" {
        return None;
    }
    let radio = m["radio"].as_str()?.to_string();
    let local = m["channel"].as_i64()?;
    let near = m["channels"].as_array().map(|a| a.iter().filter_map(|c| c.as_i64()).collect()).unwrap_or_default();
    Some((radio, Beacon { local, near, level: m["level_db"].as_f64().unwrap_or(0.0) }))
}

/// The whole band at 2.4 MS/s.
pub const BAND_CENTER_HZ: f64 = 221.05e6;
pub const BAND_RATE: f64 = 2.4e6;
/// Seconds of the band recorded when none is given.
pub const DEFAULT_SECONDS: f64 = 60.0;
/// More samples than this at full scale: lower the gain.
const CLIP_LIMIT: f64 = 0.0005;
/// A channel whose bursts demodulate worse than this is noise (real ITC is about 0.8).
const NOISE_QUALITY: f32 = 0.5;

#[derive(Default)]
struct Chan {
    bursts: u64,
    messages: u64,
    types: BTreeMap<String, u64>,
    quality: Vec<f32>,
    level: Vec<f32>,
    wius: BTreeSet<String>,
    groups: BTreeSet<String>,
}

fn median(v: &[f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    s[s.len() / 2]
}

/// The share of 8-bit samples at full scale (0 or 255) in a recording.
pub fn clip_fraction(path: &Path) -> io::Result<f64> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let (mut clipped, mut total) = (0u64, 0u64);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        clipped += buf[..n].iter().filter(|&&b| b == 0 || b == 255).count() as u64;
        total += n as u64;
    }
    Ok(if total == 0 { 0.0 } else { clipped as f64 / total as f64 })
}

/// What a survey has heard so far, channel by channel.
#[derive(Default)]
pub struct Survey {
    chans: BTreeMap<i64, Chan>,
    pub bases: BTreeMap<String, Beacon>,
}

impl Survey {
    /// One burst and the messages decoded from it.
    pub fn add(&mut self, b: &Burst, msgs: &[Value]) {
        let ch = self.chans.entry(channel_number(b.channel_hz)).or_default();
        ch.bursts += 1;
        ch.level.push(b.peak_db);
        if let Some(d) = demod_burst(b) {
            ch.quality.push(d.quality);
        }
        for m in msgs {
            ch.messages += 1;
            *ch.types.entry(m["type"].as_str().unwrap_or("OTHER").to_string()).or_default() += 1;
            if let Some(w) = m["wiu"].as_str() {
                ch.wius.insert(w.to_string());
                ch.groups.insert(w[..6.min(w.len())].to_string());
            }
            if let Some((radio, beacon)) = beacon_of(m) {
                self.bases.entry(radio).or_insert(beacon);
            }
        }
    }

    /// The report: what was heard and the settings to use (`write_text` prints it).
    pub fn report(&self, clip: f64, gain: Option<f64>, seconds: f64, live: bool) -> Value {
        report(&self.chans, &self.bases, clip, gain, seconds, live)
    }
}

/// What was found and what to do, as JSON (the text report is written from this).
fn report(chans: &BTreeMap<i64, Chan>, bases: &BTreeMap<String, Beacon>, clip: f64, gain: Option<f64>, seconds: f64, live: bool) -> Value {
    let active: Vec<i64> = chans.iter().filter(|(_, c)| c.messages > 0).map(|(n, _)| *n).collect();
    let noise: Vec<i64> =
        chans.iter().filter(|(_, c)| c.messages == 0 && c.bursts >= 10 && median(&c.quality) < NOISE_QUALITY).map(|(n, _)| *n).collect();
    let locals: BTreeSet<i64> = bases.values().map(|b| b.local).collect();
    let neighbours: BTreeSet<i64> = bases.values().flat_map(|b| b.near.iter().copied()).filter(|n| !active.contains(n)).collect();
    // what `itc decode --auto` would pick: the national channel and the bases' local channels
    let mut auto: Vec<i64> = vec![NATIONAL_CH];
    auto.extend(locals.iter().copied().filter(|c| *c != NATIONAL_CH));
    auto.sort();
    let plan = plan_capture(&active);
    let mut advice = vec![];
    if clip > CLIP_LIMIT {
        let g = gain.unwrap_or(40.0);
        advice.push(format!("{:.2}% of samples at full scale: lower the gain, to about {:.0} dB", clip * 100.0, (g - 6.0).max(0.0)));
    }
    let best = chans.values().filter(|c| c.messages > 0).map(|c| median(&c.level)).fold(0.0f32, f32::max);
    if !active.is_empty() && best < 12.0 && clip <= CLIP_LIMIT {
        advice.push(format!("the strongest channel is only {best:.0} dB above the noise: raise the gain or improve the antenna"));
    }
    if active.is_empty() {
        advice.push("nothing decoded: check the antenna (about 33 cm for 220 MHz), the gain, and that this area has ITC".into());
    }
    if !neighbours.is_empty() {
        let list: Vec<String> = neighbours.iter().map(|n| n.to_string()).collect();
        advice.push(format!(
            "neighbouring bases' channels {} were not decoded here: an outdoor antenna, or a receiver nearer them, would add them",
            list.join(", ")
        ));
    }
    let gain_arg = gain.map(|g| format!(" --gain {g}")).unwrap_or_default();
    let command = plan.map(|(c, r)| {
        let list: Vec<String> = active.iter().map(|&n| format!("{:.4}M", channel_hz(n) / 1e6)).collect();
        format!("railspotting-receiver itc decode --freq {:.4}M --rate {}M --channels {}{gain_arg}", c / 1e6, r / 1e6, list.join(","))
    });
    json!({
        "seconds": seconds,
        "live": live,
        "clipped": (clip * 1e5).round() / 1e5,
        "gain_db": gain,
        "bases": bases.iter().map(|(radio, b)| json!({ "radio": radio, "local": b.local, "neighbours": b.near, "level_db": b.level })).collect::<Vec<_>>(),
        "channels": chans.iter().filter(|(_, c)| c.messages > 0).map(|(n, c)| json!({
            "ch": n, "mhz": channel_hz(*n) / 1e6, "bursts": c.bursts, "messages": c.messages, "types": c.types,
            "quality": (median(&c.quality) * 100.0).round() / 100.0, "level_db": (median(&c.level) * 10.0).round() / 10.0,
            "wiu_units": c.wius.len(), "wiu_groups": c.groups,
        })).collect::<Vec<_>>(),
        "noise_channels": noise,
        "active": active,
        "auto_matches": auto == active,
        "plan": plan.map(|(c, r)| json!({ "freq_hz": c, "rate": r })),
        "command": command,
        "advice": advice,
    })
}

pub fn write_text<W: Write + ?Sized>(r: &Value, out: &mut W) -> io::Result<()> {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    writeln!(
        out,
        "ITC survey: {:.0} s of the whole band ({:.4} to {:.4} MHz){}",
        r["seconds"].as_f64().unwrap_or(0.0),
        BAND_LO_HZ / 1e6,
        BAND_HI_HZ / 1e6,
        r["gain_db"].as_f64().map(|g| format!(", gain {g} dB")).unwrap_or_default()
    )?;
    writeln!(out)?;
    writeln!(out, "Base stations heard")?;
    let bases = r["bases"].as_array().cloned().unwrap_or_default();
    if bases.is_empty() {
        writeln!(out, "  none")?;
    }
    for b in &bases {
        let local = b["local"].as_i64().unwrap_or(0);
        writeln!(
            out,
            "  {}  local channel {} ({:.4} MHz), {:.0} dB; neighbours {}",
            s(&b["radio"]),
            local,
            channel_hz(local) / 1e6,
            b["level_db"].as_f64().unwrap_or(0.0),
            b["neighbours"].as_array().map(|n| n.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
        )?;
    }
    writeln!(out)?;
    writeln!(out, "Channels with traffic")?;
    let chans = r["channels"].as_array().cloned().unwrap_or_default();
    if chans.is_empty() {
        writeln!(out, "  none")?;
    } else {
        writeln!(out, "  ch   MHz       bursts  messages  quality  level  wayside units  types")?;
    }
    for c in &chans {
        let types: Vec<String> = c["types"].as_object().map(|t| t.iter().map(|(k, v)| format!("{k} {v}")).collect()).unwrap_or_default();
        writeln!(
            out,
            "  {:<4} {:<9.4} {:<7} {:<9} {:<8.2} {:<6.1} {:<14} {}",
            c["ch"].as_i64().unwrap_or(0),
            c["mhz"].as_f64().unwrap_or(0.0),
            c["bursts"].as_u64().unwrap_or(0),
            c["messages"].as_u64().unwrap_or(0),
            c["quality"].as_f64().unwrap_or(0.0),
            c["level_db"].as_f64().unwrap_or(0.0),
            c["wiu_units"].as_u64().unwrap_or(0),
            types.join(", ")
        )?;
    }
    if let Some(n) = r["noise_channels"].as_array().filter(|n| !n.is_empty()) {
        let list: Vec<String> = n.iter().map(|x| x.to_string()).collect();
        writeln!(out, "  Bursts but nothing decodable (noise, or too weak): {}", list.join(" "))?;
    }
    writeln!(
        out,
        "  Clipping: {:.3}% of samples at full scale",
        r["clipped"].as_f64().unwrap_or(0.0) * 100.0
    )?;
    writeln!(out)?;
    for a in r["advice"].as_array().cloned().unwrap_or_default() {
        writeln!(out, "Note: {}", s(&a))?;
    }
    writeln!(out)?;
    match r["command"].as_str() {
        Some(c) => {
            writeln!(out, "To decode here:")?;
            writeln!(out, "  {c}")?;
            if r["auto_matches"] == true {
                writeln!(out, "  (railspotting-receiver itc decode --auto finds the same channels by itself)")?;
            }
        }
        None if r["active"].as_array().is_some_and(|a| !a.is_empty()) => {
            writeln!(out, "The channels with traffic are too far apart for one dongle; decode them in two groups.")?;
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chan(messages: u64, quality: f32, level: f32) -> Chan {
        Chan { bursts: 20, messages, quality: vec![quality; 5], level: vec![level; 5], ..Default::default() }
    }

    #[test]
    fn the_report_says_what_to_do() {
        let mut chans = BTreeMap::new();
        chans.insert(102, chan(10, 0.8, 20.0));
        chans.insert(114, chan(500, 0.8, 20.0));
        chans.insert(127, chan(0, 0.2, 8.5));
        let mut bases = BTreeMap::new();
        bases.insert("123456".to_string(), Beacon { local: 114, near: vec![114, 127], level: 20.0 });
        let r = report(&chans, &bases, 0.001, Some(40.0), 60.0, true);
        assert_eq!(r["active"], json!([102, 114]));
        assert_eq!(r["noise_channels"], json!([127]));
        assert_eq!(r["auto_matches"], true);
        assert!(r["command"].as_str().unwrap().contains("--channels 220.1375M,220.4375M --gain 40"));
        let advice: Vec<String> = r["advice"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
        assert!(advice.iter().any(|a| a.contains("lower the gain, to about 34 dB")), "{advice:?}");
        assert!(advice.iter().any(|a| a.contains("channels 127 were not decoded")), "{advice:?}");
        let mut text = vec![];
        write_text(&r, &mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("local channel 114 (220.4375 MHz)"), "{text}");
        assert!(text.contains("itc decode --auto finds the same"), "{text}");
    }

    #[test]
    fn nothing_heard_is_said_plainly() {
        let r = report(&BTreeMap::new(), &BTreeMap::new(), 0.0, None, 60.0, true);
        assert_eq!(r["command"], Value::Null);
        assert!(r["advice"][0].as_str().unwrap().contains("nothing decoded"));
    }
}
