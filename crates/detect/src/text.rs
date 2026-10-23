//! The text side of transcription: the speech model's constants and filters, and the defect
//! detector parser. The model itself is in the receiver.

use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

// small.en, since speech recognition runs only inside detector messages: it read the milepost
// number of 10 of 10 real announcements against 7 of 10 for base.en (about 88 s of CPU each
// against 40 s on two threads, which is affordable for those rare calls).
pub const MODEL: &str = "small.en";
pub const ENGINE: &str = "faster-whisper-small.en";
pub const PROMPT: &str = "Norfolk Southern detector, milepost, track one, no defects, total axles, train speed, \
                          temperature. CSX. Dispatcher. Clear signal. Approach. Over.";
pub const NUMBER_PROMPT: &str = "Norfolk Southern detector, milepost 123.4.";
pub const MIN_AUDIO_S: f64 = 1.0;
pub const REPEAT_MIN_S: f64 = 10.0; // a detector's repeat, start to start
pub const REPEAT_MAX_S: f64 = 40.0;
pub const DETECTOR_CLIP_KEEP: usize = 500; // state/detector_clips: newest detector calls
pub const MIN_CONFIDENCE: f64 = 0.45;
pub const MAX_NO_SPEECH: f64 = 0.6;
pub const FILLERS: [&str; 12] = [
    "thank you.", "thanks for watching!", "you", "bye.", "thank you for watching.", "so.",
    "noaa weather radio.", "see you next time.", "i'll see you next time.",
    "see you guys next time.", "good job. see you guys next time.",
    "nasa jet propulsion laboratory, california institute of technology",
];
pub const NUMBER_WORDS: [(&str, i64); 6] = [("one", 1), ("two", 2), ("three", 3), ("four", 4), ("five", 5), ("six", 6)];
pub const DIGIT_WORDS: [(&str, &str); 11] = [
    ("zero", "0"), ("oh", "0"), ("one", "1"), ("two", "2"), ("three", "3"), ("four", "4"),
    ("five", "5"), ("six", "6"), ("seven", "7"), ("eight", "8"), ("nine", "9"),
];

/// `text.lower() in FILLERS`: speech recognition's usual inventions on noise.
pub fn is_filler(text: &str) -> bool {
    FILLERS.contains(&text.to_lowercase().as_str())
}

fn cached(cell: &'static OnceLock<Regex>, pat: impl FnOnce() -> String) -> &'static Regex {
    cell.get_or_init(|| Regex::new(&pat()).expect("regex"))
}

/// A milepost from speech recognition text. Detectors say it digit by digit ("four seven
/// five point eight"), which comes out as "475.8", "4, 7, 5, .8" or words; join the digits and
/// keep one decimal (a trailing "475.87" is usually the next word leaking in).
pub fn milepost_from_text(text: &str) -> Option<f64> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = cached(&R, || {
        let words: Vec<&str> = DIGIT_WORDS.iter().map(|(w, _)| *w).collect();
        format!(r"\d+|\.|point|{}", words.join("|"))
    });
    let lower = text.to_lowercase();
    let mut digits = String::new();
    for m in r.find_iter(&lower) {
        let t = m.as_str();
        if t == "." || t == "point" {
            digits.push('.');
        } else if let Some((_, d)) = DIGIT_WORDS.iter().find(|(w, _)| *w == t) {
            digits.push_str(d);
        } else {
            digits.push_str(t);
        }
    }
    let (whole, frac) = match digits.split_once('.') {
        Some((w, f)) => (w.to_string(), f.replace('.', "")),
        None => (digits.clone(), String::new()),
    };
    if whole.is_empty() {
        return None;
    }
    let value: f64 = if frac.is_empty() {
        ascii_digits(&whole)?.parse().ok()?
    } else {
        let first: String = frac.chars().take(1).collect();
        format!("{}.{}", ascii_digits(&whole)?, ascii_digits(&first)?).parse().ok()?
    };
    (0.0..=10_000.0).contains(&value).then_some(value)
}

/// Unicode decimal digits as ASCII ones (a number may be written in any script's digits).
fn ascii_digits(s: &str) -> Option<String> {
    s.chars().map(digit_value).collect()
}

