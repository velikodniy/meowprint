//! Private command-line input and content preparation.
use crate::{
    args::{Content, DitherArg},
    render::{Dither, Image, QrCode, Text},
};
use anyhow::{Context, Result};
use image::GrayImage;
use meowprint::{Driver, PixelFormat};

impl Content {
    /// Return the packed pixel format that rendering will produce.
    pub const fn format(&self) -> PixelFormat {
        if matches!(
            self,
            Self::Image {
                grayscale: true,
                ..
            }
        ) {
            PixelFormat::Gray4
        } else {
            PixelFormat::Mono
        }
    }
    /// Render content for the selected printer driver.
    pub fn render(self, driver: impl Driver) -> Result<GrayImage> {
        Ok(match self {
            Self::Image {
                path,
                dither,
                grayscale,
            } => {
                let mut image = Image::open(&path)
                    .with_context(|| format!("Cannot open {}", path.display()))?;
                image.dither = match dither {
                    DitherArg::Ostromoukhov => Dither::Ostromoukhov,
                    DitherArg::Stucki => Dither::Stucki,
                    DitherArg::FloydSteinberg => Dither::FloydSteinberg,
                    DitherArg::Threshold => Dither::Threshold,
                };
                image.grayscale = grayscale;
                image.render(driver)?
            }
            Self::Qr { content } => QrCode::new(content).render(driver)?,
            Self::Text(args) => {
                let content = match args.file {
                    Some(path) => std::fs::read_to_string(path)?,
                    None => args.content.unwrap_or_default(),
                };
                let bytes = args
                    .font
                    .map(|path| {
                        std::fs::read(&path)
                            .with_context(|| format!("Cannot read font {}", path.display()))
                    })
                    .transpose()?;
                let mut text = Text::new(
                    content,
                    bytes
                        .as_deref()
                        .unwrap_or(include_bytes!("../assets/fonts/Roboto.ttf")),
                )?;
                text.size = args.size;
                text.margin = args.margin;
                text.render(driver)?
            }
        })
    }
}
