//! The command line, including sdr-list and status with automatic roles and USB ports.



use crate::common::*;
use railspotting_dsp::sdr::TranscriptDone;
use railspotting_engine::agent::{AgentOptions, AgentTranscriber, Lister, TT_RESULT, TT_TRIGGER};
use railspotting_engine::cli::{self, format_sdr_list, NO_DONGLES_HINT};
use railspotting_engine::rtl::DriverError;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn configured() -> Vec<Value> {
    vec![json!({"serial": "30002222", "role": "vhf"}), json!({"serial": "30001111", "role": "eot"})]
}

fn found() -> Vec<Value> {
    vec![json!({"index": 0, "serial": "30002222", "vendor": "Nooelec", "product": "NESDR SMArt v5"}),
         json!({"index": 1, "serial": "11112222", "vendor": "Nooelec", "product": "NESDR SMArt v5"})]
}

fn text(out: Vec<u8>) -> String {
    String::from_utf8(out).unwrap()
}

#[test]
fn sdr_list_configured_unconfigured_missing() {
    let out = format_sdr_list(Some(&found()), &configured(), None, false);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "#0  serial 30002222   Nooelec NESDR SMArt v5  ->  role vhf");
    assert_eq!(lines[1], "#1  serial 11112222   Nooelec NESDR SMArt v5  ->  not configured");
    assert!(lines[2].contains("30001111") && lines[2].contains("role eot but not connected"));
}

#[test]
fn sdr_list_placeholders_hint() {
    let out = format_sdr_list(Some(&found()[..1]), &[json!({"serial": "00000001", "role": "vhf"})], None, false);
    assert!(out.contains("placeholder"));
}

#[test]
fn sdr_list_none_found() {
    let out = format_sdr_list(Some(&[]), &configured(), None, false);
    assert!(out.starts_with("no RTL-SDR dongles found") && out.contains(NO_DONGLES_HINT));
    #[cfg(target_os = "linux")]
    assert!(out.contains("proxmox-host-setup.sh"));
}

#[test]
fn sdr_list_driver_absent() {
    let t = Tmp::new("cli1");
    let st = settings(t.path(), None, json!({"sdr": {"devices": configured()}}));
    let boom: Lister = Arc::new(|| Err(DriverError("librtlsdr.so.0: cannot open shared object file".into())));
    let mut out = vec![];
    assert_eq!(cli::cmd_sdr_list(&st, &boom, &mut out), 1);
    let out = text(out);
    assert!(out.starts_with("cannot load the RTL-SDR driver") && out.contains("apt install rtl-sdr librtlsdr0"));
}

#[test]
fn sdr_list_in_auto_mode() {
    let t = Tmp::new("cli2");
    let st = settings(t.path(), None, json!({"sdr": {"devices": []}}));
    std::fs::create_dir_all(st.state_dir()).unwrap();
    std::fs::write(st.state_dir().join("sdr_roles.json"), json!({"vhf": "30001111", "eot": "30002222"}).to_string()).unwrap();
    let l: Lister = Arc::new(|| Ok(["30002222", "30001111", "55555555"].iter().enumerate()
        .map(|(i, s)| json!({"index": i, "serial": s, "vendor": "Nooelec", "product": "NESDR SMArt v5"})).collect()));
    let mut out = vec![];
    cli::cmd_sdr_list(&st, &l, &mut out);
    let out = text(out);
    assert!(out.contains("serial 30001111") && out.contains("role vhf (automatic): connect the VHF antenna"), "{out}");
    assert!(out.contains("role eot (automatic): connect the UHF antenna"));
    assert!(out.contains("55555555") && out.contains("not used yet"));
}

#[test]
fn status_reads_status_json() {
    let t = Tmp::new("cli3");
    let c = clock();
    let h = headend(&c);
    let agent = make_agent_with(t.path(), Some(&h), None, json!({}), true,
                                AgentOptions { clock: Some(railspotting_common::system_clock()), ..Default::default() });
    agent.write_status(false).unwrap();
    let mut out = vec![];
    assert_eq!(cli::cmd_status(&agent.settings, &mut out), 0);
    let out = text(out);
    assert!(out.contains("rs-b827eb1a2b3c") && out.contains("running") && out.contains("checking the stored device ID"), "{out}");
}

#[test]
fn status_without_service() {
    let t = Tmp::new("cli4");
    let st = settings(t.path(), None, json!({}));
    let mut out = vec![];
    assert_eq!(cli::cmd_status(&st, &mut out), 1);
    assert!(text(out).contains("no status yet"));
}

