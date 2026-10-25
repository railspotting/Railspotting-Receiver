//! The agent's loops and ticks: uploads and heartbeats, automatic roles, USB presence, scanner
//! commands, the SDR reference, and the device thread.



use crate::common::*;
use railspotting_dsp::sdr::{SdrSource, TranscriptDone, QUEUE_BLOCKS};
use railspotting_dsp::sim::World;
use railspotting_engine::agent::{AgentOptions, AgentTranscriber, Lister, UsbLister};
use railspotting_engine::device::{Device, DeviceFactory, SdrHandle, SimDev};
use railspotting_engine::logging::Capture;
use railspotting_engine::rtl::DriverError;
use railspotting_scanner::fake::{CarrierWorld, FakeSR30C};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

fn event(state: &str, ts: f64) -> Map<String, Value> {
    let v = json!({"event_id": uuid::Uuid::new_v4().to_string(), "state": state, "start_ts": ts,
                   "end_ts": if state == "open" { Value::Null } else { json!(ts + 1.0) }, "freq": 160.95, "ch": 5,
                   "kind": "voice", "peak_rssi": 420, "avg_rssi": 410, "samples": 3});
    v.as_object().unwrap().clone()
}

fn approved(t: &Tmp, h: &railspotting_proto::fake::FakeHeadend, c: &railspotting_common::FakeClock) -> Arc<railspotting_engine::Agent> {
    let a = make_agent(t.path(), h, c);
    a.approved.set();
    a
}

// uploads ----------------------------------------------------------------------------------------

