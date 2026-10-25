//! RTL-SDR dongles through librtlsdr, opened at run time (never linked: librtlsdr is GPL-2.0
//! and installed as its own system package; the Android app ships it as its own library):
//! driver loading, the device list, and RtlDevice with its gain table, async reads and libusb
//! error texts.

use crate::device::Device;
use libloading::Library;
use num_complex::Complex32;
use railspotting_dsp::sdr::tuner_name;
use serde_json::{json, Map, Value};
use std::ffi::{c_char, c_int, c_uchar, c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Where librtlsdr may be, in the order tried.
pub const LIB_NAMES: [&str; 6] = ["librtlsdr.so.2", "librtlsdr.so.0", "librtlsdr.so", "librtlsdr.dylib",
                                  "/opt/homebrew/lib/librtlsdr.dylib", "/usr/local/lib/librtlsdr.dylib"];

/// The driver could not be loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct DriverError(pub String);

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type DevP = *mut c_void;
type ReadCb = unsafe extern "C" fn(*mut c_uchar, u32, *mut c_void);

/// The librtlsdr functions the receiver uses.
pub struct Lib {
    _lib: Library,
    get_device_count: unsafe extern "C" fn() -> u32,
    get_device_usb_strings: unsafe extern "C" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> c_int,
    get_index_by_serial: unsafe extern "C" fn(*const c_char) -> c_int,
    open: unsafe extern "C" fn(*mut DevP, u32) -> c_int,
    close: unsafe extern "C" fn(DevP) -> c_int,
    set_testmode: unsafe extern "C" fn(DevP, c_int) -> c_int,
    reset_buffer: unsafe extern "C" fn(DevP) -> c_int,
    get_tuner_type: unsafe extern "C" fn(DevP) -> c_int,
    get_tuner_gains: unsafe extern "C" fn(DevP, *mut c_int) -> c_int,
    set_tuner_gain_mode: unsafe extern "C" fn(DevP, c_int) -> c_int,
    set_tuner_gain: unsafe extern "C" fn(DevP, c_int) -> c_int,
    set_sample_rate: unsafe extern "C" fn(DevP, u32) -> c_int,
    set_center_freq: unsafe extern "C" fn(DevP, u32) -> c_int,
    read_async: unsafe extern "C" fn(DevP, ReadCb, *mut c_void, u32, u32) -> c_int,
    cancel_async: unsafe extern "C" fn(DevP) -> c_int,
    get_usb_strings: Option<unsafe extern "C" fn(DevP, *mut c_char, *mut c_char, *mut c_char) -> c_int>,
    /// Only in the Android app's librtlsdr: open a dongle from a USB file descriptor (Android
    /// gives apps no device list, only the descriptor of a device the owner allowed).
    open_sys_dev: Option<unsafe extern "C" fn(*mut DevP, isize) -> c_int>,
}

// librtlsdr's functions are safe to call from any thread for different devices
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

macro_rules! sym {
    ($lib:expr, $name:literal) => {
        // SAFETY: the signatures match rtl-sdr.h
        *unsafe { $lib.get($name) }.map_err(|e| DriverError(e.to_string()))?
    };
}

/// Why librtlsdr did not load: the error for a full path that exists (an installed library that
/// would not load, say for macOS's library validation), else the first one.
fn load_error(names: &[&str], errors: Vec<String>) -> String {
    let found = names.iter().zip(&errors).find(|(n, _)| n.starts_with('/') && std::path::Path::new(n).exists());
    match found {
        Some((_, e)) => e.clone(),
        None => errors.into_iter().next().unwrap_or_else(|| "librtlsdr not found".into()),
    }
}

impl Lib {
    fn load() -> Result<Lib, DriverError> {
        let mut errors = vec![];
        for name in LIB_NAMES {
            // SAFETY: loading librtlsdr runs no initialisers with preconditions
            match unsafe { Library::new(name) } {
                Ok(lib) => return Lib::from(lib),
                Err(e) => errors.push(e.to_string()),
            }
        }
        Err(DriverError(load_error(&LIB_NAMES, errors)))
    }

    fn from(lib: Library) -> Result<Lib, DriverError> {
        Ok(Lib {
            get_device_count: sym!(lib, b"rtlsdr_get_device_count\0"),
            get_device_usb_strings: sym!(lib, b"rtlsdr_get_device_usb_strings\0"),
            get_index_by_serial: sym!(lib, b"rtlsdr_get_index_by_serial\0"),
            open: sym!(lib, b"rtlsdr_open\0"),
            close: sym!(lib, b"rtlsdr_close\0"),
            set_testmode: sym!(lib, b"rtlsdr_set_testmode\0"),
            reset_buffer: sym!(lib, b"rtlsdr_reset_buffer\0"),
            get_tuner_type: sym!(lib, b"rtlsdr_get_tuner_type\0"),
            get_tuner_gains: sym!(lib, b"rtlsdr_get_tuner_gains\0"),
            set_tuner_gain_mode: sym!(lib, b"rtlsdr_set_tuner_gain_mode\0"),
            set_tuner_gain: sym!(lib, b"rtlsdr_set_tuner_gain\0"),
            set_sample_rate: sym!(lib, b"rtlsdr_set_sample_rate\0"),
            set_center_freq: sym!(lib, b"rtlsdr_set_center_freq\0"),
            read_async: sym!(lib, b"rtlsdr_read_async\0"),
            cancel_async: sym!(lib, b"rtlsdr_cancel_async\0"),
            // SAFETY: the signatures match rtl-sdr.h and the Android app's patch
            get_usb_strings: unsafe { lib.get(b"rtlsdr_get_usb_strings\0") }.ok().map(|f| *f),
            open_sys_dev: unsafe { lib.get(b"rtlsdr_open_sys_dev\0") }.ok().map(|f| *f),
            _lib: lib,
        })
    }

    /// Vendor, product and serial of dongle `index`, empty when they can't be read.
    fn usb_strings(&self, index: u32) -> (String, String, String) {
        let mut bufs = [[0u8; 256]; 3];
        let [a, b, c] = &mut bufs;
        // SAFETY: each buffer holds the 256 bytes librtlsdr writes at most
        let r = unsafe {
            (self.get_device_usb_strings)(index, a.as_mut_ptr().cast(), b.as_mut_ptr().cast(), c.as_mut_ptr().cast())
        };
        if r != 0 {
            return (String::new(), String::new(), String::new());
        }
        // each nonzero byte as one character (Latin-1), NULs dropped
        let s = |buf: &[u8; 256]| buf.iter().filter(|b| **b > 0).map(|b| *b as char).collect::<String>();
        (s(&bufs[0]), s(&bufs[1]), s(&bufs[2]))
    }
}

static LIB: OnceLock<Result<Arc<Lib>, DriverError>> = OnceLock::new();

/// The loaded driver, or why it can't be loaded.
pub fn librtlsdr() -> Result<Arc<Lib>, DriverError> {
    LIB.get_or_init(|| Lib::load().map(Arc::new)).clone()
}

/// Connected dongles with their USB strings. Reads descriptors without claiming.
pub fn list_devices() -> Result<Vec<Value>, DriverError> {
    let lib = librtlsdr()?;
    // SAFETY: no arguments
    let n = unsafe { (lib.get_device_count)() };
    Ok((0..n).map(|i| {
        let (vendor, product, serial) = lib.usb_strings(i);
        json!({"index": i, "serial": serial, "vendor": vendor, "product": product})
    }).collect())
}

/// A libusb error as the logs show it: "<LIBUSB_ERROR_NAME (code): description> \"message\"".
/// `rtlsdr_get_index_by_serial`'s answers. They are not libusb errors, though -2 and -3 share
/// libusb's numbers ("Invalid parameter", "Access denied"), which made a missing dongle read
/// like a permissions problem.
pub fn no_dongle(code: i32, serial: &str) -> String {
    match code {
        -2 => "no dongle is plugged in".to_string(),
        -3 => format!("the dongle with serial {serial} is not plugged in"),
        _ => format!("no serial to look for ({code})"),
    }
}

pub fn libusb_error(errno: i32, msg: &str) -> String {
    let t = match errno {
        -1 => Some(("LIBUSB_ERROR_IO", "Input/output error")),
        -2 => Some(("LIBUSB_ERROR_INVALID_PARAM", "Invalid parameter")),
        -3 => Some(("LIBUSB_ERROR_ACCESS", "Access denied (insufficient permissions)")),
        -4 => Some(("LIBUSB_ERROR_NO_DEVICE", "No such device (it may have been disconnected)")),
        -5 => Some(("LIBUSB_ERROR_NOT_FOUND", "Entity not found")),
        -6 => Some(("LIBUSB_ERROR_BUSY", "Resource busy")),
        -7 => Some(("LIBUSB_ERROR_TIMEOUT", "Operation timed out")),
        -8 => Some(("LIBUSB_ERROR_OVERFLOW", "Overflow")),
        -9 => Some(("LIBUSB_ERROR_PIPE", "Pipe error")),
        -10 => Some(("LIBUSB_ERROR_INTERRUPTED", "System call interrupted (perhaps due to signal)")),
        -11 => Some(("LIBUSB_ERROR_NO_MEM", "Insufficient memory")),
        -12 => Some(("LIBUSB_ERROR_NOT_SUPPORTED", "Operation not supported or unimplemented on this platform")),
        -99 => Some(("LIBUSB_ERROR_OTHER", "Other error")),
        _ => None,
    };
    match t {
        Some((id, text)) => format!("<{id} ({errno}): {text}> \"{msg}\""),
        None => format!("Error code {errno}: {msg}"),
    }
}

struct Handle(DevP);
// SAFETY: the device pointer is used by one streaming thread plus cancel/tune calls, which
// librtlsdr allows
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// A real dongle opened by serial number, with the info capabilities need.
pub struct RtlDevice {
    lib: Arc<Lib>,
    dev: Mutex<Option<Handle>>,
    gains: Vec<c_int>,
    info: Map<String, Value>,
    pub serial: String,
    canceling: AtomicBool,
}

impl RtlDevice {
    pub fn open(serial: &str) -> Result<RtlDevice, String> {
        let lib = librtlsdr().map_err(|e| e.0)?;
        let cs = CString::new(serial).map_err(|e| e.to_string())?;
        // SAFETY: a NUL-terminated serial
        let index = unsafe { (lib.get_index_by_serial)(cs.as_ptr()) };
        if index < 0 {
            return Err(no_dongle(index, serial));
        }
        let (vendor, product, _) = lib.usb_strings(index as u32);
        let mut p: DevP = std::ptr::null_mut();
        // SAFETY: open writes the device pointer
        let r = unsafe { (lib.open)(&mut p, index as u32) };
        if r < 0 {
            return Err(libusb_error(r, &format!("Could not open SDR (device index = {index})")));
        }
        RtlDevice::opened(lib, p, serial, vendor, product)
    }

    /// Open the dongle behind a USB file descriptor (Android). Needs a librtlsdr with
    /// `rtlsdr_open_sys_dev`; `name` stands in for the serial in logs.
    pub fn open_sys_dev(fd: i64, name: &str) -> Result<RtlDevice, String> {
        let lib = librtlsdr().map_err(|e| e.0)?;
        let open = lib.open_sys_dev.ok_or_else(|| "this librtlsdr cannot open a USB file descriptor".to_string())?;
        let mut p: DevP = std::ptr::null_mut();
        // SAFETY: open writes the device pointer; the descriptor stays the caller's
        let r = unsafe { open(&mut p, fd as isize) };
        if r < 0 {
            return Err(libusb_error(r, "Could not open SDR from its USB descriptor"));
        }
        let (vendor, product) = match lib.get_usb_strings {
            Some(get) => {
                let mut bufs = [[0u8; 256]; 2];
                let [a, b] = &mut bufs;
                // SAFETY: open device, 256-byte buffers, no serial wanted
                let r = unsafe { get(p, a.as_mut_ptr().cast(), b.as_mut_ptr().cast(), std::ptr::null_mut()) };
                let s = |buf: &[u8; 256]| buf.iter().take_while(|b| **b > 0).map(|b| *b as char).collect::<String>();
                if r == 0 { (s(&bufs[0]), s(&bufs[1])) } else { (String::new(), String::new()) }
            }
            None => (String::new(), String::new()),
        };
        RtlDevice::opened(lib, p, name, vendor, product)
    }

    /// The setup after either open: test mode off, buffer reset, gain table, defaults, info.
    fn opened(lib: Arc<Lib>, p: DevP, serial: &str, vendor: String, product: String) -> Result<RtlDevice, String> {
        let mut me = RtlDevice {
            lib: lib.clone(), dev: Mutex::new(Some(Handle(p))), gains: vec![], info: Map::new(),
            serial: serial.to_string(), canceling: AtomicBool::new(false),
        };
        // SAFETY: p is an open device below
        unsafe {
            let r = (lib.set_testmode)(p, 0);
            if r < 0 {
                Device::close(&me);
                return Err(libusb_error(r, "Could not set test mode"));
            }
            let r = (lib.reset_buffer)(p);
            if r < 0 {
                Device::close(&me);
                return Err(libusb_error(r, "Could not reset buffer"));
            }
            let mut buf = [0 as c_int; 50];
            let n = (lib.get_tuner_gains)(p, buf.as_mut_ptr());
            if n <= 0 {
                Device::close(&me);
                return Err("Error when getting gains".into());
            }
            me.gains = buf[..(n as usize).min(50)].to_vec();
        }
        // the defaults on open: 1.024 MS/s, 80 MHz, automatic gain
        me.set_rate(1.024e6)?;
        me.tune(80e6)?;
        me.set_gain(&json!("auto"))?;
        // SAFETY: open device
        let tuner = match unsafe { (lib.get_tuner_type)(p) } {
            t if t >= 0 => tuner_name(t),
            _ => "unknown",
        };
        let opt = |s: String| if s.is_empty() { Value::Null } else { Value::String(s) };
        me.info = json!({"vendor": opt(vendor), "product": opt(product), "chipset": "RTL2832U", "tuner": tuner})
            .as_object().cloned().unwrap_or_default();
        Ok(me)
    }

    fn ptr(&self) -> Option<DevP> {
        self.dev.lock().unwrap_or_else(|p| p.into_inner()).as_ref().map(|h| h.0)
    }

    fn call(&self, f: impl FnOnce(DevP) -> c_int, what: impl FnOnce() -> String) -> Result<(), String> {
        let p = self.ptr().ok_or_else(|| "device closed".to_string())?;
        let r = f(p);
        if r < 0 {
            Device::close(self);
            return Err(libusb_error(r, &what()));
        }
        Ok(())
    }

    fn set_rate(&self, rate: f64) -> Result<(), String> {
        let rate = rate as u32;
        // SAFETY: open device
        self.call(|p| unsafe { (self.lib.set_sample_rate)(p, rate) }, || format!("Could not set sample rate to {rate} Hz"))
    }

    fn tune(&self, center_hz: f64) -> Result<(), String> {
        let f = center_hz as u32;
        // SAFETY: open device
        self.call(|p| unsafe { (self.lib.set_center_freq)(p, f) }, || format!("Could not set center_freq to {f} Hz"))
    }

    fn set_gain(&self, gain: &Value) -> Result<(), String> {
        let p = self.ptr().ok_or_else(|| "device closed".to_string())?;
        if gain.as_str() == Some("auto") {
            // SAFETY: open device
            let r = unsafe { (self.lib.set_tuner_gain_mode)(p, 0) };
            return if r < 0 { Err(libusb_error(r, "Could not get gain mode")) } else { Ok(()) };
        }
        let g = gain.as_f64().or_else(|| gain.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| format!("bad gain {gain}"))?;
        // the supported gain nearest to the one asked for (first of equals, as list.index)
        let mut best = 0;
        for (i, v) in self.gains.iter().enumerate() {
            if (10.0 * g - *v as f64).abs() < (10.0 * g - self.gains[best] as f64).abs() {
                best = i;
            }
        }
        // SAFETY: open device
        let r = unsafe { (self.lib.set_tuner_gain_mode)(p, 1) };
        if r < 0 {
            return Err(libusb_error(r, "Could not get gain mode"));
        }
        let want = self.gains.get(best).copied().unwrap_or(0);
        // SAFETY: open device
        self.call(|p| unsafe { (self.lib.set_tuner_gain)(p, want) }, || format!("Could not set gain to {}", g as i64))
    }
}

struct ReadCtx<'a> {
    callback: &'a mut dyn FnMut(&[u8]),
    canceling: &'a AtomicBool,
}

