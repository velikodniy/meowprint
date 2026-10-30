//! Packed raster pixels.

#[cfg(feature = "image")]
use alloc::vec;
use alloc::vec::Vec;
use core::future;

#[cfg(feature = "image")]
use crate::{ImageConversion, Position};
use crate::{Result, RowSource, error::invalid};
#[cfg(feature = "image")]
use image::{DynamicImage, GenericImageView, GrayImage, Luma};

/// How pixel values are packed into bytes.
///
/// Both formats store pixels from left to right and rows from top to bottom.
/// Pixel values describe darkness: zero is white and the largest value is black.
/// Use [`Raster::rows`] or implement [`RowSource`] to supply packed pixels.
/// Rows must fill the printable width without headers or gaps between rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Eight pixels per byte. Bit 0 is leftmost. Zero is white, one is black.
    ///
    /// All built-in drivers support this format. Each 384-dot row occupies 48 bytes.
    Mono,
    /// Two pixels per byte. The low four bits hold the left pixel. Zero is white, 15 is black.
    ///
    /// Only [`X6h`](crate::X6h) and [`V5g`](crate::V5g) support this format.
    /// Each 384-dot row occupies 192 bytes.
    Gray4,
}

/// Owned packed pixels with validated dimensions, independent of a printer.
///
/// Construction and preview require no I/O or Tokio runtime. Rows contain no
/// protocol headers; zero denotes white in both supported pixel formats.
/// Rasters are limited to 32,768 rows and 16,777,216 pixels.
pub struct Raster {
    width: u32,
    format: PixelFormat,
    bytes: Vec<u8>,
}

/// A borrowed forward-only cursor over a [`Raster`]'s packed rows.
///
/// Obtain a fresh cursor with [`Raster::rows`] to explicitly replay the pixels.
/// Reading copies one row into the supplied buffer without allocating or
/// consuming the raster. A failed read never advances the cursor.
pub struct RasterRows<'a> {
    raster: &'a Raster,
    cursor: usize,
}

impl RowSource for RasterRows<'_> {
    fn width(&self) -> u32 {
        self.raster.width()
    }
    fn height(&self) -> u32 {
        self.raster.height()
    }
    fn format(&self) -> PixelFormat {
        self.raster.format()
    }
    fn read_row(&mut self, output: &mut [u8]) -> impl Future<Output = Result<()>> {
        if output.len() != self.raster.row_bytes() {
            return future::ready(Err(invalid(
                "The row buffer does not match the raster row size.",
            )));
        }
        if self.cursor == self.raster.bytes.len() {
            return future::ready(Err(invalid("The raster row source is exhausted.")));
        }
        let end = self.cursor + output.len();
        output.copy_from_slice(&self.raster.bytes[self.cursor..end]);
        self.cursor = end;
        future::ready(Ok(()))
    }
}

impl Raster {
    /// Maximum supported height in dot rows.
    pub(crate) const MAX_HEIGHT: u32 = 32_768;
    /// Maximum supported area in pixels.
    pub(crate) const MAX_PIXELS: u64 = 16_777_216;

    pub(crate) fn max_height(width: u32) -> u32 {
        if width == 0 {
            return 0;
        }
        u32::try_from(Self::MAX_PIXELS / u64::from(width))
            .unwrap_or(u32::MAX)
            .min(Self::MAX_HEIGHT)
    }

    /// Make sure that dimensions fit the raster limits before allocating pixels.
    ///
    /// # Errors
    /// Reject zero width or height, excessive height, or excessive pixel area.
    /// [`Self::new`] also requires a width that fills whole bytes in the selected format.
    pub(crate) fn validate_dimensions(width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 || height > Self::max_height(width) {
            return Err(invalid(
                "The raster must be nonempty, at most 32768 rows, and at most 16777216 pixels.",
            ));
        }
        Ok(())
    }