/// The ASCII digit for a Unicode decimal digit (category Nd). Unicode keeps every Nd set as a
/// full run of ten from 0 to 9, and adjacent runs (the mathematical digits) are back to back,
/// so a digit's value is the count of Nd code points before it in its run, modulo 10.
fn digit_value(c: char) -> Option<char> {
    static ND: OnceLock<Regex> = OnceLock::new();
    let nd = cached(&ND, || r"^\d$".into());
    let is_nd = |cp: u32| char::from_u32(cp).is_some_and(|c| nd.is_match(c.encode_utf8(&mut [0; 4])));
    if c.is_ascii_digit() {
        return Some(c);
    }
    if !is_nd(c as u32) {
        return None;
    }
    let mut cp = c as u32;
    let mut before = 0u32;
    while cp > 0 && is_nd(cp - 1) {
        cp -= 1;
        before += 1;
    }
    char::from_digit(before % 10, 10)
}

/// Whisper's loop on noise: the same word three times running ("no, no, no").
pub fn repetitive(text: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = cached(&R, || r"[a-z0-9']+".into());
    let lower = text.to_lowercase();
    let words: Vec<&str> = r.find_iter(&lower).map(|m| m.as_str()).collect();
    words.windows(3).any(|w| w[0] == w[1] && w[1] == w[2])
}

/// Words that speech recognition writes for numbers, beyond single digits.
const TEENS: [(&str, i64); 10] = [
    ("ten", 10), ("eleven", 11), ("twelve", 12), ("thirteen", 13), ("fourteen", 14), ("fifteen", 15),
    ("sixteen", 16), ("seventeen", 17), ("eighteen", 18), ("nineteen", 19),
];
const TENS: [(&str, i64); 8] = [
    ("twenty", 20), ("thirty", 30), ("forty", 40), ("fifty", 50), ("sixty", 60), ("seventy", 70),
    ("eighty", 80), ("ninety", 90),
];

/// One spoken number token: digits as written, a digit word, a teen, or a tens word (which may
/// take a following digit: "sixty eight"). Returns the digits it stands for.
fn number_tokens(words: &[&str]) -> (String, usize) {
    let digit = |w: &str| DIGIT_WORDS.iter().find(|(d, _)| *d == w).map(|(_, v)| v.to_string());
    let mut out = String::new();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        if !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()) {
            out.push_str(w);
        } else if let Some(d) = digit(w) {
            out.push_str(&d);
        } else if let Some((_, n)) = TEENS.iter().find(|(t, _)| *t == w) {
            out.push_str(&n.to_string());
        } else if let Some((_, n)) = TENS.iter().find(|(t, _)| *t == w) {
            match words.get(i + 1).and_then(|n2| digit(n2)).filter(|d| d != "0") {
                Some(d) => {
                    out.push_str(&(n + d.parse::<i64>().unwrap_or(0)).to_string());
                    i += 1;
                }
                None => out.push_str(&n.to_string()),
            }
        } else {
            break;
        }
        i += 1;
    }
    (out, i)
}

fn words_of(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '.'))
        .flat_map(|w| {
            // "68.1" stays one token; a lone "." or "point" marks the decimal
            let parts: Vec<&str> = w.trim_matches('.').split('.').collect();
            if w.starts_with('.') && w.trim_matches('.').chars().all(|c| c.is_ascii_digit()) && w.len() > 1 {
                // ".1" as in "6, 8, .1"
                vec!["point".to_string(), w.trim_matches('.').to_string()]
            } else if parts.len() > 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())) {
                // "6.8.1": digits read one by one, the last dot the decimal point (68.1)
                vec![format!("{}.{}", parts[..parts.len() - 1].concat(), parts[parts.len() - 1])]
            } else if w.contains('.') && !w.chars().all(|c| c == '.') && w.chars().any(|c| c.is_ascii_digit()) {
                vec![w.trim_matches('.').to_string()]
            } else if w.chars().all(|c| c == '.') && !w.is_empty() {
                vec!["point".to_string()]
            } else {
                // "one." at the end of a sentence
                vec![w.trim_matches('.').to_string()]
            }
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// A whole number spoken after one of the given phrases, digit by digit or in words: "total
/// axles one, seven, four" gives 174, as does "total axles 174"
/// (detectors read numbers digit by digit; "a hundred seventy four" is not handled).
pub fn spoken_count_after(text: &str, phrases: &[&str]) -> Option<i64> {
    let words = words_of(text);
    for phrase in phrases {
        let pw: Vec<&str> = phrase.split_whitespace().collect();
        for start in 0..words.len().saturating_sub(pw.len() - 1) {
            if words[start..start + pw.len()].iter().zip(&pw).all(|(a, b)| a == b) {
                let rest: Vec<&str> = words[start + pw.len()..].iter().map(String::as_str).collect();
                let (digits, _) = number_tokens(&rest);
                if !digits.is_empty() {
                    return digits.parse().ok();
                }
            }
        }
    }
    None
}

