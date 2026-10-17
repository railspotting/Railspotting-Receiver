//! 16-bit mono WAV files for the voice clips: written with the standard 44-byte header, read
//! chunk by chunk.

use std::io::{self, Read, Write};
use std::path::Path;

/// Write float audio as 16-bit PCM: `(pcm * 32767).astype("<i2")` (truncating).
pub fn write_pcm16(path: &Path, pcm: &[f32], rate: u32) -> io::Result<()> {
    let data: Vec<u8> = pcm.iter().flat_map(|x| ((x * 32767.0) as i16).to_le_bytes()).collect();
    let mut f = io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data.len() as u32).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&1u16.to_le_bytes())?; // mono
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&(rate * 2).to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&(data.len() as u32).to_le_bytes())?;
    f.write_all(&data)?;
    f.flush()
}

/// A WAV file's format and raw frames.
pub struct Wav {
    pub channels: u16,
    pub rate: u32,
    pub sample_width: u16,
    pub frames: Vec<u8>,
}

impl Wav {
    pub fn n_frames(&self) -> usize {
        let w = (self.channels as usize * self.sample_width as usize).max(1);
        self.frames.len() / w
    }

    /// 16-bit samples as float32 / 32768.
    pub fn samples(&self) -> Vec<f32> {
        self.frames.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0).collect()
    }
}

/// Read a PCM WAV file (what `wave.open` accepts).
pub fn read(path: &Path) -> io::Result<Wav> {
    let mut buf = vec![];
    std::fs::File::open(path)?.read_to_end(&mut buf)?;
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    if buf.len() < 12 || &buf[..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(bad("file does not start with RIFF id"));
    }
    let (mut pos, mut fmt, mut data) = (12usize, None, None);
    while pos + 8 <= buf.len() {
        let id = &buf[pos..pos + 4];
        let len = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]) as usize;
        let body = &buf[pos + 8..(pos + 8 + len).min(buf.len())];
        match id {
            b"fmt " if body.len() >= 16 => fmt = Some(body.to_vec()),
            b"data" => data = Some(body.to_vec()),
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    let fmt = fmt.ok_or_else(|| bad("fmt chunk missing"))?;
    let u16_at = |i: usize| u16::from_le_bytes([fmt[i], fmt[i + 1]]);
    if u16_at(0) != 1 {
        return Err(bad(&format!("unknown format: {}", u16_at(0))));
    }
    Ok(Wav {
        channels: u16_at(2),
        rate: u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]),
        sample_width: u16_at(14).div_ceil(8),
        frames: data.ok_or_else(|| bad("data chunk missing"))?,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn round_trip() {
        let d = std::env::temp_dir().join(format!("rs-wav-{}.wav", std::process::id()));
        super::write_pcm16(&d, &[0.0, 0.5, -0.5, 1.0], 16_000).unwrap();
        let w = super::read(&d).unwrap();
        assert_eq!((w.channels, w.rate, w.sample_width, w.n_frames()), (1, 16_000, 2, 4));
        assert_eq!(w.samples(), vec![0.0, 16383.0 / 32768.0, -16383.0 / 32768.0, 32767.0 / 32768.0]);
        let _ = std::fs::remove_file(d);
    }
}
