//! Ed25519 device keys and request signing (API contract v1, unchanged in v1.4).

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, URL_SAFE_NO_PAD};
use base64::engine::DecodePaddingMode;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;

pub use ed25519_dalek::SigningKey as PrivateKey;

pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

// Lenient decoding: padding optional, stray characters skipped.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

pub fn b64url_decode(text: &str) -> Result<Vec<u8>, String> {
    let clean: String = text
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '/')
        .collect();
    LENIENT.decode(clean.as_bytes()).map_err(|e| format!("Incorrect padding or bad base64: {e}"))
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn generate_private() -> SigningKey {
    SigningKey::from_bytes(&rand::random::<[u8; 32]>())
}

pub fn private_from_seed(seed: &[u8]) -> Result<SigningKey, String> {
    let bytes: [u8; 32] = seed
        .try_into()
        .map_err(|_| "An Ed25519 private key is 32 bytes long".to_string())?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub fn private_to_b64(key: &SigningKey) -> String {
    b64url(&key.to_bytes())
}

pub fn private_from_b64(text: &str) -> Result<SigningKey, String> {
    private_from_seed(&b64url_decode(text)?)
}

pub fn public_bytes(key: &SigningKey) -> [u8; 32] {
    key.verifying_key().to_bytes()
}

pub fn public_b64(key: &SigningKey) -> String {
    b64url(&public_bytes(key))
}

pub fn canonical_string(method: &str, path: &str, timestamp: i64, nonce: &str, body: &[u8]) -> String {
    let body_hash = sha256_hex(body);
    [method.to_uppercase(), path.to_string(), timestamp.to_string(), nonce.to_string(), body_hash].join("\n")
}

pub fn sign(key: &SigningKey, method: &str, path: &str, timestamp: i64, nonce: &str, body: &[u8]) -> String {
    let message = canonical_string(method, path, timestamp, nonce, body);
    b64url(&key.sign(message.as_bytes()).to_bytes())
}

pub fn new_nonce() -> String {
    hex::encode(rand::random::<[u8; 16]>())
}

/// The v1 signature headers. Linking (v1.4) signs without X-RS-Device: there is none yet.
pub fn signed_headers(key: &SigningKey, device_id: Option<&str>, method: &str, path: &str, body: &[u8],
                      timestamp: i64, nonce: Option<&str>) -> Vec<(String, String)> {
    let nonce = match nonce {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => new_nonce(),
    };
    let mut headers = Vec::new();
    if let Some(d) = device_id.filter(|d| !d.is_empty()) {
        headers.push(("X-RS-Device".to_string(), d.to_string()));
    }
    let signature = sign(key, method, path, timestamp, &nonce, body);
    headers.push(("X-RS-Timestamp".to_string(), timestamp.to_string()));
    headers.push(("X-RS-Nonce".to_string(), nonce));
    headers.push(("X-RS-Signature".to_string(), signature));
    headers
}

// credentials.json -------------------------------------------------------------

/// The credentials object; a missing file is an empty one. A bad file is an error.
pub fn load_credentials(path: &Path) -> Result<Map<String, Value>, String> {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(m)) => Ok(m),
            Ok(_) => Err(format!("{}: not a JSON object", path.display())),
            Err(e) => Err(format!("{}: {e}", path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write credentials.json with mode 0600, never readable by others.
pub fn save_credentials(path: &Path, data: &Map<String, Value>) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        let mut f = opts.open(&tmp)?;
        let text = crate::jsonfmt::dumps_indent(&Value::Object(data.clone()), 2);
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
    }
    set_private(&tmp)?;
    fs::rename(&tmp, path)
}

fn set_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// The stored key, or a new one saved to `path`. `log` says whether a new key is logged.
pub fn load_or_create_key(path: &Path, log: bool) -> Result<SigningKey, String> {
    let mut creds = load_credentials(path)?;
    if let Some(pk) = creds.get("private_key").filter(|v| truthy(v)) {
        let text = as_text(pk);
        if !text.starts_with('<') {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(path) {
                    if meta.permissions().mode() & 0o077 != 0 {
                        set_private(path).map_err(|e| format!("{}: {e}", path.display()))?;
                    }
                }
            }
            return private_from_b64(&text);
        }
    }
    let key = generate_private();
    creds.shift_remove("_comment");
    creds.insert("private_key".into(), Value::String(private_to_b64(&key)));
    save_credentials(path, &creds).map_err(|e| format!("{}: {e}", path.display()))?;
    if log {
        log::info!("created a new device key in {}", path.display());
    }
    Ok(key)
}

pub fn replace_key(path: &Path) -> Result<SigningKey, String> {
    let mut creds = load_credentials(path)?;
    let key = generate_private();
    creds.insert("private_key".into(), Value::String(private_to_b64(&key)));
    save_credentials(path, &creds).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(key)
}

/// Check a signature over the canonical string, as the headend does.
pub fn verify(public: &[u8], canonical: &str, signature_b64: &str) -> bool {
    let Ok(pk) = <[u8; 32]>::try_from(public) else { return false };
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(raw) = b64url_decode(signature_b64) else { return false };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(&raw) else { return false };
    vk.verify_strict(canonical.as_bytes(), &sig).is_ok()
}
