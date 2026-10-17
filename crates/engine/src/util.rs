//! Text formatting the log lines, status text and files depend on.

use serde_json::Value;

pub use railspotting_scanner::util::{fmt_g, repr_text, round_even, value_str, value_str_opt, round_int};

/// Whether a JSON value counts as true: not null, false, zero, or an empty string, list or object.
pub fn truthy(v: &Value) -> bool {
    railspotting_scanner::util::truthy(Some(v))
}

/// JSON on one line, with ", " and ": " separators and non-ASCII characters escaped.
pub fn json_dumps(v: &Value) -> String {
    railspotting_store::jsonfmt::dumps(v)
}

/// The same JSON, indented by `n` spaces.
pub fn json_dumps_indent(v: &Value, n: usize) -> String {
    railspotting_store::jsonfmt::dumps_indent(v, n)
}

/// A float in its shortest round trip digits ("1.0", "0.81", "1e+16").
pub fn float_repr(f: f64) -> String {
    railspotting_store::jsonfmt::float_repr(f)
}

/// A list of floats and Nones as "[0.81, None, 1.0]", or "None".
pub fn list_text_opt(v: Option<&[Option<f64>]>) -> String {
    match v {
        None => "None".into(),
        Some(xs) => format!("[{}]", xs.iter().map(|x| x.map_or("None".into(), float_repr)).collect::<Vec<_>>().join(", ")),
    }
}

/// Pad to `w` characters (not bytes), on the right or the left.
pub fn ljust(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - n)) }
}

pub fn rjust(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { s.to_string() } else { format!("{}{s}", " ".repeat(w - n)) }
}

/// The first `n` characters.
pub fn head(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn tm_of(ts: f64, utc: bool) -> libc::tm {
    let t = ts.floor() as libc::time_t;
    // SAFETY: a zeroed tm is a valid out buffer for the *_r calls
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call
    unsafe {
        if utc {
            libc::gmtime_r(&t, &mut tm);
        } else {
            libc::localtime_r(&t, &mut tm);
        }
    }
    tm
}

fn strftime(tm: &libc::tm, fmt: &str) -> String {
    let Ok(cfmt) = std::ffi::CString::new(fmt) else { return String::new() };
    let mut buf = [0u8; 128];
    // SAFETY: buf is large enough for the formats used here and strftime NUL-terminates
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// `strftime` of `ts` in local time.
pub fn local_stamp(ts: f64, fmt: &str) -> String {
    strftime(&tm_of(ts, false), fmt)
}

/// `strftime` of `ts` in UTC.
pub fn utc_stamp(ts: f64, fmt: &str) -> String {
    strftime(&tm_of(ts, true), fmt)
}

/// This machine's host name, or "" when it cannot be read.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is writable for its length
    let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if r != 0 {
        return String::new();
    }
    let n = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// The system, release and machine, e.g. Linux-6.8.12-4-pve-x86_64-with-glibc2.39.
pub fn platform() -> String {
    // SAFETY: a zeroed utsname is a valid out buffer
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: valid pointer
    if unsafe { libc::uname(&mut u) } != 0 {
        return std::env::consts::OS.to_string();
    }
    let f = |a: &[libc::c_char]| {
        let b: Vec<u8> = a.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        String::from_utf8_lossy(&b).into_owned()
    };
    let (sys, rel, mach) = (f(&u.sysname), f(&u.release), f(&u.machine));
    if sys == "Darwin" {
        return format!("macOS-{rel}-{mach}-64bit");
    }
    let out = format!("{sys}-{rel}-{mach}");
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let out = {
        // SAFETY: returns a static NUL-terminated string
        let v = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) };
        format!("{out}-with-glibc{}", v.to_string_lossy())
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_text_and_padding() {
        assert_eq!(list_text_opt(Some(&[Some(0.81), None, Some(1.0)])), "[0.81, None, 1.0]");
        assert_eq!(list_text_opt(None), "None");
        assert_eq!(ljust("ab", 4), "ab  ");
        assert_eq!(rjust("7", 3), "  7");
        assert_eq!(utc_stamp(0.0, "%m-%d %H:%M:%S"), "01-01 00:00:00");
        assert!(platform().contains('-'));
    }
}
