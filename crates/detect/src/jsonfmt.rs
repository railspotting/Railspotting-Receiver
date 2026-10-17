//! JSON text in the receiver's exact formats (a copy of `proto::jsonfmt`, so this crate doesn't
//! depend on the API crate). Template metadata is written indented by 2.
//!
//! Signed bodies are compact (`,` and `:` with no spaces, non-ASCII kept as is), and
//! credentials.json is indented by 2 and ASCII only, so files in an existing state dir stay byte
//! for byte the same. serde_json differs in float text (`1e16` where the format has `1e+16`), so
//! floats go through `float_repr`.

use serde_json::Value;

/// Float text: the shortest digits that round trip, fixed notation for 1e-4 <= |x| < 1e16, always
/// with `.0` or an exponent.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0".into() } else { "0.0".into() };
    }
    // Rust's `{:e}` gives the shortest round trip digits.
    let sci = format!("{:e}", f);
    let (neg, sci) = match sci.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, sci),
    };
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1; // digits d1d2.. with the point after decpt of them
    let n = digits.len() as i32;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            out.push_str(&"0".repeat((-decpt) as usize));
            out.push_str(&digits);
        } else if decpt >= n {
            out.push_str(&digits);
            out.push_str(&"0".repeat((decpt - n) as usize));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if n > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.abs()));
    }
    out
}

fn write_str(out: &mut String, s: &str, ascii_only: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii_only && (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04x}", unit));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_number(out: &mut String, n: &serde_json::Number) {
    if let Some(i) = n.as_i64() {
        out.push_str(&i.to_string());
    } else if let Some(u) = n.as_u64() {
        out.push_str(&u.to_string());
    } else {
        out.push_str(&float_repr(n.as_f64().unwrap_or(f64::NAN)));
    }
}

fn write(out: &mut String, v: &Value, indent: Option<usize>, level: usize, ascii_only: bool) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_str(out, s, ascii_only),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                write(out, item, indent, level + 1, ascii_only);
            }
            newline(out, indent, level);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, item)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                write_str(out, k, ascii_only);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write(out, item, indent, level + 1, ascii_only);
            }
            newline(out, indent, level);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

/// Compact JSON: no spaces after `,` and `:`, non-ASCII characters kept as is.
pub fn dumps_compact(v: &Value) -> String {
    let mut out = String::new();
    write(&mut out, v, None, 0, false);
    out
}

/// Indented JSON, ASCII only (non-ASCII as `\uXXXX`), as credentials.json is written.
pub fn dumps_indent(v: &Value, indent: usize) -> String {
    let mut out = String::new();
    write(&mut out, v, Some(indent), 0, true);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_match_reference_text() {
        // reference text of each float
        let cases: &[(f64, &str)] = &[
            (1790865400.0, "1790865400.0"), (0.1, "0.1"), (1e16, "1e+16"), (1e15, "1000000000000000.0"),
            (1.5e16, "1.5e+16"), (0.0001, "0.0001"), (0.00001, "1e-05"), (1.234e-7, "1.234e-07"),
            (-2.5, "-2.5"), (123.456, "123.456"), (1e300, "1e+300"), (5e-324, "5e-324"),
            (-0.0, "-0.0"), (160.905, "160.905"), (1.0 / 3.0, "0.3333333333333333"),
            (9.2, "9.2"), (2.0f64.powi(60), "1.152921504606847e+18"), (12345678901234567.0, "1.2345678901234568e+16"),
        ];
        for (f, want) in cases {
            assert_eq!(float_repr(*f), *want, "{f}");
        }
    }

    #[test]
    fn compact_and_indented() {
        let v = json!({"b": 1, "a": [1.0, "é\n\u{1}", null, true], "e": {}, "l": []});
        assert_eq!(dumps_compact(&v), "{\"b\":1,\"a\":[1.0,\"é\\n\\u0001\",null,true],\"e\":{},\"l\":[]}");
        assert_eq!(dumps_indent(&v, 2),
                   "{\n  \"b\": 1,\n  \"a\": [\n    1.0,\n    \"\\u00e9\\n\\u0001\",\n    null,\n    true\n  ],\n  \"e\": {},\n  \"l\": []\n}");
        assert_eq!(dumps_indent(&json!({"x": "\u{1F682}"}), 2), "{\n  \"x\": \"\\ud83d\\ude82\"\n}");
    }
}
