//! Where wayside units are: proposals for the headend (`type: "place"` records on `POST /itc`,
//! contract v1.18 section 7). No I/O.
//!
//! A unit's messages carry no position, so it is placed in three steps:
//!
//! 1. **Order along the line.** Each drop from proceed to stop is a train reaching a signal, drops
//!    close together are one train, and the order trains drop them is the order of the signals
//!    (as the status page's rail layout works it out). Units numbered one apart share a place.
//! 2. **Distance from a detector.** A train announced by a defect detector drops the signals
//!    either side of it a little before and after. Half the difference between trains one way and
//!    the other cancels the train's length and leaves each place's distance from the detector, in
//!    seconds of travel.
//! 3. **Matching OpenStreetMap signals.** Seconds become metres at some speed. The speed, the
//!    direction and the detector's place on the line are tried in turn, and the fit that puts the
//!    most places on mapped signals wins. Distances are measured along the mapped track when it is
//!    there (curves, junctions), else along the line's main direction. Only places that every
//!    equally good fit agrees on, and that no other place fits as well, are proposed, at the
//!    mapped signal's position, for an admin on the headend to confirm.
//!
//! Every map here is ordered, so the same logs and map give the same answer every run.
//!
//! Nothing here is tied to one railroad or place: what differs between them (the codes in a
//! railroad's device status table, train speeds, milepost units, how closely the map matches) is
//! in `Params`, with defaults that suit a typical North American main line.

use serde_json::{json, Map, Value};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use uuid::Uuid;

const M_PER_MILE: f64 = 1609.344;

/// What a receiver's owner can tune, as `itc_place` in receiver.json or `--set key=value`.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// Codes that let a train on, and codes for stop. By observation (`docs/itc.md`); each
    /// railroad's device status table decides them.
    pub proceed_codes: Vec<i64>,
    pub stop_codes: Vec<i64>,
    /// Codes that make a place a control point (a stop held until the dispatcher clears it).
    pub control_point_codes: Vec<i64>,
    /// Units numbered one apart are the two directions at one place.
    pub pair_twins: bool,
    /// A proceed lasting less than this is a flicker, not a signal a train passed.
    pub flicker_s: f64,
    /// Drops further apart than this belong to different trains.
    pub train_gap_s: f64,
    /// A train back at a place it dropped this long ago is another train.
    pub revisit_s: f64,
    /// A train's drops this long either side of an announcement count for that announcement.
    pub pass_window_s: f64,
    /// The detector to measure from (its milepost) and the railroad whose detectors count;
    /// None for the detector heard most.
    pub detector_milepost: Option<f64>,
    pub railroad: Option<String>,
    /// What mileposts count: "mi" or "km". Markers in the other unit are converted.
    pub milepost_unit: String,
    /// Train speeds tried, metres a second.
    pub speed_min: f64,
    pub speed_max: f64,
    /// Mapped signals closer than this are one place.
    pub cluster_m: f64,
    /// How far a place may be from the mapped signal it is matched to: a base, plus a share of
    /// its distance from the detector, since an error in the speed grows with distance.
    pub tolerance_m: f64,
    pub tolerance_share: f64,
    /// A match is left out when another place is within this much as close to its signal.
    pub ambiguous_m: f64,
    /// Fits scoring within this share of the best are rivals; only what they all agree on is
    /// proposed. A match scores 1 when it lands on its signal, less the further off it is.
    pub fit_margin: f64,
    /// A fit needs this many places on mapped signals, or more when no milepost markers hold the
    /// detector to its milepost.
    pub min_matches: usize,
    pub min_matches_without_mileposts: usize,
    /// Mileposts (in `milepost_unit`) this far from the detector's put a fit out.
    pub milepost_slack: f64,
    /// Measure along the mapped track (fetched with the signals) instead of a straight line.
    pub track: bool,
    /// A signal or marker further than this from the mapped track is off it.
    pub snap_m: f64,
    /// Without the mapped track, distances are measured along a straight line through the
    /// signals: only when their typical distance from it is under this share of its length.
    pub axis_max_spread: f64,
    /// How far around the base stations heard to look for signals, km (for the fetch).
    pub radius_km: f64,
    /// A position is sent only once this many runs in a row have proposed it, so an answer that
    /// moves as trains come in waits until it settles.
    pub confirm_runs: usize,
    /// Where the receiver is, set by its owner: the unit trains drop as they pass closest is
    /// placed at the mapped signal nearest it (basis `closest_approach`, contract v1.21).
    pub receiver_position: Option<(f64, f64)>,
    /// With no mapped signal near, the receiver's own position may be sent instead.
    pub share_receiver_position: bool,
    /// A unit counts as dropped at closest approach within this many seconds of it. Locomotives
    /// check in about every 45 s, so the loudest moment is known to about a minute.
    pub closest_window_s: f64,
    /// The mapped signal nearest the receiver is used for `closest_approach` only within this
    /// many metres of it.
    pub near_signal_m: f64,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            proceed_codes: vec![2, 3, 5, 6, 8],
            stop_codes: vec![13, 15],
            control_point_codes: vec![15],
            pair_twins: true,
            flicker_s: 10.0,
            train_gap_s: 720.0,
            revisit_s: 600.0,
            pass_window_s: 1200.0,
            detector_milepost: None,
            railroad: None,
            milepost_unit: "mi".into(),
            speed_min: 25.0 * M_PER_MILE / 3600.0,
            speed_max: 70.0 * M_PER_MILE / 3600.0,
            cluster_m: 120.0,
            tolerance_m: 250.0,
            tolerance_share: 0.08,
            ambiguous_m: 200.0,
            fit_margin: 0.15,
            min_matches: 3,
            min_matches_without_mileposts: 5,
            milepost_slack: 1.5,
            track: true,
            snap_m: 75.0,
            axis_max_spread: 0.05,
            radius_km: 30.0,
            confirm_runs: 3,
            receiver_position: None,
            share_receiver_position: false,
            closest_window_s: 120.0,
            near_signal_m: 1000.0,
        }
    }
}

/// The keys `Params::set` takes, for messages.
pub const PARAM_KEYS: &[&str] = &[
    "proceed_codes", "stop_codes", "control_point_codes", "pair_twins", "flicker_s", "train_gap_s", "revisit_s",
    "pass_window_s", "detector_milepost", "railroad", "milepost_unit", "speed_mph", "speed_kmh", "cluster_m",
    "tolerance_m", "tolerance_share", "ambiguous_m", "fit_margin", "min_matches", "min_matches_without_mileposts", "milepost_slack",
    "track", "snap_m", "axis_max_spread", "radius_km", "confirm_runs", "receiver_position", "share_receiver_position",
    "closest_window_s", "near_signal_m",
];

impl Params {
    /// The defaults with each key of `v` (a JSON object) applied.
    pub fn from_json(v: &Value) -> Result<Params, String> {
        let mut p = Params::default();
        match v {
            Value::Null => {}
            Value::Object(m) => {
                for (k, x) in m {
                    p.set(k, x)?;
                }
            }
            _ => return Err("itc_place must be an object".into()),
        }
        Ok(p)
    }

