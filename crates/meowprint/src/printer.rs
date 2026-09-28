//! Print jobs, queries, and paper movement over one connection.
//!
//! [`Printer`] uses a [`Driver`] to communicate through a [`Transport`].
//! For native Bluetooth, use `Device::connect` to obtain a printer.
//!
//! Run one operation at a time. After a successful print or query, you can reuse
//! the printer. See [`Printer`] for error handling and connection cleanup.
//!
//! [`PrintReport`] describes how printing finished.
//! [`Cancellation`] requests that an active operation stop and close its connection.
//! Implement [`Transport`] only when your application needs a different connection
//! backend or a simulated printer.

pub use crate::job::PrintJob;

use crate::{
    DeviceInfo, Driver, PixelFormat, PrinterStatus, Raster, Result,
    session::{OperationKind, Session},
};
use async_trait::async_trait;
use tokio::sync::watch;

/// The logical destination of bytes passed to [`Transport::write`].
///
/// The transport maps each variant to the endpoint for the selected driver.
/// Most drivers send both commands and image data through [`Self::Control`].
/// MXW01 uses a separate [`Self::Raster`] endpoint for image data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteChannel {
    /// Framed control commands. Common raster frames also use this channel.
    Control,
    /// Unframed MXW01 pixels through the `AE03` characteristic.
    Raster,
}

/// Incoming bytes or a lost connection, returned by [`Transport::event`].
///
/// A notification is a byte message from the printer. It can contain part of a
/// protocol message or several messages. Preserve all bytes in their original
/// order. [`Printer`] assembles and interprets the messages.
#[derive(Debug)]
pub enum TransportEvent {
    /// Exact bytes from the driver notification endpoint.
    Notification(Vec<u8>),
    /// The link closed or its services changed.
    Disconnected,
}

/// The connection interface used by [`Printer`] to send and receive bytes.
///
/// For native Bluetooth, obtain a `BleTransport` through `Device::connect`.
/// Implement this trait for another backend or for tests. Before constructing a
/// printer, connect the backend and subscribe to the selected driver notification
/// endpoint. [`Printer`] handles framing, write limits, pacing, and replies.
///
/// Implementations must preserve write order and report the selected connection
/// events. A successful write means that the local backend accepted the bytes.
/// It does not prove that the printer received or printed them.
///
/// [`Transport::event`] must be safe to cancel: dropping its future must not lose
/// an event or prevent the next call from receiving it. [`Printer`] cancels event
/// waits when a deadline expires. Writes also have time limits, and the library
/// never retries a failed or timed-out write.
///
/// # Example
///
/// This in-memory transport records writes and sends no replies. A GT01 print
/// finishes after its normal wait. Real transports must also report incoming events.
/// Add `async-trait = "0.1"` to your dependencies to implement this trait.
#[doc = concat!(
    "\n```no_run\n",
    include_str!("../examples/custom_transport.rs"),
    "\n```\n",
)]
#[async_trait]
pub trait Transport: Send {
    /// Return the maximum byte count that one [`Self::write`] call accepts.
    ///
    /// The default is 20 bytes. Return a nonzero limit for the active connection.
    /// [`Printer`] also enforces the selected driver limit.
    fn max_write_size(&self) -> usize {
        20
    }
    /// Submit bytes to the selected endpoint, in order.
    ///
    /// `bytes` contains one chunk, which can be only part of a protocol frame.
    /// Do not add framing or retry after an uncertain result.
    ///
    /// # Errors
    /// Return [`crate::Error::Transport`] on backend failure. Partial submission has an unknown outcome.
    async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()>;
    /// Wait for one event from the selected notification endpoint or link.
    ///
    /// Preserve partial frames and unknown bytes. This method must be safe to
    /// cancel and call again without losing an event.
    ///
    /// # Errors
    /// Return [`crate::Error::Transport`] when the event stream fails.
    async fn event(&mut self) -> Result<TransportEvent>;
    /// Close this connection.
    ///
    /// # Errors
    /// Return [`crate::Error::Transport`] if the backend cannot close the link.
    async fn disconnect(&mut self) -> Result<()>;
}

/// A transport for offline preparation and previews, with no connection or I/O.
///
/// Pass it to [`Printer::new`] with the selected driver. Preparation and preview
/// work without Bluetooth access or a Tokio runtime. Writes and event reads
/// return [`crate::Error::Transport`], so an attempted print cannot report success.
/// Disconnecting succeeds without doing anything.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpTransport;

