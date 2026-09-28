//! Image conversion for printer-bound jobs.

use crate::{Raster, Result, error::invalid};
use image::{DynamicImage, GenericImageView, GrayImage, Luma};

/// Horizontal image position within the printable width.
///
/// Available with the `image` feature. Unused columns stay white.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    /// Align the image with the left edge.
    Left,
    /// Align the image with the right edge.
    Right,
    /// Center the image, with any extra white column on the right.
    #[default]
    Center,
}

/// Convert and position a bounded image as packed monochrome pixels.
pub fn pack(
    image: &DynamicImage,
    width: u32,
    position: Position,
    dither: Dither,
) -> Result<Vec<u8>> {
    if image.width() == 0 || image.height() == 0 {
        return Err(invalid("The source image is empty."));
    }
    if image.width() > width {
        return Err(invalid(
            "The image exceeds the printable width. Resize it before preparation.",
        ));
    }
    Raster::validate_dimensions(width, image.height())?;

    // Flatten on white before dithering. Work only within the image so that
    // diffusion cannot put black dots in the margins or depend on its position.
    let mut gray = GrayImage::from_fn(image.width(), image.height(), |x, y| {
        let [r, g, b, a] = image.get_pixel(x, y).0.map(u32::from);
        // ITU-R BT.709 luma weights (0.2126, 0.7152, 0.0722), scaled by 10000.
        let luma = (2126 * r + 7152 * g + 722 * b + 5000) / 10000;
        let value = (luma * a + 255 * (255 - a) + 127) / 255;
        Luma([value.to_le_bytes()[0]])
    });
    dither.apply(&mut gray);

    let left = match position {
        Position::Left => 0,
        Position::Right => width - image.width(),
        Position::Center => (width - image.width()) / 2,
    };
    let row_bytes = (width / 8) as usize;
    let mut bytes = vec![0; row_bytes * image.height() as usize];
    for (x, y, pixel) in gray.enumerate_pixels() {
        if pixel[0] == 0 {
            let x = x + left;
            bytes[y as usize * row_bytes + x as usize / 8] |= 1 << (x % 8);
        }
    }
    Ok(bytes)
}

/// A method for converting gray pixels to black and white dots.
///
/// Available with the `image` feature. Pass this to
/// [`Printer::prepare_from_image`](crate::Printer::prepare_from_image), or use
/// [`Self::apply`] to convert an image before saving it.
/// Dithering represents shades with patterns of dots. Use [`Self::Threshold`]
/// for an image that is already black and white.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Dither {
    /// Ostromoukhov dithering, the default. Adjusts dot patterns to the pixel brightness.
    #[default]
    Ostromoukhov,
    /// Stucki dithering. Spreads brightness differences across more neighboring pixels.
    Stucki,
    /// Floyd–Steinberg dithering. Spreads brightness differences to nearby pixels.
    FloydSteinberg,
    /// Values below 128 become black. All other values become white.
    Threshold,
}

impl Dither {
    /// Convert an eight-bit grayscale image to black and white in place.
    ///
    /// Zero is black and 255 is white. The image keeps its dimensions.
    /// Empty images are unchanged. Available with the `image` feature.
    pub fn apply(self, image: &mut GrayImage) {
        if image.width() == 0 || image.height() == 0 {
            return;
        }
        if self == Self::Threshold {
            for pixel in image.pixels_mut() {
                pixel[0] = if pixel[0] < 128 { 0 } else { 255 };
            }
            return;
        }
        let width = image.width() as usize;
        let height = image.height() as usize;
        // Only three rows of error are needed, regardless of image height.
        let mut errors = vec![vec![0.0_f32; width]; 3];
        let (kernel, divisor): (&[(isize, usize, f32)], f32) = match self {
            Self::Stucki => (
                &[
                    (1, 0, 8.0),
                    (2, 0, 4.0),
                    (-2, 1, 2.0),
                    (-1, 1, 4.0),
                    (0, 1, 8.0),
                    (1, 1, 4.0),
                    (2, 1, 2.0),
                    (-2, 2, 1.0),
                    (-1, 2, 2.0),
                    (0, 2, 4.0),
                    (1, 2, 2.0),
                    (2, 2, 1.0),
                ],
                42.0,
            ),
            Self::FloydSteinberg => (&[(1, 0, 7.0), (-1, 1, 3.0), (0, 1, 5.0), (1, 1, 1.0)], 16.0),
            Self::Threshold | Self::Ostromoukhov => (&[], 1.0),
        };
        for y in 0..height {
            let direction = if y % 2 == 0 { 1 } else { -1 };
            for step in 0..width {
                let x = if direction == 1 {
                    step
                } else {
                    width - 1 - step
                };
                let input = image.as_raw()[y * width + x];
                let value = f32::from(input) + errors[0][x];
                let black = value < 127.5;
                image.as_mut()[y * width + x] = if black { 0 } else { 255 };
                let error = value - if black { 0.0 } else { 255.0 };
                let adaptive;
                let (kernel, divisor) = if self == Self::Ostromoukhov {
                    // Coefficients depend on the original input, not the accumulated error.
                    let [forward, backward, below] =
                        COEFFICIENTS[usize::from(input.min(255 - input))].map(f32::from);
                    adaptive = [(1, 0, forward), (-1, 1, backward), (0, 1, below)];
                    (adaptive.as_slice(), forward + backward + below)
                } else {
                    (kernel, divisor)
                };
                for &(dx, dy, weight) in kernel {
                    if let Some(dest) = x.checked_add_signed(dx * direction)
                        && dest < width
                        && y + dy < height
                    {
                        errors[dy][dest] += error * weight / divisor;
                    }
                }
            }
            errors.rotate_left(1);
            errors[2].fill(0.0);
        }
    }
}

