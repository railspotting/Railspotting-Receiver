//! The ITC link layer, from demodulated symbols to checked messages.
//!
//! A frame on the air is, after a short preamble:
//!
//! - a 64-bit unique word, `UW`;
//! - a 24-bit header (length, mode, type and a 3-bit check), sent with a K=7 rate 1/2
//!   convolutional code as 48 bits;
//! - `length` data bytes (the message and its CRC-16/ARC) in RS(16,12) blocks over GF(256),
//!   interleaved byte by byte across the blocks.
//!
//! The layout follows public notes on the protocol; this is our own implementation, checked
//! against recordings. `msg` reads the fields of the message types that are understood.
//!
//! Receive only: nothing here builds a frame for sending, except the test encoders.

use serde::Serialize;

/// The unique word that starts every frame.
pub const UW: u64 = 0xf337_eeb6_3766_0672;
/// Bit errors allowed when looking for the unique word.
pub const UW_MAX_ERRORS: u32 = 8;
/// Largest data length a header may give.
pub const MAX_LENGTH: usize = 192;

/// Symbols from `dsp::demod` (0 = +45, 1 = +135, 2 = -135, 3 = -45 degrees) to bits, two per
/// symbol, Gray coded: 00, 01, 11, 10. A gap ('.') becomes 00.
pub fn symbols_to_bits(symbols: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(symbols.len() * 2);
    for c in symbols.bytes() {
        let (a, b) = match c {
            b'1' => (0, 1),
            b'2' => (1, 1),
            b'3' => (1, 0),
            _ => (0, 0),
        };
        out.push(a);
        out.push(b);
    }
    out
}

/// `symbols_to_bits`, with each symbol's confidence (0 to 255) given to both its bits.
pub fn symbols_to_bits_conf(symbols: &str, conf: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let bits = symbols_to_bits(symbols);
    let c = conf.iter().flat_map(|&x| [x, x]).collect();
    (bits, c)
}

fn bits_value(bits: &[u8]) -> u64 {
    bits.iter().fold(0u64, |v, &b| (v << 1) | b as u64)
}