#[async_trait]
impl Transport for NoOpTransport {
    async fn write(&mut self, _: WriteChannel, _: &[u8]) -> Result<()> {
        Err(crate::Error::Transport(
            "Offline transport cannot write to a printer.".into(),
        ))
    }

    async fn event(&mut self) -> Result<TransportEvent> {
        Err(crate::Error::Transport(
            "Offline transport cannot receive printer events.".into(),
        ))
    }

    async fn disconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A shared handle that requests cancellation of a [`Printer`] operation.
///
/// Obtain a handle with [`Printer::cancellation`] before starting an operation.
/// Clones share the same request and control only this connection.
///
/// [`Self::cancel`] requests a stop but does not wait for cleanup. Continue awaiting
/// the operation until it returns. It attempts to disconnect before returning.
/// Cancellation can wait for a write to finish, so it is not immediate.
///
/// Cancellation permanently disables the connection, even when it is idle.
/// If the printer is idle, call [`Printer::disconnect`] after requesting cancellation.
/// The request cannot be reset, and previously submitted rows can still print.
#[derive(Debug, Clone)]
pub struct Cancellation {
    sender: watch::Sender<bool>,
}
impl Cancellation {
    pub(crate) fn new() -> Self {
        Self {
            sender: watch::channel(false).0,
        }
    }
}
impl Cancellation {
    /// Request that this connection stop its active operation.
    ///
    /// Keep awaiting the active operation until it returns and completes its
    /// cleanup attempt. If the printer is idle, call [`Printer::disconnect`].
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }
    /// Return whether this handle received a cancellation request.
    ///
    /// `true` does not mean that the operation stopped or the connection closed.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.sender.borrow()
    }
    pub(crate) async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        if !*receiver.borrow_and_update() {
            let _ = receiver.changed().await;
        }
    }
}

/// How a print or paper movement finished.
///
/// Read this value from [`PrintReport::completion`] before reporting completion
/// to a user. Drivers other than MXW01 wait for a fixed interval, with an optional
/// ready status. MXW01 waits for a completion message. No value guarantees print
/// quality or proves that every row reached the paper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// The printer reported readiness at the end of the job, and the wait finished.
    ///
    /// Readiness can mean that the printer accepts more data, even while it still prints.
    ReadyAfterEnd,
    /// The operation finished its wait without an observed fault or disconnect.
    ///
    /// The printer did not confirm completion. Paper movement also uses this result.
    TimedDrain,
    /// MXW01 sent its completion message after image submission.
    PrinterComplete,
}

/// The result of a successful print or paper movement.
///
/// This report describes the bytes that the transport accepted and the evidence
/// observed afterward. It is not a measurement of physical paper output.
/// Failed operations return [`crate::Error`] instead, even if some rows already printed.
#[derive(Debug, Clone)]
pub struct PrintReport {
    /// Source image height, excluding padding and paper feed.
    ///
    /// Blank rows inside the source image still count. Paper movement reports zero.
    pub image_rows: u32,
    /// Bytes accepted by successful transport writes, including commands, framing, and padding.
    pub bytes_submitted: usize,
    /// How the operation finished. See [`Completion`] for the limits of each result.
    pub completion: Completion,
}

/// A printer connection with a selected [`Driver`].
///
/// For Bluetooth, use `Device::connect`. For a custom connection, pass a connected
/// [`Transport`] to [`Self::new`]. Run prints, queries, and paper movement one at a time.
/// All asynchronous operations require a Tokio runtime.
///
/// [`Self::prepare`] creates a job that borrows this printer until printed or dropped.
/// Pass `&Default::default()` to use driver defaults.
/// An input or compression error at this stage leaves the connection usable.
/// After a successful print, query, or paper movement, you can reuse this printer.
///
/// Communication failures and dropping an active operation disable further use.
/// A known printer condition rejected before a write leaves the connection usable.
/// Use [`Self::is_usable`] to decide whether to reconnect after an error.
/// Status reads can report printer conditions without disabling the connection.
/// Always await [`Self::disconnect`] when you finish, including after errors.
pub struct Printer<T, D: Driver> {
    pub(crate) session: Session<T>,
    pub(crate) driver: D,
}

