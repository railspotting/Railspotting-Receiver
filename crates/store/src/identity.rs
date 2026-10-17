//! Device identity: the device ID the headend assigned, and the MAC reported for information.
//!
//! Since API contract v1.4 the headend assigns the device ID when the owner approves a link
//! (rs- plus 12 hex digits). Receivers registered under v1 keep the ID they derived from their MAC.

use crate::jsonfmt;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

pub const VIRTUAL_PREFIXES: [&str; 8] = ["docker", "veth", "br-", "virbr", "wg", "tun", "tap", "zt"];
pub const SYS_NET: &str = "/sys/class/net";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityError(pub String);

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for IdentityError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub iface: String,
    pub mac: String,
    pub wired: bool,
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn normalize_mac(mac: &str) -> String {
    let mac = mac.trim().to_lowercase().replace('-', ":");
    if mac.len() == 12 && is_hex(&mac) {
        return (0..12).step_by(2).map(|i| &mac[i..i + 2]).collect::<Vec<_>>().join(":");
    }
    mac
}

fn mac_ok_shape(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && is_hex(p))
}

/// Why a MAC can't be used, or None when it is acceptable.
pub fn mac_problem(mac: &str) -> Option<&'static str> {
    if !mac_ok_shape(mac) {
        return Some("not a MAC address");
    }
    let first = u8::from_str_radix(&mac[..2], 16).unwrap_or(0);
    if mac == "00:00:00:00:00:00" {
        return Some("all zero");
    }
    if first & 0x01 != 0 {
        return Some("multicast bit set");
    }
    None
}

pub fn locally_administered(mac: &str) -> bool {
    u8::from_str_radix(mac.get(..2).unwrap_or("00"), 16).unwrap_or(0) & 0x02 != 0
}

fn is_wired(name: &str) -> bool {
    name.starts_with("eth") || name.starts_with("end") || name.starts_with("en")
}

fn is_wireless(name: &str) -> bool {
    name.starts_with("wlan") || name.starts_with("wl")
}

pub fn linux_candidates(sys_net: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(sys_net) else { return out };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for entry in entries {
        let name = entry.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name == "lo" || VIRTUAL_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        // Physical interfaces have a "device" link. Virtual ones (bridges,
        // veth pairs, tunnels) don't. LXC containers are the exception: the
        // container's eth0 is a veth peer with no device entry but is still
        // the NIC we want, so accept eth*/en* without it.
        let has_device = entry.join("device").exists();
        if !has_device && !is_wired(&name) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.join("address")) else { continue };
        let mac = normalize_mac(&text);
        if mac_problem(&mac).is_some() {
            continue;
        }
        let wired = is_wired(&name);
        out.push(Candidate { iface: name, mac, wired });
    }
    out
}

/// Candidates from `networksetup -listallhardwareports` output.
pub fn parse_networksetup(text: &str) -> Vec<Candidate> {
    let field = |block: &str, key: &str| -> Option<String> {
        let i = block.find(key)?;
        let rest = block[i + key.len()..].trim_start();
        let v: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
        (!v.is_empty()).then_some(v)
    };
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let (Some(dev), Some(addr)) = (field(block, "Device:"), field(block, "Ethernet Address:")) else { continue };
        let mac = normalize_mac(&addr);
        if mac_problem(&mac).is_none() {
            // en0 first on macOS; networksetup reports the hardware address,
            // not the private Wi-Fi one.
            let wired = dev == "en0";
            out.push(Candidate { iface: dev, mac, wired });
        }
    }
    out
}

pub fn macos_candidates() -> Vec<Candidate> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(mut child) = Command::new("networksetup").arg("-listallhardwareports")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else { return Vec::new() };
    // the output is a few hundred bytes, so it fits the pipe while we wait (10 s timeout)
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut text);
    }
    parse_networksetup(&text)
}

pub fn choose(cands: &[Candidate]) -> Option<Candidate> {
    let rank = |c: &Candidate| (if c.wired { 0 } else if is_wireless(&c.iface) { 1 } else { 2 }, c.iface.clone());
    cands.iter().min_by_key(|c| rank(c)).cloned()
}