unsafe extern "C" fn on_bytes(buf: *mut c_uchar, len: u32, ctx: *mut c_void) {
    // SAFETY: ctx is the ReadCtx read_bytes_async passed, alive for the whole read
    let ctx = unsafe { &mut *(ctx as *mut ReadCtx) };
    if ctx.canceling.load(Ordering::SeqCst) || buf.is_null() {
        return;
    }
    // SAFETY: librtlsdr hands a buffer of len bytes
    let bytes = unsafe { std::slice::from_raw_parts(buf, len as usize) };
    // a panic must not unwind into C
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (ctx.callback)(bytes)));
}

impl RtlDevice {
    /// Stream raw bytes until cancelled: blocks of `num_bytes` from librtlsdr's USB thread.
    pub fn read_bytes_async(&self, callback: &mut dyn FnMut(&[u8]), num_bytes: usize) -> Result<(), String> {
        let p = self.ptr().ok_or_else(|| "device closed".to_string())?;
        self.canceling.store(false, Ordering::SeqCst);
        let mut ctx = ReadCtx { callback, canceling: &self.canceling };
        // SAFETY: ctx outlives the call, which returns only after the last callback
        let r = unsafe { (self.lib.read_async)(p, on_bytes, (&mut ctx as *mut ReadCtx).cast(), 0, num_bytes as u32) };
        if r < 0 {
            Device::close(self);
            return Err(libusb_error(r, &format!("Could not read {num_bytes} bytes")));
        }
        self.canceling.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl Device for RtlDevice {
    fn info(&self) -> Map<String, Value> {
        self.info.clone()
    }

    fn configure(&self, rate: f64, center_hz: f64, gain: &Value) -> Result<(), String> {
        self.set_rate(rate)?;
        self.tune(center_hz)?;
        self.set_gain(gain)
    }

    fn set_center(&self, center_hz: f64) {
        if let Err(e) = self.tune(center_hz) {
            log::warn!("{}: retune failed: {}", self.serial, e);
        }
    }

    fn read_samples_async(&self, callback: &mut dyn FnMut(Vec<Complex32>), num_samples: usize) -> Result<(), String> {
        let mut iq = Vec::new();
        self.read_bytes_async(&mut |bytes: &[u8]| {
            iq.clear();
            railspotting_dsp::cu8_to_iq(bytes, &mut iq);
            callback(std::mem::take(&mut iq));
        }, 2 * num_samples)
    }

    fn cancel_read_async(&self) {
        if let Some(p) = self.ptr() {
            // SAFETY: open device; extra callbacks after a cancel are ignored
            let r = unsafe { (self.lib.cancel_async)(p) };
            if r < 0 && !self.canceling.load(Ordering::SeqCst) {
                log::debug!("{}", libusb_error(r, "Could not cancel async read"));
            }
            self.canceling.store(true, Ordering::SeqCst);
        }
    }

    fn close(&self) {
        if let Some(h) = self.dev.lock().unwrap_or_else(|p| p.into_inner()).take() {
            // SAFETY: closed once
            unsafe { (self.lib.close)(h.0) };
        }
    }
}

impl Drop for RtlDevice {
    fn drop(&mut self) {
        Device::close(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_load_error_is_the_installed_librarys() {
        let errs = || vec!["not found: libx.so".to_string(), "refused: /bin/sh".to_string(), "not found: /nowhere/libx".to_string()];
        // a library that is there but would not load says why, not that a bare name is missing
        assert_eq!(load_error(&["libx.so", "/bin/sh", "/nowhere/libx"], errs()), "refused: /bin/sh");
        assert_eq!(load_error(&["libx.so", "/nowhere/a", "/nowhere/libx"], errs()), "not found: libx.so");
        assert_eq!(load_error(&[], vec![]), "librtlsdr not found");
    }

    #[test]
    fn missing_dongle_texts() {
        assert_eq!(no_dongle(-2, "30001111"), "no dongle is plugged in");
        assert_eq!(no_dongle(-3, "30001111"), "the dongle with serial 30001111 is not plugged in");
        assert_eq!(no_dongle(-1, ""), "no serial to look for (-1)");
    }

    #[test]
    fn libusb_error_texts() {
        assert_eq!(libusb_error(-3, "Could not open SDR (device index = 0)"),
                   "<LIBUSB_ERROR_ACCESS (-3): Access denied (insufficient permissions)> \"Could not open SDR (device index = 0)\"");
        assert_eq!(libusb_error(-42, "x"), "Error code -42: x");
    }

    #[test]
    fn a_descriptor_open_fails_plainly_without_the_android_driver() {
        // the system librtlsdr has no rtlsdr_open_sys_dev; -1 is never a dongle
        let e = RtlDevice::open_sys_dev(-1, "usb").err().unwrap_or_default();
        assert!(!e.is_empty());
    }

    #[test]
    fn listing_without_dongles_does_not_crash() {
        // librtlsdr may or may not be installed here; either answer is fine
        match list_devices() {
            Ok(d) => assert!(d.iter().all(|x| x.get("serial").is_some())),
            Err(e) => assert!(!e.0.is_empty()),
        }
    }
}