// Distribution coefficients from Appendix I of Victor Ostromoukhov,
// "A Simple and Efficient Error-Diffusion Algorithm", SIGGRAPH 2001.
// https://perso.liris.cnrs.fr/victor.ostromoukhov/publications/pdf/SIGGRAPH01_varcoeffED.pdf
// Each row is [forward, next-row backward, below]. Intensities 128..255 mirror 127..0.
const COEFFICIENTS: [[u16; 3]; 128] = [
    [13, 0, 5],
    [13, 0, 5],
    [21, 0, 10],
    [7, 0, 4],
    [8, 0, 5],
    [47, 3, 28],
    [23, 3, 13],
    [15, 3, 8],
    [22, 6, 11],
    [43, 15, 20],
    [7, 3, 3],
    [501, 224, 211],
    [249, 116, 103],
    [165, 80, 67],
    [123, 62, 49],
    [489, 256, 191],
    [81, 44, 31],
    [483, 272, 181],
    [60, 35, 22],
    [53, 32, 19],
    [237, 148, 83],
    [471, 304, 161],
    [3, 2, 1],
    [481, 314, 185],
    [354, 226, 155],
    [1389, 866, 685],
    [227, 138, 125],
    [267, 158, 163],
    [327, 188, 220],
    [61, 34, 45],
    [627, 338, 505],
    [1227, 638, 1075],
    [20, 10, 19],
    [1937, 1000, 1767],
    [977, 520, 855],
    [657, 360, 551],
    [71, 40, 57],
    [2005, 1160, 1539],
    [337, 200, 247],
    [2039, 1240, 1425],
    [257, 160, 171],
    [691, 440, 437],
    [1045, 680, 627],
    [301, 200, 171],
    [177, 120, 95],
    [2141, 1480, 1083],
    [1079, 760, 513],
    [725, 520, 323],
    [137, 100, 57],
    [2209, 1640, 855],
    [53, 40, 19],
    [2243, 1720, 741],
    [565, 440, 171],
    [759, 600, 209],
    [1147, 920, 285],
    [2311, 1880, 513],
    [97, 80, 19],
    [335, 280, 57],
    [1181, 1000, 171],
    [793, 680, 95],
    [599, 520, 57],
    [2413, 2120, 171],
    [405, 360, 19],
    [2447, 2200, 57],
    [11, 10, 0],
    [158, 151, 3],
    [178, 179, 7],
    [1030, 1091, 63],
    [248, 277, 21],
    [318, 375, 35],
    [458, 571, 63],
    [878, 1159, 147],
    [5, 7, 1],
    [172, 181, 37],
    [97, 76, 22],
    [72, 41, 17],
    [119, 47, 29],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [4, 1, 1],
    [65, 18, 17],
    [95, 29, 26],
    [185, 62, 53],
    [30, 11, 9],
    [35, 14, 11],
    [85, 37, 28],
    [55, 26, 19],
    [80, 41, 29],
    [155, 86, 59],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [5, 3, 2],
    [305, 176, 119],
    [155, 86, 59],
    [105, 56, 39],
    [80, 41, 29],
    [65, 32, 23],
    [55, 26, 19],
    [335, 152, 113],
    [85, 37, 28],
    [115, 48, 37],
    [35, 14, 11],
    [355, 136, 109],
    [30, 11, 9],
    [365, 128, 107],
    [185, 62, 53],
    [25, 8, 7],
    [95, 29, 26],
    [385, 112, 103],
    [65, 18, 17],
    [395, 104, 101],
    [4, 1, 1],
];

#[cfg(test)]
mod tests {
    use super::Dither;
    use image::{GrayImage, Luma};

    #[test]
    fn diffusion_preserves_tone() {
        for method in [Dither::Ostromoukhov, Dither::Stucki, Dither::FloydSteinberg] {
            for level in [64, 128, 192] {
                let mut image = GrayImage::from_pixel(64, 64, Luma([level]));
                method.apply(&mut image);
                assert!(
                    image
                        .as_raw()
                        .iter()
                        .all(|&pixel| pixel == 0 || pixel == 255)
                );
                let white = image
                    .pixels()
                    .fold(0_u32, |n, pixel| n + u32::from(pixel[0] == 255));
                let actual = f64::from(white) / (64.0 * 64.0);
                let expected = f64::from(level) / 255.0;
                assert!(
                    (actual - expected).abs() < 0.03,
                    "{method:?}, {level}: {actual}"
                );
            }
        }
    }
}
