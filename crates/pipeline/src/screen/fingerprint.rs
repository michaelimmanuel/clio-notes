//! A tiny grayscale thumbnail of a frame, for cheap change detection.

use image::RgbaImage;

pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 72;
pub const CELLS: usize = WIDTH * HEIGHT;

/// A cell counts as changed when its brightness moves by more than this
/// (0–255). Video compression noise and anti-aliasing stay below it; a line of
/// new text in the cell goes above it.
const CELL_THRESHOLD: u8 = 16;

/// The average brightness of each cell of a 128 × 72 grid laid over the frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Fingerprint {
    cells: Vec<u8>,
}

impl Fingerprint {
    pub fn of(image: &RgbaImage) -> Self {
        let (w, h) = (image.width() as usize, image.height() as usize);
        let mut sums = vec![0u32; CELLS];
        let mut counts = vec![0u32; CELLS];
        if w > 0 && h > 0 {
            let col: Vec<usize> = (0..w).map(|x| x * WIDTH / w).collect();
            let raw = image.as_raw();
            for y in 0..h {
                let row = (y * HEIGHT / h) * WIDTH;
                let pixels = &raw[y * w * 4..(y + 1) * w * 4];
                for (x, p) in pixels.as_chunks::<4>().0.iter().enumerate() {
                    let luma = (p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29) >> 8;
                    let c = row + col[x];
                    sums[c] += luma;
                    counts[c] += 1;
                }
            }
        }
        let cells = sums
            .iter()
            .zip(&counts)
            .map(|(s, n)| if *n == 0 { 0 } else { (s / n) as u8 })
            .collect();
        Self { cells }
    }

    #[cfg(test)]
    pub(crate) fn from_cells(cells: Vec<u8>) -> Self {
        assert_eq!(cells.len(), CELLS);
        Self { cells }
    }

    #[cfg(test)]
    pub(crate) fn cells(&self) -> &[u8] {
        &self.cells
    }

    /// Which cells differ noticeably from `other`.
    pub fn changed(&self, other: &Fingerprint) -> Vec<bool> {
        self.cells
            .iter()
            .zip(&other.cells)
            .map(|(a, b)| a.abs_diff(*b) > CELL_THRESHOLD)
            .collect()
    }

    /// The share of cells (0–1) that differ noticeably from `other`.
    pub fn diff(&self, other: &Fingerprint) -> f32 {
        fraction(self.changed(other).iter().filter(|c| **c).count())
    }
}

pub fn fraction(cells: usize) -> f32 {
    cells as f32 / CELLS as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn small_changes_stay_small() {
        let mut a = RgbaImage::from_pixel(1920, 1080, Rgba([255, 255, 255, 255]));
        let blank = Fingerprint::of(&a);
        assert_eq!(blank.diff(&blank), 0.0);

        // A mouse pointer: tiny.
        for y in 500..532 {
            for x in 900..920 {
                a.put_pixel(x, y, Rgba([0, 0, 0, 255]));
            }
        }
        let pointer = Fingerprint::of(&a);
        assert!(pointer.diff(&blank) < 0.002, "{}", pointer.diff(&blank));

        // A new line of text: stripes 2 px thick across 800 × 30 px.
        for y in (600..630).step_by(4) {
            for x in 200..1000 {
                for dy in 0..2 {
                    a.put_pixel(x, y + dy, Rgba([0, 0, 0, 255]));
                }
            }
        }
        let text = Fingerprint::of(&a);
        assert!(text.diff(&pointer) > 0.008, "{}", text.diff(&pointer));
    }

    #[test]
    fn works_for_any_frame_size() {
        let small = RgbaImage::from_pixel(50, 30, Rgba([10, 10, 10, 255]));
        let odd = RgbaImage::from_pixel(1366, 769, Rgba([10, 10, 10, 255]));
        assert_eq!(Fingerprint::of(&small).cells.len(), CELLS);
        assert_eq!(Fingerprint::of(&odd).diff(&Fingerprint::of(&odd)), 0.0);
    }
}
