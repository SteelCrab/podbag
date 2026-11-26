//! Bitmap images: decoded once (PNG), registered for the app's lifetime, and drawn as
//! textured quads. The GPU side keeps each image at the size it is drawn, resampled
//! on the CPU and packed into one atlas texture (`gpu`), so it stays sharp at any
//! scale. A tint draws an image as a mask of that color (monochrome icons).

use std::sync::{Arc, Mutex};

/// Straight (not premultiplied) RGBA pixels, rows top to bottom.
#[derive(Clone, PartialEq, Eq)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageData({}x{})", self.width, self.height)
    }
}

impl ImageData {
    /// Decode a PNG of any color type and depth to 8-bit RGBA.
    pub fn from_png(bytes: &[u8]) -> Result<ImageData, String> {
        use png::{ColorType, Transformations};
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder
            .set_transformations(Transformations::normalize_to_color8() | Transformations::ALPHA);
        let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
        let size = reader.output_buffer_size().ok_or("image too large")?;
        let mut buf = vec![0; size];
        let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        buf.truncate(info.buffer_size());
        let rgba = match info.color_type {
            ColorType::Rgba => buf,
            ColorType::GrayscaleAlpha => buf
                .chunks_exact(2)
                .flat_map(|ga| [ga[0], ga[0], ga[0], ga[1]])
                .collect(),
            other => return Err(format!("unexpected PNG color type {other:?}")),
        };
        ImageData::new(info.width, info.height, rgba)
    }

    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Result<ImageData, String> {
        if width == 0 || height == 0 || rgba.len() != (width * height * 4) as usize {
            return Err(format!("bad image: {width}x{height}, {} bytes", rgba.len()));
        }
        Ok(ImageData {
            width,
            height,
            rgba,
        })
    }

    /// Premultiplied RGBA at `w` x `h`: an area average when smaller, the pixels as
    /// they are at the image's own size or larger (the GPU scales those up).
    pub fn resampled(&self, w: u32, h: u32) -> (u32, u32, Vec<u8>) {
        let (sw, sh) = (self.width, self.height);
        if w >= sw && h >= sh {
            let mut out = self.rgba.clone();
            for px in out.chunks_exact_mut(4) {
                premultiply(px);
            }
            return (sw, sh, out);
        }
        let mut out = vec![0u8; (w * h * 4) as usize];
        let (fx, fy) = (sw as f32 / w as f32, sh as f32 / h as f32);
        for y in 0..h {
            let (y0, y1) = (y as f32 * fy, (y + 1) as f32 * fy);
            for x in 0..w {
                let (x0, x1) = (x as f32 * fx, (x + 1) as f32 * fx);
                let mut acc = [0f32; 4];
                let mut area = 0f32;
                let mut sy = y0.floor() as u32;
                while (sy as f32) < y1 && sy < sh {
                    let cy = (y1.min(sy as f32 + 1.) - y0.max(sy as f32)).max(0.);
                    let mut sx = x0.floor() as u32;
                    while (sx as f32) < x1 && sx < sw {
                        let cx = (x1.min(sx as f32 + 1.) - x0.max(sx as f32)).max(0.);
                        let i = ((sy * sw + sx) * 4) as usize;
                        let p = &self.rgba[i..i + 4];
                        let a = p[3] as f32 / 255.;
                        let wgt = cx * cy;
                        acc[0] += p[0] as f32 * a * wgt;
                        acc[1] += p[1] as f32 * a * wgt;
                        acc[2] += p[2] as f32 * a * wgt;
                        acc[3] += p[3] as f32 * wgt;
                        area += wgt;
                        sx += 1;
                    }
                    sy += 1;
                }
                let o = ((y * w + x) * 4) as usize;
                for c in 0..4 {
                    out[o + c] = (acc[c] / area.max(f32::EPSILON)).round().clamp(0., 255.) as u8;
                }
            }
        }
        (w, h, out)
    }
}

fn premultiply(px: &mut [u8]) {
    let a = px[3] as u16;
    for c in &mut px[..3] {
        *c = ((*c as u16 * a + 127) / 255) as u8;
    }
}

/// A registered image. Cheap to copy; valid for the life of the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Image(pub(crate) u32);

static IMAGES: Mutex<Vec<Arc<ImageData>>> = Mutex::new(Vec::new());

impl Image {
    /// Keep `data` for drawing.
    pub fn register(data: ImageData) -> Image {
        let mut all = IMAGES.lock().unwrap_or_else(|p| p.into_inner());
        all.push(Arc::new(data));
        Image(all.len() as u32 - 1)
    }

    /// Decode and register a PNG.
    pub fn png(bytes: &[u8]) -> Result<Image, String> {
        ImageData::from_png(bytes).map(Image::register)
    }

    pub fn data(self) -> Arc<ImageData> {
        let all = IMAGES.lock().unwrap_or_else(|p| p.into_inner());
        all[self.0 as usize].clone()
    }

    /// Width and height in pixels.
    pub fn size(self) -> (u32, u32) {
        let d = self.data();
        (d.width, d.height)
    }
}