/// Where the unique word starts in `bits`, from `from` on, allowing a few bit errors.
pub fn find_uw(bits: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 64 <= bits.len() {
        if (bits_value(&bits[i..i + 64]) ^ UW).count_ones() <= UW_MAX_ERRORS {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ---- header -------------------------------------------------------------------------------

const TAP_A: u32 = 0x6D;
const TAP_B: u32 = 0x4F;

fn parity(x: u32) -> u8 {
    (x.count_ones() & 1) as u8
}

/// The header's convolutional code (test use and documentation): the newest bit enters at the
/// bottom of a 7-bit register, two output bits per input bit.
pub fn conv_encode(bits: &[u8], start_state: u32) -> Vec<u8> {
    let mut reg = start_state & 63;
    let mut out = Vec::with_capacity(bits.len() * 2);
    for &u in bits {
        reg = ((reg << 1) | u as u32) & 127;
        out.push(parity(reg & TAP_A));
        out.push(parity(reg & TAP_B));
        reg &= 63;
    }
    out
}

/// Viterbi decode of `n` input bits from `2n` coded bits. The encoder's starting state is not
/// assumed (it carries on from the bits before the header), so every state starts equal.
/// Returns the bits and how many coded bits disagreed with them.
pub fn viterbi(coded: &[u8], n: usize) -> (Vec<u8>, u32) {
    const INF: u32 = u32::MAX / 2;
    let mut metric = [0u32; 64];
    let mut from = vec![[0u8; 64]; n]; // predecessor state, with the input bit in bit 7
    for (i, step) in from.iter_mut().enumerate() {
        let (r0, r1) = (coded[2 * i], coded[2 * i + 1]);
        let mut next = [INF; 64];
        for s in 0..64u32 {
            if metric[s as usize] >= INF {
                continue;
            }
            for u in 0..2u32 {
                let reg = (s << 1) | u;
                let m = metric[s as usize] + (parity(reg & TAP_A) != r0) as u32 + (parity(reg & TAP_B) != r1) as u32;
                let ns = (reg & 63) as usize;
                if m < next[ns] {
                    next[ns] = m;
                    step[ns] = s as u8 | ((u as u8) << 7);
                }
            }
        }
        metric = next;
    }
    let (mut s, &best) = metric.iter().enumerate().min_by_key(|(_, m)| **m).unwrap_or((0, &0));
    let mut out = vec![0u8; n];
    for i in (0..n).rev() {
        let p = from[i][s];
        out[i] = p >> 7;
        s = (p & 63) as usize;
    }
    (out, best)
}

/// The decoded 24-bit header.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Header {
    pub length: usize,
    pub mode: u8,
    pub kind: u8,
    /// The 3-bit check: the 24 bits divide by x^3 + x + 1.
    pub check_ok: bool,
    /// Coded bits that disagreed with the decoded header.
    pub errors: u32,
}

/// Remainder of a 24-bit value divided by x^3 + x + 1.
fn check_remainder(mut v: u32) -> u32 {
    for i in (3..24).rev() {
        if v >> i & 1 == 1 {
            v ^= 0xB << (i - 3);
        }
    }
    v
}

/// The 24-bit header value, check bits included, for a length, mode and type.
pub fn header_value(length: usize, mode: u8, kind: u8) -> u32 {
    let v = ((length as u32) << 15) | ((mode as u32 & 15) << 11) | ((kind as u32) << 3);
    v | check_remainder(v)
}

impl Header {
    pub fn from_value(v: u32, errors: u32) -> Header {
        Header {
            length: (v >> 15) as usize,
            mode: ((v >> 11) & 15) as u8,
            kind: ((v >> 3) & 255) as u8,
            check_ok: check_remainder(v) == 0,
            errors,
        }
    }

    /// A header worth reading the data for: checked, mode 1, a known shape of type, a length
    /// that holds at least the CRC.
    pub fn plausible(&self) -> bool {
        let hi = self.kind >> 4;
        self.check_ok && self.mode == 1 && self.kind != 0 && hi <= 7 && ![1, 2, 4, 6].contains(&hi) && (3..=MAX_LENGTH).contains(&self.length)
    }
}

// ---- RS(16,12) over GF(256) --------------------------------------------------------------

struct Gf {
    exp: [u8; 512],
    log: [u8; 256],
}

impl Gf {
    fn new() -> Gf {
        let mut g = Gf { exp: [0; 512], log: [0; 256] };
        let mut v: u16 = 1;
        for i in 0..255 {
            g.exp[i] = v as u8;
            g.log[v as usize] = i as u8;
            v <<= 1;
            if v & 0x100 != 0 {
                v ^= 0x11d;
            }
        }
        for i in 255..512 {
            g.exp[i] = g.exp[i - 255];
        }
        g
    }
    fn mul(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + self.log[b as usize] as usize]
        }
    }
    fn div(&self, a: u8, b: u8) -> u8 {
        if a == 0 || b == 0 {
            0
        } else {
            self.exp[self.log[a as usize] as usize + 255 - self.log[b as usize] as usize]
        }
    }
    fn pow(&self, e: usize) -> u8 {
        self.exp[e % 255]
    }
}

fn gf() -> &'static Gf {
    static GF: std::sync::OnceLock<Gf> = std::sync::OnceLock::new();
    GF.get_or_init(Gf::new)
}

/// Syndromes at alpha^0..alpha^3; the first byte of a block is the highest power.
fn syndromes(blk: &[u8; 16]) -> [u8; 4] {
    let g = gf();
    let mut s = [0u8; 4];
    for (j, sj) in s.iter_mut().enumerate() {
        for &c in blk {
            *sj = g.mul(*sj, g.pow(j)) ^ c;
        }
    }
    s
}

