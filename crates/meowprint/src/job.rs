//! A prepared job bound to the printer that created it.

#[cfg(feature = "image")]
use crate::Raster;
use crate::{Driver, PrintReport, Printer, Result, Transport};

/// An image ready to print on the printer that prepared it.
///
/// Create a job with [`Printer::prepare`]. Preparation finishes validation and
/// compression without communicating with the printer. Later changes to the
/// original options do not affect the job.
///
/// Call [`Self::print`] to consume and submit the job. Dropping an unprinted job
/// sends no bytes and releases the printer for another operation.
/// Finish or discard the job before using the printer for another operation.
/// Each job can print only once.
///
/// With the `image` feature, `Printer::prepare_from_image()` accepts decoded
/// images and `preview()` returns the prepared pixels as an image.
#[must_use = "Call print() to submit the prepared job, or drop it to discard it."]
pub struct PrintJob<'a, T, D: Driver> {
    pub(super) printer: &'a mut Printer<T, D>,
    pub(super) job: D::Job,
    pub(super) image_rows: u32,
    #[cfg(feature = "image")]
    pub(super) raster: Raster,
}

impl<T: Transport, D: Driver> PrintJob<'_, T, D> {
    /// Submit this job to the printer that prepared it.
    ///
    /// This method consumes the job and requires a Tokio runtime.
    /// The report describes how printing finished. It does not guarantee print quality.
    /// After success, the printer accepts another job.
    /// Obtain [`Printer::cancellation`] before preparing a job to stop an active print.
    ///
    /// # Errors
    /// A cancellation request before printing returns [`crate::Error::UnusableConnection`] without writing.
    /// A known printer condition rejected before a write leaves the connection usable.
    /// Any error after a write attempt disables the connection.
    /// Communication failures, timeouts, and cancellation disable the connection.
    /// Dropping this future after it starts also disables the connection.
    /// Use [`Printer::is_usable`] to decide whether to reconnect after an error.
    /// Always await [`Printer::disconnect`] when you finish, including after errors.
    pub async fn print(self) -> Result<PrintReport> {
        let mut operation = self
            .printer
            .session
            .begin(self.printer.driver, crate::session::OperationKind::Submit)?;
        let mut submitted = 0;
        let result = D::submit(&mut operation, &self.job, &mut submitted)
            .await
            .map(|completion| PrintReport {
                image_rows: self.image_rows,
                bytes_submitted: submitted,
                completion,
            });
        operation.finish(result).await
    }

    /// Return a grayscale image of the prepared pixels.
    ///
    /// Available with the `image` feature. Each call allocates a new image.
    /// Zero is black and 255 is white. The preview excludes blank rows and paper
    /// feed added by the driver. It does not predict physical print quality.
    /// Generating or editing the preview does not change the prepared job.
    #[cfg(feature = "image")]
    #[must_use]
    pub fn preview(&self) -> image::GrayImage {
        self.raster.preview()
    }
}

