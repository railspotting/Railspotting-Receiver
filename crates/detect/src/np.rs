//! The numerics the detector code depends on, with exact rounding behaviour: pairwise summation
//! (so float32 correlations round the same way every time), window functions, real FFTs, median,
//! correlation coefficient, convolution, round half to even and clamped slices.

use num_complex::Complex;
use rustfft::FftPlanner;
use std::sync::Arc;

macro_rules! pairwise {
    ($name:ident, $t:ty) => {
        /// Pairwise sum of `n` values given by `f(i)`: blocks of 8 accumulators below 128 values,
        /// halves above, which bounds the rounding error of long float32 sums.
        pub fn $name(n: usize, f: &dyn Fn(usize) -> $t) -> $t {
            fn go(off: usize, n: usize, f: &dyn Fn(usize) -> $t) -> $t {
                if n < 8 {
                    let mut r: $t = 0.0;
                    for i in 0..n {
                        r += f(off + i);
                    }
                    r
                } else if n <= 128 {
                    let mut r = [0.0 as $t; 8];
                    for (j, v) in r.iter_mut().enumerate() {
                        *v = f(off + j);
                    }
                    let whole = n - n % 8;
                    let mut i = 8;
                    while i < whole {
                        for (j, v) in r.iter_mut().enumerate() {
                            *v += f(off + i + j);
                        }
                        i += 8;
                    }
                    let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
                    for k in whole..n {
                        res += f(off + k);
                    }
                    res
                } else {
                    let mut n2 = n / 2;
                    n2 -= n2 % 8;
                    go(off, n2, f) + go(off + n2, n - n2, f)
                }
            }
            go(0, n, f)
        }
    };
}
pairwise!(sum_f32, f32);
pairwise!(sum_f64, f64);

/// Symmetric Hann window of `m` points (zero at both ends).
pub fn hanning(m: usize) -> Vec<f64> {
    if m == 0 {
        return vec![];
    }
    if m == 1 {
        return vec![1.0];
    }
    let pi = std::f64::consts::PI;
    (0..m)
        .map(|k| {
            let n = 1.0 - m as f64 + 2.0 * k as f64;
            0.5 + 0.5 * (pi * n / (m as f64 - 1.0)).cos()
        })
        .collect()
}

/// `num` points evenly spaced on a log scale from `start` to `stop`, both ends exact.
pub fn geomspace(start: f64, stop: f64, num: usize) -> Vec<f64> {
    let (a, b) = (start.log10(), stop.log10());
    let div = (num - 1) as f64;
    let step = (b - a) / div;
    let mut out: Vec<f64> = (0..num).map(|i| 10f64.powf(i as f64 * step + a)).collect();
    out[0] = start;
    out[num - 1] = stop;
    out
}

/// Bin frequencies of a real FFT of `n` samples spaced `d` apart.
pub fn rfftfreq(n: usize, d: f64) -> Vec<f64> {
    let val = 1.0 / (n as f64 * d);
    (0..n / 2 + 1).map(|k| k as f64 * val).collect()
}

/// Forward real FFTs of one length, reused across frames.
pub struct Rfft64 {
    fft: Arc<dyn rustfft::Fft<f64>>,
    buf: Vec<Complex<f64>>,
}

impl Rfft64 {
    pub fn new(n: usize) -> Self {
        Rfft64 { fft: FftPlanner::new().plan_fft_forward(n), buf: vec![Complex::new(0.0, 0.0); n] }
    }
    /// Power spectrum `|rfft(x)|^2` in float64.
    pub fn power(&mut self, x: &[f64]) -> Vec<f64> {
        for (b, v) in self.buf.iter_mut().zip(x) {
            *b = Complex::new(*v, 0.0);
        }
        self.fft.process(&mut self.buf);
        self.buf[..x.len() / 2 + 1].iter().map(|c| { let h = c.re.hypot(c.im); h * h }).collect()
    }
}

/// Real FFTs computed in float32 throughout, for float32 input.
pub struct Fft32 {
    n: usize,
    fwd: Arc<dyn rustfft::Fft<f32>>,
    inv: Arc<dyn rustfft::Fft<f32>>,
    buf: Vec<Complex<f32>>,
}

impl Fft32 {
    pub fn new(n: usize) -> Self {
        let mut p = FftPlanner::new();
        Fft32 { n, fwd: p.plan_fft_forward(n), inv: p.plan_fft_inverse(n), buf: vec![Complex::new(0.0, 0.0); n] }
    }
    /// Real FFT: the `n / 2 + 1` non-negative frequency bins.
    pub fn rfft(&mut self, x: &[f32]) -> Vec<Complex<f32>> {
        for (b, v) in self.buf.iter_mut().zip(x) {
            *b = Complex::new(*v, 0.0);
        }
        self.fft_fwd();
        self.buf[..self.n / 2 + 1].to_vec()
    }
    fn fft_fwd(&mut self) {
        self.fwd.process(&mut self.buf);
    }
    /// Inverse real FFT to `n` samples: the imaginary parts of DC and Nyquist are ignored.
    #[allow(clippy::needless_range_loop)]
    pub fn irfft(&mut self, spec: &[Complex<f32>]) -> Vec<f32> {
        let n = self.n;
        let half = n / 2;
        for k in 0..=half {
            let mut c = spec[k];
            if k == 0 || (n % 2 == 0 && k == half) {
                c.im = 0.0;
            }
            self.buf[k] = c;
            if k > 0 && k < n - k {
                self.buf[n - k] = c.conj();
            }
        }
        self.inv.process(&mut self.buf);
        let scale = 1.0 / n as f32;
        self.buf.iter().map(|c| c.re * scale).collect()
    }
}