/// The four parity bytes for 12 data bytes (test use and documentation).
pub fn rs_parity(data: &[u8; 12]) -> [u8; 4] {
    let g = gf();
    // generator (x - a^0)(x - a^1)(x - a^2)(x - a^3), highest power first
    let mut gen = vec![1u8];
    for j in 0..4 {
        let mut next = vec![0u8; gen.len() + 1];
        for (k, &c) in gen.iter().enumerate() {
            next[k] ^= c;
            next[k + 1] ^= g.mul(c, g.pow(j));
        }
        gen = next;
    }
    let mut rem = [0u8; 4];
    for &d in data {
        let f = d ^ rem[0];
        rem.copy_within(1.., 0);
        rem[3] = 0;
        for k in 0..4 {
            rem[k] ^= g.mul(f, gen[k + 1]);
        }
    }
    rem
}

/// Correct up to two byte errors in a block, never in `fixed` positions (the zero padding of a
/// short block, which was not sent). Returns how many bytes were corrected, or None.
fn rs_correct(blk: &mut [u8; 16], fixed: &[usize]) -> Option<usize> {
    let g = gf();
    let s = syndromes(blk);
    if s == [0; 4] {
        return Some(0);
    }
    // error at byte k has locator X = alpha^(15 - k)
    let pos = |x: u8| -> Option<usize> {
        (0..16).find(|&k| g.pow(15 - k) == x).filter(|k| !fixed.contains(k))
    };
    let mut fixes: Vec<(usize, u8)> = vec![];
    // one error: S1 = Y X, S0 = Y
    if s[0] != 0 {
        let x = g.div(s[1], s[0]);
        if g.mul(s[1], x) == s[2] && g.mul(s[2], x) == s[3] {
            if let Some(k) = pos(x) {
                fixes.push((k, s[0]));
            }
        }
    }
    if fixes.is_empty() {
        // two errors: the locator x^2 + l1 x + l2 from S0..S3
        let det = g.mul(s[0], s[2]) ^ g.mul(s[1], s[1]);
        if det == 0 {
            return None;
        }
        let l2 = g.div(g.mul(s[2], s[2]) ^ g.mul(s[1], s[3]), det);
        let l1 = g.div(g.mul(s[0], s[3]) ^ g.mul(s[1], s[2]), det);
        let roots: Vec<u8> = (0..16).map(|k| g.pow(15 - k)).filter(|&x| (g.mul(x, x) ^ g.mul(l1, x) ^ l2) == 0).collect();
        if roots.len() != 2 {
            return None;
        }
        let (x1, x2) = (roots[0], roots[1]);
        let y1 = g.div(s[1] ^ g.mul(s[0], x2), x1 ^ x2);
        let y2 = s[0] ^ y1;
        fixes.push((pos(x1)?, y1));
        fixes.push((pos(x2)?, y2));
    }
    let mut trial = *blk;
    for &(k, y) in &fixes {
        trial[k] ^= y;
    }
    if syndromes(&trial) != [0; 4] {
        return None;
    }
    *blk = trial;
    Some(fixes.len())
}

/// Correct a block whose errors are at `erase` (positions whose values are unknown, from soft
/// decisions): solve the syndrome equations for them. With fewer erasures than parity bytes the
/// spare syndromes must agree, which guards against a wrong guess; the frame's CRC checks the rest.
fn rs_erasures(blk: &mut [u8; 16], erase: &[usize]) -> bool {
    let g = gf();
    let e = erase.len();
    if e == 0 || e > 4 {
        return false;
    }
    let s = syndromes(blk);
    // S_j = sum over k of Y_k X_k^j, with X_k = alpha^(15 - position)
    let x: Vec<usize> = erase.iter().map(|&p| 15 - p).collect();
    let mut a: Vec<Vec<u8>> = (0..e).map(|j| x.iter().map(|&xk| g.pow(xk * j)).collect()).collect();
    let mut rhs: Vec<u8> = s[..e].to_vec();
    // Gaussian elimination over GF(256)
    for col in 0..e {
        let Some(piv) = (col..e).find(|&r| a[r][col] != 0) else { return false };
        a.swap(col, piv);
        rhs.swap(col, piv);
        let inv = g.div(1, a[col][col]);
        for v in a[col].iter_mut() {
            *v = g.mul(*v, inv);
        }
        rhs[col] = g.mul(rhs[col], inv);
        let pivot = a[col].clone();
        for r in 0..e {
            if r != col && a[r][col] != 0 {
                let f = a[r][col];
                for (v, p) in a[r].iter_mut().zip(&pivot) {
                    *v ^= g.mul(f, *p);
                }
                rhs[r] ^= g.mul(f, rhs[col]);
            }
        }
    }
    let y = rhs;
    for (j, &sj) in s.iter().enumerate().skip(e) {
        let v = x.iter().zip(&y).fold(0u8, |acc, (&xk, &yk)| acc ^ g.mul(yk, g.pow(xk * j)));
        if v != sj {
            return false;
        }
    }
    let mut trial = *blk;
    for (&p, &yk) in erase.iter().zip(&y) {
        trial[p] ^= yk;
    }
    if syndromes(&trial) != [0; 4] {
        return false;
    }
    *blk = trial;
    true
}