#[cfg(test)]
mod tests {
    use crate::driver::framing::{Decoder, encode};
    use crate::{
        Completion, Driver, Error, Gt01, Gt01Options, Mxw01, Mxw01ReplyFormat, PixelFormat,
        Printer, PrinterState, Result, WriteChannel,
    };
    use crate::{Transport, TransportEvent};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    async fn check<D: Driver>(
        driver: D,
        header: [u8; 2],
        crc: bool,
        completion: Completion,
    ) -> Result<()> {
        let mut printer = Printer::new(Mock::new(header, crc), driver);
        let report = printer
            .prepare(vec![0x81; 48], PixelFormat::Mono, &Default::default())?
            .print()
            .await?;
        assert_eq!(report.image_rows, 1);
        assert_eq!(report.completion, completion);
        assert!(report.bytes_submitted > 0);
        assert_eq!(
            report.bytes_submitted,
            printer
                .session
                .transport
                .writes
                .iter()
                .map(|(_, bytes)| bytes.len())
                .sum()
        );
        assert!(printer.is_usable());
        if header == [0x22, 0x21] {
            let raster: Vec<_> = printer
                .session
                .transport
                .writes
                .iter()
                .filter(|(channel, _)| *channel == WriteChannel::Raster)
                .flat_map(|(_, bytes)| bytes.iter().copied())
                .collect();
            assert_eq!(raster.len(), 90 * 48);
            assert_eq!(&raster[48..96], &[0x81; 48]);
        }
        assert_eq!(printer.status().await?.state, PrinterState::Ready);
        let info = printer.device_info().await?;
        if header == [0x22, 0x21] {
            assert_eq!(info.firmware_version.as_deref(), Some("v1"));
        } else {
            assert_eq!(info.description.as_deref(), Some("firmware 1"));
        }
        assert!(printer.is_usable());
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn print_and_queries_work_for_both_families() -> Result<()> {
        check(Gt01, [0x51, 0x78], true, Completion::TimedDrain).await?;
        check(
            Mxw01::default(),
            [0x22, 0x21],
            true,
            Completion::PrinterComplete,
        )
        .await?;
        check(
            Mxw01 {
                reply_format: Mxw01ReplyFormat::WithoutCrc,
            },
            [0x22, 0x21],
            false,
            Completion::PrinterComplete,
        )
        .await
    }
    #[tokio::test(start_paused = true)]
    async fn failed_write_disables_connection_without_retrying() -> Result<()> {
        let mut transport = Mock::new([0x51, 0x78], true);
        transport.fail_write = true;
        let mut printer = Printer::new(transport, Gt01);
        let result = printer
            .prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default())?
            .print()
            .await;
        assert!(matches!(result, Err(Error::Transport(_))));
        assert!(!printer.is_usable());
        assert!(printer.session.transport.disconnected);
        assert!(matches!(
            printer.prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default()),
            Err(Error::UnusableConnection)
        ));
        assert_eq!(printer.session.transport.attempts, 1);
        Ok(())
    }

    struct Mock {
        decoder: Decoder,
        header: [u8; 2],
        crc: bool,
        replies: VecDeque<TransportEvent>,
        writes: Vec<(WriteChannel, Vec<u8>)>,
        attempts: usize,
        fail_write: bool,
        disconnected: bool,
    }
    impl Mock {
        fn new(header: [u8; 2], crc: bool) -> Self {
            Self {
                decoder: Decoder::new(header, 255, true),
                header,
                crc,
                replies: VecDeque::new(),
                writes: Vec::new(),
                attempts: 0,
                fail_write: false,
                disconnected: false,
            }
        }
    }
    #[async_trait]
    impl Transport for Mock {
        async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()> {
            self.attempts += 1;
            if self.fail_write {
                return Err(Error::Transport("injected failure".into()));
            }
            self.writes.push((channel, bytes.to_vec()));
            if channel == WriteChannel::Control {
                for (_, frame) in self.decoder.feed_positioned(bytes) {
                    let is_mxw01 = self.header == [0x22, 0x21];
                    let (command, payload) = match (is_mxw01, frame.command) {
                        (true, 0xa1) => (0xa1, vec![0; 13]),
                        (true, 0xa9) => (0xa9, vec![0]),
                        (true, 0xad) => (0xaa, vec![0]),
                        (true, 0xb1) => (0xb1, b"v1\0\0".to_vec()),
                        (false, 0xa3) => (0xa3, vec![0]),
                        (false, 0xa8) => (0xa8, b"firmware 1".to_vec()),
                        _ => continue,
                    };
                    let mut reply = encode(
                        self.header,
                        command,
                        if is_mxw01 { 3 } else { 1 },
                        &payload,
                        255,
                    )?;
                    if !self.crc {
                        reply.remove(reply.len() - 2);
                    }
                    self.replies.push_back(TransportEvent::Notification(reply));
                }
            }
            Ok(())
        }
        async fn event(&mut self) -> Result<TransportEvent> {
            if let Some(event) = self.replies.pop_front() {
                return Ok(event);
            }
            std::future::pending().await
        }
        async fn disconnect(&mut self) -> Result<()> {
            self.disconnected = true;
            Ok(())
        }
    }

    mod wire_snapshots {
        //! Command streams captured before the session refactor.
        use super::Mock;
        use crate::{Driver, PixelFormat, Printer, Result};

        async fn check<D: Driver>(
            driver: D,
            format: PixelFormat,
            options: &D::Options,
            name: &str,
        ) -> Result<()> {
            let mxw01 = driver.name() == "mxw01";
            let mut printer = Printer::new(
                Mock::new(if mxw01 { [0x22, 0x21] } else { [0x51, 0x78] }, true),
                driver,
            );
            let count = if format == PixelFormat::Mono { 48 } else { 192 };
            printer
                .prepare(vec![0x81; count], format, options)?
                .print()
                .await?;
            let mut stream = Vec::new();
            for (channel, bytes) in &printer.session.transport.writes {
                stream.push(u8::from(*channel == crate::WriteChannel::Raster));
                stream.extend_from_slice(
                    &u32::try_from(bytes.len())
                        .map_err(|_| crate::error::invalid("fixture length"))?
                        .to_le_bytes(),
                );
                stream.extend_from_slice(bytes);
            }
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(format!("{name}.bin"));
            assert_eq!(
                stream,
                std::fs::read(path).map_err(|e| crate::Error::Transport(e.to_string()))?,
                "wire fixture {name}"
            );
            Ok(())
        }

        #[tokio::test(start_paused = true)]
        async fn reference_command_streams() -> Result<()> {
            use crate::*;
            check(Gt01, PixelFormat::Mono, &Gt01Options::default(), "gt01").await?;
            check(
                Gt01,
                PixelFormat::Mono,
                &Gt01Options {
                    feed: 0,
                    ..Gt01Options::default()
                },
                "gt01-no-feed",
            )
            .await?;
            check(Mx10, PixelFormat::Mono, &Mx10Options::default(), "mx10").await?;
            check(
                CommonRaw,
                PixelFormat::Mono,
                &CommonRawOptions::default(),
                "common-raw",
            )
            .await?;
            check(
                TinyRle,
                PixelFormat::Mono,
                &TinyRleOptions::default(),
                "tiny-rle",
            )
            .await?;
            check(
                PrefixedTiny,
                PixelFormat::Mono,
                &PrefixedTinyOptions::default(),
                "prefixed-tiny",
            )
            .await?;
            check(X6h, PixelFormat::Mono, &X6hOptions::default(), "x6h-mono").await?;
            check(X6h, PixelFormat::Gray4, &X6hOptions::default(), "x6h-gray").await?;
            check(V5g, PixelFormat::Mono, &V5gOptions::default(), "v5g-mono").await?;
            check(V5g, PixelFormat::Gray4, &V5gOptions::default(), "v5g-gray").await?;
            check(
                Mxw01::default(),
                PixelFormat::Mono,
                &Mxw01Options::default(),
                "mxw01",
            )
            .await
        }

        #[tokio::test(start_paused = true)]
        async fn prepared_controls_are_fixed_and_jobs_reuse_the_connection() -> Result<()> {
            use crate::{Gt01, Gt01Options};
            let mut printer = Printer::new(Mock::new([0x51, 0x78], true), Gt01);
            let mut options = Gt01Options::default();
            let job = printer.prepare(vec![0x81; 48], PixelFormat::Mono, &options)?;
            options.energy = 42;
            job.print().await?;
            let first = printer.session.transport.writes.clone();
            printer.session.transport.writes.clear();
            printer
                .prepare(vec![0x81; 48], PixelFormat::Mono, &Gt01Options::default())?
                .print()
                .await?;
            assert_eq!(first, printer.session.transport.writes);
            assert!(printer.is_usable());
            assert!(!printer.session.transport.disconnected);
            printer.session.transport.writes.clear();
            printer
                .prepare(vec![0x81; 48], PixelFormat::Mono, &options)?
                .print()
                .await?;
            assert_ne!(first, printer.session.transport.writes);
            Ok(())
        }
    }
}

