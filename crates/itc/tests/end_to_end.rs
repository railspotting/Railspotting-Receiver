//! A made-up frame, modulated as pi/4-DQPSK, demodulated and decoded again.

use num_complex::Complex32;
use std::f32::consts::PI;

/// Bits to pi/4-DQPSK samples: Gray dibits 00, 01, 11, 10 are +45, +135, -135, -45 degrees.
fn modulate(bits: &[u8], rate: f64, symbol_rate: f64, offset_hz: f32) -> Vec<Complex32> {
    let sps = (rate / symbol_rate) as usize;
    let mut phase = 0.0f32;
    let mut out = vec![Complex32::new(0.0, 0.0); 40 * sps]; // quiet before
    for (k, d) in bits.chunks(2).enumerate() {
        phase += match (d[0], d[1]) {
            (0, 0) => PI / 4.0,
            (0, 1) => 3.0 * PI / 4.0,
            (1, 1) => -3.0 * PI / 4.0,
            _ => -PI / 4.0,
        };
        for j in 0..sps {
            let t = (k * sps + j) as f32 / rate as f32;
            out.push(Complex32::from_polar(0.5, phase + 2.0 * PI * offset_hz * t));
        }
    }
    out.extend(vec![Complex32::new(0.0, 0.0); 40 * sps]);
    out
}

#[test]
fn a_frame_survives_the_radio_path_at_both_rates() {
    let msg = [0x12, 0x34, 0x56, 0x0f, 0xf0, 0x82, 0, 7, 0xb6, 0x00, 9, 8, 7, 6];
    let mut bits = vec![1u8; 14]; // preamble
    bits.extend(railspotting_itc::link::encode_frame(0x33, &msg));
    for sr in [16_000.0, 8_000.0] {
        let samples = modulate(&bits, 96_000.0, sr, 250.0);
        let d = railspotting_itc::dsp::demod(&samples, 96_000.0).expect("demodulates");
        assert_eq!(d.symbol_rate, sr);
        let f = railspotting_itc::link::frames(&railspotting_itc::link::symbols_to_bits(&d.symbols));
        assert_eq!(f.len(), 1, "{sr}: {f:?}");
        assert!(f[0].crc_ok);
        assert_eq!(f[0].payload(), &msg[..10]);
        let v = railspotting_itc::msg::describe(f[0].header.kind, f[0].payload());
        assert_eq!(v["wiu"], "1234560ff0");
        assert_eq!(v["indicators"], "b600");
    }
}