    /// Set one key. Speeds are given as `speed_mph` or `speed_kmh`: [lowest, highest].
    pub fn set(&mut self, key: &str, v: &Value) -> Result<(), String> {
        let bad = |what: &str| format!("itc_place {key}: {what}");
        let num = |lo: f64, hi: f64| v.as_f64().filter(|x| x.is_finite() && (lo..=hi).contains(x))
            .ok_or_else(|| bad(&format!("a number from {lo} to {hi}")));
        let codes = || -> Result<Vec<i64>, String> {
            let a = v.as_array().ok_or_else(|| bad("a list of codes 0 to 31"))?;
            a.iter().map(|c| c.as_i64().filter(|c| (0..=31).contains(c)).ok_or_else(|| bad("a list of codes 0 to 31"))).collect()
        };
        let flag = || v.as_bool().ok_or_else(|| bad("true or false"));
        match key {
            "proceed_codes" => self.proceed_codes = codes()?,
            "stop_codes" => self.stop_codes = codes()?,
            "control_point_codes" => self.control_point_codes = codes()?,
            "pair_twins" => self.pair_twins = flag()?,
            "flicker_s" => self.flicker_s = num(0.0, 600.0)?,
            "train_gap_s" => self.train_gap_s = num(30.0, 7200.0)?,
            "revisit_s" => self.revisit_s = num(30.0, 7200.0)?,
            "pass_window_s" => self.pass_window_s = num(60.0, 7200.0)?,
            "detector_milepost" => self.detector_milepost = if v.is_null() { None } else { Some(num(0.0, 10000.0)?) },
            "railroad" => self.railroad = match v {
                Value::Null => None,
                Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
                _ => return Err(bad("a railroad's mark, such as \"NS\", or null")),
            },
            "milepost_unit" => match v.as_str() {
                Some(u @ ("mi" | "km")) => self.milepost_unit = u.into(),
                _ => return Err(bad("\"mi\" or \"km\"")),
            },
            "speed_mph" | "speed_kmh" => {
                let per = if key == "speed_mph" { M_PER_MILE / 3600.0 } else { 1000.0 / 3600.0 };
                let a: Vec<f64> = v.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()).unwrap_or_default();
                match a.as_slice() {
                    [lo, hi] if *lo > 0.0 && lo < hi && *hi <= 400.0 => (self.speed_min, self.speed_max) = (lo * per, hi * per),
                    _ => return Err(bad("[lowest, highest], highest above lowest")),
                }
            }
            "cluster_m" => self.cluster_m = num(0.0, 2000.0)?,
            "tolerance_m" => self.tolerance_m = num(10.0, 5000.0)?,
            "tolerance_share" => self.tolerance_share = num(0.0, 0.5)?,
            "ambiguous_m" => self.ambiguous_m = num(0.0, 5000.0)?,
            "fit_margin" => self.fit_margin = num(0.0, 1.0)?,
            "min_matches" => self.min_matches = num(1.0, 1000.0)? as usize,
            "min_matches_without_mileposts" => self.min_matches_without_mileposts = num(1.0, 1000.0)? as usize,
            "milepost_slack" => self.milepost_slack = num(0.0, 100.0)?,
            "track" => self.track = flag()?,
            "snap_m" => self.snap_m = num(1.0, 2000.0)?,
            "axis_max_spread" => self.axis_max_spread = num(0.0, 1.0)?,
            "radius_km" => self.radius_km = num(1.0, 200.0)?,
            "confirm_runs" => self.confirm_runs = num(1.0, 30.0)? as usize,
            "receiver_position" => self.receiver_position = match v.as_array().map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>()) {
                _ if v.is_null() => None,
                Some(a) if a.len() == 2 && a[0].abs() <= 90.0 && a[1].abs() <= 180.0 => Some((a[0], a[1])),
                _ => return Err(bad("[latitude, longitude] or null")),
            },
            "share_receiver_position" => self.share_receiver_position = flag()?,
            "closest_window_s" => self.closest_window_s = num(10.0, 900.0)?,
            "near_signal_m" => self.near_signal_m = num(50.0, 10000.0)?,
            _ => return Err(format!("itc_place has no setting {key:?}; there are: {}", PARAM_KEYS.join(", "))),
        }
        Ok(())
    }

    /// The settings that differ from the defaults, as receiver.json would hold them.
    pub fn changed(&self) -> Value {
        let d = Params::default();
        let mut m = Map::new();
        let mph = |s: f64| (s * 3600.0 / M_PER_MILE * 10.0).round() / 10.0;
        macro_rules! diff {
            ($f:ident) => {
                if self.$f != d.$f {
                    m.insert(stringify!($f).into(), json!(self.$f));
                }
            };
        }
        diff!(proceed_codes);
        diff!(stop_codes);
        diff!(control_point_codes);
        diff!(pair_twins);
        diff!(flicker_s);
        diff!(train_gap_s);
        diff!(revisit_s);
        diff!(pass_window_s);
        diff!(detector_milepost);
        diff!(railroad);
        diff!(milepost_unit);
        if (self.speed_min, self.speed_max) != (d.speed_min, d.speed_max) {
            m.insert("speed_mph".into(), json!([mph(self.speed_min), mph(self.speed_max)]));
        }
        diff!(cluster_m);
        diff!(tolerance_m);
        diff!(tolerance_share);
        diff!(ambiguous_m);
        diff!(fit_margin);
        diff!(min_matches);
        diff!(min_matches_without_mileposts);
        diff!(milepost_slack);
        diff!(track);
        diff!(snap_m);
        diff!(axis_max_spread);
        diff!(radius_km);
        diff!(confirm_runs);
        if self.receiver_position.is_some() {
            m.insert("receiver_position".into(), json!("set"));
        }
        diff!(share_receiver_position);
        diff!(closest_window_s);
        diff!(near_signal_m);
        Value::Object(m)
    }

    fn unit_m(&self) -> f64 {
        if self.milepost_unit == "km" { 1000.0 } else { M_PER_MILE }
    }
}

/// A wayside state change (a `changes/` log line).
#[derive(Clone, Debug)]
pub struct Change {
    pub t: f64,
    pub wiu: String,
    pub codes: Vec<i64>,
}

/// A defect detector announcement (not a repeat).
#[derive(Clone, Debug)]
pub struct Pass {
    pub t: f64,
    pub milepost: f64,
    pub railroad: Option<String>,
}

/// A railway signal from OpenStreetMap.
#[derive(Clone, Debug)]
pub struct Signal {
    pub id: i64,
    pub lat: f64,
    pub lon: f64,
    pub name: Option<String>,
}

/// A milepost marker from OpenStreetMap, in metres along its railroad's own count
/// (`railway:position` "mi:485.0" or "km:12.4").
#[derive(Clone, Debug)]
pub struct Milestone {
    pub at_m: f64,
    pub lat: f64,
    pub lon: f64,
}

/// A stretch of mapped track: its nodes (OSM ID, lat, lon) in order.
#[derive(Clone, Debug, Default)]
pub struct Way {
    pub nodes: Vec<(i64, f64, f64)>,
}

/// What OpenStreetMap has near the receiver.
#[derive(Clone, Debug, Default)]
pub struct Osm {
    pub signals: Vec<Signal>,
    pub stones: Vec<Milestone>,
    pub ways: Vec<Way>,
}

/// One signal location along the line.
#[derive(Clone, Debug)]
pub struct Location {
    pub units: Vec<String>,
    /// A unit here showed a control point code.
    pub control_point: bool,
    /// Seconds of travel from the detector: positive one way along the line, negative the other.
    pub offset_s: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub locations: Vec<Location>,
    /// Units no train was seen passing.
    pub unplaced: Vec<String>,
    /// Train passes the order comes from.
    pub trains: usize,
    /// The detector the offsets are from (milepost, railroad), and how many of its passes were
    /// used.
    pub detector: Option<(f64, Option<String>, usize)>,
    /// Other detectors heard, with how many announcements each.
    pub other_detectors: Vec<(f64, Option<String>, usize)>,
}

/// A position for one unit.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub wiu: String,
    pub lat: f64,
    pub lon: f64,
    /// The mapped signals at that place (OSM node IDs).
    pub signals: Vec<i64>,
    pub name: Option<String>,
    /// Where in the layout's order it is.
    pub location: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Placement {
    pub proposals: Vec<Proposal>,
    /// Places on mapped signals in the best fit, and its rivals (fits within `fit_margin`).
    pub matched: usize,
    pub ties: usize,
    /// The fit's train speed, metres a second; whether milepost markers held the detector to its
    /// milepost; whether distances were along the mapped track.
    pub speed: f64,
    pub mileposts: bool,
    pub along_track: bool,
    pub note: String,
}

/// Units numbered one apart are the two directions at one place. In a run of three or more, the
/// parity most pairs use decides. Maps each unit to its place's key.
fn pair_units(all: &[String]) -> BTreeMap<String, String> {
    let mut groups: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for w in all {
        if let Ok(n) = u32::from_str_radix(&w[6..], 16) {
            groups.entry(&w[..6]).or_default().push(n);
        }
    }
    let mut runs: Vec<(&str, Vec<u32>)> = vec![];
    for (g, mut nums) in groups {
        nums.sort();
        nums.dedup();
        let mut run = vec![nums[0]];
        for &n in &nums[1..] {
            if n == run[run.len() - 1] + 1 {
                run.push(n);
            } else {
                runs.push((g, std::mem::replace(&mut run, vec![n])));
            }
        }
        runs.push((g, run));
    }
    let pairs = |p: u32| runs.iter().filter(|(_, r)| r.len() == 2 && r[0] % 2 == p).count();
    let parity = if pairs(1) >= pairs(0) { 1 } else { 0 };
    let id = |g: &str, n: u32| format!("{g}{n:04x}");
    let mut loc = BTreeMap::new();
    for (g, run) in &runs {
        let mut k = 0;
        if run.len() > 2 && run[0] % 2 != parity {
            loc.insert(id(g, run[0]), id(g, run[0]));
            k = 1;
        }
        while k < run.len() {
            let key = id(g, run[k]);
            loc.insert(key.clone(), key.clone());
            if k + 1 < run.len() {
                loc.insert(id(g, run[k + 1]), key);
            }
            k += 2;
        }
    }
    loc
}

/// A train's drops: each place once, at the time it first dropped.
struct Move {
    seq: Vec<String>,
    seen: BTreeMap<String, f64>,
    last: f64,
}