/// Places rectangles in a fixed-size texture, in rows ("shelves") of similar height.
/// Nothing is freed one by one: when it is full, the user clears it and packs again.
#[derive(Debug, Clone)]
pub struct ShelfPacker {
    width: u32,
    height: u32,
    /// (top, height, next free x)
    shelves: Vec<(u32, u32, u32)>,
    bottom: u32,
}

impl ShelfPacker {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            shelves: Vec::new(),
            bottom: 0,
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The top-left corner of a free `w` x `h` area, or None when full.
    pub fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w > self.width || h > self.height {
            return None;
        }
        // The lowest shelf that fits it and wastes at most a quarter of its height.
        let best = self
            .shelves
            .iter_mut()
            .filter(|(_, sh, x)| *sh >= h && *sh - h <= *sh / 4 && *x + w <= self.width)
            .min_by_key(|(_, sh, _)| *sh);
        if let Some((top, _, x)) = best {
            let at = (*x, *top);
            *x += w;
            return Some(at);
        }
        if self.bottom + h > self.height {
            return None;
        }
        let top = self.bottom;
        self.shelves.push((top, h, w));
        self.bottom += h;
        Some((0, top))
    }

    pub fn clear(&mut self) {
        self.shelves.clear();
        self.bottom = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2 x 1 RGBA PNG: opaque red, then half-transparent white (made with zlib's
    /// stored blocks, so no encoder is needed).
    fn tiny_png() -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8], data: &[u8]) {
            out.extend((data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend(data);
            out.extend(&body);
            out.extend(crc32(&body).to_be_bytes());
        }
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for b in data {
                crc ^= *b as u32;
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
            !crc
        }
        fn adler32(data: &[u8]) -> u32 {
            let (mut a, mut b) = (1u32, 0u32);
            for d in data {
                a = (a + *d as u32) % 65521;
                b = (b + a) % 65521;
            }
            (b << 16) | a
        }
        let raw = [0u8, 255, 0, 0, 255, 255, 255, 255, 128];
        let mut z = vec![0x78, 0x01, 1];
        z.extend((raw.len() as u16).to_le_bytes());
        z.extend((!(raw.len() as u16)).to_le_bytes());
        z.extend(raw);
        z.extend(adler32(&raw).to_be_bytes());
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend(2u32.to_be_bytes());
        ihdr.extend(1u32.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &z);
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn decodes_rgba_png() {
        let img = ImageData::from_png(&tiny_png()).unwrap();
        assert_eq!((img.width, img.height), (2, 1));
        assert_eq!(img.rgba, [255, 0, 0, 255, 255, 255, 255, 128]);
    }

    #[test]
    fn rejects_what_is_not_a_png() {
        assert!(ImageData::from_png(b"GIF89a").is_err());
        assert!(ImageData::new(2, 2, vec![0; 3]).is_err());
    }

    #[test]
    fn resampling_premultiplies_and_averages() {
        let img = ImageData::from_png(&tiny_png()).unwrap();
        // Own size: premultiplied only.
        let (w, h, px) = img.resampled(2, 1);
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, [255, 0, 0, 255, 128, 128, 128, 128]);
        // Half the width: the average of both, premultiplied.
        let (w, h, px) = img.resampled(1, 1);
        assert_eq!((w, h), (1, 1));
        assert_eq!(px, [192, 64, 64, 192]);
    }

    #[test]
    fn registered_images_keep_their_size() {
        let a = Image::register(ImageData::new(3, 2, vec![0; 24]).unwrap());
        let b = Image::png(&tiny_png()).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.size(), (3, 2));
        assert_eq!(b.size(), (2, 1));
    }

    #[test]
    fn packer_fills_shelves_then_says_full() {
        let mut p = ShelfPacker::new(100, 50);
        assert_eq!(p.alloc(40, 20), Some((0, 0)));
        assert_eq!(p.alloc(40, 18), Some((40, 0)));
        // Too wide for the first shelf: a new one below.
        assert_eq!(p.alloc(40, 20), Some((0, 20)));
        // Much smaller than the shelves: its own shelf.
        assert_eq!(p.alloc(10, 5), Some((0, 40)));
        assert_eq!(p.alloc(10, 5), Some((10, 40)));
        // No room left.
        assert_eq!(p.alloc(30, 30), None);
        assert_eq!(p.alloc(200, 1), None);
        p.clear();
        assert_eq!(p.alloc(30, 30), Some((0, 0)));
    }

    #[test]
    fn packed_areas_never_overlap() {
        let mut p = ShelfPacker::new(256, 256);
        let mut placed: Vec<(u32, u32, u32, u32)> = Vec::new();
        for i in 0..200u32 {
            let (w, h) = (8 + i * 7 % 40, 8 + i * 13 % 30);
            let Some((x, y)) = p.alloc(w, h) else {
                continue;
            };
            assert!(x + w <= 256 && y + h <= 256);
            for &(ox, oy, ow, oh) in &placed {
                let apart = x >= ox + ow || ox >= x + w || y >= oy + oh || oy >= y + h;
                assert!(apart, "({x},{y},{w},{h}) overlaps ({ox},{oy},{ow},{oh})");
            }
            placed.push((x, y, w, h));
        }
        assert!(placed.len() > 40);
    }
}
