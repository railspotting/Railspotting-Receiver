//! Which RTL-SDR dongles are plugged in, from Linux sysfs.
//!
//! Reading /sys/bus/usb/devices touches no device, so it is safe every few seconds while the
//! dongles stream. It also gives the USB port each dongle is in (for example 1-3, or 1-1.2 behind a
//! hub), which stays the same for a given socket.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const SYS_USB: &str = "/sys/bus/usb/devices";
/// RTL2832U dongles (DVB-T / SDR firmware)
pub const RTL_IDS: [(&str, &str); 2] = [("0bda", "2838"), ("0bda", "2832")];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDongle {
    pub serial: String,
    pub port: String,
}

impl UsbDongle {
    pub fn to_json(&self) -> Value {
        json!({"serial": self.serial, "port": self.port})
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// One entry for each RTL-SDR on USB, or None when sysfs can't be read here.
pub fn plugged_in(sys_usb: &Path) -> Option<Vec<UsbDongle>> {
    if !sys_usb.is_dir() {
        return None;
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(sys_usb).ok()?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    let mut out = Vec::new();
    for d in entries {
        let name = d.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.contains(':') {
            continue; // an interface, not a device
        }
        let (Some(vid), Some(pid)) = (read(&d.join("idVendor")), read(&d.join("idProduct"))) else { continue };
        if RTL_IDS.iter().any(|(v, p)| *v == vid && *p == pid) {
            out.push(UsbDongle { serial: read(&d.join("serial")).unwrap_or_default(), port: name });
        }
    }
    Some(out)
}

/// This machine's dongles: sysfs on Linux, None elsewhere (the receiver then asks librtlsdr).
pub fn plugged_in_here() -> Option<Vec<UsbDongle>> {
    #[cfg(target_os = "linux")]
    {
        plugged_in(Path::new(SYS_USB))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
