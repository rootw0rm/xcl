//! Texture read-backs as pictures: the texels of the common DXGI formats as numbers, an RGB
//! image of them a person can look at (UNORM formats as they are; float and integer ones
//! scaled by their largest value and brightened; the slices of a 3D texture side by side),
//! and that image as a PNG file. For diagnostics that put what a draw wrote next to its
//! numbers (`xcl replay`).

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Unorm,
    Float,
    Uint,
}

/// (bytes of a texel, channels, kind, bits of a channel; 10 and 11 are the packed formats)
fn layout(format: i32) -> Option<(usize, usize, Kind, u32)> {
    Some(match format {
        1 | 2 => (16, 4, Kind::Float, 32),
        3 => (16, 4, Kind::Uint, 32),
        9 | 10 => (8, 4, Kind::Float, 16),
        11 => (8, 4, Kind::Unorm, 16),
        12 => (8, 4, Kind::Uint, 16),
        15 | 16 => (8, 2, Kind::Float, 32),
        17 => (8, 2, Kind::Uint, 32),
        // D32_FLOAT_S8X24 and its views: the depth plane
        19..=21 => (4, 1, Kind::Float, 32),
        23 | 24 => (4, 4, Kind::Unorm, 10),
        25 => (4, 4, Kind::Uint, 10),
        26 => (4, 3, Kind::Float, 11),
        27..=29 | 87 | 88 | 90 | 91 => (4, 4, Kind::Unorm, 8),
        30 => (4, 4, Kind::Uint, 8),
        33 | 34 => (4, 2, Kind::Float, 16),
        35 => (4, 2, Kind::Unorm, 16),
        36 => (4, 2, Kind::Uint, 16),
        39..=41 => (4, 1, Kind::Float, 32),
        42 => (4, 1, Kind::Uint, 32),
        // D24S8 and its views: 24 bits of depth
        44..=46 => (4, 1, Kind::Unorm, 24),
        48 | 49 => (2, 2, Kind::Unorm, 8),
        50 => (2, 2, Kind::Uint, 8),
        54 => (2, 1, Kind::Float, 16),
        53 | 55 | 56 => (2, 1, Kind::Unorm, 16),
        57 => (2, 1, Kind::Uint, 16),
        60 | 61 | 65 => (1, 1, Kind::Unorm, 8),
        62 => (1, 1, Kind::Uint, 8),
        _ => return None,
    })
}

fn half(h: u32) -> f64 {
    let (s, e, f) = (h >> 15 & 1, h >> 10 & 31, h & 0x3FF);
    let v = match e {
        0 => f as f64 * 2f64.powi(-24),
        31 => {
            if f == 0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => (1.0 + f as f64 / 1024.0) * 2f64.powi(e as i32 - 15),
    };
    if s != 0 { -v } else { v }
}

/// an unsigned float of `m` mantissa bits and 5 exponent bits (R11G11B10_FLOAT)
fn small_float(v: u32, m: u32) -> f64 {
    let (e, f) = (v >> m, v & ((1 << m) - 1));
    match e {
        0 => f as f64 / (1u32 << m) as f64 * 2f64.powi(-14),
        31 => {
            if f == 0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        _ => (1.0 + f as f64 / (1u32 << m) as f64) * 2f64.powi(e as i32 - 15),
    }
}

fn texel(p: &[u8], channels: usize, kind: Kind, bits: u32) -> [f64; 4] {
    let mut c = [0.0; 4];
    let u32_at = |k: usize| u32::from_le_bytes(p[4 * k..4 * k + 4].try_into().unwrap());
    let u16_at = |k: usize| u16::from_le_bytes(p[2 * k..2 * k + 2].try_into().unwrap()) as u32;
    for (k, c) in c.iter_mut().enumerate().take(channels) {
        *c = match (bits, kind) {
            (32, Kind::Float) => f32::from_bits(u32_at(k)) as f64,
            (32, _) => u32_at(k) as f64,
            (16, Kind::Float) => half(u16_at(k)),
            (16, Kind::Unorm) => u16_at(k) as f64 / 65535.0,
            (16, Kind::Uint) => u16_at(k) as f64,
            (8, Kind::Unorm) => p[k] as f64 / 255.0,
            (8, _) => p[k] as f64,
            (24, _) => (u32_at(0) & 0xFF_FFFF) as f64 / 16_777_215.0,
            (10, kind) => {
                let v = if k < 3 { u32_at(0) >> (10 * k) & 0x3FF } else { u32_at(0) >> 30 };
                if kind == Kind::Unorm { v as f64 / if k < 3 { 1023.0 } else { 3.0 } } else { v as f64 }
            }
            (11, _) => match k {
                0 => small_float(u32_at(0) & 0x7FF, 6),
                1 => small_float(u32_at(0) >> 11 & 0x7FF, 6),
                _ => small_float(u32_at(0) >> 22, 5),
            },
            _ => 0.0,
        };
    }
    c
}

/// A read-back as a picture, and what its texels were.
pub struct Image {
    /// 8-bit RGB, `width` x `height`
    pub rgb: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// the least and the largest finite value of each channel
    pub min: Vec<f64>,
    pub max: Vec<f64>,
    /// texels with a bit set
    pub nonzero: usize,
}

/// `data` (rows of `row_bytes` bytes without padding, `depth` slices of `h` rows) in the DXGI
/// format `format` as an RGB image (None: a format this does not read): UNORM formats as they are, float and
/// integer ones scaled by the largest finite value and brightened (square root); the slices of
/// a 3D texture side by side, 16 a row.
pub fn image(format: i32, w: usize, h: usize, depth: usize, row_bytes: usize, data: &[u8]) -> Option<Image> {
    let (bytes, channels, kind, bits) = layout(format)?;
    if w == 0 || h == 0 || row_bytes < w * bytes || data.len() < row_bytes * h * depth {
        return None;
    }
    let bgra = matches!(format, 87 | 88 | 90 | 91);
    let n = channels.min(3);
    let (mut min, mut max) = (vec![f64::INFINITY; channels], vec![f64::NEG_INFINITY; channels]);
    let mut nonzero = 0;
    let mut texels = Vec::with_capacity(w * h * depth);
    for y in 0..h * depth {
        for x in 0..w {
            let at = y * row_bytes + x * bytes;
            let p = &data[at..at + bytes];
            nonzero += p.iter().any(|&b| b != 0) as usize;
            let c = texel(p, channels, kind, bits);
            for k in 0..channels {
                if c[k].is_finite() {
                    min[k] = min[k].min(c[k]);
                    max[k] = max[k].max(c[k]);
                }
            }
            texels.push(c);
        }
    }
    let top = max[..n].iter().copied().filter(|v| v.is_finite()).fold(0.0f64, f64::max);
    let scale = |v: f64| -> u8 {
        if !v.is_finite() {
            return 255;
        }
        let x = if kind == Kind::Unorm { v } else if top > 0.0 { (v.max(0.0) / top).sqrt() } else { 0.0 };
        (x.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
    };
    let cols = if depth > 1 { depth.min(16) } else { 1 };
    let grid_rows = depth.div_ceil(cols);
    let (width, height) = (w * cols, h * grid_rows);
    let mut rgb = vec![0u8; 3 * width * height];
    for z in 0..depth {
        let (ox, oy) = ((z % cols) * w, (z / cols) * h);
        for y in 0..h {
            for x in 0..w {
                let c = texels[(z * h + y) * w + x];
                let px = if n == 1 { [c[0], c[0], c[0]] } else if bgra { [c[2], c[1], c[0]] } else { [c[0], c[1], if n > 2 { c[2] } else { 0.0 }] };
                let at = 3 * ((oy + y) * width + ox + x);
                rgb[at] = scale(px[0]);
                rgb[at + 1] = scale(px[1]);
                rgb[at + 2] = scale(px[2]);
            }
        }
    }
    Some(Image { rgb, width, height, min, max, nonzero })
}

fn crc32(bytes: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        std::array::from_fn(|i| (0..8).fold(i as u32, |c, _| if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 }))
    });
    !bytes.iter().fold(!0u32, |c, &b| t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8))
}

