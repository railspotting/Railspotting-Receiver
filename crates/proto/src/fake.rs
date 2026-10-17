//! A headend for tests and `run --simulate`: verifies signatures exactly as contract v1.4 says.
//!
//! It is a real HTTP/1.1 server on 127.0.0.1 with a random port
//! (each request on its own connection), so the real ureq client is what gets tested. State is
//! behind a mutex: `fake.state()` locks it for reading or changing.

use crate::keys;
use crate::jsonfmt;
use railspotting_common::Clock;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub key: Vec<u8>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct Link {
    pub key: Vec<u8>,
    pub code: String,
    pub body: Value,
    pub status: String,
    pub expires: f64,
    pub last_poll: Option<f64>,
    pub device_id: Option<String>,
}

/// One HTTP request as the handler sees it. Header names are lower case.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// A reply: status, headers, body bytes.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl Reply {
    pub fn json(&self) -> Value {
        self.body.as_deref().and_then(|b| serde_json::from_slice(b).ok()).unwrap_or(Value::Null)
    }
}

/// (status, error, extra headers) for an injected failure.
pub type Injected = (u16, String, Vec<(String, String)>);

pub struct FakeState {
    pub clock: Clock,
    pub auto_approve: bool,
    pub devices: HashMap<String, Device>,
    pub events: HashMap<String, Value>,
    pub event_log: Vec<Value>,
    pub reports: Vec<Value>,
    pub heartbeats: Vec<Value>,
    pub links: HashMap<String, Link>, // link_id -> link
    pub link_starts: Vec<Value>,
    pub link_headers: Vec<BTreeMap<String, String>>,
    pub commands: Vec<Value>,
    pub results: HashMap<String, Value>,
    pub nonces: HashSet<String>,
    pub config: Value,
    /// Unreachable: connections are dropped without a reply.
    pub down: bool,
    /// (status, error, headers) answered to the next requests, before anything else.
    pub fail_next: VecDeque<Injected>,
    pub raw_bodies: Vec<Vec<u8>>,
}

fn random_code() -> String {
    let alphabet: Vec<char> = "BCDFGHJKLMNPQRSTVWXZ".chars().collect();
    (0..8).map(|_| alphabet[rand::random_range(0..alphabet.len())]).collect()
}

impl FakeState {
    pub fn new(clock: Clock, config: Value) -> Self {
        FakeState {
            clock, auto_approve: true, devices: HashMap::new(), events: HashMap::new(),
            event_log: vec![], reports: vec![], heartbeats: vec![], links: HashMap::new(),
            link_starts: vec![], link_headers: vec![], commands: vec![], results: HashMap::new(),
            nonces: HashSet::new(), config, down: false, fail_next: VecDeque::new(), raw_bodies: vec![],
        }
    }

    fn now(&self) -> f64 { (self.clock)() }

    fn resp(&self, status: u16, body: Option<Value>, headers: &[(String, String)]) -> Reply {
        let mut h = vec![("X-RS-Server-Time".to_string(), format!("{}", self.now().trunc() as i64))];
        h.extend(headers.iter().cloned());
        Reply { status, headers: h, body: body.map(|b| jsonfmt::dumps_compact(&b).into_bytes()) }
    }

    fn err(&self, status: u16, error: &str, msg: &str, headers: &[(String, String)]) -> Reply {
        self.resp(status, Some(json!({"error": error, "message": msg})), headers)
    }

    /// Answer one request, or None when the headend is down.
    pub fn handle(&mut self, request: &Request) -> Option<Reply> {
        if self.down {
            return None;
        }
        let body = request.body.clone();
        self.raw_bodies.push(body.clone());
        if let Some((status, error, headers)) = self.fail_next.pop_front() {
            return Some(self.err(status, &error, "injected", &headers));
        }
        let path = request.path.as_str();
        let method = request.method.as_str();
        if path == "/api/v1/link" || path.starts_with("/api/v1/link/") {
            self.link_headers.push(request.headers.clone());
            if method == "POST" && path == "/api/v1/link" {
                return Some(self.link_start(request, path, &body));
            }
            if method == "GET" {
                let lid = path.rsplit('/').next().unwrap_or("").to_string();
                return Some(self.link_poll(request, path, &lid));
            }
        }
        let dev = match self.verify(request, path, &body) {
            Ok(d) => d,
            Err(r) => return Some(r),
        };
        if path == "/api/v1/register/status" {
            return Some(self.resp(200, Some(json!({"status": dev.status})), &[]));
        }
        if dev.status != "approved" {
            return Some(self.err(403, &dev.status, "not approved", &[]));
        }
        let data: Option<Value> = if body.is_empty() {
            None
        } else {
            match serde_json::from_slice(&body) {
                Ok(v) => Some(v),
                Err(_) => return Some(self.err(400, "bad_request", "body is not JSON", &[])),
            }
        };
        Some(self.route(request, path, data))
    }

