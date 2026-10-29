//! Row-oriented grayscale dithering, independent of the optional image feature.

use crate::Result;

/// A method for converting gray pixels to black and white dots.
///
/// Zero is black and 255 is white. Use [`Threshold`](Self::Threshold) for an
/// image that is already black and white.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Dither {
    /// Ostromoukhov dithering, the default. Adjusts dot patterns to brightness.
    #[default]
    Ostromoukhov,
    /// Stucki dithering. Spreads errors across neighboring pixels.
    Stucki,
    /// Floyd–Steinberg dithering. Spreads errors to nearby pixels.
    FloydSteinberg,
    /// Values below 128 become black. All other values become white.
    Threshold,
}

impl Dither {
    /// Create a reusable row processor with the given grayscale row width.
    #[must_use]
    pub fn rows(self, width: usize) -> RowDither {
        RowDither {
            method: self,
            width,
            reverse: false,
            errors: std::array::from_fn(|_| {
                if self == Self::Threshold {
                    Vec::new()
                } else {
                    vec![0.0; width]
                }
            }),
        }
    }

    /// Convert an eight-bit grayscale image to black and white in place.
    ///
    /// Available with the `image` feature. Empty images are unchanged.
    #[cfg(feature = "image")]
    pub fn apply(self, image: &mut image::GrayImage) {
        let width = image.width() as usize;
        let height = image.height() as usize;
        if width == 0 || image.height() == 0 {
            return;
        }
        let mut rows = self.rows(width);
        let raw = image.as_mut();
        for row in raw.chunks_exact_mut(width).take(height) {
            apply_row(self, width, &mut rows.reverse, &mut rows.errors, row);
        }
    }
}

/// Reusable state for dithering successive grayscale rows.
pub struct RowDither {
    method: Dither,
    width: usize,
    reverse: bool,
    errors: [Vec<f32>; 3],
}

impl RowDither {
    /// Dither one grayscale row in place.
    ///
    /// Input luminance is 0 black through 255 white; output is only 0 or 255.
    /// An empty row at zero width is a no-op.
    ///
    /// # Errors
    /// Reject a width mismatch before changing pixels or diffusion state.
    pub fn apply_row(&mut self, row: &mut [u8]) -> Result<()> {
        if row.len() != self.width {
            return Err(crate::error::invalid(
                "The grayscale row width does not match the dither state.",
            ));
        }
        if self.width == 0 {
            return Ok(());
        }
        apply_row(
            self.method,
            self.width,
            &mut self.reverse,
            &mut self.errors,
            row,
        );
        Ok(())
    }

    /// Reset the processor for a new image without reallocating.
    pub fn reset(&mut self) {
        self.reverse = false;
        for row in &mut self.errors {
            row.fill(0.0);
        }
    }
}

