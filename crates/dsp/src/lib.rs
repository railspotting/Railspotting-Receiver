//! Signal processing for the RailSpotting receiver.
//!
//! The channelizer and per-channel detection (everything but the USB device threads), bursts and
//! recordings, the EOT decoder, voice audio taps and the radio side of the simulator.

pub mod bursts;
pub mod eot;
pub mod sdr;
pub mod sim;
pub mod voice;

#[cfg(test)]
mod tests;

pub use num_complex::{Complex32, Complex64};
pub use sdr::{ChannelDetector, DetAction, DetEvent, DeviceControl, Emit, SdrSource, Transcriber,
              TranscriptDone, WindowPlan};

/// `x` rounded to `ndigits` decimals: the exact binary value rounded half to even.
pub fn round_even(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    // Rust's float formatting rounds the exact binary value half to even
    format!("{:.*}", ndigits, x).parse().unwrap_or(x)
}

/// `x` rounded half to even, as an integer.
pub fn round_even_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Raw dongle bytes (interleaved unsigned I/Q) to samples, the usual RTL-SDR scaling:
/// `b / 127.5 - 1` in double precision, stored as float32. Appends to `out`.
pub fn cu8_to_iq(bytes: &[u8], out: &mut Vec<Complex32>) {
    out.reserve(bytes.len() / 2);
    for p in bytes.chunks_exact(2) {
        out.push(Complex32::new((p[0] as f64 / 127.5 - 1.0) as f32, (p[1] as f64 / 127.5 - 1.0) as f32));
    }
}