    fn route(&mut self, request: &Request, path: &str, data: Option<Value>) -> Reply {
        if path == "/api/v1/config" {
            let cfg = self.config.clone();
            let version = match &cfg["config_version"] {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            };
            let etag = format!("\"{version}\"");
            if request.header("If-None-Match") == Some(etag.as_str()) {
                return self.resp(304, None, &[]);
            }
            return self.resp(200, Some(cfg), &[("ETag".into(), etag)]);
        }
        if path == "/api/v1/events" {
            let (mut acc, mut dup) = (vec![], vec![]);
            let events = data.as_ref().and_then(|d| d.get("events")).and_then(Value::as_array).cloned()
                .unwrap_or_default();
            for ev in events {
                self.event_log.push(ev.clone());
                let id = match &ev["event_id"] {
                    Value::String(s) => s.clone(),
                    v => v.to_string(),
                };
                let old = self.events.get(&id);
                let is_dup = old == Some(&ev) || old.is_some_and(|o| o["state"] == "closed");
                if is_dup {
                    dup.push(id);
                } else {
                    self.events.insert(id.clone(), ev);
                    acc.push(id);
                }
            }
            return self.resp(200, Some(json!({"accepted": acc, "duplicates": dup})), &[]);
        }
        if path == "/api/v1/heartbeat" {
            self.heartbeats.push(data.unwrap_or(Value::Null));
            let body = json!({"server_time": self.now(), "config_version": self.config["config_version"],
                              "commands_pending": self.commands.len()});
            return self.resp(200, Some(body), &[]);
        }
        if path == "/api/v1/reports" {
            self.reports.push(data.unwrap_or(Value::Null));
            return self.resp(200, Some(json!({"stored": true})), &[]);
        }
        if path == "/api/v1/commands" {
            let cmds = std::mem::take(&mut self.commands);
            return self.resp(200, Some(json!({"commands": cmds})), &[]);
        }
        if path.starts_with("/api/v1/commands/") && path.ends_with("/result") {
            let id = path.split('/').nth(4).unwrap_or("").to_string();
            self.results.insert(id, data.unwrap_or(Value::Null));
            return self.resp(200, Some(json!({"stored": true})), &[]);
        }
        self.err(404, "not_found", "", &[])
    }

    // linking (v1.4) ---------------------------------------------------------------------

    fn check_sig(&mut self, request: &Request, path: &str, body: &[u8], public: &[u8]) -> Option<Reply> {
        if request.header("X-RS-Device").is_some() {
            return Some(self.err(400, "bad_request", "linking is signed without X-RS-Device", &[]));
        }
        self.check_common(request, path, body, public)
    }

    fn check_common(&mut self, request: &Request, path: &str, body: &[u8], public: &[u8]) -> Option<Reply> {
        let Some(ts) = request.header("X-RS-Timestamp").and_then(|t| t.trim().parse::<i64>().ok()) else {
            return Some(self.err(401, "bad_signature", "", &[]));
        };
        if (ts as f64 - self.now()).abs() > 300.0 {
            return Some(self.err(401, "clock_skew", "", &[]));
        }
        let nonce = request.header("X-RS-Nonce").unwrap_or("").to_string();
        if self.nonces.contains(&nonce) {
            return Some(self.err(401, "replay", "", &[]));
        }
        let canon = [request.method.clone(), path.to_string(), ts.to_string(), nonce.clone(), keys::sha256_hex(body)]
            .join("\n");
        let sig = request.header("X-RS-Signature").unwrap_or("");
        if request.header("X-RS-Signature").is_none() || !keys::verify(public, &canon, sig) {
            return Some(self.err(401, "bad_signature", "", &[]));
        }
        self.nonces.insert(nonce);
        None
    }

