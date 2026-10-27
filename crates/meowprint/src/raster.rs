//! Packed raster pixels.

#[cfg(feature = "image")]
use crate::{Dither, Position};
use crate::{Result, error::invalid};
#[cfg(feature = "image")]
use image::{DynamicImage, GenericImageView, GrayImage, Luma};

/// How pixel values are packed into bytes.
///
/// Both formats store pixels from left to right and rows from top to bottom.
/// Pixel values describe darkness: zero is white and the largest value is black.
/// Pass the format to [`Printer::prepare`](crate::Printer::prepare) with the packed bytes.
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

    /// Convert and position an image as packed monochrome pixels.
    ///
    /// Available with the `image` feature. The source keeps its size; resize it
    /// before calling if needed. Transparency is composited on white before
    /// dithering, and unused columns remain white regardless of dithering.
    /// The returned raster always uses [`PixelFormat::Mono`].
    ///
    /// # Errors
    /// Reject an empty source, zero or non-byte-aligned width, an image wider
    /// than `width`, or dimensions above 32,768 rows or 16,777,216 pixels.
    /// All dimension checks occur before allocating conversion buffers.
    #[cfg(feature = "image")]
    pub fn from_image(
        image: &DynamicImage,
        width: u32,
        position: Position,
        dither: Dither,
    ) -> Result<Self> {
        Self::validate_dimensions(width, image.height())?;
        if !width.is_multiple_of(8) {
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
        Ok(Self {
            width,
            format: PixelFormat::Mono,
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
        assert!(Raster::new(383, vec![0; 48], PixelFormat::Mono).is_err());
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
            assert!(
                Raster::from_image(&image, width, Position::Center, Dither::Threshold).is_err(),
                "source {source_width}x{source_height}, raster width {width}",
            );
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
        let raster = Raster::from_image(&source, 8, Position::Center, Dither::Threshold)?;
        assert_eq!(raster.format(), PixelFormat::Mono);
        assert_eq!(raster.packed(), &[0x5c]);
        assert_eq!(raster.preview().as_raw(), &[255, 255, 0, 0, 0, 255, 0, 255]);
        assert_eq!(raster.into_packed(), [0x5c]);
        Ok(())
    }

    #[cfg(feature = "image")]
    #[test]
    fn dithering_is_position_independent_and_keeps_margins_white() -> Result<()> {
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
            let left = Raster::from_image(&source, 8, Position::Left, dither)?;
            for (position, offset) in [
                (Position::Left, 0),
                (Position::Center, 2),
                (Position::Right, 5),
            ] {
                let raster = Raster::from_image(&source, 8, position, dither)?;
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