#[test]
fn retry_and_idempotency() {
    let (c, t) = (clock(), Tmp::new("up1"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    let e = event("open", 1.0);
    agent.emit(e.clone());
    h.set_down(true);
    assert!(agent.upload_once().is_err());
    assert_eq!(agent.outbox.depth().unwrap(), 1);
    h.set_down(false);
    assert!(agent.upload_once().unwrap());
    assert_eq!(agent.outbox.depth().unwrap(), 0);
    agent.emit(e.clone()); // same event again: server says duplicate, still removed
    agent.upload_once().unwrap();
    assert_eq!(agent.outbox.depth().unwrap(), 0);
    assert_eq!(h.state().events[e["event_id"].as_str().unwrap()]["state"], "open");
}

#[test]
fn two_bit_eot_telemetry_waits_for_the_headend() {
    let (c, t) = (clock(), Tmp::new("up-eot2"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    let closed = |corrected: i64| {
        let mut e = event("closed", 1.0);
        e.insert("freq".into(), json!(457.9375));
        e.insert("kind".into(), json!("telemetry"));
        let tel = railspotting_dsp::eot::telemetry_from(&[railspotting_dsp::eot::fields(
            &railspotting_dsp::eot::build_payload(70110, 88), corrected)]).unwrap();
        e.insert("telemetry".into(), Value::Object(tel));
        e
    };
    let queued = |a: &railspotting_engine::Agent| a.outbox.next_events(10).unwrap().into_iter()
        .map(|(_, e)| e).collect::<Vec<Value>>();
    let shown = Arc::new(Mutex::new(vec![]));
    let s2 = shown.clone();
    agent.set_event_hook(Some(Arc::new(move |e: &Value| s2.lock().unwrap().push(e.clone()))));
    // today's headend takes 0 or 1: the burst goes without its telemetry, and still shows here
    agent.emit(closed(2));
    agent.emit(closed(1));
    let q = queued(&agent);
    assert!(q[0].get("telemetry").is_none());
    assert_eq!(q[1]["telemetry"]["corrected_bits"], 1);
    assert_eq!(shown.lock().unwrap()[0]["telemetry"]["corrected_bits"], 2);
    agent.upload_once().unwrap();
    // a headend that takes two gets it
    let mut cfg = agent.config();
    cfg["eot_corrected_bits_max"] = json!(2);
    agent.set_config_value(cfg);
    agent.emit(closed(2));
    assert_eq!(queued(&agent)[0]["telemetry"]["corrected_bits"], 2);
}

#[test]
fn offline_queue_then_catch_up_in_order() {
    let (c, t) = (clock(), Tmp::new("up2"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    h.set_down(true);
    let mut sent = vec![];
    for i in 0..30 {
        let e = event("open", i as f64);
        agent.emit(e.clone());
        let mut cl = e.clone();
        cl.insert("state".into(), json!("closed"));
        cl.insert("end_ts".into(), json!(i as f64 + 0.5));
        agent.emit(cl);
        sent.push(e["event_id"].clone());
        c.advance(60.0); // 30 minutes offline
    }
    agent.report("radio_check", json!({"freq": 162.55, "rssi": 461, "squelch_open": 0.8, "ok": true, "error": null}));
    assert_eq!(agent.outbox.depth().unwrap(), 61);
    h.set_down(false);
    let mut cfg = agent.config();
    cfg["upload"] = json!({"max_batch": 25, "flush_interval_s": 2});
    agent.set_config_value(cfg);
    while agent.upload_once().unwrap() {}
    assert_eq!(agent.outbox.depth().unwrap(), 0);
    let st = h.state();
    let seen: Vec<Value> = st.event_log.iter().filter(|e| e["state"] == "open").map(|e| e["event_id"].clone()).collect();
    assert_eq!(seen, sent);
    assert!(sent.iter().all(|i| st.events[i.as_str().unwrap()]["state"] == "closed"));
    assert_eq!(st.reports.last().unwrap()["type"], "radio_check");
}

#[test]
fn oversize_batch_is_split() {
    let (c, t) = (clock(), Tmp::new("up3"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    for _ in 0..4 {
        agent.emit(event("open", 1.0));
    }
    h.fail_next(413, "too_large", &[]);
    agent.upload_once().unwrap();
    assert_eq!(agent.batch_limit(), Some(2));
    while agent.upload_once().unwrap() {}
    assert!(agent.outbox.depth().unwrap() == 0 && agent.batch_limit().is_none());
}

#[test]
fn heartbeat_reports_and_clears_dropped() {
    let (c, t) = (clock(), Tmp::new("up4"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    agent.outbox.set_cap(2);
    for _ in 0..3 {
        agent.emit(event("closed", 1.0));
    }
    let hb = agent.heartbeat_body();
    assert!(hb["dropped_events"] == 1 && hb["outbox_depth"] == 2);
    agent.heartbeat_once().unwrap();
    assert_eq!(h.state().heartbeats.last().unwrap()["dropped_events"], 1);
    assert_eq!(agent.heartbeat_body()["dropped_events"], 0);
}

/// A transcriber that holds every job until the test finishes it.
#[derive(Default)]
struct Holding(Mutex<Vec<(TranscriptDone, Option<Value>)>>);

impl AgentTranscriber for Holding {
    fn submit(&self, _audio: Vec<f32>, done: TranscriptDone, _tag: &str, event: Option<Value>) {
        self.0.lock().unwrap().push((done, event));
    }
}

impl railspotting_dsp::sdr::Transcriber for Holding {
    fn submit(&self, audio: Vec<f32>, done: TranscriptDone, tag: &str, event: Option<Value>) {
        AgentTranscriber::submit(self, audio, done, tag, event)
    }
}

#[test]
fn held_closed_enters_outbox_when_transcript_done() {
    let (c, t) = (clock(), Tmp::new("up5"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    let jobs = Arc::new(Holding::default());
    let a2 = agent.clone();
    let emit: railspotting_dsp::sdr::Emit = Arc::new(move |e| a2.emit(e));
    let mut src = SdrSource::new(&vhf(), &json!({}), &agent.state_dir, emit, Some(jobs.clone()), c.clock());
    let p = plan("7");
    src.set_plan(&channels(&p), Some(&p["reference"]));
    let w = World::default();
    w.carrier(161.10, 25.0, 2.0, Some(4.0), 700.0, 2500.0, "voice");
    run(&mut src, &w, 0.0, 5.0);
    let states = |a: &railspotting_engine::Agent| a.outbox.next_events(10).unwrap().into_iter()
        .map(|(_, e)| e["state"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(states(&agent), ["open"]);
    let (done, _) = jobs.0.lock().unwrap().remove(0);
    let tr = json!({"text": "Clear signal, over.", "engine": "faster-whisper-base.en", "confidence": 0.8,
                    "language": "en", "duration_s": 2.4});
    done(Some(tr.as_object().unwrap().clone()));
    let rows = agent.outbox.next_events(10).unwrap();
    assert_eq!(states(&agent), ["open", "closed"]);
    assert_eq!(rows[1].1["transcript"]["text"], "Clear signal, over.");

    // and it reaches the headend: closed exactly once, carrying the transcript as the contract says
    let eid = rows[1].1["event_id"].as_str().unwrap().to_string();
    agent.upload_once().unwrap();
    assert_eq!(agent.outbox.depth().unwrap(), 0);
    let st = h.state();
    let sent: Vec<&Value> = st.event_log.iter().filter(|e| e["event_id"] == json!(eid)).collect();
    assert_eq!(sent.iter().map(|e| e["state"].clone()).collect::<Vec<_>>(), [json!("open"), json!("closed")]);
    assert_eq!(st.events[&eid]["transcript"], tr);
    drop(st);
    assert_eq!(agent.uploaded(), json!({"events": 2, "transcripts": 1, "telemetry": 0}));
}

#[test]
fn config_change_from_heartbeat_replans_live() {
    let (c, t) = (clock(), Tmp::new("up6"));
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [vhf()]}}), true, Default::default());
    agent.approved.set();
    let src = agent.sdrs()[0].clone();
    let cfg0 = agent.config();
    src.with(|s| s.set_plan(&channels(&cfg0), Some(&cfg0["reference"])));
    let mut cfg = plan("8");
    cfg["channels"][0]["freq"] = json!(158.0); // far outside the window
    h.state().config = cfg;
    agent.heartbeat_once().unwrap();
    assert_eq!(agent.config()["config_version"], "8");
    // spec rule: when channels don't fit, center on their midpoint and list the leftovers
    let cap = src.with(|s| s.capability());
    assert!(cap["uncovered_channels"].as_array().unwrap().contains(&json!(1)));
    let want = railspotting_engine::util::round_even((158.0 + 161.37) / 2.0, 4);
    assert_eq!(src.with(|s| s.center), Some(want));
    assert!(agent.state_dir.join("config.json").exists());
    agent.heartbeat_once().unwrap(); // unchanged: no refetch needed
    assert_eq!(agent.config()["config_version"], "8");
}

#[test]
fn commands_fetched_and_results_posted() {
    let (c, t) = (clock(), Tmp::new("up7"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    h.state().commands.push(json!({"id": "c_91", "type": "detection_test", "args": {}, "issued_ts": 1.0}));
    agent.heartbeat_once().unwrap();
    let cmd = agent.next_command().unwrap();
    let result = agent.run_command(&cmd);
    agent.api.post_command_result(cmd["id"].as_str().unwrap(), &result).unwrap();
    assert_eq!(h.state().results["c_91"], json!({"ok": false, "data": null, "error": "no scanner connected"}));
}

#[test]
fn transcript_called_duplicate_is_warned() {
    let (c, t) = (clock(), Tmp::new("up8"));
    let h = headend(&c);
    let agent = approved(&t, &h, &c);
    let log = Capture::start();
    let ev = json!({"event_id": "e9", "state": "closed", "freq": 160.95, "ch": 5,
                    "transcript": {"text": "NS 123 clear at milepost 476"}});
    let set = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<HashSet<String>>();
    agent.note_uploaded(std::slice::from_ref(&ev), &set(&[]), &set(&["e9"]));
    assert!(log.text().contains("duplicate, so it may not have stored"));
    assert_eq!(agent.uploaded()["transcripts"], 0);
    agent.note_uploaded(std::slice::from_ref(&ev), &set(&["e9"]), &set(&[]));
    assert!(log.text().contains("headend accepted the transcript for 160.9500 MHz ch 5: \"NS 123"));
    assert_eq!(agent.uploaded(), json!({"events": 1, "transcripts": 1, "telemetry": 0}));
}

// automatic roles ---------------------------------------------------------------------------------

fn nooelec(serials: &[&str]) -> Vec<Value> {
    serials.iter().enumerate().map(|(i, s)| json!({"index": i, "serial": s, "vendor": "Nooelec", "product": "NESDR SMArt v5"})).collect()
}

fn lister(v: Vec<Value>) -> Lister {
    Arc::new(move || Ok(v.clone()))
}

fn auto() -> Value {
    json!({"sdr": {"devices": [], "auto_assign": true}})
}

#[test]
fn agent_assigns_and_says_which_antenna() {
    let (c, t) = (clock(), Tmp::new("as1"));
    let h = headend(&c);
    let log = Capture::start();
    let present = nooelec(&["30002222", "30001111"]);
    let opts = || AgentOptions { lister: Some(lister(present.clone())), ..Default::default() };
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true, opts());
    assert!(agent.auto_sdr);
    assert_eq!(agent.sdrs().iter().map(|s| s.with(|x| x.serial.clone())).collect::<Vec<_>>(), ["", "", "", ""]);
    agent.sdr_scan_tick(true);
    let s = agent.sdrs();
    // EOT is filled first, then ITC (then VHF, then HOT)
    assert_eq!(s[0].with(|x| (x.role.clone(), x.serial.clone())), ("eot".into(), "30001111".into()));
    assert_eq!(s[1].with(|x| (x.role.clone(), x.serial.clone())), ("itc".into(), "30002222".into()));
    assert!(log.text().contains("UHF SDR (EOT) is the dongle with serial 30001111; connect the UHF antenna"));
    assert!(log.text().contains("220 MHz SDR (ITC) is the dongle with serial 30002222; connect the 220 MHz antenna"));
    let st = agent.status();
    assert_eq!(st["sdrs"][0]["assigned"], "automatic");
    assert!(st["sdrs"][0]["antenna"].as_str().unwrap().contains("shorter"));
    assert_eq!(st["sdr_assign"], json!({"auto": true, "duplicates": [], "spare": [], "error": null, "off": [],
        "dongle_ids": {"30002222": agent.dongle_id("30002222"), "30001111": agent.dongle_id("30001111")}}));
    let caps = agent.capabilities().to_string();
    assert!(!caps.contains("30001111") && !caps.contains("30002222")); // still never sent

    // the scan repeats every 10 s; a restart uses the saved roles straight away
    let again = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true, opts());
    assert_eq!(again.sdrs().iter().map(|s| s.with(|x| x.serial.clone())).collect::<Vec<_>>(), ["30001111", "30002222", "", ""]);
}

#[test]
fn agent_driver_missing_is_reported_once() {
    let (c, t) = (clock(), Tmp::new("as2"));
    let h = headend(&c);
    let log = Capture::start();
    let broken: Lister = Arc::new(|| Err(DriverError("no librtlsdr".into())));
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true, AgentOptions { lister: Some(broken), ..Default::default() });
    agent.sdr_scan_tick(true);
    c.advance(11.0);
    agent.sdr_scan_tick(false);
    assert_eq!(log.text().matches("cannot list the dongles").count(), 1);
    assert_eq!(agent.status()["sdr_assign"]["error"], "no librtlsdr");
}

#[test]
fn configured_serials_still_win() {
    let (c, t) = (clock(), Tmp::new("as3"));
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [{"serial": "30001111", "role": "vhf"}]}}),
                                true, AgentOptions { lister: Some(lister(nooelec(&["1", "2"]))), ..Default::default() });
    assert!(!agent.auto_sdr && agent.assigner_info().is_none());
    agent.sdr_scan_tick(true);
    assert_eq!(agent.sdrs()[0].with(|x| x.serial.clone()), "30001111");
    assert_eq!(agent.status()["sdrs"][0]["assigned"], "receiver.json");
}

#[test]
fn simulation_uses_automatic_roles() {
    let (c, t) = (clock(), Tmp::new("as4"));
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true, AgentOptions { simulate: true, ..Default::default() });
    agent.sdr_scan_tick(true);
    assert_eq!(agent.sdrs().iter().map(|s| s.with(|x| x.serial.clone())).collect::<Vec<_>>(), ["SIM00001", "SIM00002", "SIM00003", ""]);
}

// USB presence ------------------------------------------------------------------------------------

struct CountDev(AtomicUsize);

impl railspotting_dsp::sdr::DeviceControl for CountDev {
    fn cancel_read_async(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
    fn set_center(&self, _c: f64) {}
}

fn usb_lister(now: Arc<Mutex<Vec<Value>>>) -> UsbLister {
    Arc::new(move || Some(now.lock().unwrap().clone()))
}

#[test]
fn unplug_and_replug() {
    let (c, t) = (clock(), Tmp::new("usb1"));
    let h = headend(&c);
    let log = Capture::start();
    let usb = Arc::new(Mutex::new(vec![json!({"serial": "30002222", "port": "1-3"}), json!({"serial": "30001111", "port": "1-4"})]));
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [vhf(), eot()]}}), true,
                                AgentOptions { usb_lister: Some(usb_lister(usb.clone())), ..Default::default() });
    let s = agent.sdrs();
    agent.usb_check_tick(true);
    assert!(s[0].with(|x| x.plugged_in == Some(true) && x.usb_port.as_deref() == Some("1-3")));
    assert_eq!(s[1].with(|x| x.usb_port.clone()).as_deref(), Some("1-4"));
    assert!(!log.text().contains("unplugged") && !log.text().contains("plugged in")); // quiet at start

    // streaming, then the VHF dongle is pulled out
    let dev = Arc::new(CountDev(AtomicUsize::new(0)));
    s[0].with(|x| {
        x.dev = Some(dev.clone());
        x.streaming = true;
        x.connected = true;
    });
    c.advance(100.0);
    *usb.lock().unwrap() = vec![json!({"serial": "30001111", "port": "1-4"})];
    agent.usb_check_tick(true);
    assert!(s[0].with(|x| x.plugged_in == Some(false) && x.since == c.get()));
    assert_eq!(dev.0.load(Ordering::SeqCst), 1);
    assert!(log.text().contains("VHF SDR: dongle 30002222 unplugged from USB port 1-3"));
    s[0].with(|x| {
        x.connected = false; // what the device loop then does
        x.streaming = false;
    });
    let st = agent.status()["sdrs"][0].clone();
    assert!(st["plugged_in"] == json!(false) && st["usb_port"] == "1-3");
    assert_eq!(st["since"].as_f64(), Some(railspotting_engine::util::round_even(c.get(), 1)));
    assert!(s[1].with(|x| x.plugged_in == Some(true) && x.since != c.get())); // the other one is untouched

    // back in, another socket
    c.advance(20.0);
    usb.lock().unwrap().push(json!({"serial": "30002222", "port": "1-2"}));
    agent.usb_check_tick(true);
    assert!(s[0].with(|x| x.plugged_in == Some(true) && x.usb_port.as_deref() == Some("1-2")));
    assert!(log.text().contains("dongle 30002222 plugged in (USB port 1-2)"));
}

#[test]
fn automatic_roles_use_sysfs() {
    let (c, t) = (clock(), Tmp::new("usb2"));
    let h = headend(&c);
    let usb = Arc::new(Mutex::new(vec![json!({"serial": "30002222", "port": "1-3"}), json!({"serial": "30001111", "port": "1-4"})]));
    let no_librtlsdr: Lister = Arc::new(|| panic!("sysfs is enough"));
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true,
                                AgentOptions { lister: Some(no_librtlsdr), usb_lister: Some(usb_lister(usb)), ..Default::default() });
    agent.usb_check_tick(true);
    let got: Vec<_> = agent.sdrs().iter().map(|s| s.with(|x| (x.role.clone(), x.serial.clone(), x.usb_port.clone()))).collect();
    assert_eq!(got, [("eot".into(), "30001111".into(), Some("1-4".into())), ("itc".into(), "30002222".into(), Some("1-3".into())),
                     ("vhf".into(), "".into(), None), ("hot".into(), "".into(), None)]);
    let saved = read_json(&agent.state_dir.join("sdr_roles.json"));
    assert_eq!(saved, json!({"eot": "30001111", "itc": "30002222", "vhf": null, "hot": null}));
}

