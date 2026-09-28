use super::Driver;
use crate::{Result, error::invalid};
use fontdue::{
    Font, FontSettings,
    layout::{CoordinateSystem, Layout, LayoutSettings, TextStyle, WrapStyle},
};
use image::{GrayImage, Luma};
use std::collections::HashMap;

/// Text rendered with a caller-supplied TrueType or OpenType font.
pub struct Text {
    content: String,
    font: Font,
    /// Font size in dots, default 28.
    pub size: f32,
    /// Horizontal margin in dots, default 12.
    pub margin: u32,
}

impl Text {
    /// Parse a font and own the text content.
    pub fn new(content: impl Into<String>, font_bytes: &[u8]) -> Result<Self> {
        let font = Font::from_bytes(font_bytes, FontSettings::default()).map_err(invalid)?;
        Ok(Self {
            content: content.into(),
            font,
            size: 28.0,
            margin: 12,
        })
    }
    fn layout(&self, content: &str, width: u32) -> Result<Layout> {
        let width_f32 = f32::from(
            u16::try_from(width).map_err(|_| invalid("The text width exceeds 65535 dots."))?,
        );
        let mut left = 0.0_f32;
        let mut right = 0.0_f32;
        let mut advance = 0.0_f32;
        for c in content.chars().filter(|&c| c != '\n') {
            if c.is_control() {
                return Err(invalid("Text contains an unsupported control character."));
            }
            if !c.is_whitespace() && self.font.lookup_glyph_index(c) == 0 {
                return Err(invalid(format!(
                    "The font has no glyph for {c:?}. Choose a font that contains it."
                )));
            }
            let metrics = self.font.metrics(c, self.size);
            let xmin = metrics.bounds.xmin.floor();
            let step = metrics.advance_width.ceil();
            let ink_width = f32::from(
                u16::try_from(metrics.width)
                    .map_err(|_| invalid("A text character is too wide. Reduce the font size."))?,
            );
            if !xmin.is_finite() || !step.is_finite() {
                return Err(invalid("The font contains invalid character dimensions."));
            }
            left = left.max(-xmin);
            right = right.max(xmin + ink_width - step);
            advance = advance.max(step);
        }
        let layout_width = width_f32 - left - right;
        if layout_width <= 0.0 || advance > layout_width {
            return Err(invalid(
                "A text character is wider than the printable area. Reduce the font size.",
            ));
        }
        let mut layout = Layout::new(CoordinateSystem::PositiveYDown);
        layout.reset(&LayoutSettings {
            x: left,
            max_width: Some(layout_width),
            wrap_style: WrapStyle::Word,
            ..LayoutSettings::default()
        });
        layout.append(&[&self.font], &TextStyle::new(content, self.size, 0));
        Ok(layout)
    }
}

impl Text {
    /// Render pixels at the selected printable width.
    pub fn render(&self, driver: impl Driver) -> Result<GrayImage> {
        let width = driver.printable_width();
        if self.content.len() > 65_536 {
            return Err(invalid(
                "Text cannot exceed 65536 UTF-8 bytes. Split it into smaller jobs.",
            ));
        }
        if self.content.trim().is_empty() {
            return Err(invalid("Text must not be empty."));
        }
        if !self.size.is_finite() || !(4.0..=512.0).contains(&self.size) {
            return Err(invalid("Font size must be between 4 and 512 dots."));
        }
        let text_width = width
            .checked_sub(self.margin.saturating_mul(2))
            .filter(|&w| w > 0)
            .ok_or_else(|| invalid("Text margins leave no printable width."))?;
        let content = self.content.replace("\r\n", "\n").replace('\t', "    ");
        let layout = self.layout(&content, text_width)?;
        let top = layout
            .glyphs()
            .iter()
            .map(|g| f64::from(g.y))
            .fold(0.0, f64::min);
        let bottom = layout
            .glyphs()
            .iter()
            .map(|g| f64::from(g.y) + f64::from(u32::try_from(g.height).unwrap_or(u32::MAX)))
            .fold(f64::from(layout.height()), f64::max);
        let extent = bottom - top;
        if !extent.is_finite() || !(0.0..=f64::from(driver.max_height())).contains(&extent) {
            return Err(invalid("The text layout exceeds 32768 rows."));
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "The finite nonnegative ceiling is bounded to 32768 rows."
        )]
        let height = (extent.ceil() as u32).saturating_add(self.margin.saturating_mul(2));
        super::validate_dimensions(driver, height)?;
        let mut image = GrayImage::from_pixel(width, height, Luma([255]));
        let mut glyph_rasters = HashMap::new();
        for glyph in layout.glyphs() {
            if glyph.char_data.is_control() || glyph.width == 0 || glyph.height == 0 {
                continue;
            }
            let (metrics, coverage) = glyph_rasters
                .entry(glyph.key)
                .or_insert_with(|| self.font.rasterize_config(glyph.key));
            let glyph_width =
                u32::try_from(metrics.width).map_err(|_| invalid("The glyph is too wide."))?;
            let glyph_height =
                u32::try_from(metrics.height).map_err(|_| invalid("The glyph is too tall."))?;
            let x = f64::from(glyph.x).floor() + f64::from(self.margin);
            let y = (f64::from(glyph.y) - top).floor() + f64::from(self.margin);
            if !x.is_finite()
                || !y.is_finite()
                || x < f64::from(self.margin)
                || y < f64::from(self.margin)
                || x + f64::from(glyph_width) > f64::from(width - self.margin)
                || y + f64::from(glyph_height) > f64::from(height - self.margin)
            {
                return Err(invalid(
                    "A text character exceeds the printable area. Reduce the font size or margin.",
                ));
            }
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "The finite integer coordinates fit the bounded image dimensions."
            )]
            let (x, y) = (x as u32, y as u32);
            for dy in 0..glyph_height {
                for dx in 0..glyph_width {
                    let pixel = image.get_pixel_mut(x + dx, y + dy);
                    pixel[0] =
                        pixel[0].min(255 - coverage[dy as usize * metrics.width + dx as usize]);
                }
            }
        }
        Ok(image)
    }
}
