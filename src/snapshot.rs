#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixFmt {
    Xrgb8888,
    Rgb565,
}

impl PixFmt {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            PixFmt::Xrgb8888 => 4,
            PixFmt::Rgb565 => 2,
        }
    }
}

/// A screen image with tightly packed rows (no padding).
#[derive(Clone)]
pub struct Snapshot {
    pub width: u32,
    pub height: u32,
    pub fmt: PixFmt,
    pub data: Vec<u8>,
}

impl Snapshot {
    pub fn row_bytes(&self) -> usize {
        self.width as usize * self.fmt.bytes_per_pixel()
    }

    pub fn row(&self, y: usize) -> &[u8] {
        let rb = self.row_bytes();
        &self.data[y * rb..(y + 1) * rb]
    }

    /// A backgrounded X server may hand back an all-black image instead of
    /// its last frame; treat that as "no usable capture".
    pub fn is_blank(&self) -> bool {
        self.data.iter().step_by(61).all(|&b| b == 0)
    }

    pub fn to_fmt(&self, fmt: PixFmt) -> Snapshot {
        if fmt == self.fmt {
            return self.clone();
        }
        let pixels = self.width as usize * self.height as usize;
        let mut out = Vec::with_capacity(pixels * fmt.bytes_per_pixel());
        match (self.fmt, fmt) {
            (PixFmt::Xrgb8888, PixFmt::Rgb565) => {
                for px in self.data.chunks_exact(4) {
                    let (b, g, r) = (px[0] as u16, px[1] as u16, px[2] as u16);
                    let v = ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3);
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
            (PixFmt::Rgb565, PixFmt::Xrgb8888) => {
                for px in self.data.chunks_exact(2) {
                    let v = u16::from_le_bytes([px[0], px[1]]);
                    let r = ((v >> 11) & 0x1f) as u8;
                    let g = ((v >> 5) & 0x3f) as u8;
                    let b = (v & 0x1f) as u8;
                    out.extend_from_slice(&[
                        (b << 3) | (b >> 2),
                        (g << 2) | (g >> 4),
                        (r << 3) | (r >> 2),
                        0,
                    ]);
                }
            }
            _ => unreachable!(),
        }
        Snapshot { width: self.width, height: self.height, fmt, data: out }
    }
}
