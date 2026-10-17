//! JSON text in the receiver's exact byte format, so files and database rows are byte for byte
//! what earlier receiver versions wrote (existing state dirs keep working).

use serde_json::Value;

/// Shortest round-trip float text with a .0 on whole numbers: fixed notation for
/// 1e-4 <= |x| < 1e16, else exponent notation with a sign and at least two digits (`1e+16`).
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
    // `{:e}` gives the shortest round trip digits; only the layout is changed below.
    let sci = format!("{:e}", f.abs());
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let decpt = exp + 1;
    let mut out = String::new();
    if f < 0.0 {
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

fn write_str(out: &mut String, s: &str) {
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
            // ensure_ascii: everything outside space..~ is escaped
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
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

fn write(out: &mut String, v: &Value, indent: Option<usize>, level: usize) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else {
                out.push_str(&float_repr(n.as_f64().unwrap_or(f64::NAN)));
            }
        }
        Value::String(s) => write_str(out, s),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                newline(out, indent, level + 1);
                write(out, item, indent, level + 1);
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
                    if indent.is_none() {
                        out.push(' ');
                    }
                }
                newline(out, indent, level + 1);
                write_str(out, k);
                out.push_str(": ");
                write(out, item, indent, level + 1);
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

/// `json.dumps(v)` with the default separators (", ", ": ") and ensure_ascii.
pub fn dumps(v: &Value) -> String {
    let mut out = String::new();
    write(&mut out, v, None, 0);
    out
}

/// `json.dumps(v, indent=n)`.
pub fn dumps_indent(v: &Value, n: usize) -> String {
    let mut out = String::new();
    write(&mut out, v, Some(n), 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn byte_format() {
        let cases: &[(f64, &str)] = &[(1790865400.0, "1790865400.0"), (1e16, "1e+16"), (0.00001, "1e-05"),
                                      (160.905, "160.905"), (-2.5, "-2.5"), (0.0001, "0.0001")];
        for (f, want) in cases {
            assert_eq!(float_repr(*f), *want);
        }
        let v = json!({"b": 1, "a": [1.0, "\u{e9}\n", null, true], "e": {}});
        assert_eq!(dumps(&v), "{\"b\": 1, \"a\": [1.0, \"\\u00e9\\n\", null, true], \"e\": {}}");
        assert_eq!(dumps_indent(&json!({"vhf": "1", "eot": null}), 2), "{\n  \"vhf\": \"1\",\n  \"eot\": null\n}");
    }
}