/// The milepost said after "milepost" (or "mile post"), in any of the forms speech recognition
/// writes: "68.1", "six eight point one", "sixty eight point one", "six, eight points, one", or
/// a mishearing like "sixteen point one". One decimal is kept.
pub fn spoken_milepost(text: &str) -> Option<f64> {
    spoken_value_after(text, "milepost", true).or_else(|| spoken_value_after(text, "mile post", true))
}

/// The number said after a phrase ("milepost", "total axle", "track"), in the forms speech
/// recognition writes: digits ("68.1", "174", "1-7-4"), digit words ("six eight point one", "one,
/// seven, four"), teens and tens, or a mishearing like "sixteen point one". With `decimal` a
/// following "point" and one more digit are read; otherwise only the whole number. One stray
/// word may come between the phrase and the number ("milepost, track six, eight points, one").
pub fn spoken_value_after(text: &str, phrase: &str, decimal: bool) -> Option<f64> {
    let words = words_of(text);
    let want: Vec<String> = words_of(phrase).into_iter().map(|w| w.trim_end_matches(':').to_string()).collect();
    if want.is_empty() || words.len() < want.len() {
        return None;
    }
    let at = (0..=words.len() - want.len())
        .find(|&i| words[i..i + want.len()].iter().zip(&want).all(|(a, b)| a == b || *a == format!("{b}s")))
        .map(|i| i + want.len())?;
    let rest: Vec<&str> = words[at..].iter().map(String::as_str).collect();
    if let Some(first) = rest.first() {
        if first.contains('.') {
            // a count heard with a decimal point ("total axle 5.6") is misheard, not 5
            if !decimal {
                return None;
            }
            let v = first.parse::<f64>().ok()?;
            return Some((v * 10.0).trunc() / 10.0);
        }
    }
    let (skip, (whole, used)) = match number_tokens(&rest) {
        (w, u) if !w.is_empty() => (0, (w, u)),
        _ if rest.len() > 1 => (1, number_tokens(&rest[1..])),
        _ => return None,
    };
    if whole.is_empty() {
        return None;
    }
    let rest = &rest[skip..];
    let mut whole = whole;
    let frac = if !decimal { None } else {
        match rest.get(used) {
            Some(&"point") | Some(&"points") => decimal_digit(&rest[used + 1..]),
            // "6/8.1": digits, then the rest of the number with its decimal
            Some(t) if t.contains('.') => match t.split_once('.') {
                Some((w, f)) if w.chars().all(|c| c.is_ascii_digit()) && f.starts_with(|c: char| c.is_ascii_digit()) => {
                    whole.push_str(w);
                    f.chars().next()
                }
                _ => None,
            },
            _ => None,
        }
    };
    let text = match frac {
        Some(d) => format!("{whole}.{d}"),
        None => whole,
    };
    text.parse().ok()
}

/// The digit said after "point": a digit, or "on", which speech recognition writes for "one"
/// there ("six, eight points on").
fn decimal_digit(words: &[&str]) -> Option<char> {
    number_tokens(words).0.chars().next().or_else(|| (words.first() == Some(&"on")).then_some('1'))
}

