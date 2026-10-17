//! Uniden SR30C driver over the CP2104 USB serial cable (firmware 1.01.04).

use crate::util::{parse_int, repr_text};
use crate::Sleep;
use serde_json::{json, Value};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const VID: u16 = 0x10C4;
pub const PID: u16 = 0xEA60;
pub const BAUD: u32 = 57600;
pub const BANDS_MHZ: [[u32; 2]; 3] = [[25, 54], [108, 174], [406, 512]];
pub const EMPTY_FREQ: &str = "00000000";
/// CLR erases memory on Uniden scanners.
pub const FORBIDDEN: [&str; 1] = ["CLR"];
/// How long one reply may take.
pub const READ_TIMEOUT_S: f64 = 1.0;

/// What went wrong talking to the scanner.
///
/// `Io` is anything the protocol or the port raises. `Value` is a reply that could not be
/// parsed as a number or had too few fields, which callers handle differently in a few places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScannerError {
    Io(String),
    Value(String),
}

impl ScannerError {
    pub fn msg(&self) -> &str {
        match self {
            ScannerError::Io(m) | ScannerError::Value(m) => m,
        }
    }

    pub fn is_io(&self) -> bool {
        matches!(self, ScannerError::Io(_))
    }
}

impl fmt::Display for ScannerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.msg())
    }
}

impl std::error::Error for ScannerError {}

impl From<io::Error> for ScannerError {
    fn from(e: io::Error) -> Self {
        ScannerError::Io(e.to_string())
    }
}

fn io_err(msg: impl Into<String>) -> ScannerError {
    ScannerError::Io(msg.into())
}

fn value_err(msg: impl Into<String>) -> ScannerError {
    ScannerError::Value(msg.into())
}

/// MHz to 8 digits in 100 Hz units: 160.905 -> 01609050.
pub fn freq_to_digits(freq_mhz: f64) -> String {
    format!("{:08}", (freq_mhz * 10_000.0).round_ties_even() as i64)
}

pub fn digits_to_freq(digits: &str) -> Option<f64> {
    let v: i64 = digits.trim().parse().ok()?;
    if v == 0 {
        None
    } else {
        Some(crate::util::round_even(v as f64 / 10_000.0, 4))
    }
}

/// A serial port as the driver uses it, so a fake scanner can stand in for the cable.
pub trait SerialLink: Send {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// Bytes up to and including `term`, or what arrived before the read timeout.
    fn read_until(&mut self, term: u8) -> io::Result<Vec<u8>>;
    fn reset_input_buffer(&mut self) -> io::Result<()>;
    fn close(&mut self) {}
}

/// The function that opens a port: `open_serial` for the cable, the fake's opener in tests.
pub type Opener = Arc<dyn Fn(&str, u32) -> io::Result<Box<dyn SerialLink>> + Send + Sync>;
/// Finds the cable's port when the setting is "auto".
pub type Finder = Arc<dyn Fn() -> Option<String> + Send + Sync>;

struct SystemSerial {
    port: Option<Box<dyn serialport::SerialPort>>,
}

impl SystemSerial {
    fn port(&mut self) -> io::Result<&mut Box<dyn serialport::SerialPort>> {
        self.port.as_mut().ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Attempting to use a port that is not open"))
    }
}