fn apply_row(
    method: Dither,
    width: usize,
    reverse: &mut bool,
    errors: &mut [Vec<f32>],
    row: &mut [u8],
) {
    if method == Dither::Threshold {
        for pixel in row {
            *pixel = if *pixel < 128 { 0 } else { 255 };
        }
        return;
    }
    let (kernel, divisor): (&[(isize, usize, f32)], f32) = match method {
        Dither::Stucki => (
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
        Dither::FloydSteinberg => (&[(1, 0, 7.0), (-1, 1, 3.0), (0, 1, 5.0), (1, 1, 1.0)], 16.0),
        Dither::Threshold | Dither::Ostromoukhov => (&[], 1.0),
    };
    let direction = if *reverse { -1isize } else { 1isize };
    for step in 0..width {
        let x = if direction == 1 {
            step
        } else {
            width - 1 - step
        };
        let input = row[x];
        let value = f32::from(input) + errors[0][x];
        let black = value < 127.5;
        row[x] = if black { 0 } else { 255 };
        let error = value - if black { 0.0 } else { 255.0 };
        let adaptive;
        let (kernel, divisor) = if method == Dither::Ostromoukhov {
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
            {
                errors[dy][dest] += error * weight / divisor;
            }
        }
    }
    errors.rotate_left(1);
    errors[2].fill(0.0);
    *reverse = !*reverse;
}

// Distribution coefficients from Appendix I of Victor Ostromoukhov, SIGGRAPH 2001.
// Intensities 128..255 mirror 127..0. Each row is [forward, backward, below].
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

    const METHODS: [Dither; 4] = [
        Dither::Ostromoukhov,
        Dither::Stucki,
        Dither::FloydSteinberg,
        Dither::Threshold,
    ];
    // FNV-1a hashes of fixed pixels captured from the original image kernel.
    const HASHES: [[u64; 12]; 4] = [
        [
            0xaf63_bd4c_8601_b7df,
            0x0832_8807_b4eb_6fed,
            0x7b27_28d4_f233_1ed1,
            0x0832_8807_b4eb_6fed,
            0x4d25_077f_9dcd_5758,
            0xeb1d_9686_4585_9d4c,
            0x19a1_5177_550a_8f3e,
            0x8e72_9e6f_4e73_a0a3,
            0x7e64_16c6_672e_42a8,
            0x8437_ca40_de61_ed49,
            0x0ac9_24ff_8b73_f1ca,
            0x9b51_879b_da8a_75bf,
        ],
        [
            0xaf63_bd4c_8601_b7df,
            0x0832_8807_b4eb_6fed,
            0x7b27_28d4_f233_1ed1,
            0x0832_8807_b4eb_6fed,
            0x4d25_077f_9dcd_5758,
            0xeb1d_8d86_4585_8e01,
            0xc44e_f878_d740_b603,
            0x25eb_755a_9953_dff2,
            0x685c_adad_0292_adbd,
            0xfcf3_995c_7a66_274f,
            0xc4fd_2dc0_c3b9_5019,
            0x72df_fb3c_0af3_817c,
        ],
        [
            0xaf63_bd4c_8601_b7df,
            0x0832_8807_b4eb_6fed,
            0x7b27_28d4_f233_1ed1,
            0x0832_8807_b4eb_6fed,
            0x4d25_077f_9dcd_5758,
            0xeb1d_9686_4585_9d4c,
            0x19a1_5177_550a_8f3e,
            0x38a1_f1f6_cf5e_d068,
            0x70f7_81a5_b253_8b8f,
            0x8437_ca40_de61_ed49,
            0xe7a3_f593_7fdc_d2d9,
            0xfff7_47f8_5d92_9353,
        ],
        [
            0xaf63_bd4c_8601_b7df,
            0x0832_8807_b4eb_6fed,
            0x7b27_28d4_f233_1ed1,
            0x0832_8807_b4eb_6fed,
            0x4d25_767f_9dce_13f5,
            0x0513_54b3_2cea_f70c,
            0x809a_fc10_32b5_7524,
            0xfb5c_09cd_bcb4_d2b7,
            0xd1c9_42c3_bc44_c3fa,
            0x76dd_cad9_45ba_3955,
            0x57ee_d71e_ae7c_7836,
            0x30ed_4f57_8c2d_4fb5,
        ],
    ];

    #[test]
    fn matches_pixels_captured_from_original_image_kernel() -> crate::Result<()> {
        for (mi, method) in METHODS.into_iter().enumerate() {
            let mut index = 0;
            for width in [1_usize, 2, 7, 17] {
                for height in [1, 2, 5] {
                    let mut pixels = (0..height)
                        .flat_map(|y| {
                            (0..width)
                                .map(move |x| (37 * x + 83 * y + (x * y) % 19).to_le_bytes()[0])
                        })
                        .collect::<Vec<_>>();
                    let mut state = method.rows(width);
                    for row in pixels.chunks_exact_mut(width) {
                        state.apply_row(row)?;
                    }
                    let hash = pixels.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
                    });
                    assert_eq!(hash, HASHES[mi][index], "{method:?}, {width}x{height}");
                    index += 1;
                }
            }
        }
        let mut edges = [0, 127, 128, 255];
        Dither::Threshold.rows(4).apply_row(&mut edges)?;
        assert_eq!(edges, [0, 0, 255, 255]);
        Ok(())
    }

    #[cfg(feature = "image")]
    #[test]
    fn image_dithering_preserves_storage_outside_declared_dimensions() -> crate::Result<()> {
        for method in METHODS {
            for (width, height) in [(1, 1), (3, 2), (0, 2), (3, 0)] {
                let mut pixels = vec![0; width as usize * height as usize];
                if let Some(first) = pixels.first_mut() {
                    *first = 128;
                }
                let mut expected = pixels.clone();
                if let Some(first) = expected.first_mut() {
                    *first = 255;
                }
                // Include both complete extra rows and a partial row.
                let trailing = [17, 128, 63, 91];
                pixels.extend_from_slice(&trailing);
                expected.extend_from_slice(&trailing);
                let mut image = image::GrayImage::from_raw(width, height, pixels)
                    .ok_or_else(|| crate::error::invalid("Invalid test image."))?;
                method.apply(&mut image);
                assert_eq!(image.into_raw(), expected, "{method:?}, {width}x{height}");
            }
        }
        Ok(())
    }

    #[test]
    fn mismatch_does_not_change_state_and_reset_matches_fresh() -> crate::Result<()> {
        for method in METHODS {
            let mut state = method.rows(7);
            let mut row = [0, 41, 80, 120, 160, 210, 255];
            state.apply_row(&mut row)?;
            let mut wrong = [1, 2];
            assert!(state.apply_row(&mut wrong).is_err());
            assert_eq!(wrong, [1, 2]);
            let mut actual = [17, 37, 57, 77, 97, 117, 137];
            let mut expected = actual;
            state.apply_row(&mut actual)?;
            let mut untouched = method.rows(7);
            untouched.apply_row(&mut [0, 41, 80, 120, 160, 210, 255])?;
            untouched.apply_row(&mut expected)?;
            assert_eq!(actual, expected);
            state.reset();
            let mut fresh = method.rows(7);
            for mut reset_row in [
                [0, 41, 80, 120, 160, 210, 255],
                [17, 37, 57, 77, 97, 117, 137],
                [255, 210, 160, 120, 80, 41, 0],
            ] {
                let mut fresh_row = reset_row;
                state.apply_row(&mut reset_row)?;
                fresh.apply_row(&mut fresh_row)?;
                assert_eq!(reset_row, fresh_row);
            }
        }
        Ok(())
    }

    #[test]
    fn zero_width_is_a_noop() -> crate::Result<()> {
        let mut state = Dither::Stucki.rows(0);
        state.apply_row(&mut [])?;
        assert!(state.apply_row(&mut [1]).is_err());
        state.reset();
        Ok(())
    }
}
