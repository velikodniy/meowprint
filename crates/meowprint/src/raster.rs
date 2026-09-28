//! Packed raster pixels.

use crate::{Result, error::invalid};

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

/// Packed pixels with validated dimensions, used during print preparation.
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
    pub(crate) fn new(width: u32, bytes: Vec<u8>, format: PixelFormat) -> Result<Self> {
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

    /// Return the width in dots.
    #[must_use]
    pub(crate) const fn width(&self) -> u32 {
        self.width
    }
    /// Return the height in rows.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Construction limits the derived height to MAX_HEIGHT rows."
    )]
    pub(crate) const fn height(&self) -> u32 {
        (self.bytes.len() / self.row_bytes()) as u32
    }
    /// Return the pixel depth and packing convention.
    #[must_use]
    pub(crate) const fn format(&self) -> PixelFormat {
        self.format
    }
    /// Borrow all pixel bytes in their original packing order.
    ///
    /// The returned slice contains no protocol headers or compression.
    #[must_use]
    pub(crate) fn packed(&self) -> &[u8] {
        &self.bytes
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
    #[cfg(feature = "image")]
    pub(crate) fn preview(&self) -> image::GrayImage {
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
        assert!(Raster::new(384, vec![], PixelFormat::Mono).is_err());
        assert!(Raster::new(384, vec![0; 49], PixelFormat::Mono).is_err());
        assert!(Raster::new(383, vec![0; 48], PixelFormat::Mono).is_err());
        assert!(Raster::new(384, vec![0; 193], PixelFormat::Gray4).is_err());
        Ok(())
    }
}
