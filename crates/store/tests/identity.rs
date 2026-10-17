//! Device identity: MAC choice, device ID and identity.json.
mod common;
use common::{logcap, tmp};
use railspotting_store::identity::{self, IdentityError};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn fake_net(root: &Path, ifaces: &[(&str, &str, bool)]) -> PathBuf {
    for (name, mac, physical) in ifaces {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("address"), format!("{mac}\n")).unwrap();
        if *physical {
            std::fs::create_dir(d.join("device")).unwrap();
        }
    }
    root.to_path_buf()
}

fn pick(net: &Path) -> (String, String) {
    identity::pick_mac(Some("Linux"), net).unwrap()
}

#[test]
fn valid_device_id() {
    assert!(identity::valid_device_id(&json!("rs-b827eb1a2b3c"))); // a v1 ID derived from a MAC
    assert!(identity::valid_device_id(&json!("rs-12ab34cd56ef"))); // assigned at link approval
    for bad in [json!("rs-B827EB1A2B3C"), json!("rs-b827eb1a2b3"), json!("b827eb1a2b3c"), Value::Null, json!(5),
                json!("rs-b827eb1a2b3c\n")] {
        assert!(!identity::valid_device_id(&bad), "{bad}");
    }
}

#[test]
fn wired_preferred_over_wireless() {
    let net = fake_net(&tmp("wired"), &[("lo", "00:00:00:00:00:00", false), ("wlan0", "b8:27:eb:00:00:01", true),
                                        ("eth0", "b8:27:eb:1a:2b:3c", true)]);
    assert_eq!(pick(&net), ("b8:27:eb:1a:2b:3c".into(), "eth0".into()));
}

#[test]
fn virtual_interfaces_ignored() {
    let net = fake_net(&tmp("virtual"), &[("docker0", "02:42:ac:11:00:01", false), ("veth12ab", "12:34:56:78:9a:bc", false),
                                          ("br-1234", "02:42:00:00:00:02", false), ("wlan0", "b8:27:eb:00:00:01", true)]);
    assert_eq!(pick(&net), ("b8:27:eb:00:00:01".into(), "wlan0".into()));
}

#[test]
fn lxc_eth0_without_device_entry_is_used() {
    let net = fake_net(&tmp("lxc"), &[("eth0", "02:00:00:3a:4b:5c", false)]);
    assert_eq!(pick(&net), ("02:00:00:3a:4b:5c".into(), "eth0".into()));
}

#[test]
fn ties_broken_by_name() {
    let net = fake_net(&tmp("ties"), &[("enp3s0", "00:11:22:33:44:66", true), ("end0", "00:11:22:33:44:55", true)]);
    assert_eq!(pick(&net).1, "end0");
}

#[test]
fn multicast_and_zero_rejected() {
    let net = fake_net(&tmp("mcast"), &[("eth0", "01:00:5e:00:00:01", true), ("eth1", "00:00:00:00:00:00", true),
                                        ("eth2", "00:11:22:33:44:55", true)]);
    assert_eq!(pick(&net).1, "eth2");
}

#[test]
fn locally_administered_warns() {
    logcap::init();
    let net = fake_net(&tmp("local"), &[("eth0", "02:11:22:33:44:55", true)]);
    assert_eq!(pick(&net).0, "02:11:22:33:44:55");
    assert!(logcap::text().contains("MAC 02:11:22:33:44:55 on eth0 is locally administered"));
}

#[test]
fn new_identity_has_no_device_id_until_linked() {
    logcap::init();
    let root = tmp("new");
    let net = fake_net(&root.join("net"), &[("eth0", "b8:27:eb:1a:2b:3c", true)]);
    let state = root.join("state");
    let ident = identity::load_or_create_identity(&state, Some("Linux"), &net).unwrap();
    assert_eq!(ident, json!({"device_id": null, "mac": "b8:27:eb:1a:2b:3c", "interface": "eth0"}));
    let text = std::fs::read_to_string(state.join("identity.json")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), ident);
    // byte for byte: indent 2, a final newline
    assert_eq!(text, "{\n  \"device_id\": null,\n  \"mac\": \"b8:27:eb:1a:2b:3c\",\n  \"interface\": \"eth0\"\n}\n");
    // the headend assigns an ID; a USB Wi-Fi dongle appears and eth0 vanishes: the ID stays
    let mut linked = ident.clone();
    linked["device_id"] = json!("rs-12ab34cd56ef");
    identity::save_identity(&state, &linked).unwrap();
    let net2 = fake_net(&root.join("net2"), &[("wlan1", "00:c0:ca:00:00:01", true)]);
    let again = identity::load_or_create_identity(&state, Some("Linux"), &net2).unwrap();
    assert_eq!(again["device_id"], "rs-12ab34cd56ef");
    assert!(logcap::text().contains("stored MAC b8:27:eb:1a:2b:3c is no longer present"));
}

#[test]
fn v1_identity_file_is_kept() {
    let root = tmp("v1");
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let v1 = json!({"device_id": "rs-0200003a4b5c", "mac": "02:00:00:3a:4b:5c", "interface": "eth0"});
    std::fs::write(state.join("identity.json"), v1.to_string()).unwrap();
    assert_eq!(identity::load_or_create_identity(&state, Some("Linux"), &root.join("none")).unwrap(), v1);
}

#[test]
fn no_usable_mac_still_creates_identity() {
    let root = tmp("nomac");
    let ident = identity::load_or_create_identity_with(&root.join("state"), Some("Linux"), &root.join("none"),
                                                       &|| 0x010000000000 | 5).unwrap();
    assert_eq!(ident, json!({"device_id": null, "mac": null, "interface": null}));
}

#[test]
fn no_interfaces_uses_getnode_or_fails() {
    let root = tmp("getnode");
    let r = identity::pick_mac_with(Some("Linux"), &root, &|| 0x010000000000 | 5); // random (multicast)
    assert_eq!(r, Err(IdentityError("no usable network interface MAC found".into())));
    assert_eq!(identity::pick_mac_with(Some("Linux"), &root, &|| 0xb827eb1a2b3c).unwrap(),
               ("b8:27:eb:1a:2b:3c".into(), "uuid.getnode".into()));
    // the default stand-in never claims to be hardware
    assert!(identity::pick_mac(Some("Linux"), &root).is_err());
}

#[test]
fn macos_networksetup_parsing() {
    let text = "Hardware Port: Ethernet\nDevice: en0\nEthernet Address: 3c:22:fb:01:02:03\n\n\
                Hardware Port: Wi-Fi\nDevice: en1\nEthernet Address: 3C-22-FB-04-05-06\n\n\
                Hardware Port: Thunderbolt Bridge\nDevice: bridge0\nEthernet Address: N/A\n\n\
                VLAN Configurations\n===================\n";
    let c = identity::parse_networksetup(text);
    assert_eq!(c.len(), 2);
    assert_eq!((c[0].iface.as_str(), c[0].mac.as_str(), c[0].wired), ("en0", "3c:22:fb:01:02:03", true));
    assert_eq!((c[1].iface.as_str(), c[1].mac.as_str(), c[1].wired), ("en1", "3c:22:fb:04:05:06", false));
    assert_eq!(identity::choose(&c).unwrap().iface, "en0");
    assert_eq!(identity::normalize_mac("B827EB1A2B3C"), "b8:27:eb:1a:2b:3c");
}
