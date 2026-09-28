//! Public print-job preparation without a device or runtime.
use async_trait::async_trait;
#[cfg(feature = "image")]
use image::{DynamicImage, GrayImage, Luma};
#[cfg(feature = "image")]
use meowprint::{Dither, Position};
use meowprint::{
    Driver, Error, Gt01, Gt01Options, PixelFormat, Printer, Result, Transport, TransportEvent,
    WriteChannel,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

struct NoIo(Arc<AtomicUsize>);
#[async_trait]
impl Transport for NoIo {
    async fn write(&mut self, _: WriteChannel, _: &[u8]) -> Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Err(Error::Transport("Unexpected write".into()))
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Err(Error::Transport("Unexpected event wait".into()))
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
fn printer<D: Driver>(driver: D) -> (Printer<NoIo, D>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    (Printer::new(NoIo(calls.clone()), driver), calls)
}

#[test]
fn preparation_errors_and_discarded_jobs_leave_the_connection_usable() -> TestResult {
    let (mut printer, calls) = printer(Gt01);
    for (pixels, format, options) in [
        (vec![], PixelFormat::Mono, Gt01Options::default()),
        (vec![0; 49], PixelFormat::Mono, Gt01Options::default()),
        (vec![0; 192], PixelFormat::Gray4, Gt01Options::default()),
        (
            vec![0; 48],
            PixelFormat::Mono,
            Gt01Options {
                speed: 0,
                ..Gt01Options::default()
            },
        ),
    ] {
        assert!(printer.prepare(pixels, format, &options).is_err());
        assert!(printer.is_usable());
    }
    drop(printer.prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default())?);
    assert!(printer.is_usable());
    // Dropping an unpolled print future must also leave the connection usable.
    drop(
        printer
            .prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default())?
            .print(),
    );
    assert!(printer.is_usable());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[tokio::test]
async fn cancellation_between_preparation_and_print_prevents_io() -> TestResult {
    let (mut printer, calls) = printer(Gt01);
    let cancellation = printer.cancellation();
    let job = printer.prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default())?;
    cancellation.cancel();
    assert!(matches!(job.print().await, Err(Error::UnusableConnection)));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(!printer.is_usable());
    Ok(())
}

#[tokio::test]
async fn offline_transport_rejects_submission() -> TestResult {
    async fn check<D: Driver>(driver: D) -> TestResult {
        let mut printer = Printer::new(meowprint::NoOpTransport, driver);
        let result = printer
            .prepare(vec![0; 48], PixelFormat::Mono, &Default::default())?
            .print()
            .await;
        assert!(matches!(result, Err(Error::Transport(_))));
        assert!(!printer.is_usable());
        printer.disconnect().await?;
        Ok(())
    }
    check(Gt01).await?;
    check(meowprint::Mxw01::default()).await
}

#[cfg(feature = "image")]
#[test]
fn image_preparation_positions_pixels_and_rejects_invalid_input() -> TestResult {
    let (mut printer, calls) = printer(Gt01);
    for (width, height) in [(0, 1), (1, 0), (385, 1), (1, 32769)] {
        let source = DynamicImage::new_luma8(width, height);
        assert!(matches!(
            printer.prepare_from_image(
                &source,
                Position::Center,
                Dither::default(),
                &Gt01Options::default(),
            ),
            Err(Error::InvalidInput(_))
        ));
        assert!(printer.is_usable());
    }
    let width = printer.printable_width();
    let source = DynamicImage::ImageLuma8(GrayImage::from_pixel(1, 1, Luma([0])));
    let job = printer.prepare_from_image(
        &source,
        Position::Right,
        Dither::Threshold,
        &Gt01Options::default(),
    )?;
    let preview = job.preview();
    assert_eq!(preview.dimensions(), (width, 1));
    assert!(preview.as_raw()[..383].iter().all(|&pixel| pixel == 255));
    assert_eq!(preview.as_raw()[383], 0);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[cfg(feature = "image")]
#[test]
fn preview_preserves_bit_nibble_order_and_all_gray_levels() -> TestResult {
    let (mut mono, mono_calls) = printer(Gt01);
    let mut bytes = vec![0; 96];
    bytes[0] = 0x81;
    bytes[48] = 0x02;
    let job = mono.prepare(bytes, PixelFormat::Mono, &Gt01Options::default())?;
    let mut preview = job.preview();
    assert_eq!(
        &preview.as_raw()[..8],
        &[0, 255, 255, 255, 255, 255, 255, 0]
    );
    assert_eq!(preview.get_pixel(1, 1)[0], 0);
    preview.put_pixel(0, 0, Luma([255]));
    assert_eq!(job.preview().get_pixel(0, 0)[0], 0);
    drop(job);
    assert!(mono.is_usable());
    assert_eq!(mono_calls.load(Ordering::Relaxed), 0);
    let (mut gray, gray_calls) = printer(meowprint::X6h);
    let bytes = (0..192)
        .map(|index| {
            let left = (index * 2) % 16;
            u8::try_from(left | ((left + 1) << 4)).unwrap_or_default()
        })
        .collect();
    let job = gray.prepare(bytes, PixelFormat::Gray4, &meowprint::X6hOptions::default())?;
    assert_eq!(
        &job.preview().as_raw()[..16],
        &[
            255, 238, 221, 204, 187, 170, 153, 136, 119, 102, 85, 68, 51, 34, 17, 0
        ]
    );
    assert_eq!(gray_calls.load(Ordering::Relaxed), 0);
    Ok(())
}

#[test]
fn public_height_bound_matches_preparation() -> TestResult {
    let (mut printer, calls) = printer(Gt01);
    let row_bytes = usize::try_from(Gt01.printable_width() / 8)?;
    let height = usize::try_from(Gt01.max_height())?;
    drop(printer.prepare(
        vec![0; row_bytes * height],
        PixelFormat::Mono,
        &Gt01Options::default(),
    )?);
    assert!(
        printer
            .prepare(
                vec![0; row_bytes * (height + 1)],
                PixelFormat::Mono,
                &Gt01Options::default()
            )
            .is_err()
    );
    assert!(printer.is_usable());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    Ok(())
}