impl<T: Transport, D: Driver> Printer<T, D> {
    /// Create a printer from a transport and its matching driver.
    ///
    /// This constructor does not connect, subscribe, or send printer commands.
    /// For communication, connect and subscribe the transport first as described
    /// by [`Transport`]. For offline preparation, use [`NoOpTransport`].
    #[must_use]
    pub fn new(transport: T, driver: D) -> Self {
        Self {
            session: Session::new(transport, driver.decoder()),
            driver,
        }
    }

    /// Return this printer's print head width in dots.
    #[must_use]
    pub fn printable_width(&self) -> u32 {
        self.driver.printable_width()
    }
    /// Obtain a cancellation handle before borrowing this printer for an operation.
    ///
    /// The handle can be moved to another task. All returned handles control this
    /// connection, and a request remains active across subsequent method calls.
    #[must_use]
    pub fn cancellation(&self) -> Cancellation {
        self.session.cancellation()
    }
    /// Return whether this connection accepts another operation.
    ///
    /// This reads local connection state without contacting the printer.
    /// It does not mean that the printer is ready or that the next operation will succeed.
    #[must_use]
    pub fn is_usable(&self) -> bool {
        self.session.is_usable()
    }
    /// Prepare packed pixels for this printer without sending any bytes.
    ///
    /// This method takes ownership of `bytes`. The pixel format and byte count
    /// determine the height. Rows must fill [`Self::printable_width`] dots without padding.
    /// [`PixelFormat`] describes the pixel order and darkness values.
    ///
    /// Pass `&Default::default()` to use driver defaults. See the [driver reference](crate::driver)
    /// for supported controls. An optional density of `None` keeps the existing
    /// printer density. The job copies the controls, so later changes to `options`
    /// do not affect it.
    ///
    /// The returned [`PrintJob`] borrows this printer exclusively. Call
    /// [`PrintJob::print`] to submit it, or drop it to release the printer without printing.
    /// With the `image` feature, the job also provides a preview of its pixels.
    ///
    /// Images must contain complete rows, with a height from 1 through
    /// [`Driver::max_height`] rows. Preparation validates and compresses the image
    /// before returning. It needs no Tokio runtime.
    ///
    /// # Errors
    /// Return [`crate::Error::InvalidInput`] for incomplete rows, unsupported formats,
    /// invalid controls, or an empty or oversized image. Input and compression
    /// errors leave the connection usable.
    /// A previously disabled connection returns [`crate::Error::UnusableConnection`].
    pub fn prepare(
        &mut self,
        bytes: Vec<u8>,
        format: PixelFormat,
        options: &D::Options,
    ) -> Result<PrintJob<'_, T, D>> {
        self.session.ensure_usable()?;
        let raster = Raster::new(self.printable_width(), bytes, format)?;
        let job = self.driver.prepare(&raster, options)?;
        let image_rows = raster.height();
        Ok(PrintJob {
            printer: self,
            job,
            image_rows,
            #[cfg(feature = "image")]
            raster,
        })
    }
    /// Prepare an image as monochrome pixels without sending any bytes.
    ///
    /// Available with the `image` feature. The image keeps its size.
    /// Resize it to at most [`Self::printable_width`] dots before calling this
    /// method. [`crate::Position`] sets the horizontal position on white paper.
    /// Transparency is combined with a white background before applying [`crate::Dither`].
    /// Dithering never changes the white margins.
    ///
    /// Use [`crate::Dither::Threshold`] for an image that is already black and
    /// white. [`crate::Dither::default`] selects Ostromoukhov diffusion.
    /// This method always produces [`PixelFormat::Mono`], even on a printer
    /// that supports grayscale. Use [`Self::prepare`] for packed grayscale.
    ///
    /// The job borrows only the printer. Later changes to the source image or
    /// options do not affect it. Preparation needs no Tokio runtime.
    /// [`PrintJob::preview`] returns the exact positioned, dithered pixels.
    ///
    /// # Errors
    /// Reject an empty image, an image wider than the print head, or a height
    /// above [`Driver::max_height`]. Invalid controls and compression errors also
    /// leave the connection usable. A disabled connection returns
    /// [`crate::Error::UnusableConnection`].
    #[cfg(feature = "image")]
    pub fn prepare_from_image(
        &mut self,
        image: &image::DynamicImage,
        position: crate::Position,
        dither: crate::Dither,
        options: &D::Options,
    ) -> Result<PrintJob<'_, T, D>> {
        self.session.ensure_usable()?;
        self.driver.validate(options, PixelFormat::Mono)?;
        let bytes = crate::imaging::pack(image, self.printable_width(), position, dither)?;
        self.prepare(bytes, PixelFormat::Mono, options)
    }

    /// Move paper forward by the requested number of dot rows.
    ///
    /// This operation uses the current device configuration. It does not apply
    /// print options or set a feed speed. The report uses [`Completion::TimedDrain`].
    ///
    /// # Errors
    /// Reject MXW01 or zero rows before writing. See [`Printer`] for connection
    /// handling after communication errors.
    pub async fn feed(&mut self, rows: u16) -> Result<PrintReport> {
        self.move_paper(rows, false).await
    }
    /// Request reverse paper movement.
    ///
    /// The command's direction and distance units remain unconfirmed on hardware.
    /// Do not rely on `rows` as a measured retraction distance.
    ///
    /// # Errors
    /// Reject MX10, MXW01, or zero rows. Communication errors invalidate the connection.
    pub async fn retract(&mut self, rows: u16) -> Result<PrintReport> {
        self.move_paper(rows, true).await
    }
    /// Send an MXW01 cancellation request, then disconnect.
    ///
    /// Use [`Self::cancellation`] to obtain a handle for an active operation.
    /// The firmware's cancellation behavior remains unconfirmed. Rows already
    /// submitted can still print.
    ///
    /// # Errors
    /// Reject drivers other than MXW01 or an unusable connection.
    /// Write and disconnect failures leave the connection unusable.
    pub async fn cancel(&mut self) -> Result<()> {
        self.session.ensure_usable()?;
        self.driver.validate_cancel()?;
        let mut operation = self.session.begin(self.driver, OperationKind::Cancel)?;
        let result = operation.cancel().await;
        operation.finish(result).await
    }

    /// Permanently close this connection within five seconds.
    ///
    /// Call this method when you finish using the printer, including after you
    /// drop an active operation future. Construct a new printer for a new connection.
    ///
    /// # Errors
    /// Return transport or timeout errors. The connection remains unusable.
    /// After a successful close, later calls perform no transport I/O.
    pub async fn disconnect(&mut self) -> Result<()> {
        self.session.disconnect().await
    }
    /// Read the printer's status, including named conditions and known readings.
    ///
    /// Return a recent notification when available. While the printer is paused,
    /// return the latest known status. Otherwise, request a fresh status.
    /// A reported printer condition leaves the connection usable.
    /// Unknown readings are `None`. An unknown state never implies readiness.
    ///
    /// # Errors
    /// Communication failures, cancellation, and malformed replies disable the connection.
    pub async fn status(&mut self) -> Result<PrinterStatus> {
        let mut operation = self.session.begin(self.driver, OperationKind::Observe)?;
        let result = D::status(&mut operation).await;
        operation.finish(result).await
    }
    /// Read device text and a firmware version when available.
    ///
    /// Fields that the firmware does not provide or the library cannot interpret
    /// are `None`.
    ///
    /// # Errors
    /// A known pause returns [`crate::Error::PrinterUnavailable`] and leaves the connection usable.
    /// Communication failures, cancellation, and malformed replies disable the connection.
    pub async fn device_info(&mut self) -> Result<DeviceInfo> {
        let mut operation = self.session.begin(self.driver, OperationKind::Observe)?;
        let result = D::device_info(&mut operation).await;
        operation.finish(result).await
    }
    async fn move_paper(&mut self, rows: u16, retract: bool) -> Result<PrintReport> {
        self.session.ensure_usable()?;
        let movement = self.driver.movement(rows, retract)?;
        let mut operation = self.session.begin(self.driver, OperationKind::Submit)?;
        let result = async {
            let mut submitted = 0;
            operation.send_movement(&movement, &mut submitted).await?;
            operation.bounded_drain().await?;
            Ok(PrintReport {
                image_rows: 0,
                bytes_submitted: submitted,
                completion: Completion::TimedDrain,
            })
        }
        .await;
        operation.finish(result).await
    }
}