/// Least reliable positions tried as erasures when a block will not correct...
const ERASE_CANDIDATES: usize = 6;
/// ...in sets of up to this many (four, the most RS(16,12) can fill, only as the four least
/// reliable together: with no spare syndrome to check, the CRC alone would guard it).
const ERASE_MAX_CHECKED: usize = 3;

/// Erasure sets to try for a block, most likely first: the least reliable positions, alone and
/// together.
fn erasure_sets(rel: &[u8; 16], fixed: &[usize]) -> Vec<Vec<usize>> {
    let mut pos: Vec<usize> = (0..16).filter(|p| !fixed.contains(p)).collect();
    pos.sort_by_key(|&p| rel[p]);
    pos.truncate(ERASE_CANDIDATES);
    let mut sets: Vec<Vec<usize>> = vec![];
    for size in 1..=ERASE_MAX_CHECKED.min(pos.len()) {
        let mut idx: Vec<usize> = (0..size).collect();
        loop {
            sets.push(idx.iter().map(|&i| pos[i]).collect());
            // next combination of `size` from pos
            let mut k = size;
            while k > 0 && idx[k - 1] == pos.len() - size + k - 1 {
                k -= 1;
            }
            if k == 0 {
                break;
            }
            idx[k - 1] += 1;
            for m in k..size {
                idx[m] = idx[m - 1] + 1;
            }
        }
    }
    if pos.len() >= 4 {
        sets.push(pos[..4].to_vec());
    }
    sets
}

/// CRC-16/ARC (reflected 0xA001, start 0). Over a message and its CRC it gives 0.
pub fn crc16_arc(data: &[u8]) -> u16 {
    let mut c: u16 = 0;
    for &b in data {
        c ^= b as u16;
        for _ in 0..8 {
            c = if c & 1 == 1 { (c >> 1) ^ 0xA001 } else { c >> 1 };
        }
    }
    c
}

/// RS blocks and bytes on the air for `length` data bytes.
pub fn data_shape(length: usize) -> (usize, usize) {
    let blocks = length.div_ceil(12);
    (blocks, length + 4 * blocks)
}

