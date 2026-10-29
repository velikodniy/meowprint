//! Private command-line input and rendered raster content.
use crate::{
    args::{Content, DitherArg},
    render::{Image, Text, qr},
};
use anyhow::{Context, Result};
use image::{DynamicImage, GrayImage};
use meowprint::{Dither, Driver, ImageConversion, PixelFormat, Position, Raster};

/// Rendered raster pixels and controls for one selected driver.
pub struct RenderedContent<D: Driver> {
    pub(crate) raster: Raster,
    pub(crate) options: D::Options,
}

impl<D: Driver> RenderedContent<D> {
    /// Return the rendered width and height in dots.
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.raster.width(), self.raster.height())
    }

    /// Expand the packed raster into grayscale pixels for an offline preview.
    pub fn preview(&self) -> GrayImage {
        self.raster.preview()
    }
}

impl Content {
    /// Validate controls and render content for the selected printer driver.
    pub fn render<D: Driver>(self, driver: D, options: D::Options) -> Result<RenderedContent<D>> {
        let format = if matches!(
            self,
            Self::Image {
                grayscale: true,
                ..
            }
        ) {
            PixelFormat::Gray4
        } else {
            PixelFormat::Mono
        };
        driver.validate(&options, format)?;
        let image = match self {
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
            Self::Qr { content } => qr::render(content.as_bytes(), driver)?,
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
        };
        let conversion = match format {
            PixelFormat::Mono => ImageConversion::Mono(Dither::Threshold),
            PixelFormat::Gray4 => ImageConversion::Gray4,
        };
        let raster = Raster::from_image(
            &DynamicImage::ImageLuma8(image),
            driver.printable_width(),
            Position::Center,
            conversion,
        )?;
        Ok(RenderedContent { raster, options })
    }
}
