//! API contract v1.4 linking, the receiver's side, and the agent's half of the API contract.



use crate::common::*;
use railspotting_proto::keys;
use railspotting_proto::link::normalize_user_code;
use railspotting_engine::agent::LINK_TRIGGER;
use railspotting_engine::logging::Capture;
use railspotting_scanner::fake::{CarrierWorld, FakeSR30C};
use serde_json::{json, Value};
use std::sync::Arc;

#[test]
fn no_device_id_starts_a_link_and_shows_the_code() {
    let (c, t) = (clock(), Tmp::new("link1"));
    let h = headend(&c);
    let log = Capture::start();
    let agent = unlinked(t.path(), &h, &c);
    assert!(agent.device_id().is_none() && agent.reg_status() == "linking");
    assert_eq!(agent.link_step(), 5.0);
    assert_eq!(agent.reg_status(), "waiting");
    let info = agent.link_info().unwrap();
    let code = info["user_code"].as_str().unwrap();
    assert!(normalize_user_code(code).is_some() && &code[4..5] == "-");
    assert_eq!(info["verification_uri"], "https://railspotting.com/link");
    assert!(log.text().contains(&format!("LINK CODE {code}")));
    let start = h.state().link_starts.last().unwrap().clone();
    assert_eq!(start["public_key"], keys::public_b64(&agent.key));
    assert_eq!(start["mac"], "b8:27:eb:1a:2b:3c");
    assert!(start.get("device_id").is_none());
}