    fn link_start(&mut self, request: &Request, path: &str, body: &[u8]) -> Reply {
        let data: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        self.link_starts.push(data.clone());
        let public = match data.get("public_key").and_then(Value::as_str).map(keys::b64url_decode) {
            Some(Ok(p)) if p.len() == 32 => p,
            _ => return self.err(400, "bad_request", "public_key", &[]),
        };
        if let Some(bad) = self.check_sig(request, path, body, &public) {
            return bad;
        }
        for (dev_id, dev) in &self.devices {
            if dev.key == public && dev.status == "approved" {
                let body = json!({"status": "approved", "device_id": dev_id});
                return self.resp(200, Some(body), &[]);
            }
        }
        let now = self.now();
        let pending = self.links.iter()
            .find(|(_, lk)| lk.key == public && lk.status == "pending" && now < lk.expires)
            .map(|(lid, lk)| (lid.clone(), lk.clone()));
        if let Some((lid, lk)) = pending {
            let answer = self.link_answer(&lid, &lk);
            return self.resp(201, Some(answer), &[]);
        }
        let code = random_code();
        let lid = keys::b64url(&rand::random::<[u8; 16]>());
        let lk = Link { key: public, code, body: data, status: "pending".into(), expires: now + 900.0,
                        last_poll: None, device_id: None };
        let answer = self.link_answer(&lid, &lk);
        self.links.insert(lid, lk);
        self.resp(201, Some(answer), &[])
    }

    fn link_answer(&self, lid: &str, lk: &Link) -> Value {
        let code = &lk.code;
        json!({"link_id": lid, "user_code": format!("{}-{}", &code[..4], &code[4..]),
               "verification_uri": "https://railspotting.com/link",
               "verification_uri_complete": format!("https://railspotting.com/link?code={code}"),
               "expires_in": (lk.expires - self.now()).trunc() as i64, "interval": 5})
    }

    fn link_poll(&mut self, request: &Request, path: &str, lid: &str) -> Reply {
        let Some(key) = self.links.get(lid).map(|lk| lk.key.clone()) else {
            return self.err(404, "not_found", "", &[]);
        };
        if let Some(bad) = self.check_sig(request, path, b"", &key) {
            return if bad.status == 401 && bad.json()["error"] == "bad_signature" {
                self.err(404, "not_found", "", &[])
            } else {
                bad
            };
        }
        let now = self.now();
        let mut lk = self.links[lid].clone();
        if lk.last_poll.is_some_and(|lp| now - lp < 5.0) {
            lk.last_poll = Some(now);
            self.links.insert(lid.to_string(), lk);
            return self.err(429, "slow_down", "", &[]);
        }
        lk.last_poll = Some(now);
        if lk.status == "pending" && now >= lk.expires {
            lk.status = "expired".into();
        }
        let reply_body = if lk.status == "pending" {
            json!({"status": "pending", "expires_in": (lk.expires - now).trunc() as i64, "interval": 5})
        } else if lk.status == "approved" {
            json!({"status": "approved", "device_id": lk.device_id})
        } else {
            json!({"status": lk.status})
        };
        self.links.insert(lid.to_string(), lk);
        self.resp(200, Some(reply_body), &[])
    }

    pub fn find_link(&self, user_code: &str) -> Option<String> {
        let code: String = user_code.to_uppercase().chars().filter(|c| *c != ' ' && *c != '-').collect();
        self.links.iter().find(|(_, lk)| lk.code == code).map(|(lid, _)| lid.clone())
    }

    /// The owner approves on the website: the headend assigns a locally administered ID.
    pub fn approve_link(&mut self, user_code: &str, device_id: Option<&str>) -> Option<String> {
        let lid = self.find_link(user_code)?;
        let device_id = match device_id {
            Some(d) => d.to_string(),
            None => {
                let first = (rand::random::<u8>() | 0x02) & !0x01;
                format!("rs-{first:02x}{}", hex::encode(rand::random::<[u8; 5]>()))
            }
        };
        let lk = self.links.get_mut(&lid)?;
        lk.status = "approved".into();
        lk.device_id = Some(device_id.clone());
        let key = lk.key.clone();
        self.devices.insert(device_id.clone(), Device { key, status: "approved".into() });
        Some(device_id)
    }

