//! `.npy` files of float32 matrices (format version 1.0, little endian, C order), written byte
//! for byte as the existing template files are.

use std::path::Path;

/// A float32 matrix in C order: `rows` x `cols` (a 1D array is `n` x 1 with `ndim` 1).
#[derive(Debug, Clone, PartialEq)]
pub struct Npy {
    pub shape: Vec<usize>,
    pub data: Vec<f32>,
}

/// The format's magic string: 0x93 then the format name in capitals.
const MAGIC: &[u8] = &[0x93, b'N', b'U', b'M', b'P', b'Y'];

/// The `.npy` file bytes of a float32 array of this shape.
pub fn to_bytes(shape: &[usize], data: &[f32]) -> Vec<u8> {
    let shape_txt = match shape.len() {
        1 => format!("({},)", shape[0]),
        _ => format!("({})", shape.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")),
    };
    let mut header = format!("{{'descr': '<f4', 'fortran_order': False, 'shape': {shape_txt}, }}");
    // the header leaves room for the first axis to grow to 21 digits, then aligns to 64 bytes
    let first = shape.first().map(|s| s.to_string().len()).unwrap_or(0);
    header.push_str(&" ".repeat(21usize.saturating_sub(first)));
    let total = MAGIC.len() + 2 + 2 + header.len() + 1;
    header.push_str(&" ".repeat((64 - total % 64) % 64));
    header.push('\n');
    let mut out = Vec::with_capacity(10 + header.len() + data.len() * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&[1, 0]);
    out.extend_from_slice(&(header.len() as u16).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    for v in data {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

pub fn save(path: &Path, shape: &[usize], data: &[f32]) -> std::io::Result<()> {
    std::fs::write(path, to_bytes(shape, data))
}

fn header_value<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let k = format!("'{key}':");
    let at = header.find(&k)? + k.len();
    Some(header[at..].trim_start())
}

/// Parse `.npy` bytes: float32 (or float64, converted), C order, any number of dimensions.
pub fn from_bytes(b: &[u8]) -> Result<Npy, String> {
    if b.len() < 10 || &b[..6] != MAGIC {
        return Err("not a .npy file".into());
    }
    let (hlen, start) = match b[6] {
        1 => (u16::from_le_bytes([b[8], b[9]]) as usize, 10),
        2 | 3 if b.len() >= 12 => (u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize, 12),
        v => return Err(format!("unsupported .npy version {v}")),
    };
    let header = b.get(start..start + hlen).ok_or("truncated .npy header")?;
    let header = std::str::from_utf8(header).map_err(|_| "bad .npy header")?;
    let descr = header_value(header, "descr").ok_or("no descr in .npy header")?;
    // '<' little endian, '|' or '=' native (little endian on every platform this runs on)
    let (width, big) = if ["'<f4'", "'|f4'", "'=f4'"].iter().any(|d| descr.starts_with(d)) {
        (4, false)
    } else if ["'<f8'", "'|f8'", "'=f8'"].iter().any(|d| descr.starts_with(d)) {
        (8, false)
    } else if descr.starts_with("'>f4'") {
        (4, true)
    } else if descr.starts_with("'>f8'") {
        (8, true)
    } else {
        return Err(format!("unsupported dtype {}", descr.split(',').next().unwrap_or("")));
    };
    let fortran = header_value(header, "fortran_order").ok_or("no fortran_order in .npy header")?;
    if fortran.starts_with("True") {
        return Err("fortran order .npy not supported".into());
    }
    let shape_txt = header_value(header, "shape").ok_or("no shape in .npy header")?;
    let inner = shape_txt.strip_prefix('(').and_then(|s| s.split(')').next()).ok_or("bad .npy shape")?;
    let shape = inner
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<usize>().map_err(|_| format!("bad .npy shape {inner}")))
        .collect::<Result<Vec<_>, _>>()?;
    let body = &b[start + hlen..];
    let bytes = shape.iter().try_fold(width, |n: usize, s| n.checked_mul(*s));
    let Some(bytes) = bytes.filter(|n| *n <= body.len()) else {
        return Err(format!("cannot reshape array of size {} into shape {shape:?}", body.len() / width));
    };
    let data = if width == 4 {
        body[..bytes]
            .chunks_exact(4)
            .map(|c| {
                let a = [c[0], c[1], c[2], c[3]];
                if big { f32::from_be_bytes(a) } else { f32::from_le_bytes(a) }
            })
            .collect()
    } else {
        body[..bytes]
            .chunks_exact(8)
            .map(|c| {
                let a = [c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]];
                (if big { f64::from_be_bytes(a) } else { f64::from_le_bytes(a) }) as f32
            })
            .collect()
    };
    Ok(Npy { shape, data })
}

pub fn load(path: &Path) -> Result<Npy, String> {
    let b = std::fs::read(path).map_err(|e| e.to_string())?;
    from_bytes(&b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_reference_bytes() {
        // a float32 array of 3 zeros, byte for byte as a reference file has it
        let b = to_bytes(&[3], &[0.0; 3]);
        let want = [&[0x93u8, b'N', b'U', b'M', b'P', b'Y', 1, 0, b'v', 0][..],
                    format!("{{'descr': '<f4', 'fortran_order': False, 'shape': (3,), }}{}\n", " ".repeat(60)).as_bytes()]
            .concat();
        assert_eq!(&b[..128], &want[..]);
        assert_eq!(b.len(), 128 + 12);
        let back = from_bytes(&to_bytes(&[2, 3], &[1.0, 2.0, 3.0, 4.0, 5.0, 6.5])).unwrap();
        assert_eq!(back.shape, vec![2, 3]);
        assert_eq!(back.data[5], 6.5);
        assert!(from_bytes(b"nope").is_err());
        // a shape too big to exist is an error, not an overflow
        let mut huge = to_bytes(&[2, 3], &[0.0; 6]);
        let at = huge.windows(6).position(|w| w == b"(2, 3)").unwrap();
        huge.splice(at..at + 6, b"(99999999999, 99999999999)".iter().copied());
        assert!(from_bytes(&huge).is_err());
    }
}