#[test]
fn approved_link_stores_id_and_runs_as_v1() {
    let (c, t) = (clock(), Tmp::new("link2"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    let code = agent.link().unwrap().user_code.to_lowercase(); // the owner types it any way
    let assigned = h.approve_link(&code, None).unwrap();
    let first = u8::from_str_radix(&assigned[3..5], 16).unwrap();
    assert!(first & 0x02 != 0 && first & 0x01 == 0);
    c.advance(5.0);
    assert_eq!(agent.link_step(), 0.0);
    assert_eq!(agent.device_id().as_deref(), Some(assigned.as_str()));
    assert!(agent.approved.is_set() && agent.reg_status() == "approved" && agent.link().is_none());
    assert_eq!(agent.config()["config_version"], "7"); // GET /config ran
    let stored = read_json(&agent.state_dir.join("identity.json"));
    assert_eq!(stored["device_id"], json!(assigned));
    assert_eq!(stored["mac"], "b8:27:eb:1a:2b:3c");
    agent.heartbeat_once().unwrap(); // v1, with X-RS-Device
    assert!(!h.state().heartbeats.is_empty());

    // restart: the stored ID is used and nothing is linked again
    let starts = h.state().link_starts.len();
    let again = make_agent(t.path(), &h, &c);
    assert_eq!(again.device_id().as_deref(), Some(assigned.as_str()));
    assert_eq!(again.reg_status(), "checking");
    again.on_device_id();
    assert!(again.approved.is_set() && again.reg_status() == "approved");
    assert_eq!(h.state().link_starts.len(), starts);
}

#[test]
fn pending_poll_repeats_code_log_every_30_s() {
    let (c, t) = (clock(), Tmp::new("link3"));
    let h = headend(&c);
    let log = Capture::start();
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    for _ in 0..12 {
        // 60 s of polling every 5 s
        c.advance(5.0);
        assert_eq!(agent.link_step(), 5.0);
    }
    assert_eq!(log.text().matches("LINK CODE").count(), 3);
}

#[test]
fn restart_while_waiting_shows_the_same_code() {
    let (c, t) = (clock(), Tmp::new("link4"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    let code = agent.link().unwrap().user_code;
    c.advance(120.0);
    let again = unlinked(t.path(), &h, &c);
    again.link_step();
    let lk = again.link().unwrap();
    assert_eq!(lk.user_code, code);
    assert!((lk.expires_at - (c.get() + 780.0)).abs() <= 1.0);
}

#[test]
fn key_already_approved_gets_its_id_at_once() {
    let (c, t) = (clock(), Tmp::new("link5"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    h.add_device("rs-12ab34cd56ef", &keys::public_bytes(&agent.key), "approved");
    assert_eq!(agent.link_step(), 0.0);
    assert_eq!(agent.device_id().as_deref(), Some("rs-12ab34cd56ef"));
    assert!(agent.approved.is_set());
}

#[test]
fn expired_starts_again_with_a_new_code_at_most_once_a_minute() {
    let (c, t) = (clock(), Tmp::new("link6"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    let lk = agent.link().unwrap();
    h.state().links.get_mut(&lk.link_id).unwrap().expires = c.get() + 10.0;
    c.advance(10.0);
    assert_eq!(agent.link_step(), 0.0); // expired: no link to show
    assert!(agent.link().is_none());
    let wait = agent.link_step(); // not yet: the last start was 10 s ago
    assert!((wait - 50.0).abs() < 1e-6 && agent.link().is_none());
    c.advance(wait);
    agent.link_step();
    let new = agent.link().unwrap();
    assert!(new.user_code != lk.user_code && new.link_id != lk.link_id);
}

#[test]
fn local_expiry_without_answers() {
    let (c, t) = (clock(), Tmp::new("link7"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    let code = agent.link().unwrap().user_code;
    h.set_down(true);
    c.advance(901.0);
    agent.link_step(); // gives up on the old code, start fails
    assert!(agent.link().is_none() && agent.reg_status() == "linking");
    h.set_down(false);
    c.advance(60.0);
    agent.link_step();
    assert_ne!(agent.link().unwrap().user_code, code);
}

#[test]
fn denied_stops_until_the_owner_asks() {
    let (c, t) = (clock(), Tmp::new("link8"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    h.deny_link(&agent.link().unwrap().user_code);
    c.advance(5.0);
    agent.link_step();
    assert!(agent.reg_status() == "denied" && agent.link().is_none());
    let starts = h.state().link_starts.len();
    for _ in 0..5 {
        c.advance(600.0);
        agent.link_step();
    }
    assert_eq!(h.state().link_starts.len(), starts);
    // a restart remembers the denial
    let again = unlinked(t.path(), &h, &c);
    again.link_step();
    assert!(again.reg_status() == "denied" && h.state().link_starts.len() == starts);
    // `railspotting-receiver link` asks the running service to start again
    std::fs::write(again.state_dir.join(LINK_TRIGGER), b"").unwrap();
    again.link_trigger_tick().unwrap();
    again.link_step();
    assert!(again.reg_status() == "waiting" && h.state().link_starts.len() == starts + 1);
}

#[test]
fn slow_down_adds_five_seconds() {
    let (c, t) = (clock(), Tmp::new("link9"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    c.advance(5.0);
    agent.link_step();
    c.advance(2.0); // too soon
    assert_eq!(agent.link_step(), 10.0);
    c.advance(10.0);
    assert_eq!(agent.link_step(), 10.0); // keeps the slower interval
}

#[test]
fn not_found_starts_a_new_link() {
    let (c, t) = (clock(), Tmp::new("link10"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    h.state().links.clear();
    c.advance(5.0);
    assert_eq!(agent.link_step(), 0.0);
    assert!(agent.link().is_none());
    c.advance(60.0);
    agent.link_step();
    assert!(agent.reg_status() == "waiting" && h.state().links.len() == 1);
}

#[test]
fn network_failure_keeps_the_code_and_backs_off() {
    let (c, t) = (clock(), Tmp::new("link11"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    let code = agent.link().unwrap().user_code;
    h.set_down(true);
    c.advance(5.0);
    let d = agent.link_step();
    assert!((1.0..=2.0).contains(&d), "{d}");
    assert!(agent.link().unwrap().user_code == code && agent.reg_status() == "waiting");
    h.set_down(false);
    c.advance(5.0);
    agent.link_step();
    assert_eq!(agent.link().unwrap().user_code, code);
}

#[test]
fn invalid_assigned_id_is_refused() {
    let (c, t) = (clock(), Tmp::new("link12"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    h.approve_link(&agent.link().unwrap().user_code, Some("rs-NOTVALID"));
    c.advance(5.0);
    agent.link_step();
    assert!(agent.device_id().is_none() && !agent.approved.is_set());
}

#[test]
fn revoked_links_again_with_the_same_key() {
    let (c, t) = (clock(), Tmp::new("link13"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    agent.on_device_id();
    let public = keys::public_b64(&agent.key);
    h.state().devices.get_mut(DEVICE_ID).unwrap().status = "revoked".into();
    let e = agent.heartbeat_once().unwrap_err();
    agent.handle_api_error(&e);
    assert!(agent.device_id().is_none() && !agent.approved.is_set());
    agent.link_step();
    assert_eq!(agent.reg_status(), "waiting");
    assert_eq!(h.state().link_starts.last().unwrap()["public_key"], json!(public));
}

#[test]
fn v1_receiver_keeps_its_id() {
    // an approved v1 receiver (ID from its MAC) keeps working without linking
    let (c, t) = (clock(), Tmp::new("link14"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    assert_eq!(agent.device_id().as_deref(), Some(DEVICE_ID));
    agent.on_device_id();
    agent.heartbeat_once().unwrap();
    assert!(agent.reg_status() == "approved" && h.state().link_starts.is_empty());
}

#[test]
fn v1_receiver_never_approved_switches_to_linking() {
    for (status, error) in [(403, "pending"), (403, "rejected"), (401, "unknown_device")] {
        let (c, t) = (clock(), Tmp::new("link15"));
        let h = headend(&c);
        let agent = make_agent(t.path(), &h, &c);
        if error == "unknown_device" {
            h.state().devices.remove(DEVICE_ID);
        } else {
            h.state().devices.get_mut(DEVICE_ID).unwrap().status = error.into();
        }
        agent.on_device_id();
        assert!(agent.device_id().is_none() && !agent.approved.is_set(), "{status} {error}");
        agent.link_step();
        assert_eq!(agent.reg_status(), "waiting");
    }
}

#[test]
fn registration_loop_links_then_returns_in_foreground_mode() {
    let (c, t) = (clock(), Tmp::new("link16"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    let a2 = agent.clone();
    let step = || {
        let d = a2.link_step();
        if let Some(lk) = a2.link() {
            if a2.link_info().is_some() && h.state().devices.is_empty() {
                h.approve_link(&lk.user_code, None);
                c.advance(5.0);
                return 0.0;
            }
        }
        d.min(0.01)
    };
    agent.registration_loop_with(true, &step);
    assert!(agent.device_id().is_some() && agent.approved.is_set());
}

#[test]
fn status_shows_the_link() {
    let (c, t) = (clock(), Tmp::new("link17"));
    let h = headend(&c);
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    let st = agent.status();
    assert!(st["registration"] == "waiting" && st["device_id"].is_null());
    assert_eq!(st["link"]["user_code"], agent.link_info().unwrap()["user_code"]);
    assert!(st["link"].get("link_id").is_none());
    let mut st2 = st.clone();
    st2["link"]["expires_at"] = json!(railspotting_common::now() + 600.0);
    let text = railspotting_engine::cli::describe_registration(&st2);
    assert!(text.contains(st["link"]["user_code"].as_str().unwrap()) && text.contains("https://railspotting.com/link"));
}

#[test]
fn rekey_forgets_the_device_id() {
    let (c, t) = (clock(), Tmp::new("link18"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    let old = keys::public_b64(&agent.key);
    let cfg = t.path().join("receiver.json");
    std::fs::write(&cfg, json!({"state_dir": agent.state_dir.to_string_lossy()}).to_string()).unwrap();
    let mut out = vec![];
    let code = railspotting_engine::cli::main_with(
        vec!["railspotting-receiver".into(), "--config".into(), cfg.to_string_lossy().into(), "--rekey".into()], &mut out);
    assert_eq!(code, 0);
    let stored = read_json(&agent.state_dir.join("identity.json"));
    assert!(stored["device_id"].is_null());
    let new = keys::load_or_create_key(&t.path().join("credentials.json"), false).unwrap();
    assert_ne!(keys::public_b64(&new), old);
    assert!(String::from_utf8(out).unwrap().contains("rs-b827eb1a2b3c"));
}

// the API contract, the agent's half ---------------------------------------------------------------

#[test]
fn revoked_403_sends_agent_back_to_linking() {
    let (c, t) = (clock(), Tmp::new("contract1"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    agent.approved.set();
    h.fail_next(403, "revoked", &[]);
    let e = agent.heartbeat_once().unwrap_err();
    agent.handle_api_error(&e);
    assert!(!agent.approved.is_set() && agent.device_id().is_none());
    assert!(agent.api.device_id().is_none() && agent.reg_status() == "linking");
    let stored = read_json(&agent.state_dir.join("identity.json"));
    assert!(stored["device_id"].is_null() && stored["mac"].is_string());
}

#[test]
fn privacy_in_link_and_heartbeat() {
    let (c, t) = (clock(), Tmp::new("contract2"));
    let h = headend(&c);
    let fake = Arc::new(FakeSR30C::new(Arc::new(CarrierWorld::default()), c.clock()));
    let agent = make_agent(t.path(), &h, &c);
    let opener = fake.opener();
    agent.monitor.set_hooks(|hk| {
        hk.opener = opener;
        hk.finder = Arc::new(|| Some("/dev/serial/by-id/usb-Silicon_Labs_CP2104_USB_to_UART_Bridge_Controller_0A1B2C3D-if00-port0".into()));
        hk.sleep = Arc::new(|_| {});
    });
    assert!(agent.monitor.try_connect());
    let body = agent.link_body();
    assert!(body["scanner"]["cable_serial"].is_null() && body["scanner"]["port"].is_null());
    assert_eq!(body["scanner"]["model"], "SR30C");
    let blob = body.to_string();
    assert!(!blob.contains("0A1B2C3D") && !blob.contains("/dev/"));
    agent.heartbeat_once().unwrap();
    let hb = h.state().heartbeats.last().unwrap().clone();
    assert!(hb["scanner"]["cable_serial"].is_null() && hb["scanner"]["port"].is_null());
    let blob = hb.to_string();
    assert!(!blob.contains("0A1B2C3D") && !blob.contains("/dev/"));
    assert_eq!(agent.monitor.local_info()["cable_serial"], "0A1B2C3D"); // local status only
}

#[test]
fn capabilities_contain_no_serials() {
    let (c, t) = (clock(), Tmp::new("contract3"));
    let h = headend(&c);
    let world = Arc::new(railspotting_dsp::sim::World::default());
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [
        {"serial": "30002222", "role": "vhf", "sample_rate": 2880000.0, "gain": 20.7},
        {"serial": "30001111", "role": "eot", "sample_rate": 2400000.0, "gain": 40.2}]}}), true, Default::default());
    let p = plan("7");
    for s in agent.sdrs() {
        s.with(|x| {
            x.set_plan(&channels(&p), Some(&p["reference"]));
            x.info = railspotting_dsp::sim::SimDevice::new(&x.serial, world.clone(), c.clock(), false).info.clone();
            x.connected = true;
        });
    }
    let caps = agent.capabilities();
    let blob = caps.to_string();
    assert!(!blob.contains("30002222") && !blob.contains("30001111"));
    let (v, e) = (&caps["receivers"][1], &caps["receivers"][2]);
    assert_eq!(v["window_mhz"], json!([160.2204, 162.87]));
    assert_eq!(v["channels"], json!([1, 2, 3, 4, 5, 6, 7, 8]));
    assert_eq!(e["window_mhz"], json!([457.0345, 459.2425]));
    assert_eq!(e["channels"], json!([9]));
    assert!(v["product"] == "NESDR SMArt v5" && v["tuner"] == "R820T/R820T2");
    assert!(caps["receivers"][0]["connected"] == json!(false) && caps["receivers"][0]["vendor"].is_null());
    let f = &caps["features"];
    assert!(f["simultaneous_voice"] == json!(true) && f["continuous_eot"] == json!(true) && f["eot_decode"] == json!(true));
    assert!(f["continuous_hot"] == json!(false) && f["transcription"] == json!(false));
}

#[test]
fn private_key_never_sent_or_logged() {
    let (c, t) = (clock(), Tmp::new("contract4"));
    let h = headend(&c);
    let log = Capture::start();
    let agent = unlinked(t.path(), &h, &c);
    agent.link_step();
    h.approve_link(&agent.link().unwrap().user_code, None);
    c.advance(5.0);
    agent.link_step();
    assert!(agent.device_id().is_some());
    agent.heartbeat_once().unwrap();
    let secret = keys::private_to_b64(&agent.key);
    assert!(h.state().raw_bodies.iter().all(|b| !String::from_utf8_lossy(b).contains(&secret)));
    assert!(!log.text().contains(&secret));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(agent.settings.credentials_path()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let _: Value = json!(null);
}
