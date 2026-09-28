//! Offline previews use the same preparation path as connected printing.
use image::GrayImage;
use meowprint::{Driver, NoOpTransport, PixelFormat, Printer, Result};

/// Prepare and expand pixels without a real connection or runtime.
pub fn render(driver: impl Driver, image: GrayImage, format: PixelFormat) -> Result<GrayImage> {
    let mut printer = Printer::new(NoOpTransport, driver);
    Ok(crate::render::prepare(&mut printer, image, format, &Default::default())?.preview())
}