#[test]
fn status_line_says_unplugged() {
    let t = Tmp::new("cli5");
    let c = clock();
    let h = headend(&c);
    let usb = Arc::new(Mutex::new(vec![json!({"serial": "30001111", "port": "1-4"})]));
    let u2 = usb.clone();
    let agent = make_agent_with(t.path(), Some(&h), None, json!({"sdr": {"devices": [vhf(), eot()]}}), true, AgentOptions {
        clock: Some(railspotting_common::system_clock()),
        usb_lister: Some(Arc::new(move || Some(u2.lock().unwrap().clone()))),
        ..Default::default()
    });
    agent.usb_check_tick(true);
    agent.sdrs()[0].with(|s| s.plugged_in = Some(true));
    agent.usb_check_tick(true);
    agent.write_status(false).unwrap();
    let mut out = vec![];
    cli::cmd_status(&agent.settings, &mut out);
    let out = text(out);
    assert!(out.contains("VHF SDR (serial 30002222, role vhf): UNPLUGGED"), "{out}");
    assert!(out.contains("UHF SDR (serial 30001111, USB port 1-4, role eot): plugged in, NOT STREAMING"));
}

/// A transcriber that answers at once.
struct Instant;

impl AgentTranscriber for Instant {
    fn submit(&self, _audio: Vec<f32>, done: TranscriptDone, _tag: &str, _event: Option<Value>) {
        let r = json!({"text": "This is the NOAA weather radio, all hazards.", "confidence": 0.81,
                       "engine": "x", "language": "en", "duration_s": 20});
        done(Some(r.as_object().unwrap().clone()));
    }
}

#[test]
fn transcription_test_trigger() {
    let (c, t) = (clock(), Tmp::new("cli6"));
    let h = headend(&c);
    // a narrow window around the reference keeps the 20 s of samples small
    let dev = json!({"serial": "30002222", "role": "vhf", "sample_rate": 1_024_000.0, "center_mhz": 162.45, "name": "VHF SDR"});
    let agent = make_agent_with(t.path(), Some(&h), Some(&c), json!({"sdr": {"devices": [dev]}}), true,
                                AgentOptions { transcriber: Some(Arc::new(Instant)), ..Default::default() });
    let v = agent.sdrs()[0].clone();
    let cfg = agent.config();
    v.with(|s| {
        s.set_plan(&channels(&cfg), Some(&cfg["reference"]));
        s.streaming = true;
    });
    std::fs::write(agent.state_dir.join(TT_TRIGGER), b"").unwrap();
    agent.transcription_test_tick().unwrap();
    assert!(agent.tt_pending().is_some());
    // 20 s of the reference: feed blocks of silence through the tap until it is long enough
    let block = vec![num_complex::Complex32::new(0.0, 0.0); v.with(|s| s.block)];
    for i in 0..2000 {
        if !std::fs::exists(agent.state_dir.join(TT_RESULT)).unwrap() {
            v.with(|s| s.process_block(&block, i as f64 * 0.04));
        }
    }
    let res = read_json(&agent.state_dir.join(TT_RESULT));
    assert_eq!(res["ok"], json!(true));
    assert_eq!(res["line"], "transcription test: \"This is the NOAA weather radio, all hazards.\" (81%)");
    assert!(agent.outbox.next_reports(10).unwrap().is_empty()); // local only, never sent
}

#[test]
fn reset_identity_needs_typed_confirmation() {
    let (c, t) = (clock(), Tmp::new("cli7"));
    let h = headend(&c);
    let agent = make_agent(t.path(), &h, &c);
    let mut out = vec![];
    assert_eq!(cli::cmd_reset_identity(&agent.settings, &mut |_| "yes".into(), &mut out), 1);
    assert!(agent.state_dir.join("identity.json").exists());
    assert_eq!(cli::cmd_reset_identity(&agent.settings, &mut |_| "RESET\n".into(), &mut out), 0);
    assert!(!agent.state_dir.join("identity.json").exists());
    let creds = read_json(&agent.settings.credentials_path());
    assert!(creds.get("private_key").is_none());
}

#[test]
fn help_and_usage() {
    let mut out = vec![];
    assert_eq!(cli::main_with(vec!["railspotting-receiver".into(), "--config".into(), "/nonexistent/receiver.json".into()], &mut out), 2);
    let out = text(out);
    for line in ["RailSpotting receiver: hears railroad radio and reports to the headend", "normal operation",
                 "link this receiver to an account (start again after a denial)", "list connected RTL-SDR dongles",
                 "path to receiver.json (default ./receiver.json)", "delete device ID and key"] {
        assert!(out.contains(line), "{line}\n{out}");
    }
}