/// An 8-bit RGB PNG.
pub fn encode_png(rgb: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity((3 * width + 1) * height);
    for row in rgb.chunks_exact(3 * width) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let at = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[at..]);
        out.extend_from_slice(&crc.to_be_bytes());
    };
    let mut head = Vec::new();
    head.extend_from_slice(&(width as u32).to_be_bytes());
    head.extend_from_slice(&(height as u32).to_be_bytes());
    head.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &head);
    chunk(b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 3));
    chunk(b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_png_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    /// The PNG has the chunks a reader wants and its data inflates to the rows.
    #[test]
    fn png_holds_the_rows() {
        let rgb: Vec<u8> = (0..2 * 3 * 3).map(|i| i as u8).collect();
        let png = encode_png(&rgb, 3, 2);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(u32::from_be_bytes(png[16..20].try_into().unwrap()), 3);
        assert_eq!(u32::from_be_bytes(png[20..24].try_into().unwrap()), 2);
        let idat = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes(png[idat - 4..idat].try_into().unwrap()) as usize;
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&png[idat + 4..idat + 4 + len]).unwrap();
        assert_eq!(raw.len(), 2 * (1 + 9));
        assert_eq!(&raw[1..10], &rgb[..9]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn floats_and_integers_are_scaled_and_3d_slices_tiled() {
        // R32_UINT 2x1x2: slice 0 = (0, 4), slice 1 = (1, 0)
        let data: Vec<u8> = [0u32, 4, 1, 0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let img = image(42, 2, 1, 2, 8, &data).unwrap();
        assert_eq!((img.width, img.height), (4, 1));
        assert_eq!(img.nonzero, 2);
        assert_eq!(img.max[0], 4.0);
        // 4 of 4 is white, 1 of 4 is half (square root), 0 black; grey
        assert_eq!(&img.rgb[..], &[0, 0, 0, 255, 255, 255, 128, 128, 128, 0, 0, 0]);
        // R8G8B8A8_UNORM as it is; B8G8R8A8 swapped
        assert_eq!(image(28, 1, 1, 1, 4, &[10, 20, 30, 255]).unwrap().rgb, vec![10, 20, 30]);
        assert_eq!(image(87, 1, 1, 1, 4, &[10, 20, 30, 255]).unwrap().rgb, vec![30, 20, 10]);
        assert!(image(999, 1, 1, 1, 4, &[0; 4]).is_none());
        assert_eq!(half(0x3C00), 1.0);
        assert_eq!(half(0xC000), -2.0);
    }
}