/// The platform name: `Linux`, `Darwin`, `Windows` and so on.
pub fn system_name() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        _ => "",
    }
}

fn candidates(system: Option<&str>, sys_net: &Path) -> Vec<Candidate> {
    match system.unwrap_or(system_name()) {
        "Linux" => linux_candidates(sys_net),
        "Darwin" => macos_candidates(),
        _ => Vec::new(),
    }
}

/// The node ID fallback when no interface qualifies: a random 48-bit number with the multicast
/// bit set, so `pick_mac` rejects it.
pub fn getnode() -> u64 {
    let r = uuid::Uuid::new_v4().as_u128() as u64 & 0xffff_ffff_ffff;
    r | 0x0100_0000_0000
}

/// (mac, interface) of the best hardware MAC on this machine.
pub fn pick_mac(system: Option<&str>, sys_net: &Path) -> Result<(String, String), IdentityError> {
    pick_mac_with(system, sys_net, &getnode)
}

pub fn pick_mac_with(system: Option<&str>, sys_net: &Path, getnode: &dyn Fn() -> u64)
    -> Result<(String, String), IdentityError> {
    let (mac, iface) = match choose(&candidates(system, sys_net)) {
        Some(best) => (best.mac, best.iface),
        None => {
            let node = getnode();
            if node >> 40 & 0x01 != 0 {
                return Err(IdentityError("no usable network interface MAC found".into()));
            }
            let mac = normalize_mac(&format!("{node:012x}"));
            if mac_problem(&mac).is_some() {
                return Err(IdentityError("no usable network interface MAC found".into()));
            }
            (mac, "uuid.getnode".to_string())
        }
    };
    if locally_administered(&mac) {
        log::warn!("MAC {mac} on {iface} is locally administered (often a randomized address); using it anyway");
    }
    Ok((mac, iface))
}

pub fn present_macs(system: Option<&str>, sys_net: &Path) -> BTreeSet<String> {
    candidates(system, sys_net).into_iter().map(|c| c.mac).collect()
}

/// rs- plus 12 lowercase hex digits, nothing else.
pub fn valid_device_id(device_id: &Value) -> bool {
    device_id.as_str().is_some_and(|s| s.len() == 15 && s.starts_with("rs-") && is_hex(&s[3..]))
}

pub fn save_identity(state_dir: &Path, ident: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("identity.json");
    let tmp = state_dir.join("identity.json.tmp");
    std::fs::write(&tmp, jsonfmt::dumps_indent(ident, 2) + "\n")?;
    std::fs::rename(tmp, path)
}

/// Read state/identity.json, or create it on first run with a MAC and no device ID yet.
///
/// The MAC is only reported to the headend for the admin's information, so a machine without
/// a usable one still runs.
pub fn load_or_create_identity(state_dir: &Path, system: Option<&str>, sys_net: &Path) -> Result<Value, String> {
    load_or_create_identity_with(state_dir, system, sys_net, &getnode)
}

pub fn load_or_create_identity_with(state_dir: &Path, system: Option<&str>, sys_net: &Path,
                                    getnode: &dyn Fn() -> u64) -> Result<Value, String> {
    let path = state_dir.join("identity.json");
    if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut ident: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let obj = ident.as_object_mut().ok_or_else(|| format!("{}: not a JSON object", path.display()))?;
        obj.entry("device_id").or_insert(Value::Null);
        let macs = present_macs(system, sys_net);
        if let Some(mac) = obj.get("mac").and_then(Value::as_str).filter(|m| !m.is_empty()) {
            if !macs.is_empty() && !macs.contains(mac) {
                log::warn!("stored MAC {mac} is no longer present; it is only reported for information");
            }
        }
        return Ok(ident);
    }
    let (mac, iface) = match pick_mac_with(system, sys_net, getnode) {
        Ok((m, i)) => (Value::String(m), Value::String(i)),
        Err(e) => {
            log::warn!("{e}; linking without a MAC");
            (Value::Null, Value::Null)
        }
    };
    let ident = json!({"device_id": null, "mac": mac, "interface": iface});
    save_identity(state_dir, &ident).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(ident)
}