/// Bytes as sent to `length` data bytes, with RS correction. Returns the data, bytes corrected
/// and whether every block could be checked.
fn deinterleave(sent: &[u8], rel: Option<&[u8]>, length: usize) -> (Vec<u8>, usize, bool) {
    let (nb, ebp) = data_shape(length);
    let mut buf = vec![0u8; 16 * nb];
    // how sure the demodulator was of each byte, where known (255 = sure)
    let mut rbuf = vec![255u8; 16 * nb];
    let mut k = 0;
    for col in 0..16 {
        for blk in 0..nb {
            let dst = col + 16 * blk;
            if dst < ebp {
                buf[dst] = sent[k];
                rbuf[dst] = rel.map_or(255, |r| r[k]);
                k += 1;
            }
        }
    }
    // the short last block's parity follows its data; RS wants it at the end of the block
    let tail: Vec<u8> = buf[ebp - 4..ebp].to_vec();
    buf[ebp - 4..ebp].fill(0);
    buf[16 * nb - 4..].copy_from_slice(&tail);
    let rtail: Vec<u8> = rbuf[ebp - 4..ebp].to_vec();
    rbuf[ebp - 4..ebp].fill(255);
    rbuf[16 * nb - 4..].copy_from_slice(&rtail);
    let last_data = length - 12 * (nb - 1);
    let mut corrected = 0;
    let mut all_ok = true;
    for b in 0..nb {
        let fixed: Vec<usize> = if b == nb - 1 { (last_data..12).collect() } else { vec![] };
        let mut blk = [0u8; 16];
        blk.copy_from_slice(&buf[16 * b..16 * b + 16]);
        let fixed_or_erased = rs_correct(&mut blk, &fixed).or_else(|| {
            // hard decisions failed: try the least reliable bytes as erasures (when known)
            rel?;
            let mut r = [255u8; 16];
            r.copy_from_slice(&rbuf[16 * b..16 * b + 16]);
            erasure_sets(&r, &fixed).into_iter().find(|set| rs_erasures(&mut blk, set)).map(|set| set.len())
        });
        match fixed_or_erased {
            Some(n) => {
                corrected += n;
                buf[16 * b..16 * b + 16].copy_from_slice(&blk);
            }
            None => all_ok = false,
        }
    }
    let data: Vec<u8> = (0..nb).flat_map(|b| buf[16 * b..16 * b + 12].to_vec()).take(length).collect();
    (data, corrected, all_ok)
}

/// The bytes sent for `data` (a message with its CRC): RS blocks, interleaved (test use).
pub fn interleave(data: &[u8]) -> Vec<u8> {
    let length = data.len();
    let (nb, ebp) = data_shape(length);
    let mut buf = vec![0u8; 16 * nb];
    for b in 0..nb {
        let mut d = [0u8; 12];
        let chunk = &data[12 * b..(12 * b + 12).min(length)];
        d[..chunk.len()].copy_from_slice(chunk);
        buf[16 * b..16 * b + 12].copy_from_slice(&d);
        buf[16 * b + 12..16 * b + 16].copy_from_slice(&rs_parity(&d));
    }
    let tail: Vec<u8> = buf[16 * nb - 4..].to_vec();
    buf[ebp - 4..ebp].copy_from_slice(&tail);
    let mut out = Vec::with_capacity(ebp);
    for col in 0..16 {
        for blk in 0..nb {
            let dst = col + 16 * blk;
            if dst < ebp {
                out.push(buf[dst]);
            }
        }
    }
    out
}

// ---- frames -------------------------------------------------------------------------------

/// One decoded frame.
#[derive(Debug, Clone, Serialize)]
pub struct Frame {
    /// Bit where the unique word starts, within the burst.
    pub bit: usize,
    pub uw_errors: u32,
    pub header: Header,
    /// The data bytes after RS correction: message, then CRC (low byte first).
    pub data: Vec<u8>,
    /// Bytes RS corrected.
    pub corrected: usize,
    /// Every RS block checked out.
    pub rs_ok: bool,
    pub crc_ok: bool,
}

impl Frame {
    /// The message without its CRC, and for types above 5 without the 4-byte trailer.
    pub fn payload(&self) -> &[u8] {
        let end = self.data.len().saturating_sub(if self.header.kind > 5 { 6 } else { 2 });
        &self.data[..end]
    }

    /// The 4 bytes before the CRC on types above 5 (thought to be an authentication code).
    pub fn trailer(&self) -> &[u8] {
        if self.header.kind > 5 && self.data.len() >= 6 {
            &self.data[self.data.len() - 6..self.data.len() - 2]
        } else {
            &[]
        }
    }

    /// Bits the frame takes, unique word included.
    pub fn bits(&self) -> usize {
        64 + 48 + 8 * data_shape(self.header.length).1
    }
}