    pub fn deny_link(&mut self, user_code: &str) {
        if let Some(lid) = self.find_link(user_code) {
            if let Some(lk) = self.links.get_mut(&lid) {
                lk.status = "denied".into();
            }
        }
    }

    pub fn approve(&mut self, device_id: &str) {
        if let Some(d) = self.devices.get_mut(device_id) {
            d.status = "approved".into();
        }
    }

    fn verify(&mut self, request: &Request, path: &str, body: &[u8]) -> Result<Device, Reply> {
        let Some(dev) = self.devices.get(request.header("X-RS-Device").unwrap_or("")).cloned() else {
            return Err(self.err(401, "unknown_device", "", &[]));
        };
        match self.check_common(request, path, body, &dev.key) {
            Some(bad) => Err(bad),
            None => Ok(dev),
        }
    }
}

/// The running server. Stops when dropped.
pub struct FakeHeadend {
    state: Arc<Mutex<FakeState>>,
    port: u16,
    stop: Arc<AtomicBool>,
}

impl FakeHeadend {
    /// Start on 127.0.0.1 with a random port.
    pub fn start(clock: Clock, config: Value) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let state = Arc::new(Mutex::new(FakeState::new(clock, config)));
        let stop = Arc::new(AtomicBool::new(false));
        let (st, sp) = (state.clone(), stop.clone());
        std::thread::Builder::new().name("fake-headend".into()).spawn(move || {
            for conn in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = conn else { continue };
                let st = st.clone();
                let _ = std::thread::Builder::new().name("fake-headend-conn".into())
                    .spawn(move || serve(stream, &st));
            }
        })?;
        Ok(FakeHeadend { state, port, stop })
    }

    pub fn url(&self) -> String { format!("http://127.0.0.1:{}", self.port) }
    pub fn port(&self) -> u16 { self.port }

    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn approve_link(&self, user_code: &str, device_id: Option<&str>) -> Option<String> {
        self.state().approve_link(user_code, device_id)
    }
    pub fn deny_link(&self, user_code: &str) { self.state().deny_link(user_code) }
    pub fn approve(&self, device_id: &str) { self.state().approve(device_id) }
    pub fn find_link(&self, user_code: &str) -> Option<String> { self.state().find_link(user_code) }
    pub fn set_down(&self, down: bool) { self.state().down = down; }
    pub fn add_device(&self, device_id: &str, public: &[u8], status: &str) {
        self.state().devices.insert(device_id.into(), Device { key: public.to_vec(), status: status.into() });
    }
    pub fn fail_next(&self, status: u16, error: &str, headers: &[(&str, &str)]) {
        let h = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.state().fail_next.push_back((status, error.to_string(), h));
    }
}

impl Drop for FakeHeadend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wake the accept loop
    }
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = BTreeMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).ok()? == 0 {
            return None;
        }
        let h = h.trim_end_matches(['\r', '\n']);
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let mut body = Vec::new();
    if let Some(n) = headers.get("content-length").and_then(|n| n.parse::<usize>().ok()) {
        body.resize(n, 0);
        reader.read_exact(&mut body).ok()?;
    } else if headers.get("transfer-encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let n = usize::from_str_radix(size.trim().split(';').next()?, 16).ok()?;
            let mut chunk = vec![0; n + 2];
            reader.read_exact(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }
    Some(Request { method, path, headers, body })
}

fn serve(mut stream: TcpStream, state: &Mutex<FakeState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(request) = read_request(&stream) else { return };
    let reply = state.lock().unwrap_or_else(|p| p.into_inner()).handle(&request);
    let Some(reply) = reply else {
        // down: close without an answer, the client sees a transport failure
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    };
    let reason = ureq::http::StatusCode::from_u16(reply.status).ok()
        .and_then(|s| s.canonical_reason()).unwrap_or("");
    let mut out = format!("HTTP/1.1 {} {}\r\n", reply.status, reason);
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = reply.body.unwrap_or_default();
    if !body.is_empty() {
        out.push_str("Content-Type: application/json\r\n");
    }
    out.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&body);
    let _ = stream.write_all(&bytes);
    let _ = stream.flush();
}