/// The first number with a decimal spoken anywhere ("six eight point one", "68.1"): the milepost
/// when the word "milepost" itself was misheard. Whole numbers are skipped, since axle counts and
/// speeds are whole, and the search stops at the axle count or speed: a decimal there is a
/// misheard count ("total axle 5.6"), never the milepost.
pub fn spoken_decimal(text: &str) -> Option<f64> {
    let words = words_of(text);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let mut i = 0;
    while i < words.len() {
        if matches!(words[i], "axle" | "axles" | "speed") {
            return None;
        }
        if words[i].contains('.') {
            if let Ok(v) = words[i].parse::<f64>() {
                return Some((v * 10.0).trunc() / 10.0);
            }
        }
        let (whole, used) = number_tokens(&words[i..]);
        if !whole.is_empty() && matches!(words.get(i + used), Some(&"point") | Some(&"points")) {
            if let Some(d) = decimal_digit(&words[i + used + 1..]) {
                return format!("{whole}.{d}").parse().ok();
            }
        }
        i += used.max(1);
    }
    None
}

/// Words in the form of a detector's `spoken` text (contract v1.6): digits one by one, "point"
/// for the decimal, short capitals as letters, so "CSX ... 68.1" and "c s x ... six eight point
/// one" compare equal. Placeholders like "{axles}" become None (any words may stand there).
fn spoken_words(text: &str) -> Vec<Option<String>> {
    let mut out = vec![];
    for raw in text.to_lowercase().split(|c: char| c.is_whitespace() || c == ',' || c == '/' || c == '-' || c == '?') {
        let w = raw.trim_matches(|c: char| !(c.is_alphanumeric() || c == '{' || c == '}' || c == '.'));
        let w = w.trim_end_matches('.');
        if w.is_empty() {
            continue;
        }
        if w.starts_with('{') && w.ends_with('}') {
            out.push(None);
        } else if w.chars().all(|c| c.is_ascii_digit() || c == '.') {
            for c in w.chars() {
                out.push(Some(if c == '.' { "point".to_string() } else { c.to_string() }));
            }
        } else if let Some((_, d)) = DIGIT_WORDS.iter().find(|(word, _)| *word == w) {
            out.push(Some(d.to_string()));
        } else if w.len() == 1 && w.chars().all(|c| c.is_ascii_alphabetic()) {
            // "c s x": letters spelled out are joined back into the word
            match out.last_mut() {
                Some(Some(prev)) if prev.len() < 4 && prev.chars().all(|c| c.is_ascii_alphabetic())
                    && !DIGIT_WORDS.iter().any(|(_, d)| d == prev) && prev != "point" => prev.push_str(w),
                _ => out.push(Some(w.to_string())),
            }
        } else {
            out.push(Some(w.replace('.', "")));
        }
    }
    out
}

/// How much of a detector's expected wording (its `spoken` text) was heard, in order: the share
/// of its fixed words found in the transcript as a common subsequence. Placeholders are skipped.
pub fn spoken_similarity(heard: &str, spoken: &str) -> f64 {
    let want: Vec<String> = spoken_words(spoken).into_iter().flatten().collect();
    let got: Vec<String> = spoken_words(heard).into_iter().flatten().collect();
    if want.is_empty() {
        return 0.0;
    }
    let mut row = vec![0usize; got.len() + 1];
    for w in &want {
        let mut prev = 0;
        for (j, g) in got.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = if w == g { prev + 1 } else { row[j + 1].max(row[j]) };
            prev = cur;
        }
    }
    row[got.len()] as f64 / want.len() as f64
}

