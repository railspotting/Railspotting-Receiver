//! USB presence from sysfs (the agent parts are tested in the engine crate).
mod common;
use common::tmp;
use railspotting_store::usb::{plugged_in, UsbDongle};
use std::path::{Path, PathBuf};

fn fake_sysfs(root: &Path, devices: &[(&str, &str, &str, Option<&str>)]) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    for (port, vid, pid, serial) in devices {
        let d = root.join(port);
        std::fs::create_dir(&d).unwrap();
        std::fs::write(d.join("idVendor"), format!("{vid}\n")).unwrap();
        std::fs::write(d.join("idProduct"), format!("{pid}\n")).unwrap();
        if let Some(s) = serial {
            std::fs::write(d.join("serial"), format!("{s}\n")).unwrap();
        }
    }
    std::fs::create_dir(root.join("1-3:1.0")).unwrap(); // an interface entry, ignored
    root.to_path_buf()
}

fn dongle(serial: &str, port: &str) -> UsbDongle {
    UsbDongle { serial: serial.into(), port: port.into() }
}

#[test]
fn plugged_in_reads_rtl_dongles_only() {
    let t = tmp("usb");
    let root = fake_sysfs(&t.join("usb"), &[
        ("1-3", "0bda", "2838", Some("30002222")), ("1-4", "0bda", "2838", Some("30001111")),
        ("1-1.2", "0bda", "2832", None), ("usb1", "1d6b", "0002", Some("0000:00:15.0")),
        ("1-5", "10c4", "ea60", Some("0A1B2C3D"))]);
    assert_eq!(plugged_in(&root), Some(vec![dongle("", "1-1.2"), dongle("30002222", "1-3"), dongle("30001111", "1-4")]));
    assert_eq!(plugged_in(&t.join("missing")), None);
    assert_eq!(dongle("1", "1-3").to_json(), serde_json::json!({"serial": "1", "port": "1-3"}));
}
