//! Detector voice profiles: how one style of defect detector speaks, so a transcript of its
//! announcement can be read. The built-in profiles are the files in the repository's profiles/
//! folder (see its README); owners can add custom ones on the receiver.

use crate::text::{self, spoken_similarity, spoken_value_after};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

include!(concat!(env!("OUT_DIR"), "/builtin_profiles.rs"));

/// The profile for detectors of a railroad without one of its own.
pub const GENERIC: &str = "generic-v1";
/// Custom profile ids start with this, so they never collide with built-in ones.
pub const CUSTOM_PREFIX: &str = "custom-";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variable {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default = "digits")]
    pub spoken: String,
}

fn digits() -> String {
    "digits".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Audio {
    pub filter: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllClear {
    pub phrase: String,
    pub times: u32,
    pub min_wording: f64,
    /// how speech recognition mishears the phrase in this voice ("so defects" for "no defects"),
    /// read as the phrase so a mishearing is never taken for a defect report
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heard_as: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub min_s: f64,
    pub max_s: f64,
    pub max_other_channels: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub railroad: String,
    #[serde(default)]
    pub description: String,
    pub template: String,
    #[serde(default)]
    pub variables: Vec<Variable>,
    pub audio: Audio,
    pub all_clear: AllClear,
    #[serde(default)]
    pub defect_words: Vec<String>,
    pub trigger: Trigger,
}

/// What a transcript says, read with a profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// values read and within their limits, by variable name
    pub fields: Map<String, Value>,
    /// values read but outside their limits (misheard), by variable name
    pub out_of_range: Map<String, Value>,
    /// share of the template's words heard, in order
    pub wording: f64,
    /// how many times the all-clear phrase was heard
    pub all_clear_said: u32,
    /// defect words heard outside the all-clear phrase
    pub defect_words: Vec<String>,
}

impl Reading {
    /// The detector's usual all-clear: its phrase said often enough, enough of its wording
    /// heard, and nothing about a defect.
    pub fn all_clear(&self, p: &Profile) -> bool {
        self.all_clear_said >= p.all_clear.times && self.wording >= p.all_clear.min_wording && self.defect_words.is_empty()
    }

    /// A defect report: a defect word heard outside the all-clear phrase.
    pub fn defect(&self) -> bool {
        !self.defect_words.is_empty()
    }
}

impl Profile {
    /// Every built-in profile, in file name order.
    pub fn builtin() -> Vec<Profile> {
        BUILTIN.iter().filter_map(|(file, text)| match serde_json::from_str::<Profile>(text) {
            Ok(p) => Some(p),
            Err(e) => {
                log::error!("built-in profile {file} is not valid: {e}");
                None
            }
        }).collect()
    }

    /// The problems that make a profile unusable; empty when it is fine.
    pub fn problems(&self) -> Vec<String> {
        let mut out = vec![];
        let id_ok = !self.id.is_empty() && self.id.len() <= 60
            && self.id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.');
        if !id_ok {
            out.push("id: lowercase letters, digits, '-' and '.' only, at most 60".into());
        }
        if self.name.trim().is_empty() || self.name.len() > 80 {
            out.push("name: 1 to 80 characters".into());
        }
        if self.template.trim().is_empty() || self.template.len() > 300 {
            out.push("template: 1 to 300 characters".into());
        }
        let placeholders = self.placeholders();
        for v in &self.variables {
            if !matches!(v.kind.as_str(), "decimal" | "integer") {
                out.push(format!("variable {}: type must be decimal or integer", v.name));
            }
            if !matches!(v.spoken.as_str(), "digits" | "words") {
                out.push(format!("variable {}: spoken must be digits or words", v.name));
            }
            if let (Some(a), Some(b)) = (v.min, v.max) {
                if a > b {
                    out.push(format!("variable {}: min is above max", v.name));
                }
            }
        }
        for name in &placeholders {
            if !self.variables.iter().any(|v| &v.name == name) {
                out.push(format!("template uses {{{name}}} but no variable defines it"));
            }
        }
        if !matches!(self.audio.filter.as_str(), "voice" | "none") {
            out.push("audio.filter must be voice or none".into());
        }
        if self.all_clear.phrase.trim().is_empty() || self.all_clear.times == 0 {
            out.push("all_clear: a phrase, said at least once".into());
        }
        if !(0.0..=1.0).contains(&self.all_clear.min_wording) {
            out.push("all_clear.min_wording: 0 to 1".into());
        }
        if !(self.trigger.min_s > 0.0 && self.trigger.min_s < self.trigger.max_s && self.trigger.max_s <= 120.0) {
            out.push("trigger: 0 < min_s < max_s <= 120".into());
        }
        out
    }

    /// A transcript with this voice's known mishearings of the all-clear phrase put right
    /// ("so defects" becomes "no defects"), keeping the rest as heard. Applied right after speech
    /// recognition, so what is read, stored and sent says what the detector said.
    pub fn correct(&self, transcript: &str) -> String {
        let mut out = transcript.to_string();
        for misheard in &self.all_clear.heard_as {
            let pat = format!(r"(?i)\b{}\b", regex::escape(misheard));
            if let Ok(re) = regex::Regex::new(&pat) {
                let phrase = &self.all_clear.phrase;
                out = re.replace_all(&out, |c: &regex::Captures| {
                    // keep a capital at the start of a sentence ("So defects." becomes "No defects.")
                    if c[0].starts_with(|ch: char| ch.is_uppercase()) {
                        let mut it = phrase.chars();
                        it.next().map(|f| f.to_uppercase().collect::<String>() + it.as_str()).unwrap_or_default()
                    } else {
                        phrase.clone()
                    }
                }).into_owned();
            }
        }
        // "no" heard as "oh" and written as a zero after a value with a fixed number of decimals:
        // "milepost 475.80 defects" is milepost 475.8, then "no defects"
        let phrase = self.all_clear.phrase.trim();
        if let Some(rest) = phrase.strip_prefix("no ").or_else(|| phrase.strip_prefix("No ")) {
            for v in self.variables.iter().filter(|v| v.kind == "decimal") {
                let Some(d) = v.decimals.filter(|d| *d > 0) else { continue };
                let pat = format!(r"(?i)(\d+\.\d{{{d}}})(?:0+|[\s,]+(?:0|oh|zero))[\s,]*({}s?)\b", regex::escape(rest.trim_end_matches('s')));
                if let Ok(re) = regex::Regex::new(&pat) {
                    out = re.replace_all(&out, |c: &regex::Captures| format!("{}, no {}", &c[1], &c[2])).into_owned();
                }
            }
        }
        out
    }

    /// The `{name}` placeholders of the template, in order.
    pub fn placeholders(&self) -> Vec<String> {
        let mut out = vec![];
        let mut rest = self.template.as_str();
        while let Some(i) = rest.find('{') {
            let Some(j) = rest[i..].find('}') else { break };
            out.push(rest[i + 1..i + j].to_string());
            rest = &rest[i + j + 1..];
        }
        out
    }

    /// The speech prompt: the wording with the values left out, since giving the expected
    /// number biases what is heard.
    pub fn prompt(&self) -> String {
        text::prompt_from_template(&self.template)
    }

    /// The words just before a placeholder in the template ("milepost", "total axle"), which
    /// say where its value is spoken.
    fn anchor(&self, name: &str) -> Option<String> {
        let at = self.template.find(&format!("{{{name}}}"))?;
        let before = &self.template[..at];
        let words: Vec<&str> = before.split(|c: char| !(c.is_alphanumeric() || c == '\''))
            .filter(|w| !w.is_empty()).collect();
        let n = words.len().min(2);
        let tail = &words[words.len() - n..];
        // a lone short word ("at") says too little; one word is enough when it is a real anchor
        Some(if n == 2 && tail[1].len() >= 5 { tail[1].to_string() } else { tail.join(" ") })
    }

    /// Read a transcript: the values (within their limits), how much of the wording was heard,
    /// the all-clear phrase, and any defect words outside it.
    pub fn read(&self, transcript: &str) -> Reading {
        let mut r = Reading::default();
        for v in &self.variables {
            let anchor = self.anchor(&v.name).unwrap_or_else(|| v.name.clone());
            let decimal = v.kind == "decimal";
            let value = spoken_value_after(transcript, &anchor, decimal)
                .or_else(|| (v.name == "milepost").then(|| text::spoken_decimal(transcript)).flatten());
            let Some(x) = value else { continue };
            let x = match (decimal, v.decimals) {
                (true, Some(d)) => (x * 10f64.powi(d as i32)).trunc() / 10f64.powi(d as i32),
                (false, _) => x.trunc(),
                _ => x,
            };
            let json = if decimal { serde_json::json!(x) } else { serde_json::json!(x as i64) };
            if v.min.is_some_and(|m| x < m) || v.max.is_some_and(|m| x > m) {
                r.out_of_range.insert(v.name.clone(), json);
            } else {
                r.fields.insert(v.name.clone(), json);
            }
        }
        let lower = self.correct(transcript).to_lowercase();
        let phrase = self.all_clear.phrase.to_lowercase();
        // the wording heard, with the voice's known mishearings put right
        r.wording = spoken_similarity(&lower, &self.template);
        let singular = phrase.trim_end_matches('s').to_string();
        r.all_clear_said = lower.matches(singular.as_str()).count() as u32;
        // defect words outside the all-clear phrase ("no defects" mentions "defect")
        let outside = lower.replace(&phrase, " ").replace(&singular, " ");
        r.defect_words = self.defect_words.iter().filter(|w| outside.contains(&w.to_lowercase())).cloned().collect();
        r
    }

    /// As JSON, with `builtin` saying where it came from.
    pub fn to_value(&self, builtin: bool) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            o.insert("builtin".into(), Value::Bool(builtin));
        }
        v
    }
}

/// The default profile for a railroad's detectors: its own built-in one, else the generic one.
pub fn default_for(profiles: &[Profile], railroad: &str) -> Option<String> {
    profiles.iter().find(|p| !p.railroad.is_empty() && p.railroad.eq_ignore_ascii_case(railroad)).map(|p| p.id.clone())
        .or_else(|| profiles.iter().find(|p| p.id == GENERIC).map(|p| p.id.clone()))
}