/// Every frame in a burst's bits. Frames that fail the CRC are kept, marked, when their header
/// is sound; the caller decides what to do with them.
pub fn frames(bits: &[u8]) -> Vec<Frame> {
    frames_soft(bits, None)
}

/// `frames`, with how sure the demodulator was of each bit (0 to 255, one per bit): when an RS
/// block will not correct, its least reliable bytes are tried as erasures.
pub fn frames_soft(bits: &[u8], conf: Option<&[u8]>) -> Vec<Frame> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = find_uw(bits, from) {
        let uw_errors = (bits_value(&bits[i..i + 64]) ^ UW).count_ones();
        let h0 = i + 64;
        if h0 + 48 > bits.len() {
            break;
        }
        let (hb, errors) = viterbi(&bits[h0..h0 + 48], 24);
        let header = Header::from_value(bits_value(&hb) as u32, errors);
        let d0 = h0 + 48;
        let (_, ebp) = data_shape(header.length);
        if !header.plausible() || d0 + 8 * ebp > bits.len() {
            from = i + 1;
            continue;
        }
        let sent: Vec<u8> = (0..ebp).map(|k| bits_value(&bits[d0 + 8 * k..d0 + 8 * k + 8]) as u8).collect();
        let rel: Option<Vec<u8>> = conf.map(|c| (0..ebp).map(|k| c[d0 + 8 * k..d0 + 8 * k + 8].iter().copied().min().unwrap_or(0)).collect());
        let (data, corrected, rs_ok) = deinterleave(&sent, rel.as_deref(), header.length);
        let crc_ok = crc16_arc(&data) == 0;
        let f = Frame { bit: i, uw_errors, header, data, corrected, rs_ok, crc_ok };
        from = if crc_ok { i + f.bits() } else { i + 1 };
        out.push(f);
    }
    out
}

