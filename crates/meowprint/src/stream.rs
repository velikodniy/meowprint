//! Streaming printer submissions.

use crate::session::{Operation, OperationKind};
use crate::{Driver, PixelFormat, PrintReport, Printer, Raster, Result, RowSource, Transport};

/// Reads exactly the declared number of packed rows from a source.
pub struct RowReader<'a, S> {
    source: &'a mut S,
    remaining: u32,
    row_bytes: usize,
}

impl<'a, S: RowSource> RowReader<'a, S> {
    pub(crate) const fn new(
        source: &'a mut S,
        height: u32,
        width: u32,
        format: PixelFormat,
    ) -> Self {
        let row_bytes = match format {
            PixelFormat::Mono => width / 8,
            PixelFormat::Gray4 => width / 2,
        };
        Self {
            source,
            remaining: height,
            row_bytes: row_bytes as usize,
        }
    }

    /// Fill as many whole rows as fit, up to the declared height.
    pub(crate) async fn fill<T: Transport, D: Driver>(
        &mut self,
        operation: &mut Operation<'_, T, D>,
        output: &mut [u8],
    ) -> Result<usize> {
        if self.remaining == 0 {
            return Ok(0);
        }
        let row_bytes = self.row_bytes;
        let capacity = output.len() / row_bytes;
        let count = capacity.min(self.remaining as usize);
        for row in output[..count * row_bytes].chunks_exact_mut(row_bytes) {
            operation.read_row(self.source, row).await?;
            self.remaining -= 1;
        }
        Ok(count)
    }
}