impl SerialLink for SystemSerial {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        io::Write::write_all(self.port()?, data)
    }

    fn read_until(&mut self, term: u8) -> io::Result<Vec<u8>> {
        // One byte at a time against one overall deadline.
        let deadline = Instant::now() + Duration::from_secs_f64(READ_TIMEOUT_S);
        let port = self.port()?;
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(out);
            }
            port.set_timeout(left).map_err(io::Error::from)?;
            match io::Read::read(port, &mut b) {
                Ok(0) => return Ok(out),
                Ok(_) => {
                    out.push(b[0]);
                    if b[0] == term {
                        return Ok(out);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(out),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn reset_input_buffer(&mut self) -> io::Result<()> {
        self.port()?.clear(serialport::ClearBuffer::Input).map_err(io::Error::from)
    }

    fn close(&mut self) {
        self.port = None;
    }
}

/// Open the cable: 8N1, no flow control, 1 s reads.
pub fn open_serial(port: &str, baud: u32) -> io::Result<Box<dyn SerialLink>> {
    let p = serialport::new(port, baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(Duration::from_secs_f64(READ_TIMEOUT_S))
        .open()
        .map_err(io::Error::from)?;
    Ok(Box::new(SystemSerial { port: Some(p) }))
}

pub fn system_opener() -> Opener {
    Arc::new(open_serial)
}

pub fn system_finder() -> Finder {
    Arc::new(find_port)
}

/// USB serial ports with the CP2104's vendor and product IDs.
fn cp2104_ports() -> Vec<String> {
    // serialport's sysfs listing panics when /sys/class/tty is missing (some containers)
    #[cfg(target_os = "linux")]
    if !Path::new("/sys/class/tty").is_dir() {
        return Vec::new();
    }
    let Ok(ports) = serialport::available_ports() else { return Vec::new() };
    let mut out: Vec<String> = ports
        .into_iter()
        .filter(|p| matches!(&p.port_type, serialport::SerialPortType::UsbPort(u) if u.vid == VID && u.pid == PID))
        .map(|p| p.port_name)
        .collect();
    // macOS lists each device as /dev/tty.* and /dev/cu.*; cu is the one that doesn't wait for DCD
    out.sort_by_key(|p| !p.starts_with("/dev/cu."));
    out
}

fn glob_dir(dir: &str, pred: impl Fn(&str) -> bool) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| pred(n))
                .map(|n| format!("{}/{}", dir.trim_end_matches('/'), n))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn same_file(a: &str, b: &str) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

#[cfg(unix)]
fn is_char_device(p: &str) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(p).map(|m| m.file_type().is_char_device()).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_char_device(_p: &str) -> bool {
    false
}

/// Auto-detect the CP2104 cable, preferring the stable /dev/serial/by-id path.
pub fn find_port() -> Option<String> {
    let by_id = glob_dir("/dev/serial/by-id", |n| n.contains("CP2104"));
    if let Some(dev) = cp2104_ports().into_iter().next() {
        if dev.starts_with("/dev/tty") {
            if let Some(link) = by_id.iter().find(|l| same_file(l, &dev)) {
                return Some(link.clone());
            }
        }
        return Some(dev);
    }
    // without USB details from the port listing, the by-id name still says CP2104
    if let Some(link) = by_id.into_iter().next() {
        return Some(link);
    }
    #[cfg(target_os = "macos")]
    {
        let cu = glob_dir("/dev", |n| n.starts_with("cu.usbserial") || n.starts_with("cu.SLAB_USBtoUART"));
        if let Some(p) = cu.into_iter().next() {
            return Some(p);
        }
    }
    if Path::new("/dev/railspotting-scanner").exists() {
        return Some("/dev/railspotting-scanner".into());
    }
    // with the scanner unplugged, the LXC bind mount leaves an empty placeholder file here
    if is_char_device("/dev/ttyUSB0") {
        return Some("/dev/ttyUSB0".into());
    }
    None
}

/// The CP2104 USB serial identifies the cable. Local hardware info only.
pub fn cable_serial_from_port(port: Option<&str>) -> Option<String> {
    let port = port.filter(|p| !p.is_empty())?;
    static RE: std::sync::OnceLock<(regex::Regex, regex::Regex)> = std::sync::OnceLock::new();
    let (a, b) = RE.get_or_init(|| {
        (regex::Regex::new(r"Controller_([0-9A-Za-z]+)-if").expect("valid regex"),
         regex::Regex::new(r"usbserial-(\w+)").expect("valid regex"))
    });
    a.captures(port).or_else(|| b.captures(port)).map(|c| c[1].to_string())
}

/// GLG: what the radio is on right now.
#[derive(Debug, Clone, PartialEq)]
pub struct Glg {
    pub freq: Option<f64>,
    pub modulation: String,
    pub sql: bool,
    pub mute: bool,
}

/// One channel's memory as CIN reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelMem {
    pub ch: i64,
    pub name: String,
    pub freq: Option<f64>,
    pub modulation: String,
    pub ctcss: String,
    pub delay: String,
    pub lockout: bool,
    pub priority: String,
}

impl ChannelMem {
    /// The JSON object saved in scanner_restore.json.
    pub fn to_json(&self) -> Value {
        json!({"ch": self.ch, "name": self.name, "freq": self.freq, "mod": self.modulation,
               "ctcss": self.ctcss, "delay": self.delay, "lockout": self.lockout, "priority": self.priority})
    }
}

/// Command layer. One command at a time; replies end with CR.
///
/// Exclusive use comes from `&mut self` (the monitor holds the scanner behind its lock).
pub struct Scanner {
    ser: Box<dyn SerialLink>,
    pub port: Option<String>,
    sleep: Sleep,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub in_program: bool,
}

fn second(reply: &str) -> Result<&str, ScannerError> {
    reply.split(',').nth(1).ok_or_else(|| value_err("list index out of range"))
}

impl Scanner {
    pub fn new(ser: Box<dyn SerialLink>, port: Option<String>, sleep: Sleep) -> Self {
        Scanner { ser, port, sleep, model: None, firmware: None, in_program: false }
    }

    pub fn open(port: &str, baud: u32, opener: &Opener, sleep: Sleep) -> Result<Scanner, ScannerError> {
        let mut s = Scanner::new(opener(port, baud)?, Some(port.to_string()), sleep);
        s.wake()?;
        s.identify()?;
        Ok(s)
    }

    pub fn wake(&mut self) -> Result<(), ScannerError> {
        self.ser.write(b"\r")?;
        (self.sleep)(0.15);
        self.ser.reset_input_buffer()?;
        Ok(())
    }

    pub fn close(&mut self) {
        self.ser.close();
    }

    pub fn cmd(&mut self, text: &str) -> Result<String, ScannerError> {
        let head = text.split(',').next().unwrap_or("");
        if FORBIDDEN.contains(&head.to_uppercase().as_str()) {
            return Err(io_err(format!("refusing to send {text}")));
        }
        if !text.is_ascii() {
            return Err(io_err(format!("serial error: 'ascii' codec can't encode {}", repr_text(text))));
        }
        let mut data = text.as_bytes().to_vec();
        data.push(b'\r');
        let raw = self
            .ser
            .write(&data)
            .and_then(|_| self.ser.read_until(b'\r'))
            .map_err(|e| io_err(format!("serial error: {e}")))?;
        if raw.last() != Some(&b'\r') {
            return Err(io_err(format!("no reply to {head}")));
        }
        let text: String = raw.iter().map(|&b| if b.is_ascii() { b as char } else { '\u{fffd}' }).collect();
        Ok(text.trim().to_string())
    }

    pub fn ok(&mut self, text: &str) -> Result<(), ScannerError> {
        let reply = self.cmd(text)?;
        if !reply.ends_with(",OK") {
            return Err(io_err(format!("{text} -> {reply}")));
        }
        Ok(())
    }

    // identification ---------------------------------------------------------------

    pub fn identify(&mut self) -> Result<(), ScannerError> {
        let mdl = self.cmd("MDL")?;
        let ver = self.cmd("VER")?;
        let Some(model) = mdl.strip_prefix("MDL,") else {
            return Err(io_err(format!("unexpected MDL reply {}", repr_text(&mdl))));
        };
        self.model = Some(model.to_string());
        self.firmware = ver.strip_prefix("VER,").map(String::from);
        Ok(())
    }

    // status --------------------------------------------------------------------------

    pub fn glg(&mut self) -> Result<Glg, ScannerError> {
        let reply = self.cmd("GLG")?;
        let mut r: Vec<&str> = reply.split(',').collect();
        if r[0] != "GLG" {
            return Err(io_err(format!("bad GLG reply {}", repr_text(&reply))));
        }
        while r.len() < 11 {
            r.push("");
        }
        let freq = if r[1].is_empty() { 0.0 } else { r[1].trim().parse::<f64>().unwrap_or(0.0) };
        Ok(Glg {
            freq: if freq != 0.0 { Some(crate::util::round_even(freq, 4)) } else { None },
            modulation: r[2].to_string(),
            sql: r[8] == "1",
            mute: r[9] == "1",
        })
    }

    pub fn pwr(&mut self) -> Result<(i64, Option<f64>), ScannerError> {
        let reply = self.cmd("PWR")?;
        let r: Vec<&str> = reply.split(',').collect();
        if r[0] != "PWR" || r.len() < 3 {
            return Err(io_err(format!("bad PWR reply {}", repr_text(&reply))));
        }
        Ok((parse_int(r[1]).map_err(value_err)?, digits_to_freq(r[2])))
    }

    pub fn volume(&mut self) -> Result<i64, ScannerError> {
        let reply = self.cmd("VOL")?;
        parse_int(second(&reply)?).map_err(value_err)
    }

    pub fn set_volume(&mut self, level: i64) -> Result<(), ScannerError> {
        self.ok(&format!("VOL,{}", level.clamp(0, 15)))
    }

    pub fn squelch(&mut self) -> Result<i64, ScannerError> {
        let reply = self.cmd("SQL")?;
        parse_int(second(&reply)?).map_err(value_err)
    }

    pub fn set_squelch(&mut self, level: i64) -> Result<(), ScannerError> {
        self.ok(&format!("SQL,{}", level.clamp(0, 15)))
    }

    pub fn key(&mut self, k: &str) -> Result<(), ScannerError> {
        if !k.is_empty() && k.chars().all(|c| c.is_ascii_digit()) {
            return Err(io_err("number keys toggle banks while scanning; not sent"));
        }
        self.ok(&format!("KEY,{k},P"))
    }

    /// KEY,H,P releases a park (EPG leaves the radio parked on channel 1).
    pub fn resume_scan(&mut self) -> Result<(), ScannerError> {
        self.key("H")
    }

    // program mode -----------------------------------------------------------------------

    pub fn prg(&mut self) -> Result<(), ScannerError> {
        self.ok("PRG")?;
        self.in_program = true;
        Ok(())
    }

    pub fn epg(&mut self, resume: bool) -> Result<(), ScannerError> {
        self.ok("EPG")?;
        self.in_program = false;
        if resume {
            self.resume_scan()?;
        }
        Ok(())
    }

    pub fn cin_read(&mut self, n: i64) -> Result<ChannelMem, ScannerError> {
        let reply = self.cmd(&format!("CIN,{n}"))?;
        let r: Vec<&str> = reply.split(',').collect();
        if r[0] != "CIN" || r.len() < 9 {
            return Err(io_err(format!("bad CIN reply {}", repr_text(&reply))));
        }
        Ok(ChannelMem {
            ch: parse_int(r[1]).map_err(value_err)?,
            name: r[2].into(),
            freq: digits_to_freq(r[3]),
            modulation: r[4].into(),
            ctcss: r[5].into(),
            delay: r[6].into(),
            lockout: r[7] == "1",
            priority: r[8].into(),
        })
    }

    pub fn cin_write(&mut self, n: i64, freq_mhz: f64, delay_on: bool, lockout: bool) -> Result<(), ScannerError> {
        self.ok(&format!("CIN,{n},,{},AUTO,0,{},{},0", freq_to_digits(freq_mhz), if delay_on { 2 } else { 0 },
                         if lockout { 1 } else { 0 }))
    }

    pub fn cin_clear(&mut self, n: i64) -> Result<(), ScannerError> {
        self.ok(&format!("CIN,{n},,{EMPTY_FREQ},,,0,1,0"))
    }

    pub fn scg(&mut self) -> Result<String, ScannerError> {
        let reply = self.cmd("SCG")?;
        Ok(second(&reply)?.to_string())
    }

    pub fn set_scg(&mut self, banks: &str) -> Result<(), ScannerError> {
        if banks.chars().count() != 10 || banks.chars().any(|c| c != '0' && c != '1') {
            return Err(io_err(format!("bad bank mask {}", repr_text(banks))));
        }
        self.ok(&format!("SCG,{banks}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct RecordingPort {
        written: Arc<Mutex<Vec<Vec<u8>>>>,
        flushed: Arc<Mutex<u32>>,
    }

    impl SerialLink for RecordingPort {
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            self.written.lock().unwrap().push(data.to_vec());
            Ok(())
        }
        fn read_until(&mut self, _term: u8) -> io::Result<Vec<u8>> {
            let last = self.written.lock().unwrap().last().cloned().unwrap_or_default();
            Ok(match last.as_slice() {
                b"MDL\r" => b"MDL,SR30C\r".to_vec(),
                b"VER\r" => b"VER,Version 1.01.04\r".to_vec(),
                _ => b"ERR\r".to_vec(),
            })
        }
        fn reset_input_buffer(&mut self) -> io::Result<()> {
            *self.flushed.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn open_wakes_flushes_and_identifies() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let flushed = Arc::new(Mutex::new(0));
        let slept = Arc::new(Mutex::new(Vec::new()));
        let (w, f) = (written.clone(), flushed.clone());
        let opener: Opener = Arc::new(move |_p, _b| Ok(Box::new(RecordingPort { written: w.clone(), flushed: f.clone() }) as Box<dyn SerialLink>));
        let sl = slept.clone();
        let s = Scanner::open("/dev/x", BAUD, &opener, Arc::new(move |t| sl.lock().unwrap().push(t))).unwrap();
        assert_eq!(written.lock().unwrap()[0], b"\r");
        assert_eq!(*slept.lock().unwrap(), vec![0.15]);
        assert_eq!(*flushed.lock().unwrap(), 1);
        assert_eq!((s.model.as_deref(), s.firmware.as_deref()), (Some("SR30C"), Some("Version 1.01.04")));
    }

    #[test]
    fn freq_digits() {
        assert_eq!(freq_to_digits(160.905), "01609050");
        assert_eq!(freq_to_digits(457.9375), "04579375");
        assert_eq!(digits_to_freq("01609050"), Some(160.905));
        assert_eq!(digits_to_freq("00000000"), None);
        assert_eq!(digits_to_freq("x"), None);
    }

    #[test]
    fn cable_serial() {
        assert_eq!(cable_serial_from_port(Some("/dev/serial/by-id/usb-Silicon_Labs_CP2104_USB_to_UART_Bridge_Controller_0A1B2C3D-if00-port0")),
                   Some("0A1B2C3D".into()));
        assert_eq!(cable_serial_from_port(Some("/dev/cu.usbserial-0A1B2C3D")), Some("0A1B2C3D".into()));
        assert_eq!(cable_serial_from_port(Some("/dev/ttyUSB0")), None);
        assert_eq!(cable_serial_from_port(None), None);
    }
}
