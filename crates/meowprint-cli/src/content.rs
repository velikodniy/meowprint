//! Private command-line input and content preparation.
use crate::{
    args::{Content, DitherArg},
    render::{Dither, Image, QrCode, Text},
};
use anyhow::{Context, Result};
use image::{DynamicImage, GrayImage};
use meowprint::{Driver, NoOpTransport, PixelFormat, Position, PrintJob, Printer, Transport};

/// Rendered pixels and controls for one selected driver.
pub struct RenderedContent<D: Driver> {
    driver: D,
    image: GrayImage,
    format: PixelFormat,
    options: D::Options,
}

impl<D: Driver> RenderedContent<D> {
    /// Return the rendered width and height in dots.
    pub fn dimensions(&self) -> (u32, u32) {
        self.image.dimensions()
    }

    /// Create a job bound to the supplied printer session without communication.
    pub fn prepare<T: Transport>(
        self,
        printer: &mut Printer<T, D>,
    ) -> meowprint::Result<PrintJob<'_, T, D>> {
        if self.image.width() != printer.printable_width() {
            return Err(meowprint::Error::InvalidInput(
                "Image width does not match the selected printer driver.".into(),
            ));
        }
        match self.format {
            PixelFormat::Mono => printer.prepare_from_image(
                &DynamicImage::ImageLuma8(self.image),
                Position::Center,
                Dither::Threshold,
                &self.options,
            ),
            PixelFormat::Gray4 => {
                let bytes = self
                    .image
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
                printer.prepare(bytes, self.format, &self.options)
            }
        }
    }

    /// Prepare and expand pixels offline without a connection or runtime.
    pub fn preview(self) -> meowprint::Result<GrayImage> {
        let mut printer = Printer::new(NoOpTransport, self.driver);
        Ok(self.prepare(&mut printer)?.preview())
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
        };
        Ok(RenderedContent {
            driver,
            image,
            format,
            options,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use meowprint::{Gt01, Printer, Transport, TransportEvent, WriteChannel, X6h};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct NoIo(Arc<AtomicUsize>);

    #[async_trait]
    impl Transport for NoIo {
        async fn write(&mut self, _: WriteChannel, _: &[u8]) -> meowprint::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(meowprint::Error::Transport("Unexpected write".into()))
        }

        async fn event(&mut self) -> meowprint::Result<TransportEvent> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(meowprint::Error::Transport("Unexpected event read".into()))
        }

        async fn disconnect(&mut self) -> meowprint::Result<()> {
            Ok(())
        }
    }

    fn compare_previews(driver: impl Driver, fixture: &str, grayscale: bool) -> Result<()> {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture);
        let content = || Content::Image {
            path: source.clone(),
            dither: DitherArg::Threshold,
            grayscale,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let mut printer = Printer::new(NoIo(Arc::clone(&calls)), driver);
        let connected = content()
            .render(driver, Default::default())?
            .prepare(&mut printer)?
            .preview();
        let offline = content().render(driver, Default::default())?.preview()?;
        assert_eq!(connected, offline);
        assert_eq!(offline, image::open(source)?.to_luma8());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[test]
    fn prepared_previews_match_offline_and_connected_sessions_without_io() -> Result<()> {
        compare_previews(Gt01, "image.png", false)?;
        compare_previews(X6h, "gray.png", true)
    }
}
