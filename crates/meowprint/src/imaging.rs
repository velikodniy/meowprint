//! Image positioning and conversion choices.

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

/// How an image is converted to packed printer pixels.
///
/// Available with the `image` feature. Pass to [`crate::Raster::from_image`].
/// Both conversions composite transparency on white before reducing pixel depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageConversion {
    /// Convert to [`crate::PixelFormat::Mono`] using the selected dither method.
    Mono(crate::Dither),
    /// Convert to [`crate::PixelFormat::Gray4`] without monochrome dithering.
    ///
    /// Luminance is rounded to the nearest of 16 equally spaced levels.
    Gray4,
}
