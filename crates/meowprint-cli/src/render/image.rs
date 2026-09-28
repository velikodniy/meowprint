use super::{Dither, Driver};
use crate::{Result, error::invalid};
use image::{
    DynamicImage, GenericImageView, GrayImage, ImageDecoder, ImageReader, Pixel, Rgba,
    imageops::FilterType,
};
use std::path::Path;

/// An image scaled to the printable width with its aspect ratio intact.
pub struct Image {
    source: DynamicImage,
    /// Monochrome conversion method, default Ostromoukhov.
    pub dither: Dither,
    /// Select sixteen gray levels instead of dithering.
    pub grayscale: bool,
}

impl Image {
    /// Own an image with default monochrome rendering.
    pub fn new(source: DynamicImage) -> Self {
        Self {
            source,
            dither: Dither::default(),
            grayscale: false,
        }
    }

    /// Load a bounded image and apply camera orientation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let mut reader = ImageReader::open(path)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(32_768);
        limits.max_image_height = Some(32_768);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits.clone());
        let mut decoder = reader.into_decoder()?;
        // Reserve the output buffer, as ImageReader::decode does before decoding.
        limits.reserve(decoder.total_bytes())?;
        decoder.set_limits(limits)?;
        let orientation = decoder.orientation()?;
        let mut source = DynamicImage::from_decoder(decoder)?;
        source.apply_orientation(orientation);
        Ok(Self::new(source))
    }
}

impl Image {
    /// Render pixels at the selected printable width.
    pub fn render(&self, driver: impl Driver) -> Result<GrayImage> {
        let width = driver.printable_width();
        if self.source.width() == 0 || self.source.height() == 0 {
            return Err(invalid("The source image is empty."));
        }
        let height = ((u64::from(self.source.height()) * u64::from(width)
            + u64::from(self.source.width()) / 2)
            / u64::from(self.source.width()))
        .max(1);
        let height = u32::try_from(height).map_err(|_| invalid("The scaled image is too tall."))?;
        super::validate_dimensions(driver, height)?;
        // Flatten on white before filtering so transparent dark pixels cannot bleed.
        let gray = GrayImage::from_fn(self.source.width(), self.source.height(), |x, y| {
            let mut pixel = Rgba([255; 4]);
            pixel.blend(&self.source.get_pixel(x, y));
            pixel.to_luma()
        });
        let mut scaled = image::imageops::resize(&gray, width, height, FilterType::Lanczos3);
        if !self.grayscale {
            self.dither.apply(&mut scaled);
        }
        Ok(scaled)
    }
}