    /// Take ownership of packed pixels at the given width in dots.
    ///
    /// The byte count determines the height. No pixels are converted or resized.
    ///
    /// # Errors
    /// Reject empty images, incomplete rows, or dimensions outside the raster limits.
    /// The width must be a multiple of eight for monochrome or two for four-bit grayscale.
    pub fn new(width: u32, bytes: Vec<u8>, format: PixelFormat) -> Result<Self> {
        Self::validate_dimensions(width, 1)?;
        let pixels_per_byte = match format {
            PixelFormat::Mono => 8,
            PixelFormat::Gray4 => 2,
        };
        if !width.is_multiple_of(pixels_per_byte) {
            return Err(invalid(
                "The raster width must fill whole bytes in the selected pixel format.",
            ));
        }
        let raster = Self {
            width,
            format,
            bytes,
        };
        let row_bytes = raster.row_bytes();
        if !raster.bytes.len().is_multiple_of(row_bytes) {
            return Err(invalid("Packed raster bytes must contain complete rows."));
        }
        let height = u32::try_from(raster.bytes.len() / row_bytes)
            .map_err(|_| invalid("The raster exceeds the maximum height."))?;
        Self::validate_dimensions(width, height)?;
        Ok(raster)
    }

    /// Convert and position an image as packed monochrome or four-bit grayscale pixels.
    ///
    /// Available with the `image` feature. The source keeps its size; resize it
    /// before calling if needed. Transparency is composited on white before
    /// conversion, and unused columns remain white.
    /// [`ImageConversion::Mono`] dithers only the source image, not its margins.
    /// [`ImageConversion::Gray4`] rounds luminance to 16 levels and packs the
    /// left pixel in the low nibble, with zero denoting white.
    ///
    /// # Errors
    /// Reject an empty source, zero width, an image wider than `width`, or
    /// dimensions above 32,768 rows or 16,777,216 pixels. The output width must
    /// be a multiple of eight for Mono or two for Gray4; source widths may be odd.
    /// All dimension checks occur before allocating conversion buffers.
    #[cfg(feature = "image")]
    pub fn from_image(
        image: &DynamicImage,
        width: u32,
        position: Position,
        conversion: ImageConversion,
    ) -> Result<Self> {
        Self::validate_dimensions(width, image.height())?;
        let (format, pixels_per_byte) = match conversion {
            ImageConversion::Mono(_) => (PixelFormat::Mono, 8),
            ImageConversion::Gray4 => (PixelFormat::Gray4, 2),
        };
        if !width.is_multiple_of(pixels_per_byte) {
            return Err(invalid(
                "The raster width must fill whole bytes in the selected pixel format.",
            ));
        }
        if image.width() == 0 || image.height() == 0 {
            return Err(invalid("The source image is empty."));
        }
        if image.width() > width {
            return Err(invalid(
                "The image exceeds the printable width. Resize it before preparation.",
            ));
        }

        let luminance = |x, y| {
            let [r, g, b, a] = image.get_pixel(x, y).0.map(u32::from);
            // ITU-R BT.709 luma weights (0.2126, 0.7152, 0.0722), scaled by 10000.
            let luma = (2126 * r + 7152 * g + 722 * b + 5000) / 10000;
            let value = (luma * a + 255 * (255 - a) + 127) / 255;
            value.to_le_bytes()[0]
        };

        let left = match position {
            Position::Left => 0,
            Position::Right => width - image.width(),
            Position::Center => (width - image.width()) / 2,
        };
        let row_bytes = (width / pixels_per_byte) as usize;
        let mut bytes = vec![0; row_bytes * image.height() as usize];
        match conversion {
            ImageConversion::Mono(dither) => {
                // Diffuse only within the source so margins cannot affect its dots.
                let mut gray = GrayImage::from_fn(image.width(), image.height(), |x, y| {
                    Luma([luminance(x, y)])
                });
                dither.apply(&mut gray);
                for (x, y, pixel) in gray.enumerate_pixels() {
                    if pixel[0] == 0 {
                        let x = x + left;
                        bytes[y as usize * row_bytes + x as usize / 8] |= 1 << (x % 8);
                    }
                }
            }
            ImageConversion::Gray4 => {
                // Quantize directly into packed output; no grayscale scratch image.
                for y in 0..image.height() {
                    for x in 0..image.width() {
                        let darkness = 15 - (u16::from(luminance(x, y)) + 8) / 17;
                        let x = x + left;
                        bytes[y as usize * row_bytes + x as usize / 2] |=
                            darkness.to_le_bytes()[0] << ((x % 2) * 4);
                    }
                }
            }
        }
        Ok(Self {
            width,
            format,
            bytes,
        })
    }

