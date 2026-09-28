//! Private content rendering and exact pixel previews.
mod image;
mod qr;
mod text;

use crate::{Result, error::invalid};
use ::image::{DynamicImage, GrayImage};
pub use image::Image;
pub use meowprint::Dither;
use meowprint::{Driver, PixelFormat, Position, PrintJob, Printer, Transport};
pub use qr::QrCode;
pub use text::Text;

fn validate_dimensions(driver: impl Driver, height: u32) -> Result<()> {
    if height == 0 || height > driver.max_height() {
        return Err(invalid(format!(
            "The raster must contain 1 through {} rows for this driver.",
            driver.max_height()
        )));
    }
    Ok(())
}

/// Prepare rendered pixels through the library's existing image and packed-pixel APIs.
pub fn prepare<'printer, T: Transport, D: Driver>(
    printer: &'printer mut Printer<T, D>,
    image: GrayImage,
    format: PixelFormat,
    options: &D::Options,
) -> meowprint::Result<PrintJob<'printer, T, D>> {
    if image.width() != printer.printable_width() {
        return Err(meowprint::Error::InvalidInput(
            "Image width does not match the selected printer driver.".into(),
        ));
    }
    match format {
        PixelFormat::Mono => printer.prepare_from_image(
            &DynamicImage::ImageLuma8(image),
            Position::Center,
            Dither::Threshold,
            options,
        ),
        PixelFormat::Gray4 => {
            let bytes = image
                .as_raw()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    let left = 15 - (u16::from(pair[0]) + 8) / 17;
                    let right = 15 - (u16::from(pair[1]) + 8) / 17;
                    (left | (right << 4)).to_le_bytes()[0]
                })
                .collect();
            printer.prepare(bytes, format, options)
        }
    }
}