impl<T: Transport, D: Driver> Printer<T, D> {
    /// Stream packed rows to the printer.
    ///
    /// Metadata and options are validated before rows are requested or printer I/O
    /// begins. The printer, source, and options remain borrowed until submission
    /// settles. Exactly the declared height is read, with no extra EOF probe.
    /// The source owns input deadlines; an unpaused source can wait indefinitely.
    ///
    /// # Errors
    /// Invalid metadata or options cause no source reads or transport I/O and
    /// leave the printer usable. Source and encoding failures after submission
    /// begins disable it, even before the first write; earlier output may print.
    /// A genuine printer condition rejected before a write can leave it usable.
    /// Communication failures, cancellation, and dropping an active future disable
    /// the connection. Obtain cancellation before starting, keep awaiting after
    /// cancellation, and always await [`Self::disconnect`], including after errors.
    pub async fn print<S: RowSource>(
        &mut self,
        source: &mut S,
        options: &D::Options,
    ) -> Result<PrintReport> {
        self.session.ensure_usable()?;
        let width = source.width();
        let height = source.height();
        let format = source.format();
        if width != self.driver.printable_width() {
            return Err(crate::error::invalid(
                "The row source width does not match the printer width.",
            ));
        }
        Raster::validate_dimensions(width, height)?;
        if height > self.driver.max_height() {
            return Err(crate::error::invalid(
                "The row source exceeds the printer height limit.",
            ));
        }
        let mut stream = self.driver.stream(height, format, options)?;
        let mut operation = self.session.begin(self.driver, OperationKind::Submit)?;
        let mut rows = RowReader::new(source, height, width, format);
        let mut submitted = 0;
        let result = D::submit(&mut operation, &mut stream, &mut rows, &mut submitted)
            .await
            .map(|completion| PrintReport {
                image_rows: height,
                bytes_submitted: submitted,
                completion,
            });
        operation.finish(result).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::framing::{Decoder, encode};
    use crate::{
        CommonRaw, CommonRawOptions, Completion, Error, Gt01, Gt01Options, Mx10, Mx10Options,
        Mxw01, Mxw01Options, Mxw01ReplyFormat, PixelFormat, PrefixedTiny, PrefixedTinyOptions,
        PrinterState, TinyRle, TinyRleOptions, TransportEvent, V5g, WriteChannel, X6h,
    };
    use std::{collections::VecDeque, future::poll_fn, task::Poll};

    struct Pixels {
        format: PixelFormat,
        height: u32,
        bytes: Vec<u8>,
        cursor: usize,
    }
    impl RowSource for Pixels {
        fn width(&self) -> u32 {
            384
        }
        fn height(&self) -> u32 {
            self.height
        }
        fn format(&self) -> PixelFormat {
            self.format
        }
        fn read_row(&mut self, output: &mut [u8]) -> impl Future<Output = Result<()>> + Send {
            if output.len() != self.row_bytes() || self.cursor == self.bytes.len() {
                return std::future::ready(Err(crate::error::invalid("invalid test row request")));
            }
            let end = self.cursor + output.len();
            output.copy_from_slice(&self.bytes[self.cursor..end]);
            self.cursor = end;
            std::future::ready(Ok(()))
        }
    }
    impl Pixels {
        fn new(format: PixelFormat, rows: u32) -> Self {
            let row_bytes = match format {
                PixelFormat::Mono => 48,
                PixelFormat::Gray4 => 192,
            };
            Self {
                format,
                height: rows,
                bytes: vec![0x81; row_bytes * rows as usize],
                cursor: 0,
            }
        }
        fn row_bytes(&self) -> usize {
            match self.format {
                PixelFormat::Mono => 48,
                PixelFormat::Gray4 => 192,
            }
        }
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
    impl Transport for Mock {
        fn write(
            &mut self,
            channel: WriteChannel,
            bytes: &[u8],
        ) -> impl Future<Output = Result<()>> + Send {
            poll_fn(move |_| {
                self.attempts += 1;
                if self.fail_write {
                    return Poll::Ready(Err(Error::Transport("injected failure".into())));
                }
                self.writes.push((channel, bytes.to_vec()));
                if channel == WriteChannel::Control {
                    for (_, frame) in self.decoder.feed_positioned(bytes) {
                        let mxw01 = self.header == [0x22, 0x21];
                        let (command, payload) = match (mxw01, frame.command) {
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
                            if mxw01 { 3 } else { 1 },
                            &payload,
                            255,
                        )?;
                        if !self.crc {
                            reply.remove(reply.len() - 2);
                        }
                        self.replies.push_back(TransportEvent::Notification(reply));
                    }
                }
                Poll::Ready(Ok(()))
            })
        }
        async fn event(&mut self) -> Result<TransportEvent> {
            if let Some(event) = self.replies.pop_front() {
                return Ok(event);
            }
            std::future::pending().await
        }
        fn disconnect(&mut self) -> impl Future<Output = Result<()>> + Send {
            poll_fn(move |_| {
                self.disconnected = true;
                Poll::Ready(Ok(()))
            })
        }
    }

    async fn wire<D: Driver>(
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
        let mut source = Pixels::new(format, 1);
        printer.print(&mut source, options).await?;
        let mut bytes = Vec::new();
        for (channel, write) in &printer.session.transport.writes {
            bytes.push(u8::from(*channel == WriteChannel::Raster));
            bytes.extend_from_slice(
                &u32::try_from(write.len())
                    .map_err(|_| crate::error::invalid("fixture length"))?
                    .to_le_bytes(),
            );
            bytes.extend_from_slice(write);
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{name}.bin"));
        assert_eq!(
            bytes,
            std::fs::read(path).map_err(|e| Error::Transport(e.to_string()))?,
            "wire fixture {name}"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn reference_command_streams() -> Result<()> {
        use crate::{Gt01Options, V5gOptions, X6hOptions};
        wire(Gt01, PixelFormat::Mono, &Gt01Options::default(), "gt01").await?;
        wire(
            Gt01,
            PixelFormat::Mono,
            &Gt01Options {
                feed: 0,
                ..Default::default()
            },
            "gt01-no-feed",
        )
        .await?;
        wire(Mx10, PixelFormat::Mono, &Mx10Options::default(), "mx10").await?;
        wire(
            CommonRaw,
            PixelFormat::Mono,
            &CommonRawOptions::default(),
            "common-raw",
        )
        .await?;
        wire(
            TinyRle,
            PixelFormat::Mono,
            &TinyRleOptions::default(),
            "tiny-rle",
        )
        .await?;
        wire(
            PrefixedTiny,
            PixelFormat::Mono,
            &PrefixedTinyOptions::default(),
            "prefixed-tiny",
        )
        .await?;
        wire(X6h, PixelFormat::Mono, &X6hOptions::default(), "x6h-mono").await?;
        wire(X6h, PixelFormat::Gray4, &X6hOptions::default(), "x6h-gray").await?;
        wire(V5g, PixelFormat::Mono, &V5gOptions::default(), "v5g-mono").await?;
        wire(V5g, PixelFormat::Gray4, &V5gOptions::default(), "v5g-gray").await?;
        wire(
            Mxw01::default(),
            PixelFormat::Mono,
            &Mxw01Options::default(),
            "mxw01",
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn streaming_and_queries_work_for_both_families() -> Result<()> {
        for (driver, header, crc, completion) in [
            (
                Mxw01::default(),
                [0x22, 0x21],
                true,
                Completion::PrinterComplete,
            ),
            (
                Mxw01 {
                    reply_format: Mxw01ReplyFormat::WithoutCrc,
                },
                [0x22, 0x21],
                false,
                Completion::PrinterComplete,
            ),
        ] {
            let mut printer = Printer::new(Mock::new(header, crc), driver);
            let mut source = Pixels::new(PixelFormat::Mono, 1);
            let report = printer.print(&mut source, &Mxw01Options::default()).await?;
            assert_eq!(report.image_rows, 1);
            assert_eq!(report.completion, completion);
            assert!(report.bytes_submitted > 0);
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
            assert_eq!(printer.status().await?.state, PrinterState::Ready);
            assert_eq!(
                printer.device_info().await?.firmware_version.as_deref(),
                Some("v1")
            );
            assert!(printer.is_usable());
        }
        let mut printer = Printer::new(Mock::new([0x51, 0x78], true), Gt01);
        let mut source = Pixels::new(PixelFormat::Mono, 1);
        let report = printer.print(&mut source, &Gt01Options::default()).await?;
        assert_eq!(report.completion, Completion::TimedDrain);
        assert_eq!(printer.status().await?.state, PrinterState::Ready);
        assert_eq!(
            printer.device_info().await?.description.as_deref(),
            Some("firmware 1")
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn failed_write_disables_connection_without_retrying() -> Result<()> {
        let mut transport = Mock::new([0x51, 0x78], true);
        transport.fail_write = true;
        let mut printer = Printer::new(transport, Gt01);
        let mut source = Pixels::new(PixelFormat::Mono, 1);
        assert!(matches!(
            printer.print(&mut source, &Gt01Options::default()).await,
            Err(Error::Transport(_))
        ));
        assert!(!printer.is_usable());
        assert!(printer.session.transport.disconnected);
        assert_eq!(printer.session.transport.attempts, 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn controls_are_applied_per_stream_and_connection_is_reusable() -> Result<()> {
        let mut printer = Printer::new(Mock::new([0x51, 0x78], true), Gt01);
        let mut options = Gt01Options::default();
        let mut source = Pixels::new(PixelFormat::Mono, 1);
        printer.print(&mut source, &options).await?;
        let first = printer.session.transport.writes.clone();
        printer.session.transport.writes.clear();
        let mut source = Pixels::new(PixelFormat::Mono, 1);
        printer.print(&mut source, &Gt01Options::default()).await?;
        assert_eq!(first, printer.session.transport.writes);
        assert!(printer.is_usable());
        assert!(!printer.session.transport.disconnected);
        printer.session.transport.writes.clear();
        options.energy = 42;
        let mut source = Pixels::new(PixelFormat::Mono, 1);
        printer.print(&mut source, &options).await?;
        assert_ne!(first, printer.session.transport.writes);
        Ok(())
    }
}

// Compile-fail contracts stay out of the ordinary printing guide.
#[cfg(doctest)]
mod compile_fail_examples {
    /// A live print future exclusively borrows its printer:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01, Gt01Options, Printer, RowSource, Transport};
    /// async fn printer_borrow<T: Transport, S: RowSource>(printer: &mut Printer<T, Gt01>, source: &mut S) -> meowprint::Result<()> {
    ///     let options = Gt01Options::default();
    ///     let active = printer.print(source, &options);
    ///     printer.status().await?;
    ///     active.await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// Its row source is exclusively borrowed too:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01, Gt01Options, Printer, RowSource, Transport};
    /// async fn source_borrow<T: Transport, S: RowSource>(printer: &mut Printer<T, Gt01>, source: &mut S) -> meowprint::Result<()> {
    ///     let options = Gt01Options::default();
    ///     let active = printer.print(source, &options);
    ///     source.read_row(&mut [0; 48]).await?;
    ///     active.await?;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// The selected driver determines the control type:
    ///
    /// ```compile_fail
    /// use meowprint::{Gt01, Mx10Options, Printer, RowSource, Transport};
    /// async fn wrong_options<T: Transport, S: RowSource>(printer: &mut Printer<T, Gt01>, source: &mut S) -> meowprint::Result<()> {
    ///     printer.print(source, &Mx10Options::default()).await?;
    ///     Ok(())
    /// }
    /// ```
    struct Ownership;
}
