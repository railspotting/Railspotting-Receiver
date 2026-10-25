//! Recordings for the ITC decoder: raw RTL-SDR bytes (`name.cu8`) with a `name.json` sidecar
//! saying where the dongle was tuned, how fast it sampled and when the first sample was taken.
//! Live samples come from the receiver's dongles (librtlsdr); a recording plays them back.
//!
//! Receive only: nothing here can transmit.

use num_complex::Complex32;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// What a stream of samples is: where it was tuned, how fast, and when it started.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meta {
    pub center_hz: f64,
    pub rate: f64,
    /// Unix seconds of the first sample.
    pub start: f64,
    /// Always "cu8" for now: interleaved unsigned 8-bit I and Q.
    pub format: String,
    #[serde(default)]
    pub gain_db: Option<f64>,
}

pub trait Source {
    fn meta(&self) -> &Meta;
    /// Append up to `n` samples to `out`; 0 means the stream ended.
    fn read(&mut self, out: &mut Vec<Complex32>, n: usize) -> io::Result<usize>;
    /// A live radio, as opposed to a recording.
    fn live(&self) -> bool {
        true
    }
}

/// 8-bit RTL-SDR bytes to complex samples.
pub fn cu8_to_complex(bytes: &[u8], out: &mut Vec<Complex32>) {
    out.extend(bytes.chunks_exact(2).map(|p| Complex32::new((p[0] as f32 - 127.5) / 127.5, (p[1] as f32 - 127.5) / 127.5)));
}

/// A recording: `name.cu8` and `name.json` (its `Meta`).
pub struct File {
    meta: Meta,
    r: io::BufReader<fs::File>,
    buf: Vec<u8>,
}

impl File {
    pub fn open(path: &Path) -> io::Result<File> {
        let meta: Meta = serde_json::from_str(&fs::read_to_string(sidecar(path))?)
            .map_err(|e| io::Error::other(format!("{}: {e}", sidecar(path).display())))?;
        Ok(File { meta, r: io::BufReader::with_capacity(1 << 20, fs::File::open(path)?), buf: vec![] })
    }
}

impl Source for File {
    fn meta(&self) -> &Meta {
        &self.meta
    }
    fn live(&self) -> bool {
        false
    }
    fn read(&mut self, out: &mut Vec<Complex32>, n: usize) -> io::Result<usize> {
        self.buf.resize(2 * n, 0);
        let mut got = 0;
        while got < self.buf.len() {
            match self.r.read(&mut self.buf[got..])? {
                0 => break,
                k => got += k,
            }
        }
        cu8_to_complex(&self.buf[..got & !1], out);
        Ok(got / 2)
    }
}

/// The `.json` beside a recording.
pub fn sidecar(path: &Path) -> PathBuf {
    path.with_extension("json")
}

/// Writes raw bytes straight from a live source to `name.cu8` and its `.json`.
pub struct Recorder {
    w: BufWriter<fs::File>,
}

impl Recorder {
    pub fn create(path: &Path, meta: &Meta) -> io::Result<Recorder> {
        fs::write(sidecar(path), serde_json::to_string_pretty(meta).map_err(io::Error::other)? + "\n")?;
        Ok(Recorder { w: BufWriter::with_capacity(1 << 20, fs::File::create(path)?) })
    }

    /// Back to 8-bit bytes, exactly as the dongle sent them.
    pub fn write(&mut self, samples: &[Complex32]) -> io::Result<()> {
        let mut b = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            b.push((s.re * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8);
            b.push((s.im * 127.5 + 127.5).round().clamp(0.0, 255.0) as u8);
        }
        self.w.write_all(&b)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_trip_through_samples() {
        let bytes: Vec<u8> = (0..=255).collect();
        let mut s = vec![];
        cu8_to_complex(&bytes, &mut s);
        assert_eq!(s.len(), 128);
        assert!((s[0].re + 1.0).abs() < 1e-6 && (s[127].im - 1.0).abs() < 1e-6);
        let dir = std::env::temp_dir().join(format!("itc-input-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.cu8");
        let meta = Meta { center_hz: 220.2875e6, rate: 1.024e6, start: 1.0, format: "cu8".into(), gain_db: Some(40.2) };
        let mut r = Recorder::create(&path, &meta).unwrap();
        r.write(&s).unwrap();
        r.finish().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut f = File::open(&path).unwrap();
        assert_eq!(f.meta(), &meta);
        let mut back = vec![];
        assert_eq!(f.read(&mut back, 1000).unwrap(), 128);
        assert_eq!(back, s);
        fs::remove_dir_all(&dir).unwrap();
    }
}
