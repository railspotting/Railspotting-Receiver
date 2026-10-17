//! Signed HTTPS client for the RailSpotting headend (API contract v1.4).

use crate::keys;
use crate::keys::PrivateKey;
use railspotting_common::Clock;
use serde_json::{json, Map, Value};
use std::fmt;
use std::sync::Mutex;
use std::time::Duration;
use ureq::http;

pub const API_BASE: &str = "/api/v1";
pub const MAX_SKEW_S: f64 = 300.0;
pub const WARN_DRIFT_S: f64 = 60.0;
/// How long an idle connection to the headend is kept for the next request. Longer than the
/// 60 s heartbeat, so a heartbeat reuses the connection instead of a new TCP and TLS handshake
/// (about 8 KB a minute on a metered link, against under 2 KB), and shorter than nginx's default
/// 75 s keepalive_timeout, so the server never closes one we are about to use.
pub const KEEP_IDLE_S: u64 = 70;

/// A non-success response or a transport failure. `clock_offset` is set only for the
/// local refusal to sign when the clock is off.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: Option<u16>,
    pub error: String,
    pub message: String,
    pub retry_after: Option<f64>,
    pub body: Map<String, Value>,
    pub clock_offset: Option<f64>,
}

impl ApiError {
    pub fn new(status: Option<u16>, error: &str, message: &str, retry_after: Option<f64>,
               body: Option<Map<String, Value>>) -> Self {
        ApiError { status, error: error.into(), message: message.into(), retry_after,
                   body: body.unwrap_or_default(), clock_offset: None }
    }

    /// ClockError: the local clock is too far from the server's to sign.
    pub fn clock(offset: f64) -> Self {
        let mut e = ApiError::new(None, "clock_wrong",
                                  &format!("local clock is {offset:+.0} s from the server; not signing"),
                                  None, None);
        e.clock_offset = Some(offset);
        e
    }

    pub fn auth_failed(&self) -> bool { self.status == Some(401) }
    pub fn not_allowed(&self) -> bool { self.status == Some(403) }
    pub fn network(&self) -> bool { self.status.is_none() }
    pub fn is_clock_error(&self) -> bool { self.clock_offset.is_some() }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self.status {
            None => self.message.clone(),
            Some(s) => format!("{s} {}: {}", self.error, self.message).trim().to_string(),
        };
        f.write_str(if text.is_empty() { &self.error } else { &text })
    }
}

impl std::error::Error for ApiError {}

/// Request bodies as compact JSON: no spaces after `,` and `:`, non-ASCII text left as UTF-8.
pub fn dumps(obj: &Value) -> Vec<u8> {
    crate::jsonfmt::dumps_compact(obj).into_bytes()
}

/// Exponential backoff with jitter, 2 s to 5 min.
pub struct Backoff {
    pub base: f64,
    pub cap: f64,
    pub failures: u32,
    rng: Box<dyn FnMut() -> f64 + Send>,
}

impl Default for Backoff {
    fn default() -> Self { Backoff::new(2.0, 300.0) }
}

impl Backoff {
    pub fn new(base: f64, cap: f64) -> Self {
        Backoff::with_rng(base, cap, Box::new(rand::random::<f64>))
    }

    /// `rng` returns uniform values in [0, 1).
    pub fn with_rng(base: f64, cap: f64, rng: Box<dyn FnMut() -> f64 + Send>) -> Self {
        Backoff { base, cap, failures: 0, rng }
    }

    pub fn reset(&mut self) { self.failures = 0; }

    pub fn next_delay(&mut self, retry_after: Option<f64>) -> f64 {
        self.failures += 1;
        let mut delay = self.cap.min(self.base * 2f64.powi(self.failures as i32 - 1));
        delay *= 0.5 + (self.rng)() / 2.0;
        if let Some(r) = retry_after.filter(|r| *r != 0.0) {
            delay = delay.max(r);
        }
        (self.base / 2.0).max(self.cap.min(delay))
    }
}

/// A response read in full. Header names are kept lower case.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Result<Value, ApiError> {
        serde_json::from_slice(&self.body).map_err(|e| {
            ApiError::new(Some(self.status), "invalid_json", &format!("response is not JSON: {e}"), None, None)
        })
    }
}