#[test]
fn the_headend_chooses_each_dongles_job() {
    // contract v1.11: dongles go up under anonymous IDs, spares included; /config `sdr_roles`
    // names a job for each; unnamed dongles stay automatic
    let (c, t) = (clock(), Tmp::new("usb-roles"));
    let h = headend(&c);
    let log = Capture::start();
    // the old EOT dongle is gone; the new one went to VHF (what happened on the Mac)
    let state = t.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("sdr_roles.json"), r#"{"eot": "30001111", "vhf": "30002222"}"#).unwrap();
    let usb = Arc::new(Mutex::new(vec![json!({"serial": "30002222", "port": "1-3"})]));
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true,
                                AgentOptions { usb_lister: Some(usb_lister(usb.clone())), ..Default::default() });
    agent.usb_check_tick(true);
    assert!(agent.hb_wake.is_set(), "a new kit goes to the headend at once");
    let id = agent.dongle_id("30002222");
    assert!(id.starts_with("sdr-") && id.len() == 12, "{id}");
    assert_eq!(id, agent.dongle_id("30002222"), "stable");
    let caps = agent.capabilities();
    let sdrs: Vec<&Value> = caps["receivers"].as_array().unwrap().iter().filter(|r| r["type"] == "sdr").collect();
    // only the dongle plugged in: the gone one is not reported
    assert_eq!(sdrs.iter().map(|r| (r["role"].as_str(), r["dongle_id"].as_str())).collect::<Vec<_>>(),
               vec![(Some("vhf"), Some(id.as_str()))]);

    // streaming as VHF; the owner makes it the EOT dongle on the website
    let dev = Arc::new(CountDev(AtomicUsize::new(0)));
    agent.sdrs()[2].with(|x| {
        x.dev = Some(dev.clone());
        x.streaming = true;
        x.connected = true;
    });
    let mut cfg = agent.config();
    cfg["sdr_roles"] = json!({id.clone(): "eot"});
    agent.apply_config(cfg, None);
    let got: Vec<_> = agent.sdrs().iter().map(|s| s.with(|x| (x.role.clone(), x.serial.clone()))).collect();
    assert_eq!(got, [("eot".into(), "30002222".into()), ("itc".into(), "".into()), ("vhf".into(), "".into()), ("hot".into(), "".into())]);
    assert!(log.text().contains(&format!("the headend gives dongle {id} (serial 30002222) the eot job")));
    assert!(log.text().contains("UHF SDR (EOT) is the dongle with serial 30002222 (instead of 30001111, as chosen on the website)"));
    assert!(log.text().contains("VHF SDR (voice) has no dongle now"));
    assert_eq!(dev.0.load(Ordering::SeqCst), 1, "VHF lets the dongle go");
    let st = agent.status();
    assert!(st["sdrs"][0]["assigned"] == "website" && st["sdrs"][0]["dongle_id"] == json!(id));

    // a second dongle, not named: it fills the next role in the order (ITC) by itself and is
    // reported with its own ID
    usb.lock().unwrap().push(json!({"serial": "30003333", "port": "1-4"}));
    agent.usb_check_tick(true);
    assert_eq!(agent.sdrs()[1].with(|x| x.serial.clone()), "30003333");

    // turned off on the website: no role, but still reported so it can be given one again
    let mut cfg = agent.config();
    cfg["sdr_roles"] = json!({id.clone(): "eot", agent.dongle_id("30003333"): "off"});
    agent.apply_config(cfg, None);
    assert_eq!(agent.sdrs()[1].with(|x| x.serial.clone()), "");
    let caps = agent.capabilities();
    let spare: Vec<&Value> = caps["receivers"].as_array().unwrap().iter().filter(|r| r["role"].is_null() && r["type"] == "sdr").collect();
    assert_eq!(spare.len(), 1);
    assert_eq!(spare[0]["dongle_id"], agent.dongle_id("30003333"));
    let sa = agent.status()["sdr_assign"].clone();
    assert_eq!(sa["off"], json!(["30003333"]));
    assert_eq!(sa["dongle_ids"]["30003333"], json!(agent.dongle_id("30003333")));
    let text = caps.to_string();
    assert!(!text.contains("30002222") && !text.contains("30003333"), "serials are never sent");
}