    /// Return the width in dots.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }
    /// Return the height in rows.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Construction limits the derived height to MAX_HEIGHT rows."
    )]
    pub const fn height(&self) -> u32 {
        (self.bytes.len() / self.row_bytes()) as u32
    }
    /// Return the pixel depth and packing convention.
    #[must_use]
    pub const fn format(&self) -> PixelFormat {
        self.format
    }
    /// Borrow all pixel bytes in their original packing order.
    ///
    /// The returned slice contains no protocol headers or compression.
    #[must_use]
    pub fn packed(&self) -> &[u8] {
        &self.bytes
    }
    /// Consume the raster and return its packed pixels without copying.
    #[must_use]
    pub fn into_packed(self) -> Vec<u8> {
        self.bytes
    }
    /// Borrow a fresh row cursor for [`crate::Printer::print`].
    ///
    /// Each call starts at the first row. No pixels are cloned or consumed.
    /// Raster validation does not prevalidate driver controls or compression;
    /// later compression failures during printing may leave partial output.
    #[must_use]
    pub const fn rows(&self) -> RasterRows<'_> {
        RasterRows {
            raster: self,
            cursor: 0,
        }
    }
    /// Return the byte count for one row.
    #[must_use]
    pub(crate) const fn row_bytes(&self) -> usize {
        (self.width()
            / match self.format {
                PixelFormat::Mono => 8,
                PixelFormat::Gray4 => 2,
            }) as usize
    }
    /// Expand the exact packed pixels into an eight-bit luminance image.
    ///
    /// Available with the `image` feature. Zero is black and 255 is white in
    /// the returned image. Editing it does not change this raster. No dithering,
    /// driver-added blank rows, or paper feed is included.
    #[must_use]
    #[cfg(feature = "image")]
    pub fn preview(&self) -> image::GrayImage {
        let raster = self;
        image::GrayImage::from_fn(raster.width(), raster.height(), |x, y| {
            let value = match raster.format() {
                PixelFormat::Mono => {
                    let byte = raster.packed()[y as usize * raster.row_bytes() + x as usize / 8];
                    if byte & (1 << (x % 8)) == 0 { 255 } else { 0 }
                }
                PixelFormat::Gray4 => {
                    let byte = raster.packed()[y as usize * raster.row_bytes() + x as usize / 2];
                    255 - ((byte >> ((x % 2) * 4)) & 15) * 17
                }
            };
            image::Luma([value])
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[tokio::test]
    async fn row_cursor_preserves_position_on_errors_and_restarts_explicitly() -> Result<()> {
        for (format, width) in [(PixelFormat::Mono, 16), (PixelFormat::Gray4, 4)] {
            let raster = Raster::new(width, vec![0x81, 0x23, 0x45, 0x67], format)?;
            let mut rows = raster.rows();
            assert_eq!(
                (rows.width(), rows.height(), rows.format()),
                (width, 2, format)
            );
            let mut wrong = [0xee; 3];
            assert!(matches!(
                rows.read_row(&mut wrong).await,
                Err(crate::Error::InvalidInput(_))
            ));
            assert_eq!(wrong, [0xee; 3]);
            let mut row = [0; 2];
            rows.read_row(&mut row).await?;
            assert_eq!(row, [0x81, 0x23]);
            assert!(rows.read_row(&mut []).await.is_err());
            rows.read_row(&mut row).await?;
            assert_eq!(row, [0x45, 0x67]);
            for _ in 0..2 {
                assert!(matches!(
                    rows.read_row(&mut row).await,
                    Err(crate::Error::InvalidInput(_))
                ));
                assert_eq!(row, [0x45, 0x67]);
            }
            raster.rows().read_row(&mut row).await?;
            assert_eq!(row, [0x81, 0x23]);
        }
        Ok(())
    }
    #[test]
    fn bounds_derive_from_width_and_reject_partial_rows() -> Result<()> {
        assert_eq!(Raster::max_height(384), 32768);
        assert_eq!(Raster::max_height(2040), 8224);
        assert_eq!(Raster::max_height(0), 0);
        for width in [384, 2040] {
            let height = Raster::max_height(width);
            Raster::validate_dimensions(width, height)?;
            assert!(Raster::validate_dimensions(width, height + 1).is_err());
        }
        assert!(Raster::new(0, vec![0], PixelFormat::Mono).is_err());
        assert!(Raster::new(7, vec![0], PixelFormat::Mono).is_err());
        assert!(Raster::new(3, vec![0], PixelFormat::Gray4).is_err());
        assert!(Raster::new(384, vec![], PixelFormat::Mono).is_err());
        assert!(Raster::new(384, vec![0; 49], PixelFormat::Mono).is_err());
        assert!(Raster::new(384, vec![0; 193], PixelFormat::Gray4).is_err());
        Ok(())
    }

    #[test]
    fn packed_dimensions_enforce_height_and_area_limits() -> Result<()> {
        for (width, height) in [(8, 32768), (1024, 16384)] {
            let bytes = (width / 8 * height) as usize;
            let raster = Raster::new(width, vec![0; bytes], PixelFormat::Mono)?;
            assert_eq!(raster.height(), height);
            assert!(
                Raster::new(
                    width,
                    vec![0; bytes + (width / 8) as usize],
                    PixelFormat::Mono
                )
                .is_err()
            );
        }
        assert!(Raster::new(16_777_224, vec![], PixelFormat::Mono).is_err());
        Ok(())
    }

    #[cfg(feature = "image")]
    #[test]
    fn image_conversion_rejects_invalid_dimensions() {
        for (source_width, source_height, width) in [
            (1, 1, 0),
            (1, 1, 7),
            (0, 1, 8),
            (1, 0, 8),
            (9, 1, 8),
            (1, 32769, 8),
            (1, 16385, 1024),
            (1, 1, 16_777_224),
        ] {
            let image = DynamicImage::new_luma8(source_width, source_height);
            for conversion in [
                ImageConversion::Mono(crate::Dither::Threshold),
                ImageConversion::Gray4,
            ] {
                assert!(
                    Raster::from_image(&image, width, Position::Center, conversion).is_err(),
                    "source {source_width}x{source_height}, raster width {width}, {conversion:?}",
                );
            }
        }
    }

    #[cfg(feature = "image")]
    #[test]
    fn image_conversion_composites_alpha_and_preserves_packed_preview() -> Result<()> {
        let pixels = [
            [0, 0, 0, 0],
            [0, 0, 0, 127],
            [0, 0, 0, 128],
            [0, 0, 0, 255],
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 255, 255],
        ];
        let source = DynamicImage::ImageRgba8(image::RgbaImage::from_fn(8, 1, |x, _| {
            image::Rgba(pixels[x as usize])
        }));
        let raster = Raster::from_image(
            &source,
            8,
            Position::Center,
            ImageConversion::Mono(crate::Dither::Threshold),
        )?;
        assert_eq!(raster.format(), PixelFormat::Mono);
        assert_eq!(raster.packed(), &[0x5c]);
        assert_eq!(raster.preview().as_raw(), &[255, 255, 0, 0, 0, 255, 0, 255]);
        assert_eq!(raster.into_packed(), [0x5c]);
        let gray = Raster::from_image(&source, 8, Position::Center, ImageConversion::Gray4)?;
        assert_eq!(gray.format(), PixelFormat::Gray4);
        assert_eq!(gray.packed(), &[0x70, 0xf8, 0x4c, 0x0e]);
        assert_eq!(
            gray.preview().as_raw(),
            &[255, 136, 119, 0, 51, 187, 17, 255]
        );
        Ok(())
    }

    #[cfg(feature = "image")]
    #[test]
    fn gray4_quantizes_boundaries_and_packs_all_levels() -> Result<()> {
        let values = [0, 8, 9, 25, 26, 246, 247, 255];
        let source =
            DynamicImage::ImageLuma8(GrayImage::from_fn(8, 1, |x, _| Luma([values[x as usize]])));
        let raster = Raster::from_image(&source, 8, Position::Left, ImageConversion::Gray4)?;
        assert_eq!(raster.packed(), &[0xff, 0xee, 0x1d, 0x00]);

        let source = DynamicImage::ImageLuma8(GrayImage::from_fn(16, 1, |x, _| {
            Luma([(x * 17).to_le_bytes()[0]])
        }));
        let raster = Raster::from_image(&source, 16, Position::Left, ImageConversion::Gray4)?;
        assert_eq!(
            raster.packed(),
            &[0xef, 0xcd, 0xab, 0x89, 0x67, 0x45, 0x23, 0x01]
        );
        assert_eq!(raster.preview(), source.to_luma8());
        Ok(())
    }

    #[cfg(feature = "image")]
    #[test]
    fn gray4_positions_odd_width_rows_without_crossing_row_boundaries() -> Result<()> {
        let values = [[0, 119, 255], [255, 136, 0]];
        let source = DynamicImage::ImageLuma8(GrayImage::from_fn(3, 2, |x, y| {
            Luma([values[y as usize][x as usize]])
        }));
        for (position, expected) in [
            (Position::Left, [0x8f, 0, 0, 0x70, 0x0f, 0]),
            (Position::Center, [0xf0, 0x08, 0, 0, 0xf7, 0]),
            (Position::Right, [0, 0xf0, 0x08, 0, 0, 0xf7]),
        ] {
            let raster = Raster::from_image(&source, 6, position, ImageConversion::Gray4)?;
            assert_eq!((raster.width(), raster.height()), (6, 2));
            assert_eq!(raster.packed(), &expected, "{position:?}");
        }
        let single = DynamicImage::new_luma8(1, 1);
        let raster = Raster::from_image(&single, 2, Position::Right, ImageConversion::Gray4)?;
        assert_eq!(raster.packed(), &[0xf0]);
        assert!(
            Raster::from_image(
                &single,
                2,
                Position::Right,
                ImageConversion::Mono(crate::Dither::Threshold)
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(feature = "image")]
    #[test]
    fn dithering_is_position_independent_and_keeps_margins_white() -> Result<()> {
        use crate::Dither;
        let source = DynamicImage::ImageLuma8(GrayImage::from_fn(3, 3, |x, y| {
            Luma([match (x + y) % 3 {
                0 => 0,
                1 => 128,
                _ => 255,
            }])
        }));
        for dither in [
            Dither::Ostromoukhov,
            Dither::Stucki,
            Dither::FloydSteinberg,
            Dither::Threshold,
        ] {
            let left =
                Raster::from_image(&source, 8, Position::Left, ImageConversion::Mono(dither))?;
            for (position, offset) in [
                (Position::Left, 0),
                (Position::Center, 2),
                (Position::Right, 5),
            ] {
                let raster =
                    Raster::from_image(&source, 8, position, ImageConversion::Mono(dither))?;
                let preview = raster.preview();
                for (y, (&row, &original)) in raster.packed().iter().zip(left.packed()).enumerate()
                {
                    assert_eq!(row, original << offset);
                    for x in 0..8 {
                        let expected = if (offset..offset + 3).contains(&x) {
                            if original & (1 << (x - offset)) == 0 {
                                255
                            } else {
                                0
                            }
                        } else {
                            255
                        };
                        assert_eq!(preview.as_raw()[y * 8 + x], expected);
                    }
                }
            }
        }
        Ok(())
    }
}