/// Round to 1 decimal: half even on the exact binary value, as `{:.1}` does.
fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

/// Transport error names for `last_results` and logs. The names are kept stable so reports and
/// log searches from earlier receiver versions still match.
fn error_name(e: &ureq::Error) -> &'static str {
    use std::io::ErrorKind as K;
    use ureq::Timeout as T;
    match e {
        ureq::Error::Timeout(T::Connect | T::Resolve) => "ConnectTimeout",
        ureq::Error::Timeout(T::SendRequest | T::SendBody | T::Await100) => "WriteTimeout",
        ureq::Error::Timeout(T::RecvResponse | T::RecvBody) => "ReadTimeout",
        ureq::Error::Timeout(_) => "TimeoutException",
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Tls(_) => "ConnectError",
        ureq::Error::Io(io) => match io.kind() {
            K::ConnectionRefused | K::AddrNotAvailable | K::NotFound | K::PermissionDenied => "ConnectError",
            K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => "RemoteProtocolError",
            K::TimedOut | K::WouldBlock => "ReadTimeout",
            _ => "ReadError",
        },
        ureq::Error::Protocol(_) => "RemoteProtocolError",
        ureq::Error::BadUri(_) | ureq::Error::Http(_) => "UnsupportedProtocol",
        _ => "TransportError",
    }
}

fn split_url(url: &str) -> (String, String) {
    // (scheme://netloc, path) as urlsplit gives them
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("", url),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = &rest[..end];
    let tail = &rest[end..];
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    (format!("{scheme}://{netloc}"), tail[..path_end].to_string())
}

pub struct ApiClient {
    pub server_url: String,
    pub prefix: String,
    origin: String,
    device_id: Mutex<Option<String>>, // None until a link is approved
    key: PrivateKey,
    clock: Clock,
    agent: ureq::Agent,
    offset: Mutex<Option<f64>>, // server time minus local time
    last_results: Mutex<Map<String, Value>>,
}