/// Median, the mean of the two middle values for an even count (NaN when any value is NaN).
pub fn median(x: &[f64]) -> f64 {
    if x.is_empty() || x.iter().any(|v| v.is_nan()) {
        return f64::NAN;
    }
    let mut v = x.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// Mean, with a pairwise sum.
pub fn mean(x: &[f64]) -> f64 {
    sum_f64(x.len(), &|i| x[i]) / x.len() as f64
}

/// Population standard deviation (NaN when empty).
pub fn std(x: &[f64]) -> f64 {
    let m = mean(x);
    (sum_f64(x.len(), &|i| (x[i] - m) * (x[i] - m)) / x.len() as f64).sqrt()
}

/// Pearson correlation of `x` and `y`, clamped to [-1, 1]: NaN when either is constant or too short.
pub fn corrcoef(x: &[f64], y: &[f64]) -> f64 {
    let n = x.len().min(y.len());
    if n == 0 {
        return f64::NAN;
    }
    let (mx, my) = (mean(&x[..n]), mean(&y[..n]));
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for i in 0..n {
        let (a, b) = (x[i] - mx, y[i] - my);
        sxy += a * b;
        sxx += a * a;
        syy += b * b;
    }
    let fact = (n as f64 - 1.0).max(0.0);
    let (cxy, cxx, cyy) = (sxy / fact, sxx / fact, syy / fact);
    let r = cxy / cxx.sqrt() / cyy.sqrt();
    if r.is_nan() { r } else { r.clamp(-1.0, 1.0) }
}

/// Convolution of `a` and `v`, cut to the centre `max(len(a), len(v))` values ("same" mode).
pub fn convolve_same(a: &[f64], v: &[f64]) -> Vec<f64> {
    let (a, v) = if v.len() > a.len() { (v, a) } else { (a, v) };
    let (n, m) = (a.len(), v.len());
    if m == 0 {
        return vec![];
    }
    let off = (m - 1) / 2;
    (0..n)
        .map(|i| {
            let k = i + off; // index into the full convolution
            let mut s = 0.0;
            for j in 0..m {
                if k >= j && k - j < n {
                    s += a[k - j] * v[j];
                }
            }
            s
        })
        .collect()
}

/// `x` rounded to `ndigits` decimals: the exact binary value rounded half to even.
pub fn round_even(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{:.*}", ndigits, x).parse().unwrap_or(x)
}

/// `x` rounded half to even, as an integer.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Slice bounds `[start:stop]` on a sequence of `len`, clamped to it (negative counts from the end,
/// and an empty range when stop is before start).
pub fn clamp_slice(len: usize, start: Option<i64>, stop: Option<i64>) -> (usize, usize) {
    let l = len as i64;
    let fix = |v: i64| if v < 0 { (v + l).max(0) } else { v.min(l) };
    let a = start.map(fix).unwrap_or(0);
    let b = stop.map(fix).unwrap_or(l);
    (a as usize, b.max(a) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_and_slices() {
        assert_eq!(round_even(0.125, 2), 0.12);
        assert_eq!(round_even(0.375, 2), 0.38);
        assert_eq!(round_even(2.675, 2), 2.67);   // 2.67499999... in binary
        assert_eq!(round_even(-0.004, 2), -0.0);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(260.00000000000003), 260);
        assert_eq!(clamp_slice(10, Some(3), Some(-2)), (3, 8));
        assert_eq!(clamp_slice(10, Some(12), None), (10, 10));
        assert_eq!(clamp_slice(10, None, Some(-20)), (0, 0));
    }

    #[test]
    fn numeric_helpers() {
        let h = hanning(5);
        assert!((h[2] - 1.0).abs() < 1e-15 && h[0].abs() < 1e-15);
        let g = geomspace(300.0, 3400.0, 25);
        assert_eq!((g[0], g[24]), (300.0, 3400.0));
        assert_eq!(median(&[3.0, 1.0, 2.0, 10.0]), 2.5);
        assert_eq!(convolve_same(&[1.0, 2.0, 3.0], &[1.0, 1.0, 1.0]), vec![3.0, 6.0, 5.0]);
        assert!((corrcoef(&[1.0, 2.0, 3.0], &[2.0, 4.0, 7.0]) - 0.9933992677987828).abs() < 1e-12);
        assert!(corrcoef(&[1.0, 1.0], &[1.0, 2.0]).is_nan());
        // the pairwise float32 sum of i / 3 for i in 1..20, to the last bit
        let x: Vec<f32> = (1..20).map(|i| i as f32 / 3.0).collect();
        assert_eq!(sum_f32(x.len(), &|i| x[i]), 63.333332);
    }
}