/// Each unit's drops from proceed to stop, as (time, place). `gaps` are spans with no
/// reception: a drop just after one may be a state missed, not a train.
fn drops(changes: &[Change], loc: &BTreeMap<String, String>, gaps: &[(f64, f64)], p: &Params) -> Vec<(f64, String)> {
    let mut sorted: Vec<&Change> = changes.iter().collect();
    sorted.sort_by(|a, b| a.t.total_cmp(&b.t));
    // each field's history: [(t, code)] at every change
    let mut fields: BTreeMap<&str, Vec<Vec<(f64, i64)>>> = BTreeMap::new();
    for c in sorted {
        let f = fields.entry(&c.wiu).or_default();
        for (i, &code) in c.codes.iter().enumerate() {
            if f.len() <= i {
                f.push(vec![]);
            }
            if f[i].last().is_none_or(|l| l.1 != code) {
                f[i].push((c.t, code));
            }
        }
    }
    let after_gap = |t: f64| gaps.iter().any(|&(_, b)| t >= b && t - b < 30.0);
    let mut out = vec![];
    for (w, fs) in &fields {
        let place = loc.get(*w).cloned().unwrap_or_else(|| w.to_string());
        for h in fs {
            for k in 1..h.len() {
                let (t0, c0) = h[k - 1];
                let (t1, c1) = h[k];
                if p.proceed_codes.contains(&c0) && p.stop_codes.contains(&c1) && t1 - t0 >= p.flicker_s && !after_gap(t1) {
                    out.push((t1, place.clone()));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// The order of places along the line from the order trains drop them, and each train.
fn line_order(drops: &[(f64, String)], p: &Params) -> (Vec<String>, Vec<Move>) {
    let mut moves: Vec<Move> = vec![];
    for (t, l) in drops {
        let new = match moves.last() {
            None => true,
            Some(m) => t - m.last > p.train_gap_s || m.seen.get(l).is_some_and(|s| t - s > p.revisit_s),
        };
        if new {
            moves.push(Move { seq: vec![], seen: BTreeMap::new(), last: *t });
        }
        let Some(m) = moves.last_mut() else { continue };
        if !m.seq.contains(l) {
            m.seq.push(l.clone());
        }
        m.seen.entry(l.clone()).or_insert(*t);
        m.last = *t;
    }
    moves.retain(|m| m.seq.len() > 1);
    // a train going the other way passes them in reverse: turn each to agree with what is known
    let mut votes: BTreeMap<(String, String), i64> = BTreeMap::new();
    let v = |votes: &BTreeMap<(String, String), i64>, a: &str, b: &str| *votes.get(&(a.to_string(), b.to_string())).unwrap_or(&0);
    let mut by_len: Vec<&Move> = moves.iter().collect();
    by_len.sort_by_key(|m| Reverse(m.seq.len()));
    for m in &by_len {
        let (mut agree, mut against) = (0, 0);
        for i in 0..m.seq.len() {
            for j in i + 1..m.seq.len() {
                agree += v(&votes, &m.seq[i], &m.seq[j]);
                against += v(&votes, &m.seq[j], &m.seq[i]);
            }
        }
        let seq: Vec<&String> = if against > agree { m.seq.iter().rev().collect() } else { m.seq.iter().collect() };
        for i in 0..seq.len() {
            for j in i + 1..seq.len() {
                *votes.entry((seq[i].clone(), seq[j].clone())).or_insert(0) += 1;
            }
        }
    }
    // longest trains first, as the status page does: ties in the score keep this order
    let mut placed: Vec<String> = vec![];
    for m in &by_len {
        for l in &m.seq {
            if !placed.contains(l) {
                placed.push(l.clone());
            }
        }
    }
    let score = |u: &str| -> i64 { placed.iter().map(|o| (v(&votes, u, o) - v(&votes, o, u)).signum()).sum() };
    let scores: BTreeMap<String, i64> = placed.iter().map(|l| (l.clone(), score(l))).collect();
    placed.sort_by(|a, b| scores[b].cmp(&scores[a]));
    (placed, moves)
}

/// The places along the line, from the state changes, and every drop as (time, place index).
/// `gaps` are spans with no reception.
pub fn layout(changes: &[Change], gaps: &[(f64, f64)], p: &Params) -> (Layout, Vec<(f64, usize)>) {
    let mut all: Vec<String> = changes.iter().map(|c| c.wiu.clone()).filter(|w| w.len() == 10).collect();
    all.sort();
    all.dedup();
    if all.is_empty() {
        return (Layout::default(), vec![]);
    }
    let loc = if p.pair_twins { pair_units(&all) } else { BTreeMap::new() };
    let ds = drops(changes, &loc, gaps, p);
    let (order, moves) = line_order(&ds, p);
    let mut cp: BTreeSet<&str> = BTreeSet::new();
    for c in changes {
        if c.codes.iter().any(|x| p.control_point_codes.contains(x)) {
            cp.insert(&c.wiu);
        }
    }
    let mut locations: Vec<Location> = order.iter().map(|_| Location { units: vec![], control_point: false, offset_s: None }).collect();
    let index: BTreeMap<&str, usize> = order.iter().enumerate().map(|(i, l)| (l.as_str(), i)).collect();
    let mut unplaced = vec![];
    for w in &all {
        let l = loc.get(w).map(String::as_str).unwrap_or(w);
        match index.get(l) {
            Some(&i) => {
                locations[i].units.push(w.clone());
                locations[i].control_point |= cp.contains(w.as_str());
            }
            None => unplaced.push(w.clone()),
        }
    }
    let placed_drops = ds.into_iter().filter_map(|(t, l)| index.get(l.as_str()).map(|&i| (t, i))).collect();
    (Layout { locations, unplaced, trains: moves.len(), ..Default::default() }, placed_drops)
}

/// Each place's distance from a detector, in seconds of travel: the one `p` names, else the
/// one heard most. `drops` are (time, place index) from `layout`. Places need trains both ways.
pub fn anchor(lay: &mut Layout, drops: &[(f64, usize)], passes: &[Pass], p: &Params) {
    let key = |mp: f64| (mp * 100.0).round() as i64;
    let mut by_det: BTreeMap<(i64, Option<String>), Vec<&Pass>> = BTreeMap::new();
    for x in passes {
        let rr = x.railroad.as_ref().map(|r| r.to_ascii_uppercase());
        if p.railroad.as_ref().is_some_and(|want| rr.as_deref() != Some(want.to_ascii_uppercase().as_str())) {
            continue;
        }
        by_det.entry((key(x.milepost), rr)).or_default().push(x);
    }
    let chosen = match p.detector_milepost {
        Some(mp) => by_det.iter().filter(|(k, _)| k.0 == key(mp)).max_by_key(|(_, v)| v.len()),
        None => by_det.iter().max_by_key(|(_, v)| v.len()),
    };
    let Some((det_key, ps)) = chosen else {
        lay.other_detectors = by_det.iter().map(|(k, v)| (v[0].milepost, k.1.clone(), v.len())).collect();
        return;
    };
    lay.other_detectors = by_det.iter().filter(|(k, _)| *k != det_key).map(|(k, v)| (v[0].milepost, k.1.clone(), v.len())).collect();
    let (mp, rr) = (ps[0].milepost, det_key.1.clone());
    // each train: each place's drop nearest its announcement, as seconds after it
    let trains: Vec<BTreeMap<usize, f64>> = ps.iter().map(|x| {
        let mut at: BTreeMap<usize, f64> = BTreeMap::new();
        for &(t, i) in drops.iter().filter(|(t, _)| (t - x.t).abs() < p.pass_window_s) {
            let off = t - x.t;
            if at.get(&i).is_none_or(|o| off.abs() < o.abs()) {
                at.insert(i, off);
            }
        }
        at
    }).filter(|at| at.len() >= 3).collect();
    let Some(reference) = trains.iter().max_by_key(|t| t.len()) else {
        lay.detector = Some((mp, rr, 0));
        return;
    };
    // Which way each train went. Not from the order along the line, which a few odd drops can
    // shuffle: first from whether its timings rise with those of the train that dropped the
    // most, then, again and again, with the distances those directions give.
    let mut dir: Vec<f64> = trains.iter().map(|t| covariance(t, reference).signum()).collect();
    let mut offsets: BTreeMap<usize, f64> = BTreeMap::new();
    for _ in 0..10 {
        let mut up: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
        let mut down: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
        for (t, &d) in trains.iter().zip(&dir) {
            if d == 0.0 {
                continue;
            }
            for (&i, &o) in t {
                (if d > 0.0 { &mut up } else { &mut down }).entry(i).or_default().push(o);
            }
        }
        // half the difference between the two ways cancels the train's length
        offsets = up.iter().filter_map(|(i, a)| down.get(i).map(|b| (*i, (median(a) - median(b)) / 2.0))).collect();
        let next: Vec<f64> = trains.iter().zip(&dir).map(|(t, &d)| {
            let c = covariance(t, &offsets);
            if c == 0.0 { d } else { c.signum() }
        }).collect();
        if next == dir {
            break;
        }
        dir = next;
    }
    for (i, l) in lay.locations.iter_mut().enumerate() {
        l.offset_s = offsets.get(&i).copied();
    }
    lay.detector = Some((mp, rr, dir.iter().filter(|d| **d != 0.0).count()));
}

/// How two sets of timings rise together over the places they share.
fn covariance(a: &BTreeMap<usize, f64>, b: &BTreeMap<usize, f64>) -> f64 {
    let both: Vec<(f64, f64)> = a.iter().filter_map(|(i, x)| b.get(i).map(|y| (*x, *y))).collect();
    if both.len() < 2 {
        return 0.0;
    }
    let n = both.len() as f64;
    let (mx, my) = (both.iter().map(|p| p.0).sum::<f64>() / n, both.iter().map(|p| p.1).sum::<f64>() / n);
    both.iter().map(|(x, y)| (x - mx) * (y - my)).sum()
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 { s[n / 2] } else { (s[n / 2 - 1] + s[n / 2]) / 2.0 }
}

/// Positions in metres on a flat local grid around (lat0, lon0).
fn grid(lat0: f64, lon0: f64) -> impl Fn(f64, f64) -> (f64, f64) {
    let k = lat0.to_radians().cos();
    move |lat, lon| ((lon - lon0) * k * 111_320.0, (lat - lat0) * 110_540.0)
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// Mapped signals grouped into places: each place's signals and its middle.
fn signal_places(signals: &[Signal], cluster_m: f64) -> Vec<(Vec<usize>, f64, f64)> {
    if signals.is_empty() {
        return vec![];
    }
    let g = grid(signals[0].lat, signals[0].lon);
    let xy: Vec<(f64, f64)> = signals.iter().map(|s| g(s.lat, s.lon)).collect();
    let mut group: Vec<usize> = (0..signals.len()).collect();
    fn root(g: &mut [usize], i: usize) -> usize {
        let mut r = i;
        while g[r] != r {
            r = g[r];
        }
        g[i] = r;
        r
    }
    for i in 0..xy.len() {
        for j in i + 1..xy.len() {
            if dist(xy[i], xy[j]) < cluster_m {
                let (a, b) = (root(&mut group, i), root(&mut group, j));
                group[a] = b;
            }
        }
    }
    let mut places: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..xy.len() {
        let r = root(&mut group, i);
        places.entry(r).or_default().push(i);
    }
    places.into_values().map(|m| {
        let n = m.len() as f64;
        (m.clone(), m.iter().map(|&s| signals[s].lat).sum::<f64>() / n, m.iter().map(|&s| signals[s].lon).sum::<f64>() / n)
    }).collect()
}

/// Distances along the line from a point, signed by side. Along the mapped track when there is
/// one, else along the main direction of the mapped signals.
enum Line {
    Track { xy: BTreeMap<i64, (f64, f64)>, next: BTreeMap<i64, Vec<(i64, f64)>>, g: (f64, f64) },
    Axis { g: (f64, f64), mid: (f64, f64), axis: (f64, f64) },
    /// No mapped track, and the signals don't lie along a straight line: nothing to measure on.
    Bent { spread: f64 },
}

impl Line {
    fn new(osm: &Osm, places: &[(Vec<usize>, f64, f64)], p: &Params) -> Line {
        let g0 = places.first().map(|q| (q.1, q.2)).unwrap_or((0.0, 0.0));
        let g = grid(g0.0, g0.1);
        if p.track && !osm.ways.is_empty() {
            let mut xy = BTreeMap::new();
            let mut next: BTreeMap<i64, Vec<(i64, f64)>> = BTreeMap::new();
            for w in &osm.ways {
                for n in &w.nodes {
                    xy.insert(n.0, g(n.1, n.2));
                }
                for pair in w.nodes.windows(2) {
                    let d = dist(g(pair[0].1, pair[0].2), g(pair[1].1, pair[1].2));
                    next.entry(pair[0].0).or_default().push((pair[1].0, d));
                    next.entry(pair[1].0).or_default().push((pair[0].0, d));
                }
            }
            return Line::Track { xy, next, g: g0 };
        }
        // the straight line most signal places lie close to: of the lines through two of them,
        // the one with the smallest median distance to all, so a stray signal can't tilt it
        let pts: Vec<(f64, f64)> = places.iter().map(|q| g(q.1, q.2)).collect();
        let across = |mid: (f64, f64), axis: (f64, f64)| -> Vec<f64> {
            pts.iter().map(|q| (-(q.0 - mid.0) * axis.1 + (q.1 - mid.1) * axis.0).abs()).collect()
        };
        // (median distance, a point on it, its direction)
        let (mut m, mut mid, mut axis) = (f64::INFINITY, (0.0, 0.0), (1.0, 0.0));
        for i in 0..pts.len() {
            for j in i + 1..pts.len() {
                let d = dist(pts[i], pts[j]);
                if d < 500.0 {
                    continue;
                }
                let dir = ((pts[j].0 - pts[i].0) / d, (pts[j].1 - pts[i].1) / d);
                let md = median(&across(pts[i], dir));
                if md < m {
                    (m, mid, axis) = (md, pts[i], dir);
                }
            }
        }
        if !m.is_finite() {
            return Line::Bent { spread: 1.0 };
        }
        // how far the places stray from it, against how long it is: a curving line folds over
        // when measured straight, so it is refused
        let mut along: Vec<f64> = pts.iter().map(|q| (q.0 - mid.0) * axis.0 + (q.1 - mid.1) * axis.1).collect();
        along.sort_by(f64::total_cmp);
        let length = along[along.len() - 1] - along[0];
        let spread = if length > 0.0 { m / length } else { 1.0 };
        if spread > p.axis_max_spread {
            return Line::Bent { spread };
        }
        Line::Axis { g: g0, mid, axis }
    }

    fn along_track(&self) -> bool {
        matches!(self, Line::Track { .. })
    }

    /// Each point's signed distance along the line from `from`, or None when it is off the
    /// mapped track (further than `snap_m`) or not connected to it.
    fn from(&self, from: (f64, f64), pts: &[(f64, f64)], snap_m: f64) -> Vec<Option<f64>> {
        match self {
            Line::Bent { .. } => vec![None; pts.len()],
            Line::Axis { g, mid, axis } => {
                let gr = grid(g.0, g.1);
                let along = |lat: f64, lon: f64| {
                    let q = gr(lat, lon);
                    (q.0 - mid.0) * axis.0 + (q.1 - mid.1) * axis.1
                };
                let a0 = along(from.0, from.1);
                pts.iter().map(|&(lat, lon)| Some(along(lat, lon) - a0)).collect()
            }
            Line::Track { xy, next, g } => {
                let gr = grid(g.0, g.1);
                let snap = |lat: f64, lon: f64| -> Option<(i64, f64)> {
                    let q = gr(lat, lon);
                    xy.iter().map(|(id, &n)| (*id, dist(n, q))).filter(|x| x.1 <= snap_m).min_by(|a, b| a.1.total_cmp(&b.1))
                };
                let Some((start, _)) = snap(from.0, from.1) else { return vec![None; pts.len()] };
                // the two ways out of the start: each first step's side, by its direction
                let s = xy[&start];
                let out = next.get(&start).cloned().unwrap_or_default();
                let heading = |n: i64| { let q = xy[&n]; (q.0 - s.0, q.1 - s.1) };
                let h0 = out.first().map(|o| heading(o.0)).unwrap_or((1.0, 0.0));
                let mut best: BTreeMap<i64, (f64, f64)> = BTreeMap::new(); // node: (distance, side)
                let mut heap = BinaryHeap::new();
                best.insert(start, (0.0, 1.0));
                for &(n, d) in &out {
                    let h = heading(n);
                    let side = if h.0 * h0.0 + h.1 * h0.1 >= 0.0 { 1.0 } else { -1.0 };
                    if best.get(&n).is_none_or(|b| d < b.0) {
                        best.insert(n, (d, side));
                        heap.push((Reverse(OrdF(d)), n));
                    }
                }
                while let Some((Reverse(OrdF(d)), n)) = heap.pop() {
                    if best.get(&n).is_some_and(|b| d > b.0) {
                        continue;
                    }
                    let side = best[&n].1;
                    for &(m, e) in next.get(&n).into_iter().flatten() {
                        let nd = d + e;
                        if m != start && best.get(&m).is_none_or(|b| nd < b.0) {
                            best.insert(m, (nd, side));
                            heap.push((Reverse(OrdF(nd)), m));
                        }
                    }
                }
                pts.iter().map(|&(lat, lon)| {
                    let (n, _) = snap(lat, lon)?;
                    best.get(&n).map(|b| b.0 * b.1)
                }).collect()
            }
        }
    }
}

/// An f64 that sorts, for the queue (distances are never NaN).
struct OrdF(f64);
impl PartialEq for OrdF {
    fn eq(&self, o: &Self) -> bool {
        self.0.total_cmp(&o.0).is_eq()
    }
}
impl Eq for OrdF {}
impl PartialOrd for OrdF {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for OrdF {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0)
    }
}

/// Mileposts along the line: a straight fit through the markers, as (milepost at distance 0,
/// mileposts per metre), from (distance, milepost) pairs. None without two markers that fit.
fn milepost_fit(xs: &[(f64, f64)], unit_m: f64) -> Option<(f64, f64)> {
    if xs.len() < 2 {
        return None;
    }
    // the median of the slopes between markers a kilometre or more apart, so a marker from
    // another line (or a typo) can't pull it
    let mut slopes = vec![];
    for i in 0..xs.len() {
        for j in i + 1..xs.len() {
            if (xs[j].0 - xs[i].0).abs() >= 1000.0 {
                slopes.push((xs[j].1 - xs[i].1) / (xs[j].0 - xs[i].0));
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    let b = median(&slopes);
    // about one milepost a mile (or kilometre), or the markers are not on this line
    if !(0.5..=1.5).contains(&(b.abs() * unit_m)) {
        return None;
    }
    let a = median(&xs.iter().map(|x| x.1 - b * x.0).collect::<Vec<_>>());
    // most markers must agree with it to within one milepost
    let fits = xs.iter().filter(|x| (a + b * x.0 - x.1).abs() <= 1.0).count();
    if fits * 2 <= xs.len() || fits < 2 {
        return None;
    }
    Some((a, b))
}

/// One way of laying the places on the map.
#[derive(Clone)]
struct Fit {
    speed: f64,
    mileposts: bool,
    /// (place, mapped place), and the ones another place fits nearly as well.
    pairs: Vec<(usize, usize)>,
    unsure: Vec<(usize, usize)>,
}

/// Match the layout to mapped signals. Only places every best fit agrees on are proposed.
pub fn place(lay: &Layout, osm: &Osm, p: &Params) -> Placement {
    let mut out = Placement::default();
    let anchored: Vec<(usize, f64)> = lay.locations.iter().enumerate().filter_map(|(i, l)| l.offset_s.map(|o| (i, o))).collect();
    let Some((det_mp, _, _)) = lay.detector else {
        out.note = "no defect detector announcements to measure from".into();
        return out;
    };
    let need = p.min_matches.min(p.min_matches_without_mileposts);
    if anchored.len() < need {
        out.note = format!("only {} places timed against the detector both ways; {need} needed", anchored.len());
        return out;
    }
    let places = signal_places(&osm.signals, p.cluster_m);
    if places.len() < need {
        out.note = format!("only {} mapped signal places nearby", places.len());
        return out;
    }
    let line = Line::new(osm, &places, p);
    out.along_track = line.along_track();
    if let Line::Bent { spread } = line {
        out.note = format!("no mapped track, and the signals don't lie along a straight line (spread {spread:.2}, limit {}): \
                            fetch the track (\"track\": true) or raise axis_max_spread", p.axis_max_spread);
        return out;
    }
    let unit_m = p.unit_m();
    let place_pts: Vec<(f64, f64)> = places.iter().map(|q| (q.1, q.2)).collect();
    let stone_pts: Vec<(f64, f64)> = osm.stones.iter().map(|s| (s.lat, s.lon)).collect();
    // the place nearest the detector sits on a mapped signal: try each, at each speed and way
    let (near_i, near_o) = anchored.iter().copied().min_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap_or((0, 0.0));
    // every distinct way of laying them out, with its score
    let mut fits: Vec<(f64, Fit)> = vec![];
    for k in 0..places.len() {
        let at = line.from(place_pts[k], &place_pts, p.snap_m);
        if at[k].is_none() {
            continue; // this signal is off the mapped track
        }
        // markers on this line, as (distance from here, milepost)
        let marks: Vec<(f64, f64)> = line.from(place_pts[k], &stone_pts, p.snap_m.max(500.0)).iter().zip(&osm.stones)
            .filter_map(|(x, s)| x.map(|x| (x, s.at_m / unit_m))).collect();
        let mp = milepost_fit(&marks, unit_m);
        for sign in [1.0, -1.0] {
            let mut v = p.speed_min;
            while v <= p.speed_max + 1e-9 {
                let speed = v;
                v += 0.5;
                let d = -sign * speed * near_o;
                if let Some((a, b)) = mp {
                    if ((a + b * d) - det_mp).abs() > p.milepost_slack {
                        continue;
                    }
                }
                // each place to the nearest free mapped place within tolerance, nearest first
                let mut cand: Vec<(f64, usize, usize)> = vec![];
                for &(i, o) in &anchored {
                    let x = d + sign * speed * o;
                    let tol = p.tolerance_m + p.tolerance_share * (x - d).abs();
                    for (j, pj) in at.iter().enumerate() {
                        if let Some(pj) = pj {
                            let e = (pj - x).abs();
                            if e <= tol {
                                cand.push((e, i, j));
                            }
                        }
                    }
                }
                cand.sort_by(|a, b| a.0.total_cmp(&b.0));
                let (mut li, mut lj, mut pairs) = (BTreeSet::new(), BTreeSet::new(), vec![]);
                for &(_, i, j) in &cand {
                    if li.insert(i) && lj.insert(j) {
                        pairs.push((i, j));
                    }
                }
                if !pairs.iter().any(|&(i, _)| i == near_i) {
                    continue;
                }
                // two places that fit one mapped signal about as well can't be told apart
                let err = |i: usize, j: usize| cand.iter().find(|c| c.1 == i && c.2 == j).map(|c| c.0).unwrap_or(0.0);
                let mut unsure: Vec<(usize, usize)> = pairs.iter().copied()
                    .filter(|&(i, j)| cand.iter().any(|c| c.2 == j && c.1 != i && c.0 - err(i, j) < p.ambiguous_m)).collect();
                unsure.sort();
                // each match counts by how close it lands: 1 on the signal, nothing at the edge of
                // the tolerance, so chance alignments near the edge don't rival a true fit.
                // Control points at places with several mapped heads count a little more.
                let score: f64 = pairs.iter().map(|&(i, j)| {
                    let o = anchored.iter().find(|a| a.0 == i).map(|a| a.1).unwrap_or(0.0);
                    let tol = p.tolerance_m + p.tolerance_share * (speed * o).abs();
                    let close = 1.0 - err(i, j) / tol;
                    close * if lay.locations[i].control_point && places[j].0.len() > 1 { 1.25 } else { 1.0 }
                }).sum();
                pairs.sort();
                let fit = Fit { speed, mileposts: mp.is_some(), pairs, unsure };
                match fits.iter_mut().find(|f| f.1.pairs == fit.pairs) {
                    Some(f) if score > f.0 => *f = (score, fit),
                    Some(_) => {}
                    None => fits.push((score, fit)),
                }
            }
        }
    }
    fits.sort_by(|a, b| b.0.total_cmp(&a.0));
    let Some((top, first)) = fits.first().cloned() else {
        out.note = "no fit puts the place nearest the detector on a mapped signal".into();
        return out;
    };
    // fits nearly as good are rivals: what they disagree on is chance, not evidence
    let best: Vec<&Fit> = fits.iter().filter(|f| f.0 >= top * (1.0 - p.fit_margin) - 1e-9).map(|f| &f.1).collect();
    out.matched = first.pairs.len();
    out.ties = best.len();
    out.speed = first.speed;
    out.mileposts = best.iter().all(|b| b.mileposts);
    // without mileposts the detector could be anywhere along the line: ask for more agreement
    let min = if out.mileposts { p.min_matches } else { p.min_matches_without_mileposts };
    if out.matched < min {
        out.note = format!("the best fit puts only {} places on mapped signals; {min} needed{}", out.matched,
                           if out.mileposts { "" } else { " without milepost markers" });
        return out;
    }
    // only what no rival contradicts (putting that place on another signal, or another place on
    // that signal; a rival that just lacks the match doesn't), and no other place fits as well
    let agreed: Vec<(usize, usize)> = first.pairs.iter().copied().filter(|&(i, j)| {
        best.iter().all(|b| !b.unsure.contains(&(i, j)) && !b.pairs.iter().any(|&(i2, j2)| (i2 == i) != (j2 == j)))
    }).collect();
    if agreed.len() < min {
        let speeds: Vec<f64> = best.iter().map(|b| b.speed * 3600.0 / M_PER_MILE).collect();
        let (lo, hi) = (speeds.iter().cloned().fold(f64::MAX, f64::min), speeds.iter().cloned().fold(f64::MIN, f64::max));
        out.note = format!("the evidence doesn't single out one fit: {} fits within {:.0}% of the best (at {lo:.0} to {hi:.0} mph) \
                            agree on only {} places, {min} needed. Narrow speed_mph to the line's usual speed, or wait for more trains",
                           best.len(), p.fit_margin * 100.0, agreed.len());
        return out;
    }
    if agreed.len() < out.matched {
        out.note = format!("{} of {} matches are left out: {} rival fits disagree, or another place fits the same signal as well",
                           out.matched - agreed.len(), out.matched, best.len());
    }
    for (i, j) in agreed {
        let q = &places[j];
        let name = q.0.iter().find_map(|&s| osm.signals[s].name.clone());
        for w in &lay.locations[i].units {
            out.proposals.push(Proposal {
                wiu: w.clone(), lat: (q.1 * 1e5).round() / 1e5, lon: (q.2 * 1e5).round() / 1e5,
                signals: q.0.iter().map(|&s| osm.signals[s].id).collect(), name: name.clone(), location: i,
            });
        }
    }
    out.proposals.sort_by(|a, b| a.location.cmp(&b.location).then(a.wiu.cmp(&b.wiu)));
    out
}

/// One run's proposals: each unit's position.
pub type Run = BTreeMap<String, (f64, f64)>;

/// The units the last `n` runs (newest last) all proposed at the same position: settled
/// enough to send.
pub fn steady(runs: &[Run], n: usize) -> Vec<String> {
    let n = n.max(1);
    if runs.len() < n {
        return vec![];
    }
    let recent = &runs[runs.len() - n..];
    recent[n - 1].iter().filter(|(w, at)| recent.iter().all(|r| r.get(*w) == Some(*at))).map(|(w, _)| w.clone()).collect()
}

/// The `place` record for a proposal. Its ID comes from the unit and position, so the same
/// proposal sent again is a duplicate on the headend.
pub fn record(ns: &Uuid, p: &Proposal, t: f64, basis: &str) -> Value {
    let id = Uuid::new_v5(ns, format!("itc-place:{}:{:.5}:{:.5}", p.wiu, p.lat, p.lon).as_bytes());
    json!({"id": id.to_string(), "type": "place", "wiu": p.wiu, "lat": p.lat, "lon": p.lon, "basis": basis,
           "t": (t * 1000.0).round() / 1000.0})
}

/// A unit's place: its 12-digit decimal ID less the track (the last 2 digits), so the tracks at
/// one place go together; the ID itself when it doesn't read that way.
pub fn place_key(wiu: &str) -> String {
    match u64::from_str_radix(wiu, 16) {
        Ok(n) if (700_000_000_000..800_000_000_000).contains(&n) => (n / 100).to_string(),
        _ => wiu.to_string(),
    }
}

/// From `pass` records: the places whose units dropped within `window_s` of the pass's closest
/// approach (its locomotives loudest), with their units and how many passes, most first.
pub fn closest_places(passes: &[Value], window_s: f64) -> Vec<(String, Vec<String>, usize)> {
    let mut by: BTreeMap<String, (BTreeSet<String>, usize)> = BTreeMap::new();
    for p in passes {
        let Some(c) = p["closest"].as_f64() else { continue };
        let mut here: BTreeSet<String> = BTreeSet::new();
        for d in p["drops"].as_array().into_iter().flatten() {
            if let (Some(w), Some(t)) = (d["wiu"].as_str(), d["t"].as_f64()) {
                if (t - c).abs() <= window_s {
                    here.insert(w.to_string());
                }
            }
        }
        let keys: BTreeSet<String> = here.iter().map(|w| place_key(w)).collect();
        for k in keys {
            let e = by.entry(k.clone()).or_default();
            e.1 += 1;
            e.0.extend(here.iter().filter(|w| place_key(w) == k).cloned());
        }
    }
    let mut out: Vec<(String, Vec<String>, usize)> = by.into_iter().map(|(k, (u, n))| (k, u.into_iter().collect(), n)).collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    out
}

/// The `closest_approach` proposal: the one place most often dropped at closest approach, on at
/// least `min_passes` passes and more often than any other, at the mapped signal nearest the
/// receiver (within `near_m`), or at the receiver itself when its owner allows that.
pub fn closest_approach(passes: &[Value], osm: &Osm, p: &Params, min_passes: usize, near_m: f64) -> Vec<Proposal> {
    let Some((rlat, rlon)) = p.receiver_position else { return vec![] };
    let places = closest_places(passes, p.closest_window_s);
    let Some((_, units, n)) = places.first() else { return vec![] };
    if *n < min_passes.max(1) || places.get(1).is_some_and(|q| q.2 == *n) {
        return vec![];
    }
    let g = grid(rlat, rlon);
    let near = signal_places(&osm.signals, p.cluster_m).into_iter()
        .map(|q| (dist(g(q.1, q.2), (0.0, 0.0)), q)).filter(|(d, _)| *d <= near_m)
        .min_by(|a, b| a.0.total_cmp(&b.0));
    let (lat, lon, signals, name) = match near {
        Some((_, q)) => (q.1, q.2, q.0.iter().map(|&s| osm.signals[s].id).collect(), q.0.iter().find_map(|&s| osm.signals[s].name.clone())),
        None if p.share_receiver_position => (rlat, rlon, vec![], None),
        None => return vec![],
    };
    units.iter().map(|w| Proposal {
        wiu: w.clone(), lat: (lat * 1e5).round() / 1e5, lon: (lon * 1e5).round() / 1e5, signals: signals.clone(), name: name.clone(), location: 0,
    }).collect()
}

/// The Overpass query for signals, milepost markers and (with `track`) the track itself within
/// `radius_m` of each point.
pub fn osm_query(points: &[(f64, f64)], radius_m: f64, track: bool) -> String {
    let mut q = String::from("[out:json][timeout:120];(");
    for (lat, lon) in points {
        q.push_str(&format!("node[\"railway\"~\"^(signal|milestone)$\"](around:{radius_m:.0},{lat:.5},{lon:.5});"));
        if track {
            q.push_str(&format!("way[\"railway\"=\"rail\"](around:{radius_m:.0},{lat:.5},{lon:.5});"));
        }
    }
    q.push_str(");out body geom;");
    q
}

/// Signals, milepost markers and track from an Overpass answer (`out body geom`). Markers given
/// with no unit ("485.0") are taken to be in `default_unit`.
pub fn read_osm(v: &Value, default_unit: &str) -> Osm {
    let mut osm = Osm::default();
    for e in v["elements"].as_array().into_iter().flatten() {
        let tags = &e["tags"];
        if e["type"] == "way" {
            let ids = e["nodes"].as_array().cloned().unwrap_or_default();
            let geom = e["geometry"].as_array().cloned().unwrap_or_default();
            let nodes: Vec<(i64, f64, f64)> = ids.iter().zip(&geom)
                .filter_map(|(id, g)| Some((id.as_i64()?, g["lat"].as_f64()?, g["lon"].as_f64()?))).collect();
            if nodes.len() >= 2 {
                osm.ways.push(Way { nodes });
            }
            continue;
        }
        let (Some(id), Some(lat), Some(lon)) = (e["id"].as_i64(), e["lat"].as_f64(), e["lon"].as_f64()) else { continue };
        match tags["railway"].as_str() {
            Some("signal") => osm.signals.push(Signal { id, lat, lon, name: tags["ref"].as_str().map(String::from) }),
            Some("milestone") => {
                // "mi:485.0", "km:12.4", or a plain number in the railroad's own unit
                let pos = tags["railway:position"].as_str().unwrap_or("").trim();
                let (unit, num) = match pos.split_once(':') {
                    Some((u, n)) => (u, n),
                    None => (default_unit, pos),
                };
                let per = match unit { "mi" => M_PER_MILE, "km" => 1000.0, _ => continue };
                if let Ok(x) = num.trim().parse::<f64>() {
                    osm.stones.push(Milestone { at_m: x * per, lat, lon });
                }
            }
            _ => {}
        }
    }
    osm
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: Uuid = uuid::uuid!("6f1c4e1e-6a0b-4a52-9d55-4d3c1f0a7b15");
    /// A made-up line running east, mileposts rising westward like many lines.
    const LAT: f64 = 40.0;
    const LON0: f64 = -90.0;
    /// Places along the line in metres from its start, with their units (made-up group 123456).
    const PLACES: [(f64, &str, bool); 6] = [
        (0.0, "1234560101", true),
        (2_400.0, "1234560103", false),
        (5_100.0, "1234560105", true),
        (7_300.0, "1234560107", false),
        (10_200.0, "1234560109", true),
        (13_000.0, "123456010b", false),
    ];
    const DETECTOR_M: f64 = 5_400.0;
    const DETECTOR_MP: f64 = 488.0;

    fn p() -> Params {
        Params::default()
    }

    /// A straight line east.
    fn straight(x: f64) -> (f64, f64) {
        (LAT, LON0 + x / (LAT.to_radians().cos() * 111_320.0))
    }

    /// A line that runs east 6 km, turns through a half circle and comes back west: straight-line
    /// distances fold over, distances along the track don't.
    fn hairpin(x: f64) -> (f64, f64) {
        let k = LAT.to_radians().cos() * 111_320.0;
        let (leg, r) = (6_000.0, 1_500.0);
        let half = std::f64::consts::PI * r;
        let (e, n) = if x <= leg {
            (x, 0.0)
        } else if x <= leg + half {
            let a = (x - leg) / r;
            (leg + r * a.sin(), r - r * a.cos())
        } else {
            (leg - (x - leg - half), 2.0 * r)
        };
        (LAT + n / 110_540.0, LON0 + e / k)
    }

    /// Trains at 20 m/s, alternating each way, each announced at the detector after its 1.6 km
    /// length has passed it; each place drops (to `stop`) when the head reaches it.
    fn traffic(trains: usize, stop: (i64, i64)) -> (Vec<Change>, Vec<Pass>) {
        let (mut ch, mut passes) = (vec![], vec![]);
        for n in 0..trains {
            let t0 = 10_000.0 + n as f64 * 7_200.0;
            let forward = n % 2 == 0;
            let v = 20.0;
            let start = if forward { -1_000.0 } else { 14_000.0 };
            let at = |x: f64| t0 + (x - start).abs() / v;
            passes.push(Pass { t: at(DETECTOR_M) + 1_600.0 / v, milepost: DETECTOR_MP, railroad: Some("XX".into()) });
            for &(x, w, cp) in &PLACES {
                let code = if cp { stop.1 } else { stop.0 };
                let twin = format!("{}{:02x}", &w[..8], u8::from_str_radix(&w[8..], 16).unwrap() + 1);
                for unit in [w.to_string(), twin] {
                    ch.push(Change { t: at(x) - 300.0, wiu: unit.clone(), codes: vec![3] });
                    ch.push(Change { t: at(x), wiu: unit, codes: vec![code] });
                }
            }
        }
        (ch, passes)
    }

    /// Mapped signals at every place but one, a stray one off the line, markers every mile (or
    /// kilometre) along it, and the track itself when `track`.
    fn osm(shape: fn(f64) -> (f64, f64), unit: &str, track: bool) -> Osm {
        let mut o = Osm::default();
        for (k, &(x, _, _)) in PLACES.iter().enumerate() {
            if k == 3 {
                continue; // not mapped
            }
            let (a, b) = (shape(x + 30.0), shape(x - 30.0));
            o.signals.push(Signal { id: 100 + k as i64, lat: a.0, lon: a.1, name: None });
            o.signals.push(Signal { id: 200 + k as i64, lat: b.0, lon: b.1, name: Some(format!("CP {k}")) });
        }
        let stray = shape(6_000.0);
        o.signals.push(Signal { id: 999, lat: stray.0 + 0.05, lon: stray.1, name: None });
        let per = if unit == "km" { 1000.0 } else { M_PER_MILE };
        // the detector's milepost, falling along the line, in the given unit
        let det = if unit == "km" { 785.4 } else { DETECTOR_MP };
        for k in 0..((14_000.0 / per) as usize + 1) {
            let x = k as f64 * per;
            let (lat, lon) = shape(x);
            o.stones.push(Milestone { at_m: (det + (DETECTOR_M - x) / per) * per, lat, lon });
        }
        if track {
            let nodes = (0..=280).map(|i| { let x = i as f64 * 50.0 - 500.0; let (a, b) = shape(x); (i as i64 + 1, a, b) }).collect();
            o.ways.push(Way { nodes });
        }
        o
    }

    fn placed(changes: &[Change], passes: &[Pass], o: &Osm, params: &Params) -> (Layout, Placement) {
        let (mut lay, drops) = layout(changes, &[], params);
        anchor(&mut lay, &drops, passes, params);
        let pl = place(&lay, o, params);
        (lay, pl)
    }

    /// Every proposal is at its own place, along `shape`.
    fn check(pl: &Placement, shape: fn(f64) -> (f64, f64)) {
        for pr in &pl.proposals {
            // a twin (even number) shares its odd-numbered partner's place
            let n = u8::from_str_radix(&pr.wiu[8..], 16).unwrap();
            let base = format!("{}{:02x}", &pr.wiu[..8], if n % 2 == 0 { n - 1 } else { n });
            let x = PLACES.iter().find(|q| q.1 == base).unwrap().0;
            let want = shape(x);
            assert!((pr.lat - want.0).abs() < 0.001 && (pr.lon - want.1).abs() < 0.001, "{} at {},{} want {want:?}", pr.wiu, pr.lat, pr.lon);
            assert_ne!(base, PLACES[3].1);
        }
    }

    #[test]
    fn the_order_comes_from_trains_and_twins_share_a_place() {
        let (ch, _) = traffic(6, (13, 15));
        let (lay, _) = layout(&ch, &[], &p());
        assert_eq!(lay.trains, 6);
        let firsts: Vec<&str> = lay.locations.iter().map(|l| l.units[0].as_str()).collect();
        let mut expect: Vec<&str> = PLACES.iter().map(|p| p.1).collect();
        if firsts[0] != expect[0] {
            expect.reverse();
        }
        assert_eq!(firsts, expect);
        assert!(lay.locations.iter().all(|l| l.units.len() == 2));
        assert_eq!(lay.locations.iter().filter(|l| l.control_point).count(), 3);
    }

    #[test]
    fn offsets_from_the_detector_cancel_the_train_length() {
        let (ch, passes) = traffic(6, (13, 15));
        let (mut lay, drops) = layout(&ch, &[], &p());
        anchor(&mut lay, &drops, &passes, &p());
        assert_eq!(lay.detector, Some((DETECTOR_MP, Some("XX".into()), 6)));
        // 20 m/s: each place's metres from the detector over 20, signed one way or the other
        let first = lay.locations.iter().find(|l| l.units[0] == PLACES[0].1).unwrap();
        let sign = first.offset_s.unwrap().signum() * (PLACES[0].0 - DETECTOR_M).signum();
        for l in &lay.locations {
            let x = PLACES.iter().find(|p| p.1 == l.units[0]).unwrap().0;
            let want = sign * (x - DETECTOR_M) / 20.0;
            assert!((l.offset_s.unwrap() - want).abs() < 1.0, "{}: {:?} want {want}", l.units[0], l.offset_s);
        }
    }

    #[test]
    fn places_go_on_the_mapped_signals_and_unmapped_ones_are_left() {
        let (ch, passes) = traffic(6, (13, 15));
        for track in [false, true] {
            let (_, pl) = placed(&ch, &passes, &osm(straight, "mi", track), &p());
            assert_eq!(pl.along_track, track);
            assert!(pl.mileposts, "{}", pl.note);
            assert_eq!(pl.matched, 5, "{}", pl.note);
            assert_eq!(pl.proposals.len(), 10, "{}", pl.note);
            check(&pl, straight);
        }
    }

    #[test]
    fn the_same_evidence_gives_the_same_answer() {
        let (ch, passes) = traffic(6, (13, 15));
        let o = osm(hairpin, "mi", true);
        let runs: Vec<Vec<Proposal>> = (0..5).map(|_| placed(&ch, &passes, &o, &p()).1.proposals).collect();
        assert!(!runs[0].is_empty());
        assert!(runs.iter().all(|r| *r == runs[0]));
    }

    #[test]
    fn along_the_track_a_curving_line_is_placed_right() {
        let (ch, passes) = traffic(6, (13, 15));
        let (_, pl) = placed(&ch, &passes, &osm(hairpin, "mi", true), &p());
        assert!(pl.along_track);
        assert_eq!(pl.matched, 5, "{}", pl.note);
        check(&pl, hairpin);
        // without the track, distances would fold over: it refuses rather than guess
        let (_, flat) = placed(&ch, &passes, &osm(hairpin, "mi", false), &p());
        assert!(flat.proposals.is_empty(), "{:?}", flat.proposals);
        assert!(flat.note.contains("straight line"), "{}", flat.note);
    }

    #[test]
    fn kilometre_markers_and_other_codes_work_when_set() {
        // a railroad whose stop codes are 1 and 4, counting in kilometres
        let (ch, mut passes) = traffic(6, (1, 4));
        for x in passes.iter_mut() {
            x.milepost = 785.4;
        }
        let o = osm(straight, "km", true);
        let (_, none) = placed(&ch, &passes, &o, &p());
        assert!(none.proposals.is_empty());
        let mut params = p();
        params.set("stop_codes", &json!([1, 4])).unwrap();
        params.set("control_point_codes", &json!([4])).unwrap();
        params.set("milepost_unit", &json!("km")).unwrap();
        let (lay, pl) = placed(&ch, &passes, &o, &params);
        assert_eq!(lay.locations.iter().filter(|l| l.control_point).count(), 3);
        assert!(pl.mileposts, "{}", pl.note);
        assert_eq!(pl.matched, 5, "{}", pl.note);
        check(&pl, straight);
    }

    #[test]
    fn the_detector_and_railroad_can_be_chosen() {
        let (ch, mut passes) = traffic(6, (13, 15));
        // a busier detector of another railroad, far off: by default it would be used
        for k in 0..10 {
            passes.push(Pass { t: 5_000.0 + k as f64 * 100.0, milepost: 12.0, railroad: Some("YY".into()) });
        }
        let (mut lay, drops) = layout(&ch, &[], &p());
        anchor(&mut lay, &drops, &passes, &p());
        assert_eq!(lay.detector.as_ref().map(|d| d.0), Some(12.0));
        for (k, v) in [("detector_milepost", json!(DETECTOR_MP)), ("railroad", json!("xx"))] {
            let mut params = p();
            params.set(k, &v).unwrap();
            let (mut lay, drops) = layout(&ch, &[], &params);
            anchor(&mut lay, &drops, &passes, &params);
            assert_eq!(lay.detector.as_ref().map(|d| d.0), Some(DETECTOR_MP), "{k}");
            assert_eq!(lay.other_detectors.len(), if k == "railroad" { 0 } else { 1 });
        }
    }

    #[test]
    fn two_places_at_one_distance_are_left_out() {
        let (ch, passes) = traffic(6, (13, 15));
        let (mut lay, drops) = layout(&ch, &[], &p());
        anchor(&mut lay, &drops, &passes, &p());
        // the last place timed just like the one before it: either could be at that signal
        let k = lay.locations.iter().position(|l| l.units[0] == PLACES[4].1).unwrap();
        let j = lay.locations.iter().position(|l| l.units[0] == PLACES[5].1).unwrap();
        lay.locations[j].offset_s = lay.locations[k].offset_s.map(|o| o + 2.0);
        let pl = place(&lay, &osm(straight, "mi", true), &p());
        assert!(!pl.proposals.iter().any(|q| q.location == k || q.location == j), "{:?}", pl.proposals);
        assert!(!pl.proposals.is_empty(), "{}", pl.note);
    }

    #[test]
    fn a_marker_from_another_line_does_not_spoil_the_mileposts() {
        let marks: Vec<(f64, f64)> = (0..8).map(|k| (k as f64 * M_PER_MILE, 488.0 - k as f64))
            .chain([(3_000.0, 595.0)]).collect();
        let (a, b) = milepost_fit(&marks, M_PER_MILE).expect("a fit");
        assert!((a - 488.0).abs() < 0.1 && (b * M_PER_MILE + 1.0).abs() < 0.01, "{a} {b}");
    }

    #[test]
    fn nothing_is_proposed_without_a_detector_or_enough_signals() {
        let (ch, passes) = traffic(6, (13, 15));
        let (mut lay, drops) = layout(&ch, &[], &p());
        let o = osm(straight, "mi", true);
        assert!(place(&lay, &o, &p()).proposals.is_empty());
        anchor(&mut lay, &drops, &passes, &p());
        let few = Osm { signals: o.signals[..4].to_vec(), ..o.clone() };
        assert!(place(&lay, &few, &p()).proposals.is_empty());
    }

    #[test]
    fn settings_are_checked_and_only_changes_shown() {
        let ps = Params::from_json(&json!({"speed_mph": [20, 60], "tolerance_m": 400, "railroad": "NS"})).unwrap();
        assert!((ps.speed_max - 60.0 * M_PER_MILE / 3600.0).abs() < 1e-9);
        assert_eq!(ps.changed(), json!({"railroad": "NS", "speed_mph": [20.0, 60.0], "tolerance_m": 400.0}));
        assert_eq!(Params::from_json(&Value::Null).unwrap(), Params::default());
        assert!(Params::from_json(&json!({"speed_mph": [60, 20]})).is_err());
        assert!(Params::from_json(&json!({"stop_codes": [40]})).is_err());
        let e = Params::from_json(&json!({"tolerence_m": 1})).unwrap_err();
        assert!(e.contains("tolerance_m"), "{e}");
    }

    #[test]
    fn only_positions_the_last_runs_agree_on_are_steady() {
        let run = |v: &[(&str, f64)]| -> Run { v.iter().map(|(w, x)| (w.to_string(), (40.0, *x))).collect() };
        let a = run(&[("1234560101", -90.0), ("1234560103", -90.1)]);
        let b = run(&[("1234560101", -90.0), ("1234560103", -90.2)]);
        assert!(steady(&[a.clone(), a.clone()], 3).is_empty());
        assert_eq!(steady(&[a.clone(), a.clone(), a.clone()], 3), vec!["1234560101", "1234560103"]);
        assert_eq!(steady(&[a.clone(), a.clone(), b.clone()], 3), vec!["1234560101"]);
        // a run with nothing to propose starts the count again
        assert!(steady(&[a.clone(), a.clone(), Run::new(), a.clone()], 3).is_empty());
        assert_eq!(steady(&[b], 1), vec!["1234560101", "1234560103"]);
    }

    #[test]
    fn the_place_trains_drop_at_closest_approach_goes_at_the_nearest_signal() {
        // made-up NS-style IDs: 7 550 61 1126 01 and 02 are two tracks at one place
        let a = format!("{:x}", 755_061_112_601u64);
        let b = format!("{:x}", 755_061_112_602u64);
        let other = format!("{:x}", 755_061_114_201u64);
        assert_eq!(place_key(&a), place_key(&b));
        let pass = |c: f64, drops: &[(&str, f64)]| json!({"closest": c, "drops": drops.iter().map(|(w, t)| json!({"wiu": w, "t": t})).collect::<Vec<_>>()});
        let passes = vec![
            pass(1000.0, &[(&a, 1010.0), (&other, 700.0)]),
            // 100 s after closest approach: inside the default window of 120 s
            pass(5000.0, &[(&b, 5100.0)]),
            pass(9000.0, &[(&a, 9040.0), (&other, 9055.0)]),
            json!({"closest": null, "drops": [{"wiu": a, "t": 1.0}]}),
        ];
        let top = closest_places(&passes, 120.0);
        assert_eq!(top[0].2, 3);
        assert_eq!(top[0].1, vec![a.clone(), b.clone()]);
        let osm = Osm { signals: vec![Signal { id: 7, lat: 40.001, lon: -90.0, name: Some("CP A".into()) }], ..Default::default() };
        let mut p = Params::default();
        assert!(closest_approach(&passes, &osm, &p, 3, 1000.0).is_empty(), "no receiver position: nothing");
        p.set("receiver_position", &json!([40.0, -90.0])).unwrap();
        let got = closest_approach(&passes, &osm, &p, 3, 1000.0);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].lat, got[0].lon, got[0].signals.clone()), (40.001, -90.0, vec![7]));
        assert!(closest_approach(&passes, &osm, &p, 4, 1000.0).is_empty(), "too few passes");
        let mut narrow = p.clone();
        narrow.set("closest_window_s", &json!(60)).unwrap();
        assert!(closest_approach(&passes, &osm, &narrow, 3, 1000.0).is_empty(), "the 100 s drop is outside 60 s");
        // no mapped signal near: only when the owner shares the receiver's position
        let far = Osm { signals: vec![Signal { id: 8, lat: 41.0, lon: -90.0, name: None }], ..Default::default() };
        assert!(closest_approach(&passes, &far, &p, 3, 1000.0).is_empty());
        p.set("share_receiver_position", &json!(true)).unwrap();
        assert_eq!(closest_approach(&passes, &far, &p, 3, 1000.0)[0].lat, 40.0);
    }

    #[test]
    fn a_record_has_the_contract_fields_and_a_stable_id() {
        let pr = Proposal { wiu: "1234560101".into(), lat: 40.0, lon: -90.0, signals: vec![1], name: None, location: 0 };
        let r = record(&NS, &pr, 1_000.25, "osm_signal");
        assert_eq!(r["type"], "place");
        assert_eq!(r["basis"], "osm_signal");
        assert_eq!(r["wiu"], "1234560101");
        assert_eq!(r["id"], record(&NS, &pr, 2_000.0, "closest_approach")["id"]);
        assert_eq!(r["id"], Uuid::new_v5(&NS, b"itc-place:1234560101:40.00000:-90.00000").to_string());
    }

    #[test]
    fn overpass_signals_markers_and_track_are_read() {
        let v = json!({"elements": [
            {"type": "node", "id": 1, "lat": 40.0, "lon": -90.0, "tags": {"railway": "signal", "ref": "CP A"}},
            {"type": "node", "id": 2, "lat": 40.0, "lon": -90.1, "tags": {"railway": "milestone", "railway:position": "mi:485.0"}},
            {"type": "node", "id": 3, "lat": 40.0, "lon": -90.2, "tags": {"railway": "milestone", "railway:position": "km:12.5"}},
            {"type": "node", "id": 4, "lat": 40.0, "lon": -90.3, "tags": {"railway": "milestone", "railway:position": "7"}},
            {"type": "node", "id": 5, "lat": 40.0, "lon": -90.2, "tags": {"railway": "crossing"}},
            {"type": "way", "id": 6, "nodes": [10, 11], "geometry": [{"lat": 40.0, "lon": -90.0}, {"lat": 40.0, "lon": -90.01}],
             "tags": {"railway": "rail"}}]});
        let o = read_osm(&v, "km");
        assert_eq!(o.signals.len(), 1);
        assert_eq!(o.signals[0].name.as_deref(), Some("CP A"));
        let at: Vec<f64> = o.stones.iter().map(|s| s.at_m.round()).collect();
        assert_eq!(at, vec![(485.0 * M_PER_MILE).round(), 12_500.0, 7_000.0]);
        assert_eq!(o.ways.len(), 1);
        assert_eq!(o.ways[0].nodes[1].0, 11);
        assert!(osm_query(&[(40.0, -90.0)], 30_000.0, true).contains("way[\"railway\"=\"rail\"]"));
    }
}