impl ApiClient {
    /// `ca_file` replaces the system roots, for TLS verification. `timeout` is
    /// applied to connect, send and receive alike.
    pub fn new(server_url: &str, device_id: Option<String>, private_key: PrivateKey, ca_file: Option<&str>,
               clock: Clock, timeout: f64) -> Result<Self, ApiError> {
        let server_url = server_url.trim_end_matches('/').to_string();
        let (origin, path) = split_url(&server_url);
        let prefix = path.trim_end_matches('/').to_string();
        // a negative, NaN or infinite timeout means none rather than a panic
        let t = Duration::try_from_secs_f64(timeout).ok();
        let mut cfg = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0) // redirects are not followed
            .max_redirects_will_error(false)
            .user_agent("railspotting-receiver")
            .max_idle_age(Duration::from_secs(KEEP_IDLE_S))
            .timeout_resolve(t)
            .timeout_connect(t)
            .timeout_send_request(t)
            .timeout_send_body(t)
            .timeout_recv_response(t)
            .timeout_recv_body(t);
        if let Some(path) = ca_file.filter(|p| !p.is_empty()) {
            let pem = std::fs::read(path)
                .map_err(|e| ApiError::new(None, "ca_file", &format!("{path}: {e}"), None, None))?;
            let mut certs = Vec::new();
            for item in ureq::tls::parse_pem(&pem) {
                match item {
                    Ok(ureq::tls::PemItem::Certificate(c)) => certs.push(c),
                    Ok(_) => {}
                    Err(e) => return Err(ApiError::new(None, "ca_file", &format!("{path}: {e}"), None, None)),
                }
            }
            let tls = ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
                .build();
            cfg = cfg.tls_config(tls);
        }
        Ok(ApiClient {
            server_url, prefix, origin,
            device_id: Mutex::new(device_id),
            key: private_key,
            clock,
            agent: cfg.build().into(),
            offset: Mutex::new(None),
            last_results: Mutex::new(Map::new()),
        })
    }

    /// The usual client: system roots, 20 s timeout.
    pub fn simple(server_url: &str, device_id: Option<String>, private_key: PrivateKey, clock: Clock)
                  -> Result<Self, ApiError> {
        ApiClient::new(server_url, device_id, private_key, None, clock, 20.0)
    }

    /// How long an idle connection is kept for reuse.
    pub fn keep_idle(&self) -> Duration { self.agent.config().max_idle_age() }
    pub fn device_id(&self) -> Option<String> { lock(&self.device_id).clone() }
    pub fn set_device_id(&self, device_id: Option<String>) { *lock(&self.device_id) = device_id; }
    pub fn key(&self) -> &PrivateKey { &self.key }
    pub fn offset(&self) -> Option<f64> { *lock(&self.offset) }
    pub fn set_offset(&self, offset: Option<f64>) { *lock(&self.offset) = offset; }

    /// endpoint -> {"ts", "status", "error"} for the status page.
    pub fn last_results(&self) -> Map<String, Value> { lock(&self.last_results).clone() }

    // clock -------------------------------------------------------------------

    pub fn clock_known_bad(&self) -> bool {
        self.offset().is_some_and(|o| o.abs() > MAX_SKEW_S - 30.0)
    }

    pub fn clock_synced(&self) -> Option<bool> {
        self.offset().map(|o| o.abs() <= WARN_DRIFT_S)
    }

    fn note_server_time(&self, resp: &Response) {
        let Some(value) = resp.header("X-RS-Server-Time").filter(|v| !v.is_empty()) else { return };
        let Ok(server) = value.trim().parse::<f64>() else { return };
        let offset = server - (self.clock)();
        let prev = {
            let mut o = lock(&self.offset);
            let prev = *o;
            *o = Some(offset);
            prev
        };
        if offset.abs() > WARN_DRIFT_S && prev.is_none_or(|p| p.abs() <= WARN_DRIFT_S) {
            log::warn!("clock drift: local clock is {:+.0} s from the server; check NTP on the Proxmox host",
                       -offset);
        }
    }

    fn send(&self, method: &str, path: &str, body: Option<&[u8]>, headers: &[(String, String)])
            -> Result<Response, ureq::Error> {
        // the method goes out upper case, as it is signed
        let mut req = http::Request::builder().method(method.to_uppercase().as_str())
            .uri(format!("{}{}", self.origin, path));
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = match body {
            Some(b) => self.agent.run(req.body(b.to_vec())?)?,
            None => self.agent.run(req.body(())?)?,
        };
        let status = resp.status();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
            .collect();
        let body = resp.into_body().with_config().limit(64 * 1024 * 1024).read_to_vec()?;
        Ok(Response {
            status: status.as_u16(),
            reason: status.canonical_reason().unwrap_or("").to_string(),
            headers,
            body,
        })
    }

    /// Learn the server time with an unsigned request (the 401 reply still carries it).
    pub fn probe_clock(&self) -> Result<(), ApiError> {
        let path = format!("{}{}/register/status", self.prefix, API_BASE);
        match self.send("GET", &path, None, &[]) {
            Ok(resp) => {
                self.note_server_time(&resp);
                Ok(())
            }
            Err(e) => Err(ApiError::new(None, "network", &e.to_string(), None, None)),
        }
    }

    // requests ----------------------------------------------------------------

    /// One request. 2xx, 3xx and 409 come back as a Response; anything else 400 and up, a
    /// transport failure or a known bad clock is an ApiError.
    pub fn request(&self, method: &str, endpoint: &str, body: Option<&Value>, signed: bool,
                   headers: &[(String, String)], query: &str, as_device: bool) -> Result<Response, ApiError> {
        let path = format!("{}{}{}{}", self.prefix, API_BASE, endpoint,
                           if query.is_empty() { String::new() } else { format!("?{query}") });
        let raw = body.map(dumps).unwrap_or_default();
        let mut hdrs: Vec<(String, String)> = headers.to_vec();
        if body.is_some() {
            set_header(&mut hdrs, "Content-Type", "application/json");
        }
        if signed {
            if self.clock_known_bad() {
                return Err(ApiError::clock(-self.offset().unwrap_or(0.0)));
            }
            let device = if as_device { self.device_id() } else { None };
            let ts = (self.clock)().trunc() as i64;
            for (k, v) in keys::signed_headers(&self.key, device.as_deref(), method, &path, &raw, ts, None) {
                set_header(&mut hdrs, &k, &v);
            }
        }
        let resp = match self.send(method, &path, body.map(|_| raw.as_slice()), &hdrs) {
            Ok(r) => r,
            Err(e) => {
                let name = error_name(&e);
                self.record(endpoint, None, Some(name));
                return Err(ApiError::new(None, "network", &format!("{name}: {e}"), None, None));
            }
        };
        self.note_server_time(&resp);
        self.record(endpoint, Some(resp.status), None);
        if resp.status >= 400 && resp.status != 409 {
            return Err(Self::error(&resp));
        }
        Ok(resp)
    }

    fn record(&self, endpoint: &str, status: Option<u16>, error: Option<&str>) {
        let key = endpoint.split('?').next().unwrap_or(endpoint).to_string();
        let entry = json!({"ts": round1((self.clock)()), "status": status, "error": error});
        lock(&self.last_results).insert(key, entry);
    }

    fn error(resp: &Response) -> ApiError {
        let body = match serde_json::from_slice::<Value>(&resp.body) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        let retry = resp.header("Retry-After").filter(|v| !v.is_empty());
        let retry_s = retry.and_then(|r| r.trim().parse::<f64>().ok());
        // a missing, null, false, zero or empty value falls back
        let text = |k: &str| -> Option<String> {
            match body.get(k) {
                None | Some(Value::Null) | Some(Value::Bool(false)) => None,
                Some(Value::String(s)) if s.is_empty() => None,
                Some(Value::Number(n)) if n.as_f64() == Some(0.0) => None,
                Some(Value::Array(a)) if a.is_empty() => None,
                Some(Value::Object(m)) if m.is_empty() => None,
                Some(Value::String(s)) => Some(s.clone()),
                Some(v) => Some(v.to_string()),
            }
        };
        let error = text("error").unwrap_or_else(|| format!("http_{}", resp.status));
        let message = text("message").unwrap_or_else(|| resp.reason.clone());
        ApiError::new(Some(resp.status), &error, &message, retry_s, Some(body))
    }

    fn signed(&self, method: &str, endpoint: &str, body: Option<&Value>) -> Result<Response, ApiError> {
        self.request(method, endpoint, body, true, &[], "", true)
    }

    // endpoints ---------------------------------------------------------------

    /// POST /link, signed with the key in the body. 201 a link to show, 200 already approved.
    pub fn link_start(&self, body: &Value) -> Result<(u16, Value), ApiError> {
        let resp = self.request("POST", "/link", Some(body), true, &[], "", false)?;
        Ok((resp.status, resp.json()?))
    }

    pub fn link_poll(&self, link_id: &str) -> Result<Value, ApiError> {
        self.request("GET", &format!("/link/{link_id}"), None, true, &[], "", false)?.json()
    }

    /// (config, etag); (None, the same etag) on 304.
    pub fn get_config(&self, etag: Option<&str>) -> Result<(Option<Value>, Option<String>), ApiError> {
        let headers: Vec<(String, String)> = match etag.filter(|e| !e.is_empty()) {
            Some(e) => vec![("If-None-Match".into(), e.into())],
            None => vec![],
        };
        let resp = self.request("GET", "/config", None, true, &headers, "", true)?;
        if resp.status == 304 {
            return Ok((None, etag.map(str::to_string)));
        }
        Ok((Some(resp.json()?), resp.header("ETag").map(str::to_string)))
    }

    pub fn post_events(&self, events: &[Value]) -> Result<Value, ApiError> {
        self.signed("POST", "/events", Some(&json!({"events": events})))?.json()
    }

    pub fn heartbeat(&self, body: &Value) -> Result<Value, ApiError> {
        self.signed("POST", "/heartbeat", Some(body))?.json()
    }

    pub fn post_report(&self, report: &Value) -> Result<Value, ApiError> {
        self.signed("POST", "/reports", Some(report))?.json()
    }

    pub fn get_commands(&self) -> Result<Vec<Value>, ApiError> {
        let data = self.signed("GET", "/commands", None)?.json()?;
        Ok(data.get("commands").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    pub fn post_command_result(&self, command_id: &str, result: &Value) -> Result<Value, ApiError> {
        self.signed("POST", &format!("/commands/{command_id}/result"), Some(result))?.json()
    }

    pub fn close(&self) {}
}

/// Set a header, replacing any of the same name.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value.to_string()));
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_range() {
        let mut b = Backoff::default();
        let delays: Vec<f64> = (0..20).map(|_| b.next_delay(None)).collect();
        assert!((1.0..=2.0).contains(&delays[0]));
        assert!(delays.iter().all(|d| *d <= 300.0));
        b.reset();
        assert!(b.next_delay(None) <= 2.0);
        assert!(Backoff::default().next_delay(Some(42.0)) >= 42.0);
        // fixed jitter: 2 * 2^(n-1) * 0.75, capped at 300
        let mut b = Backoff::with_rng(2.0, 300.0, Box::new(|| 0.5));
        let d: Vec<f64> = (0..10).map(|_| b.next_delay(None)).collect();
        assert_eq!(d, vec![1.5, 3.0, 6.0, 12.0, 24.0, 48.0, 96.0, 192.0, 225.0, 225.0]);
    }

    #[test]
    fn error_text() {
        let e = ApiError::new(Some(429), "rate_limited", "slow", Some(42.0), None);
        assert_eq!(e.to_string(), "429 rate_limited: slow");
        assert_eq!(ApiError::new(Some(500), "http_500", "", None, None).to_string(), "500 http_500:");
        assert_eq!(ApiError::new(None, "network", "", None, None).to_string(), "network");
        let c = ApiError::clock(-1000.4);
        assert_eq!(c.message, "local clock is -1000 s from the server; not signing");
        assert_eq!(ApiError::clock(0.4).message, "local clock is +0 s from the server; not signing");
        assert!(c.network() && c.is_clock_error() && c.error == "clock_wrong");
        // %+.0f rounds half to even
        for (x, want) in [(0.5, "+0"), (1.5, "+2"), (2.5, "+2"), (-2.5, "-2"), (-0.2, "-0")] {
            assert_eq!(format!("{x:+.0}"), want);
        }
    }

    #[test]
    fn error_from_reply_body() {
        let reply = |body: &str, retry: Option<&str>| Response {
            status: 503, reason: "Service Unavailable".into(),
            headers: retry.map(|r| vec![("retry-after".to_string(), r.to_string())]).unwrap_or_default(),
            body: body.as_bytes().to_vec(),
        };
        let e = ApiClient::error(&reply(r#"{"error":"busy","message":"later"}"#, Some(" 7 ")));
        assert_eq!((e.error.as_str(), e.message.as_str(), e.retry_after), ("busy", "later", Some(7.0)));
        // null, false, zero and empty values fall back
        for body in [r#"{"error":false,"message":0}"#, r#"{"error":[],"message":{}}"#, "not json", "[1]"] {
            let e = ApiClient::error(&reply(body, Some("Wed, 21 Oct 2026 07:28:00 GMT")));
            assert_eq!((e.error.as_str(), e.message.as_str(), e.retry_after),
                       ("http_503", "Service Unavailable", None), "{body}");
        }
    }

    #[test]
    fn bad_timeout_does_not_panic() {
        let clock: Clock = std::sync::Arc::new(|| 0.0);
        let key = crate::keys::generate_private();
        for t in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(ApiClient::new("http://127.0.0.1:9", None, key.clone(), None, clock.clone(), t).is_ok());
        }
    }

    #[test]
    fn url_split() {
        assert_eq!(split_url("https://api.railspotting.com"), ("https://api.railspotting.com".into(), "".into()));
        assert_eq!(split_url("http://127.0.0.1:8000/rs/"), ("http://127.0.0.1:8000".into(), "/rs/".into()));
        assert_eq!(round1(1790865400.25), 1790865400.2);
    }
}
