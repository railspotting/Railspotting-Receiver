#![allow(dead_code)]
//! Shared test setup: a fake clock, the plan, a running fake headend.

use railspotting_common::{builtin_config, FakeClock};
use railspotting_proto::fake::FakeHeadend;
use railspotting_proto::keys::{self, PrivateKey};
use railspotting_proto::ApiClient;
use serde_json::{json, Value};
use std::path::PathBuf;

pub const DEVICE_ID: &str = "rs-b827eb1a2b3c";
pub const SEED_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

pub fn clock() -> FakeClock {
    FakeClock::new(1_790_865_400.0)
}

pub fn plan(version: &str) -> Value {
    let mut cfg = builtin_config();
    cfg["config_version"] = json!(version);
    cfg
}

pub fn headend(clock: &FakeClock) -> FakeHeadend {
    FakeHeadend::start(clock.clock(), plan("7")).expect("fake headend starts")
}

pub fn test_key() -> PrivateKey {
    keys::private_from_seed(&hex::decode(SEED_HEX).unwrap()).unwrap()
}

/// A client for the fake headend, with a device ID the headend knows and has approved.
pub fn approved_client(h: &FakeHeadend, clock: &FakeClock) -> ApiClient {
    let key = test_key();
    h.add_device(DEVICE_ID, &keys::public_bytes(&key), "approved");
    ApiClient::new(&h.url(), Some(DEVICE_ID.into()), key, None, clock.clock(), 5.0).unwrap()
}

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

pub fn vectors() -> Value {
    let text = std::fs::read_to_string(fixtures().join("api/vectors.json")).expect("api/vectors.json");
    serde_json::from_str(&text).unwrap()
}

pub fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rs-proto-{name}-{}-{}", std::process::id(),
                                               rand::random::<u32>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