// Keep API misuse examples as doctests without displaying them in the user guide.
#[cfg(doctest)]
mod compile_fail_examples {
    /// The job has no destination parameter, even when printers have identical drivers:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01, PixelFormat, Printer, Transport};
    /// async fn wrong_printer<T: Transport>(first: &mut Printer<T, Gt01>, second: &mut Printer<T, Gt01>) -> meowprint::Result<()> {
    ///     let job = first.prepare(vec![0; 48], PixelFormat::Mono, &Default::default())?;
    ///     job.print(second).await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// A pending job keeps exclusive access to its printer:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01, PixelFormat, Printer, Transport};
    /// async fn simultaneous_access<T: Transport>(printer: &mut Printer<T, Gt01>) -> meowprint::Result<()> {
    ///     let job = printer.prepare(vec![0; 48], PixelFormat::Mono, &Default::default())?;
    ///     printer.status().await?;
    ///     job.print().await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// Printing consumes the job:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01, PixelFormat, Printer, Transport};
    /// async fn print_twice<T: Transport>(printer: &mut Printer<T, Gt01>) -> meowprint::Result<()> {
    ///     let job = printer.prepare(vec![0; 48], PixelFormat::Mono, &Default::default())?;
    ///     job.print().await?;
    ///     job.print().await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// Options must belong to this printer's driver:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01Options, Mxw01, PixelFormat, Printer, Transport};
    /// fn wrong_options<T: Transport>(printer: &mut Printer<T, Mxw01>) {
    ///     let _ = printer.prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default());
    /// }
    /// ```
    ///
    struct BorrowingRules;
}