#[test]
fn the_headend_orders_the_roles_and_receiver_json_keeps_it() {
    // proposed contract v1.13: /config `sdr_role_order`; roles left out take no dongle. The
    // receiver writes it into receiver.json so it holds without the headend.
    let (c, t) = (clock(), Tmp::new("role-order"));
    let h = headend(&c);
    let log = Capture::start();
    let file = t.path().join("receiver.json");
    std::fs::write(&file, "{\n  \"state_dir\": \"state\",\n  \"sdr\": {\"devices\": [], \"auto_assign\": true}\n}\n").unwrap();
    let mut st = settings(t.path(), Some(&h), auto());
    st.path = Some(file.clone());
    std::fs::create_dir_all(t.path().join("state")).unwrap();
    std::fs::write(t.path().join("state").join("identity.json"), json!({"device_id": DEVICE_ID, "mac": MAC, "interface": "eth0"}).to_string()).unwrap();
    let usb = Arc::new(Mutex::new(vec![json!({"serial": "30001111", "port": "1-3"}), json!({"serial": "30002222", "port": "1-4"})]));
    let agent = railspotting_engine::agent::Agent::new(st, AgentOptions { usb_lister: Some(usb_lister(usb)), clock: Some(c.clock()), ..Default::default() })
        .expect("agent");
    agent.usb_check_tick(true);
    let roles = |a: &railspotting_engine::agent::Agent| a.sdrs().iter().map(|s| s.with(|x| (x.role.clone(), x.serial.clone()))).collect::<Vec<_>>();
    assert_eq!(roles(&agent)[1], ("itc".into(), "30002222".into()));

    // the owner puts VHF first and switches ITC off
    let mut cfg = agent.config();
    cfg["sdr_role_order"] = json!(["vhf", "eot", "hot"]);
    agent.apply_config(cfg, None);
    let got = roles(&agent);
    assert_eq!(got[1], ("itc".into(), "".into()), "ITC is off");
    assert_eq!(got[2], ("vhf".into(), "30002222".into()), "its dongle went to VHF");
    assert_eq!(got[0], ("eot".into(), "30001111".into()), "EOT keeps its own");
    assert!(log.text().contains("dongle role order from the website: vhf, eot, hot"));
    let saved = read_json(&file);
    assert_eq!(saved["sdr"]["role_order"], json!(["vhf", "eot", "hot"]));
    assert_eq!(saved["state_dir"], "state", "the rest of receiver.json is kept");

    // a restart reads the same order without the headend
    let again = railspotting_common::Settings::load(&file).unwrap();
    assert_eq!(railspotting_store::assign::role_order(again.sdr()), vec!["vhf", "eot", "hot"]);
}

