//! Downloading the speech model into models_dir, for the desktop app's first run.
//!
//! The Linux installer fetches the model with curl. The app does it here instead, so it can show
//! progress. The file goes to a `.part` file first and is renamed only after its size and
//! SHA-256 match, so a cut download never looks like a model.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const MODEL_URL_BASE: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/";

/// A model file and what it must look like once downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    /// Hex SHA-256, as Hugging Face lists it for the file.
    pub sha256: &'static str,
}

/// small.en, 8-bit: the file the receiver tries first.
pub const SPEECH_MODEL: ModelFile = ModelFile {
    name: "ggml-small.en-q8_0.bin",
    size: 264_477_561,
    sha256: "67a179f608ea6114bd3fdb9060e762b588a3fb3bd00c4387971be4d177958067",
};

const CHUNK: usize = 1 << 16;

/// Whether the model is already in models_dir with the right size.
pub fn is_present(models_dir: &Path, model: &ModelFile) -> bool {
    fs::metadata(models_dir.join(model.name)).is_ok_and(|m| m.len() == model.size)
}

/// Download `model` into `models_dir` and return its path. `progress` gets the bytes done and
/// the total. Setting `cancel` stops the download and removes the partial file.
pub fn download(models_dir: &Path, model: &ModelFile, progress: &mut dyn FnMut(u64, u64),
                cancel: &AtomicBool) -> Result<PathBuf, String> {
    if is_present(models_dir, model) {
        return Ok(models_dir.join(model.name));
    }
    let url = format!("{MODEL_URL_BASE}{}", model.name);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent("railspotting-receiver")
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .into();
    // Hugging Face redirects to its CDN; ureq follows redirects by default
    let resp = agent.get(&url).call().map_err(|e| format!("{url}: {e}"))?;
    let reader = resp.into_body().into_reader();
    save(reader, models_dir, model, progress, cancel)
}

/// Write `src` to models_dir as `model`, checking size and hash. Split from `download` for tests.
pub fn save(mut src: impl Read, models_dir: &Path, model: &ModelFile, progress: &mut dyn FnMut(u64, u64),
            cancel: &AtomicBool) -> Result<PathBuf, String> {
    fs::create_dir_all(models_dir).map_err(|e| format!("{}: {e}", models_dir.display()))?;
    let dest = models_dir.join(model.name);
    let part = models_dir.join(format!("{}.part", model.name));
    let result = copy_checked(&mut src, &part, model, progress, cancel);
    if let Err(e) = result {
        let _ = fs::remove_file(&part);
        return Err(e);
    }
    fs::rename(&part, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(dest)
}

fn copy_checked(src: &mut dyn Read, part: &Path, model: &ModelFile, progress: &mut dyn FnMut(u64, u64),
                cancel: &AtomicBool) -> Result<(), String> {
    let err = |e: io::Error| format!("{}: {e}", part.display());
    let mut out = fs::File::create(part).map_err(err)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut done: u64 = 0;
    progress(0, model.size);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("download cancelled".into());
        }
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("download failed after {done} bytes: {e}")),
        };
        done += n as u64;
        if done > model.size {
            return Err(format!("{} is larger than the expected {} bytes", model.name, model.size));
        }
        out.write_all(&buf[..n]).map_err(err)?;
        hash.update(&buf[..n]);
        progress(done, model.size);
    }
    out.sync_all().map_err(err)?;
    if done != model.size {
        return Err(format!("{} is {done} bytes, expected {}", model.name, model.size));
    }
    let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got != model.sha256 {
        return Err(format!("{} has SHA-256 {got}, expected {}", model.name, model.sha256));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATA: &[u8] = b"not really a model, but enough bytes to need a few reads";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rs-models-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn model_for(data: &[u8]) -> ModelFile {
        let sha: String = Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
        ModelFile { name: "m.bin", size: data.len() as u64, sha256: Box::leak(sha.into_boxed_str()) }
    }

    /// Reads at most `step` bytes at a time, so progress is reported more than once.
    struct Slow<'a>(&'a [u8], usize);
    impl Read for Slow<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.0.len().min(self.1).min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn saves_a_good_file_and_reports_progress() {
        let d = tmp("ok");
        let m = model_for(DATA);
        let mut seen = Vec::new();
        let p = save(Slow(DATA, 16), &d, &m, &mut |a, b| seen.push((a, b)), &AtomicBool::new(false)).unwrap();
        assert_eq!(fs::read(&p).unwrap(), DATA);
        assert!(!d.join("m.bin.part").exists());
        assert_eq!(seen.first(), Some(&(0, DATA.len() as u64)));
        assert_eq!(seen.last(), Some(&(DATA.len() as u64, DATA.len() as u64)));
        assert!(seen.len() > 3);
        assert!(is_present(&d, &m));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn short_download_leaves_nothing() {
        let d = tmp("short");
        let m = model_for(DATA);
        let e = save(&DATA[..10], &d, &m, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(e.contains("expected"), "{e}");
        assert!(!d.join("m.bin").exists() && !d.join("m.bin.part").exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn wrong_hash_is_refused() {
        let d = tmp("hash");
        let mut m = model_for(DATA);
        m.sha256 = "00";
        let e = save(DATA, &d, &m, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(e.contains("SHA-256"), "{e}");
        assert!(!d.join("m.bin").exists() && !d.join("m.bin.part").exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn too_long_is_refused() {
        let d = tmp("long");
        let m = model_for(&DATA[..10]);
        assert!(save(DATA, &d, &m, &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err().contains("larger"));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn cancel_removes_the_part_file() {
        let d = tmp("cancel");
        let m = model_for(DATA);
        let e = save(DATA, &d, &m, &mut |_, _| {}, &AtomicBool::new(true)).unwrap_err();
        assert!(e.contains("cancelled"));
        assert!(!d.join("m.bin.part").exists());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn speech_model_is_the_one_the_receiver_tries_first() {
        assert_eq!(SPEECH_MODEL.name, "ggml-small.en-q8_0.bin");
        assert_eq!(SPEECH_MODEL.sha256.len(), 64);
    }
}
