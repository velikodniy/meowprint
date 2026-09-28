use super::Driver;
use crate::{Result, error::invalid};
use image::{GrayImage, Luma};
use qrcode::{Color, EcLevel};

/// A QR symbol with a four-module white border and integer-sized modules.
pub struct QrCode {
    data: Vec<u8>,
}

impl QrCode {
    /// Own content for a medium-correction QR symbol.
    pub fn new(data: impl Into<Vec<u8>>) -> Self {
        Self { data: data.into() }
    }
}

impl QrCode {
    /// Render pixels at the selected printable width.
    pub fn render(&self, driver: impl Driver) -> Result<GrayImage> {
        let width = driver.printable_width();
        if self.data.is_empty() {
            return Err(invalid("QR content must not be empty."));
        }
        let code = qrcode::QrCode::with_error_correction_level(&self.data, EcLevel::M)
            .map_err(|e| invalid(format!("Cannot encode the QR content: {e}")))?;
        let code_width = u32::try_from(code.width())
            .map_err(|_| invalid("The QR symbol exceeds the maximum width."))?;
        let modules = code_width + 8;
        let scale = width / modules;
        if scale < 2 {
            return Err(invalid(
                "The QR content is too dense for this printable width. Use less content or a wider head.",
            ));
        }
        let side = modules * scale;
        super::validate_dimensions(driver, side)?;
        let left = (width - side) / 2 + 4 * scale;
        let mut image = GrayImage::from_pixel(width, side, Luma([255]));
        for y in 0..code_width {
            for x in 0..code_width {
                if code[(x as usize, y as usize)] == Color::Dark {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            image.put_pixel(left + x * scale + dx, (y + 4) * scale + dy, Luma([0]));
                        }
                    }
                }
            }
        }
        Ok(image)
    }
}