/// A detector's wording as a speech prompt: placeholders dropped, with the punctuation left
/// around them tidied.
pub fn prompt_from_template(template: &str) -> String {
    static PH: OnceLock<Regex> = OnceLock::new();
    let ph = cached(&PH, || r"\s*\{[a-z_]+\}".into());
    let t = ph.replace_all(template, "").into_owned();
    let t = t.replace(" ,", ",").replace(" .", ".").replace(":.", ".").replace(":,", ",");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Detector fields from announcement text. With trust_defects false (text from speech
/// recognition) "defects" is never set: on real recordings it turned a detector's "no defects"
/// into "so, defect" 4 times in 6, so neither reading can be trusted.
pub fn parse(text: &str, trust_defects: bool) -> Option<Value> {
    static TRACK: OnceLock<Regex> = OnceLock::new();
    static PATS: OnceLock<Vec<(&'static str, Regex, bool)>> = OnceLock::new();
    let t = text.to_lowercase();
    if !t.contains("detector") && !t.contains("milepost") && !t.contains("mile post") {
        return None;
    }
    let t = t.replace(',', " ");
    // digits read one by one and written "1-7-4" or "1/7/4" are one number
    static JOINED: OnceLock<Regex> = OnceLock::new();
    let joined = cached(&JOINED, || r"\b\d(?:[/-]\d)+\b".into());
    let t = joined.replace_all(&t, |c: &regex::Captures| c[0].replace(['/', '-'], "")).into_owned();
    let track = cached(&TRACK, || r"track (one|two|three|four|five|six)\b".into());
    let t = track
        .replace_all(&t, |c: &regex::Captures| {
            let n = NUMBER_WORDS.iter().find(|(w, _)| *w == &c[1]).map(|(_, n)| *n).unwrap_or(0);
            format!("track {n}")
        })
        .into_owned();
    let mut out = Map::new();
    out.insert("type".into(), json!("defect_detector"));
    if t.contains("norfolk") {
        out.insert("railroad".into(), json!("NS"));
    } else if t.contains("csx") {
        out.insert("railroad".into(), json!("CSX"));
    }
    // (key, pattern, float?)
    let pats = PATS.get_or_init(|| {
        let num = r"(\d+(?:\.\d+)?)";
        let p = |s: String| Regex::new(&s).expect("regex");
        vec![
            ("milepost", p(format!(r"mile ?post\s+{num}")), true),
            ("track", p(r"track\s+(\d+)".into()), false),
            // "total axles 312", "axles 312", "axle count 312"; a lone "axle 112" in a defect report
            // is the defective axle, not the count
            ("axles", p(r"(?:total\s+axles?|axles|axle\s+count)\s+(?:count\s+)?(\d+)".into()), false),
            ("speed_mph", p(r"speed\s+(\d+)".into()), false),
            ("temperature_f", p(r"temperature\s+(-?\d+)".into()), false),
        ]
    });
    let mut found = false;
    for (key, pat, is_float) in pats {
        if let Some(c) = pat.captures(&t) {
            let raw = &c[1];
            let (sign, body) = raw.strip_prefix('-').map(|b| ("-", b)).unwrap_or(("", raw));
            let ascii = body
                .split('.')
                .map(ascii_digits)
                .collect::<Option<Vec<_>>>()
                .map(|parts| format!("{sign}{}", parts.join(".")));
            let Some(ascii) = ascii else { continue };
            let v = if *is_float {
                ascii.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number)
            } else {
                // past i64 the value is kept as a float rather than overflowing
                ascii.parse::<i64>().map(|i| json!(i)).ok()
                    .or_else(|| ascii.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number))
            };
            if let Some(v) = v {
                out.insert((*key).into(), v);
                found = true;
            }
        }
    }
    // detectors read counts digit by digit, which speech recognition writes as words
    if !out.contains_key("axles") {
        if let Some(n) = spoken_count_after(text, &["total axles", "total axle", "axle count"]) {
            out.insert("axles".into(), json!(n));
            found = true;
        }
    }
    if !out.contains_key("speed_mph") {
        if let Some(n) = spoken_count_after(text, &["train speed", "speed"]) {
            out.insert("speed_mph".into(), json!(n));
            found = true;
        }
    }
    if trust_defects && t.contains("no defect") {
        out.insert("defects".into(), json!(false));
        found = true;
    } else if trust_defects && t.contains("defect") {
        out.insert("defects".into(), json!(true));
        found = true;
    }
    found.then_some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_digits() {
        assert_eq!(ascii_digits("475").as_deref(), Some("475"));
        assert_eq!(ascii_digits("\u{0664}\u{0667}\u{0665}").as_deref(), Some("475")); // Arabic-Indic
        assert_eq!(ascii_digits("\u{0ae6}\u{0aef}").as_deref(), Some("09")); // Gujarati
        assert_eq!(milepost_from_text("\u{0664}\u{0667}\u{0665}.8"), Some(475.8));
        // mathematical digits: five runs of ten back to back, not aligned to 16
        assert_eq!(ascii_digits("\u{1d7ce}\u{1d7d7}\u{1d7d8}\u{1d7e3}\u{1d7ff}").as_deref(), Some("09019"));
        assert_eq!(ascii_digits("\u{0f29}").as_deref(), Some("9")); // Tibetan nine
        assert_eq!(ascii_digits("\u{00b2}"), None); // superscript two is not Nd
    }
}
