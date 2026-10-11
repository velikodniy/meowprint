//! Private image, text, and QR rendering.
mod image;
mod qr;
mod text;

use crate::{Result, error::invalid};
pub use image::Image;
pub use meowprint::Dither;
use meowprint::Driver;
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