#[test]
fn receiver_json_gives_dongles_jobs_and_the_website_wins() {
    // sdr.assign, set in the app: a job for each dongle by serial; the others stay automatic and
    // a job named on the website wins
    let (c, t) = (clock(), Tmp::new("local-roles"));
    let h = headend(&c);
    let log = Capture::start();
    let over = json!({"sdr": {"devices": [], "auto_assign": true, "assign": {"30001111": "vhf", "30003333": "off"}}});
    let usb = Arc::new(Mutex::new(vec![json!({"serial": "30001111", "port": "1-3"}), json!({"serial": "30002222", "port": "1-4"}),
                                       json!({"serial": "30003333", "port": "1-5"})]));
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), over, true,
                                AgentOptions { usb_lister: Some(usb_lister(usb)), ..Default::default() });
    agent.usb_check_tick(true);
    let roles = || agent.sdrs().iter().map(|s| s.with(|x| (x.role.clone(), x.serial.clone()))).collect::<Vec<_>>();
    assert_eq!(roles(), [("eot".into(), "30002222".into()), ("itc".into(), "".into()), ("vhf".into(), "30001111".into()),
                         ("hot".into(), "".into())]);
    assert!(log.text().contains("receiver.json gives dongle 30001111 the vhf job"));
    let st = agent.status();
    assert_eq!(st["sdrs"][2]["assigned"], "receiver.json");
    assert_eq!(st["sdr_assign"]["off"], json!(["30003333"]));

    // the website names 30001111 for EOT: that wins over receiver.json
    let mut cfg = agent.config();
    cfg["sdr_roles"] = json!({agent.dongle_id("30001111"): "eot"});
    agent.apply_config(cfg, None);
    let got = roles();
    assert_eq!(got[0], ("eot".into(), "30001111".into()));
    assert_eq!(agent.status()["sdrs"][0]["assigned"], "website");
}