/// Bits for a whole frame (unique word, coded header, interleaved data), for tests. `msg` is
/// the message without its CRC.
pub fn encode_frame(kind: u8, msg: &[u8]) -> Vec<u8> {
    let mut data = msg.to_vec();
    let crc = crc16_arc(&data);
    data.extend_from_slice(&crc.to_le_bytes());
    let v = header_value(data.len(), 1, kind);
    let hbits: Vec<u8> = (0..24).rev().map(|i| (v >> i & 1) as u8).collect();
    let mut bits: Vec<u8> = (0..64).rev().map(|i| (UW >> i & 1) as u8).collect();
    bits.extend(conv_encode(&hbits, (UW & 63) as u32));
    for b in interleave(&data) {
        bits.extend((0..8).rev().map(|i| b >> i & 1));
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_check_and_fields() {
        let v = header_value(29, 1, 0x02);
        assert_eq!(v, 0x0E8810); // the documented beacon header
        let h = Header::from_value(v, 0);
        assert!(h.plausible());
        assert_eq!((h.length, h.mode, h.kind), (29, 1, 2));
        assert!(!Header::from_value(v ^ 0x10, 0).check_ok);
    }

    #[test]
    fn viterbi_undoes_the_code_with_errors() {
        let v = header_value(17, 1, 0x34);
        let bits: Vec<u8> = (0..24).rev().map(|i| (v >> i & 1) as u8).collect();
        let mut coded = conv_encode(&bits, 0b101101);
        coded[5] ^= 1;
        coded[30] ^= 1;
        let (back, errors) = viterbi(&coded, 24);
        assert_eq!(back, bits);
        assert_eq!(errors, 2);
    }

    #[test]
    fn erasures_fill_what_errors_cannot() {
        let data: [u8; 12] = *b"ITC 220 MHz!";
        let mut blk = [0u8; 16];
        blk[..12].copy_from_slice(&data);
        blk[12..].copy_from_slice(&rs_parity(&data));
        let good = blk;
        // three wrong bytes: too many to correct, fine as erasures (one spare syndrome checks)
        for p in [1, 7, 13] {
            blk[p] ^= 0xa5;
        }
        assert_eq!(rs_correct(&mut blk.clone(), &[]), None);
        assert!(rs_erasures(&mut blk, &[1, 7, 13]));
        assert_eq!(blk, good);
        // four erasures: as many as there are parity bytes
        let mut four = good;
        for p in [0, 5, 10, 15] {
            four[p] ^= 0x3c;
        }
        assert!(rs_erasures(&mut four, &[0, 5, 10, 15]));
        assert_eq!(four, good);
        // a wrong guess at three is refused by the spare syndrome
        let mut wrong = good;
        for p in [1, 7, 13] {
            wrong[p] ^= 0xa5;
        }
        assert!(!rs_erasures(&mut wrong, &[2, 7, 13]));
    }

    #[test]
    fn soft_decisions_rescue_a_frame() {
        let msg: Vec<u8> = (0..20).collect();
        let bits = encode_frame(0x05, &msg);
        let mut conf = vec![255u8; bits.len()];
        // three damaged bytes in the data, all in the first RS block (bytes 0, 2 and 4 sent are
        // block 0's first three columns), marked unsure
        let d0 = 64 + 48;
        let mut bad = bits.clone();
        for byte in [0, 2, 4] {
            for b in 0..8 {
                bad[d0 + 8 * byte + b] ^= (b % 2) as u8;
                conf[d0 + 8 * byte + b] = 10;
            }
        }
        assert!(!frames(&bad)[0].crc_ok, "hard decisions cannot fix three bytes in a block");
        let f = &frames_soft(&bad, Some(&conf))[0];
        assert!(f.crc_ok && f.rs_ok);
        assert_eq!(f.payload(), msg.as_slice());
    }

    #[test]
    fn crc_arc_check_value() {
        assert_eq!(crc16_arc(b"123456789"), 0xBB3D);
    }

    #[test]
    fn rs_corrects_two_bytes_per_block() {
        let data: [u8; 12] = *b"ITC 220 MHz!";
        let mut blk = [0u8; 16];
        blk[..12].copy_from_slice(&data);
        blk[12..].copy_from_slice(&rs_parity(&data));
        assert_eq!(syndromes(&blk), [0; 4]);
        let mut bad = blk;
        bad[3] ^= 0x5a;
        bad[14] ^= 0x01;
        assert_eq!(rs_correct(&mut bad, &[]), Some(2));
        assert_eq!(bad, blk);
        let mut one = blk;
        one[0] ^= 0xff;
        assert_eq!(rs_correct(&mut one, &[]), Some(1));
        assert_eq!(one, blk);
    }

    #[test]
    fn frames_round_trip_through_symbols_and_noise() {
        // two made-up frames back to back, as in one TDMA burst
        let a = encode_frame(0x34, &[0x12, 0x34, 0x56, 0xbd, 0xef, 0x00, 1, 2, 3, 4]);
        let b = encode_frame(0x02, &(0..27).collect::<Vec<u8>>());
        let mut bits = vec![1, 1, 1, 1, 1, 1, 0, 0];
        bits.extend(&a);
        bits.extend(&b);
        bits.extend([0, 1, 1, 0]);
        // a few bit errors in the data of each frame
        bits[8 + 112 + 3] ^= 1;
        bits[8 + a.len() + 112 + 40] ^= 1;
        bits[8 + a.len() + 112 + 41] ^= 1;
        let f = frames(&bits);
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|f| f.crc_ok && f.rs_ok));
        assert_eq!(f[0].header.kind, 0x34);
        assert_eq!(f[0].payload(), &[0x12, 0x34, 0x56, 0xbd, 0xef, 0x00]);
        assert_eq!(f[0].trailer(), &[1, 2, 3, 4]);
        assert_eq!(f[1].payload(), (0..27).collect::<Vec<u8>>().as_slice());
        assert!(f[0].corrected + f[1].corrected >= 2);
    }

    #[test]
    fn symbols_are_gray_coded() {
        assert_eq!(symbols_to_bits("0123."), vec![0, 0, 0, 1, 1, 1, 1, 0, 0, 0]);
    }
}
