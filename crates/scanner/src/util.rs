//! Number and text formatting the log lines and reports depend on, plus JSON value helpers.

use serde_json::Value;

/// `round(x, n)`: the correctly rounded decimal, ties to even on the exact binary value.
pub fn round_even(x: f64, n: usize) -> f64 {
    format!("{:.*}", n, x).parse().unwrap_or(x)
}

/// `int(round(x))`
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `statistics.median`
pub fn median(xs: &[f64]) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// `f"{x:g}"`
pub fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return if x.is_nan() { "nan".into() } else if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let sci = format!("{:.5e}", x);
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip = |s: &str| -> String {
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() }
    };
    if (-4..6).contains(&exp) {
        strip(&format!("{:.*}", (5 - exp) as usize, x))
    } else {
        format!("{}e{}{:02}", strip(mant), if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// `f"{x:.0%}"`
pub fn pct0(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// `str(v)` for the JSON values a plan or event holds.
pub fn value_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True".into() } else { "False".into() },
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() { n.to_string() } else { crate::jsonfmt::float_repr(n.as_f64().unwrap_or(f64::NAN)) }
        }
        other => other.to_string(),
    }
}

/// `str(None)` for a missing value too.
pub fn value_str_opt(v: Option<&Value>) -> String {
    v.map_or_else(|| "None".to_string(), value_str)
}

/// `repr(s)` for a str.
pub fn repr_text(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

/// Truthiness of a JSON value: null, false, zero and empty text, lists and objects are false.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `float(v)`
pub fn value_f64(v: Option<&Value>) -> Result<f64, String> {
    match v {
        Some(Value::Number(n)) => n.as_f64().ok_or_else(|| "bad number".into()),
        Some(Value::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
        Some(Value::String(s)) => s.trim().parse().map_err(|_| format!("could not convert string to float: {}", repr_text(s))),
        Some(Value::Null) | None => Err("float() argument must be a string or a real number, not 'NoneType'".into()),
        Some(_) => Err("float() argument must be a string or a real number".into()),
    }
}

/// `int(v)`
pub fn value_i64(v: Option<&Value>) -> Result<i64, String> {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64))
            .ok_or_else(|| "cannot convert float to integer".into()),
        Some(Value::Bool(b)) => Ok(*b as i64),
        Some(Value::String(s)) => parse_int(s),
        Some(Value::Null) | None => Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into()),
        Some(_) => Err("int() argument must be a string, a bytes-like object or a real number".into()),
    }
}

/// `int(text)`
pub fn parse_int(s: &str) -> Result<i64, String> {
    s.trim().parse::<i64>().map_err(|_| format!("invalid literal for int() with base 10: {}", repr_text(s)))
}

/// `a == b` for JSON scalars, with numbers compared by value (5 == 5.0).
pub fn scalar_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    let a = a.unwrap_or(&Value::Null);
    let b = b.unwrap_or(&Value::Null);
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// Combine a result with a cleanup step: an error in the cleanup wins.
pub fn finally<T>(r: Result<T, crate::ScannerError>, f: Result<(), crate::ScannerError>) -> Result<T, crate::ScannerError> {
    match f {
        Err(e) => Err(e),
        Ok(()) => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(round_even(0.125, 2), 0.12);
        assert_eq!(round_even(2.675, 2), 2.67);
        assert_eq!(round_even(1000.035, 2), 1000.03);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(fmt_g(30.0), "30");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(1.0 / 3.0), "0.333333");
        assert_eq!(fmt_g(1e7), "1e+07");
        assert_eq!(pct0(0.8), "80%");
        assert_eq!(pct0(0.0), "0%");
        assert_eq!(repr_text("ERR"), "'ERR'");
        assert_eq!(median(&[3.0, 1.0, 2.0, 10.0]), Some(2.5));
        assert_eq!(value_str(&serde_json::json!(160.5)), "160.5");
        assert_eq!(value_str(&serde_json::json!(5)), "5");
    }
}