#[test]
fn without_sysfs_falls_back_to_librtlsdr() {
    let (c, t) = (clock(), Tmp::new("usb3"));
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), auto(), true, AgentOptions {
        usb_lister: Some(Arc::new(|| None)), lister: Some(lister(vec![json!({"index": 0, "serial": "11112222"})])),
        ..Default::default()
    });
    agent.usb_check_tick(true);
    assert!(agent.sdrs()[0].with(|x| x.serial == "11112222" && x.plugged_in.is_none()));
}

// scanner commands --------------------------------------------------------------------------------

#[test]
fn commands_need_scanner() {
    let (c, t) = (clock(), Tmp::new("sc1"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    for ty in ["reception_test", "detection_test", "report_channel_memory", "resume_scan"] {
        assert_eq!(agent.run_command(&json!({"id": "c1", "type": ty, "args": {}})),
                   json!({"ok": false, "data": null, "error": "no scanner connected"}));
    }
    assert_eq!(agent.run_command(&json!({"id": "c2", "type": "set_volume", "args": {"level": 3}}))["error"], "no scanner connected");
    let r = agent.run_command(&json!({"id": "c3", "type": "radio_check", "args": {}}));
    assert!(r["ok"] == json!(false) && r["error"].as_str().is_some_and(|e| !e.is_empty()));
}

fn connect_fake(agent: &railspotting_engine::Agent, c: &railspotting_common::FakeClock) -> Arc<FakeSR30C> {
    let fake = Arc::new(FakeSR30C::new(Arc::new(CarrierWorld::default()), c.clock()));
    let opener = fake.opener();
    let sleep = railspotting_scanner::fake_sleep(c);
    agent.monitor.set_hooks(|hk| {
        hk.opener = opener;
        hk.finder = Arc::new(|| Some("/dev/ttyUSB0".into()));
        hk.sleep = sleep;
    });
    assert!(agent.monitor.try_connect());
    fake
}

fn connect_fake_on(agent: &railspotting_engine::Agent, c: &railspotting_common::FakeClock, port: &'static str) -> Arc<FakeSR30C> {
    let fake = Arc::new(FakeSR30C::new(Arc::new(CarrierWorld::default()), c.clock()));
    let opener = fake.opener();
    let sleep = railspotting_scanner::fake_sleep(c);
    agent.monitor.set_hooks(|hk| {
        hk.opener = opener;
        hk.finder = Arc::new(move || Some(port.into()));
        hk.sleep = sleep;
    });
    assert!(agent.monitor.try_connect());
    fake
}

/// Two scanner ticks across the role hold, so a new job is applied.
fn settle(agent: &railspotting_engine::Agent, c: &railspotting_common::FakeClock) {
    agent.scanner_tick().unwrap();
    c.advance(20.0);
    agent.scanner_tick().unwrap();
}

#[test]
fn the_website_chooses_the_scanners_job() {
    // contract v1.12: the scanner goes up as scanner_id; /config scanner_roles names its job
    let (c, t) = (clock(), Tmp::new("scn-jobs"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    assert_eq!(agent.scanner_id(), None, "no scanner, no ID");
    let _fake = connect_fake_on(&agent, &c, "/dev/cu.usbserial-30004444");
    let id = agent.scanner_id().unwrap();
    assert!(id.starts_with("scn-") && id.len() == 12, "{id}");
    agent.hb_wake.clear();
    agent.scanner_tick().unwrap();
    assert!(agent.hb_wake.is_set(), "a scanner plugged in goes to the headend at once");

    // automatic: no SDR is on EOT, so the scanner holds EOT
    settle(&agent, &c);
    assert_eq!(agent.scanner_role().as_deref(), Some("eot"));
    assert_eq!(agent.status()["scanner_job_from"], "automatic");
    let caps = agent.capabilities();
    let sc = &caps["receivers"][0];
    assert!(sc["type"] == "scanner" && sc["scanner_id"] == json!(id) && sc["role"] == "eot");
    assert!(!caps.to_string().contains("30004444"), "the cable serial is never sent");

    // the owner picks HOT on the website
    let mut cfg = agent.config();
    cfg["scanner_roles"] = json!({id.clone(): "hot"});
    agent.set_config_value(cfg);
    settle(&agent, &c);
    assert_eq!(agent.scanner_role().as_deref(), Some("hot"));
    let st = agent.status();
    assert!(st["scanner_job_from"] == "website" && st["scanner_id"] == json!(id));
    assert_eq!(agent.capabilities()["receivers"][0]["role"], "hot");

    // off: connected, no job
    let mut cfg = agent.config();
    cfg["scanner_roles"] = json!({id.clone(): "off"});
    agent.set_config_value(cfg);
    settle(&agent, &c);
    assert_eq!(agent.scanner_role().as_deref(), Some("off"));
    let sc = agent.capabilities()["receivers"][0].clone();
    assert!(sc["connected"] == true && sc["role"].is_null(), "{sc}");

    // a choice for another scanner changes nothing: back to automatic
    let mut cfg = agent.config();
    cfg["scanner_roles"] = json!({"scn-00000000": "scan"});
    agent.set_config_value(cfg);
    settle(&agent, &c);
    assert_eq!(agent.scanner_role().as_deref(), Some("eot"));
}

#[test]
fn the_scanner_id_is_stable_and_receiver_json_wins() {
    let (c, t) = (clock(), Tmp::new("scn-fixed"));
    let h = headend(&c);
    let first = make_agent(t.path(), &h, &c);
    let _f1 = connect_fake_on(&first, &c, "/dev/cu.usbserial-30004444");
    let id = first.scanner_id().unwrap();
    // a restart: the same scanner has the same ID
    let again = make_agent_with(t.path(), Some(&h), Some(&c), json!({"scanner_role": "scan"}), true, AgentOptions::default());
    let _f2 = connect_fake_on(&again, &c, "/dev/cu.usbserial-30004444");
    assert_eq!(again.scanner_id().unwrap(), id);
    // set in receiver.json: the website's choice is ignored
    let mut cfg = again.config();
    cfg["scanner_roles"] = json!({id.clone(): "hot"});
    again.set_config_value(cfg);
    settle(&again, &c);
    assert_eq!(again.scanner_role().as_deref(), Some("scan"));
    assert_eq!(again.status()["scanner_job_from"], "receiver.json");
    // another cable, another ID
    let t2 = Tmp::new("scn-other");
    let other = make_agent(t2.path(), &h, &c);
    let _f3 = connect_fake_on(&other, &c, "/dev/cu.usbserial-30005555");
    assert_ne!(other.scanner_id().unwrap(), id);
}

#[test]
fn commands_with_scanner() {
    let c = railspotting_common::FakeClock::new(1000.0);
    let t = Tmp::new("sc2");
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    let fake = connect_fake(&agent, &c);
    assert_eq!(agent.run_command(&json!({"id": "c", "type": "set_volume", "args": {"level": 4}}))["ok"], json!(true));
    assert_eq!(fake.lock().volume, 4);
    assert_eq!(agent.run_command(&json!({"id": "c", "type": "set_squelch", "args": {"level": 99}}))["ok"], json!(false));
    let r = agent.run_command(&json!({"id": "c", "type": "report_channel_memory", "args": {}}));
    assert!(r["ok"] == json!(true) && r["data"]["channels"].as_array().unwrap().len() == 50);
    assert_eq!(agent.outbox.next_reports(10).unwrap().last().unwrap().1["type"], "channel_memory");
}

// the SDR reference -------------------------------------------------------------------------------

const REF_NAME: &str = "NOAA WXJ21 Paris Mountain";

fn vhf_agent(t: &Tmp, h: &railspotting_proto::fake::FakeHeadend, c: &railspotting_common::FakeClock) -> Arc<railspotting_engine::Agent> {
    make_agent_with(t.path(), Some(h), Some(c), json!({"sdr": {"devices": [vhf()]}}), true, Default::default())
}

#[test]
fn watch_reference() {
    let c = railspotting_common::FakeClock::new(1000.0);
    let t = Tmp::new("ref1");
    let h = headend(&c);
    let agent = vhf_agent(&t, &h, &c);
    let s = agent.sdrs()[0].clone();
    let p = plan("7");
    s.with(|x| {
        x.connected = true;
        x.set_plan(&channels(&p), Some(&p["reference"]));
    });
    let set_ref = |snr: f64| s.with(|x| x.reference = Some(json!({"name": REF_NAME, "freq": 162.55, "snr_db": snr, "ts": c.get()})));
    let checks = || reports(&agent, "radio_check");
    for _ in 0..12 {
        // 55 s weak: nothing yet
        set_ref(6.0);
        agent.watch_reference().unwrap();
        c.advance(5.0);
    }
    assert!(checks().is_empty());
    set_ref(6.0);
    agent.watch_reference().unwrap();
    let r = checks();
    assert!(r.len() == 1 && r[0]["data"]["ok"] == json!(false));
    assert_eq!(r[0]["data"]["error"], "NOAA WXJ21 Paris Mountain only +6 dB on the SDR");
    assert_eq!(r[0]["data"]["rssi"], 391);
    for _ in 0..59 {
        // still weak, < 5 min since the last report
        c.advance(5.0);
        set_ref(6.0);
        agent.watch_reference().unwrap();
    }
    assert_eq!(checks().len(), 1);
    c.advance(5.0);
    set_ref(6.0);
    agent.watch_reference().unwrap();
    assert_eq!(checks().len(), 2); // repeated after 5 minutes
    c.advance(5.0);
    set_ref(30.0);
    agent.watch_reference().unwrap();
    let r = checks();
    assert!(r.len() == 3 && r[2]["data"]["ok"] == json!(true));
}

#[test]
fn watch_reference_quiet_with_scanner() {
    let c = railspotting_common::FakeClock::new(1000.0);
    let t = Tmp::new("ref2");
    let h = headend(&c);
    let agent = vhf_agent(&t, &h, &c);
    connect_fake(&agent, &c);
    let s = agent.sdrs()[0].clone();
    s.with(|x| x.connected = true);
    for _ in 0..100 {
        s.with(|x| x.reference = Some(json!({"name": "x", "freq": 162.55, "snr_db": 2.0, "ts": c.get()})));
        agent.watch_reference().unwrap();
        c.advance(5.0);
    }
    assert!(agent.outbox.next_reports(100).unwrap().is_empty());
}

#[test]
fn scheduled_radio_check_uses_sdr_reference() {
    let c = railspotting_common::FakeClock::new(1000.0);
    let t = Tmp::new("ref3");
    let h = headend(&c);
    let agent = vhf_agent(&t, &h, &c);
    let s = agent.sdrs()[0].clone();
    s.with(|x| {
        x.connected = true;
        x.streaming = true;
        x.reference = Some(json!({"name": REF_NAME, "freq": 162.55, "snr_db": 36.0, "ts": c.get()}));
    });
    let data = agent.radio_check(true).unwrap();
    assert_eq!(data, json!({"freq": 162.55, "rssi": 421, "squelch_open": 1.0, "ok": true, "error": null}));
    s.with(|x| {
        x.reference.as_mut().unwrap()["ts"] = json!(c.get() - 60.0); // stale; not in any window any more
        x.plan.reference = None;
    });
    let data = agent.radio_check(false).unwrap();
    assert!(data["ok"] == json!(false) && data["error"].as_str().unwrap().contains("outside"));
}

#[test]
fn history_signal_tick_once_a_minute() {
    let (c, t) = (clock(), Tmp::new("hist1"));
    let h = headend(&c);
    let agent = vhf_agent(&t, &h, &c);
    agent.history_signal_tick().unwrap();
    assert!(agent.history.signal(0.0).unwrap().is_empty());
    c.advance(61.0);
    agent.history_signal_tick().unwrap();
    agent.history_signal_tick().unwrap();
    let rows = agent.history.signal(0.0).unwrap();
    assert!(rows.len() == 1 && rows[0]["sdrs"][0]["role"] == "vhf");
}

#[test]
fn emit_records_history_with_plan_label() {
    let (c, t) = (clock(), Tmp::new("hist2"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    let ev = json!({"event_id": "e1", "state": "closed", "start_ts": c.get(), "end_ts": c.get() + 3.0,
                    "freq": 160.95, "ch": 5, "kind": "voice", "peak_rssi": 400});
    agent.emit(ev.as_object().unwrap().clone());
    let got = agent.history.events(100, None, None).unwrap()[0].clone();
    assert!(got["label"] == "Charlotte Dist. Road" && got["source"] == "sdr");
    assert!(agent.outbox.next_events(10).unwrap()[0].1.get("label").is_none()); // the upload is unchanged
}

// the device thread: the device loop and the processing loop -------------------------------------

#[test]
fn device_loop_recovers_after_loss() {
    // a lost dongle closes open events, waits and reopens by itself
    railspotting_engine::logging::setup_for_tests();
    let t = Tmp::new("dev1");
    let w = Arc::new(World::default());
    let opened: Arc<Mutex<Vec<Arc<SimDev>>>> = Arc::default();
    let fail = Arc::new(AtomicUsize::new(1));
    let (o2, w2, f2) = (opened.clone(), w.clone(), fail.clone());
    let factory: DeviceFactory = Arc::new(move |serial: &str| {
        if f2.load(Ordering::SeqCst) > 0 {
            f2.fetch_sub(1, Ordering::SeqCst);
            return Err("usb_open error -4".into());
        }
        let d = Arc::new(SimDev(railspotting_dsp::sim::SimDevice::new(serial, w2.clone(), railspotting_common::system_clock(), false)));
        o2.lock().unwrap().push(d.clone());
        Ok(d as Arc<dyn Device>)
    });
    let (_events, emit) = sink();
    let src = SdrSource::new(&vhf(), &json!({}), t.path(), emit, None, railspotting_common::system_clock());
    let h = SdrHandle::new(src, Some(factory));
    let p = plan("7");
    h.with(|s| s.set_plan(&channels(&p), Some(&p["reference"])));
    *h.reopen_delay_s.lock().unwrap() = 0.05;
    h.start();
    let wait = |f: &dyn Fn() -> bool| {
        for _ in 0..400 {
            if f() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        false
    };
    assert!(wait(&|| h.with(|s| s.connected && s.blocks > 5)));
    assert!(!opened.lock().unwrap().is_empty());
    let last = opened.lock().unwrap().last().unwrap().clone();
    last.cancel_read_async(); // stream ends as if unplugged
    assert!(wait(&|| opened.lock().unwrap().len() >= 2 && h.with(|s| s.connected)));
    h.stop();
    h.join();
}

#[test]
fn overrun_counted_when_queue_full() {
    let t = Tmp::new("dev2");
    let (_events, emit) = sink();
    let src = SdrSource::new(&vhf(), &json!({}), t.path(), emit, None, railspotting_common::system_clock());
    let h = SdrHandle::new(src, None);
    for _ in 0..QUEUE_BLOCKS + 3 {
        h.on_samples(vec![num_complex::Complex32::new(0.0, 0.0); 16]);
    }
    assert_eq!(h.overruns(), 3);
    assert_eq!(h.with(|s| s.overruns), 3);
}
