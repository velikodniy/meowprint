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

#[cfg(feature = "tokio")]
use crate::TokioClock;
use crate::{
    Clock, DeviceInfo, Driver, PixelFormat, PrinterState, Result,
    session::{OperationKind, Session},
};
use alloc::vec::Vec;

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

macro_rules! declare_io_traits {
    ($($send:ident)?) => {
        /// The connection interface used by [`Printer`] to send and receive bytes.
        ///
        /// For native Bluetooth, obtain a `BleTransport` through `Device::connect`.
        /// Connect and subscribe to the selected driver notification endpoint before
        /// constructing a printer. [`Printer`] handles framing, limits, pacing, and replies.
        ///
        /// Preserve write order and report events for this connection. A successful
        /// write means the local backend accepted bytes, not that paper was printed.
        /// Event waits must be cancellation-safe: dropping a future must not lose
        /// events or prevent subsequent calls. Writes have deadlines and are never retried.
        ///
        /// On browser and bare-metal targets, this trait and its futures can be local.
        /// Other targets require `Send`.
        #[cfg_attr(feature = "tokio", doc = concat!(
            "\n# Example\n\n```no_run\n",
            include_str!("../examples/custom_transport.rs"),
            "\n```\n",
        ))]
        pub trait Transport $(: $send)? {
            /// Return the maximum bytes accepted by one [`Self::write`] call.
            ///
            /// Return a nonzero connection limit. The default is 20 bytes;
            /// [`Printer`] also enforces the selected driver limit.
            fn max_write_size(&self) -> usize { 20 }
            /// Submit one chunk to the selected endpoint, in order.
            ///
            /// A chunk can be part of a frame. Do not add framing or retry after
            /// an uncertain result.
            ///
            /// # Errors
            /// Return [`crate::Error::Transport`] on backend failure. Partial
            /// submission has an unknown outcome.
            fn write(&mut self, channel: WriteChannel, bytes: &[u8])
                -> impl core::future::Future<Output = Result<()>> $(+ $send)?;
            /// Wait for a notification or link event, preserving all bytes in order.
            ///
            /// This must be safe to cancel and call again without losing an event.
            ///
            /// # Errors
            /// Return [`crate::Error::Transport`] if the event stream fails.
            fn event(&mut self)
                -> impl core::future::Future<Output = Result<TransportEvent>> $(+ $send)?;
            /// Close this connection.
            ///
            /// # Errors
            /// Return [`crate::Error::Transport`] if the backend cannot close the link.
            fn disconnect(&mut self)
                -> impl core::future::Future<Output = Result<()>> $(+ $send)?;
        }

        /// A forward-only source of packed image rows.
        ///
        /// Metadata must remain fixed during submission. Rows run top to bottom
        /// using [`PixelFormat`]'s darkness polarity and bit/nibble order. Methods
        /// must not perform printer I/O and need not retain previous rows.
        ///
        /// Printing requests exactly the declared height, without an EOF probe.
        /// Document framing, trailing input, and input deadlines belong to the source
        /// or caller. A deadline-free row can wait until cancellation or printer fault.
        /// Abandoning printing may drop its future.
        ///
        /// On browser and bare-metal targets, this trait and its futures can be local.
        /// Other targets require `Send`.
        pub trait RowSource $(: $send)? {
            /// Return the fixed width in dots.
            fn width(&self) -> u32;
            /// Return the exact number of rows to submit.
            fn height(&self) -> u32;
            /// Return the packed pixel format.
            fn format(&self) -> PixelFormat;
            /// Fill the entire destination with the next packed row.
            ///
            /// The destination contains width/8 bytes for Mono or width/2 for Gray4.
            /// Notifications do not restart a pending read. Printer faults and
            /// invalid replies abort the operation.
            ///
            /// # Errors
            /// Return an error if a row cannot be supplied. Its contents are ignored
            /// after errors, and the connection is disabled even before any write.
            /// Earlier rows may already print.
            fn read_row(&mut self, output: &mut [u8])
                -> impl core::future::Future<Output = Result<()>> $(+ $send)?;
        }
    };
}
#[cfg(not(any(target_family = "wasm", target_os = "none")))]
declare_io_traits!(Send);
#[cfg(any(target_family = "wasm", target_os = "none"))]
declare_io_traits!();

#[cfg(target_has_atomic = "ptr")]
use cancellation_atomic as cancellation_backend;
#[cfg(not(target_has_atomic = "ptr"))]
use cancellation_local as cancellation_backend;

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
///
/// On targets without pointer atomics, the handle is local to its executor and must
/// not be transferred across threads or interrupts.
#[derive(Clone)]
pub struct Cancellation {
    state: cancellation_backend::State,
}

impl core::fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Cancellation {
    pub(crate) fn new() -> Self {
        Self {
            state: cancellation_backend::State::new(),
        }
    }

    /// Request that this connection stop its active operation.
    ///
    /// Keep awaiting the active operation until it returns and completes its
    /// cleanup attempt. If the printer is idle, call [`Printer::disconnect`].
    pub fn cancel(&self) {
        self.state.cancel();
    }

    /// Return whether this handle received a cancellation request.
    ///
    /// `true` does not mean that the operation stopped or the connection closed.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.is_cancelled()
    }

    pub(crate) const fn cancelled(&self) -> cancellation_backend::Cancelled<'_> {
        self.state.cancelled()
    }
}

#[cfg(target_has_atomic = "ptr")]
mod cancellation_atomic {
    use alloc::sync::Arc;
    use atomic_waker::AtomicWaker;
    use core::{
        future::Future,
        pin::Pin,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };

    #[derive(Clone)]
    pub(super) struct State(Arc<Inner>);

    struct Inner {
        cancelled: AtomicBool,
        waker: AtomicWaker,
    }

    impl State {
        pub(super) fn new() -> Self {
            Self(Arc::new(Inner {
                cancelled: AtomicBool::new(false),
                waker: AtomicWaker::new(),
            }))
        }

        pub(super) fn cancel(&self) {
            self.0.cancelled.store(true, Ordering::Release);
            self.0.waker.wake();
        }

        pub(super) fn is_cancelled(&self) -> bool {
            self.0.cancelled.load(Ordering::Acquire)
        }

        pub(super) const fn cancelled(&self) -> Cancelled<'_> {
            Cancelled {
                state: self,
                done: false,
            }
        }
    }

    pub struct Cancelled<'a> {
        state: &'a State,
        done: bool,
    }

    impl Future for Cancelled<'_> {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.state.is_cancelled() {
                drop(self.state.0.waker.take());
                self.done = true;
                return Poll::Ready(());
            }
            self.state.0.waker.register(cx.waker());
            if self.state.is_cancelled() {
                drop(self.state.0.waker.take());
                self.done = true;
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for Cancelled<'_> {
        fn drop(&mut self) {
            if !self.done {
                drop(self.state.0.waker.take());
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use alloc::{
            sync::{Arc, Weak},
            task::Wake,
        };
        use core::task::Waker;

        #[derive(Default)]
        struct CountWake(core::sync::atomic::AtomicUsize);

        impl Wake for CountWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        #[test]
        fn cancellation_registration_and_waiter_drop() {
            let before = State::new();
            before.cancel();
            let noop = Waker::noop();
            let mut cx = Context::from_waker(noop);
            let mut ready = core::pin::pin!(before.cancelled());
            assert!(ready.as_mut().poll(&mut cx).is_ready());

            let state = State::new();
            let wake = Arc::new(CountWake::default());
            let weak: Weak<CountWake> = Arc::downgrade(&wake);
            {
                let waker = Waker::from(wake);
                let mut cx = Context::from_waker(&waker);
                let mut cancelled = core::pin::pin!(state.cancelled());
                assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            }
            assert!(weak.upgrade().is_none());

            let wake = Arc::new(CountWake::default());
            let waker = Waker::from(wake.clone());
            let mut cx = Context::from_waker(&waker);
            let mut cancelled = core::pin::pin!(state.cancelled());
            assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            state.cancel();
            state.cancel();
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
            assert!(cancelled.as_mut().poll(&mut cx).is_ready());
            assert!(state.is_cancelled());
            let mut already = core::pin::pin!(state.cancelled());
            assert!(already.as_mut().poll(&mut cx).is_ready());
        }

        #[test]
        fn public_cancellation_is_cross_thread() {
            let cancellation = super::super::Cancellation::new();
            let abandoned = cancellation.clone();
            drop(abandoned);
            assert!(!cancellation.is_cancelled());
            let clone = cancellation.clone();
            assert!(std::thread::spawn(move || clone.cancel()).join().is_ok());
            assert!(cancellation.is_cancelled());
        }
    }
}

#[cfg(any(not(target_has_atomic = "ptr"), test))]
mod cancellation_local {
    use alloc::rc::Rc;
    use core::{
        cell::{Cell, RefCell},
        future::Future,
        pin::Pin,
        task::{Context, Poll, Waker},
    };

    #[derive(Clone)]
    pub(super) struct State(Rc<Inner>);

    struct Inner {
        cancelled: Cell<bool>,
        waker: RefCell<Option<Waker>>,
    }

    impl State {
        pub(super) fn new() -> Self {
            Self(Rc::new(Inner {
                cancelled: Cell::new(false),
                waker: RefCell::new(None),
            }))
        }

        pub(super) fn cancel(&self) {
            self.0.cancelled.set(true);
            let waker = self.0.waker.borrow_mut().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }

        pub(super) fn is_cancelled(&self) -> bool {
            self.0.cancelled.get()
        }

        pub(super) const fn cancelled(&self) -> Cancelled<'_> {
            Cancelled {
                state: self,
                done: false,
            }
        }
    }

    pub struct Cancelled<'a> {
        state: &'a State,
        done: bool,
    }

    impl Future for Cancelled<'_> {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.state.is_cancelled() {
                drop(self.state.0.waker.borrow_mut().take());
                self.done = true;
                return Poll::Ready(());
            }
            *self.state.0.waker.borrow_mut() = Some(cx.waker().clone());
            if self.state.is_cancelled() {
                drop(self.state.0.waker.borrow_mut().take());
                self.done = true;
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for Cancelled<'_> {
        fn drop(&mut self) {
            if !self.done {
                drop(self.state.0.waker.borrow_mut().take());
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use alloc::{
            sync::{Arc, Weak},
            task::Wake,
        };
        use core::task::Waker;

        #[derive(Default)]
        struct CountWake(core::sync::atomic::AtomicUsize);

        impl Wake for CountWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            }
        }

        #[test]
        fn local_waker_lifecycle_and_sticky_cancellation() {
            let before = State::new();
            before.cancel();
            let mut ready = core::pin::pin!(before.cancelled());
            let noop = Waker::noop();
            let mut cx = Context::from_waker(noop);
            assert!(ready.as_mut().poll(&mut cx).is_ready());

            let state = State::new();
            let abandoned = state.clone();
            drop(abandoned);
            assert!(!state.is_cancelled());
            let wake = Arc::new(CountWake::default());
            let weak: Weak<CountWake> = Arc::downgrade(&wake);
            {
                let waker = Waker::from(wake);
                let mut cx = Context::from_waker(&waker);
                let mut cancelled = core::pin::pin!(state.cancelled());
                assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            }
            assert!(weak.upgrade().is_none());
            let wake = Arc::new(CountWake::default());
            let waker = Waker::from(wake.clone());
            let mut cx = Context::from_waker(&waker);
            let mut cancelled = core::pin::pin!(state.cancelled());
            assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            state.cancel();
            state.cancel();
            assert_eq!(wake.0.load(core::sync::atomic::Ordering::SeqCst), 1);
            assert!(cancelled.as_mut().poll(&mut cx).is_ready());
            assert!(state.is_cancelled());
            let mut already = core::pin::pin!(state.cancelled());
            assert!(already.as_mut().poll(&mut cx).is_ready());
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
/// For Bluetooth, use `Device::connect`. For custom connections, pass a connected
/// [`Transport`] to [`Self::with_clock`]. Run one operation at a time.
/// All asynchronous operations use the clock supplied at construction.
///
/// [`Self::print`] borrows this printer, its row source, and its controls until
/// printing completes. Pass `&Default::default()` for driver defaults.
/// Invalid metadata or controls leave the connection usable without reading rows.
/// Source or compression errors after submission begins disable the connection,
/// even before the first write; later errors may leave partial physical output.
/// After a successful print, query, or paper movement, you can reuse this printer.
///
/// Communication failures and dropping an active operation disable further use.
/// A condition reported by the printer before any write can leave the connection usable.
/// Use [`Self::is_usable`] to decide whether to reconnect after an error.
/// Status reads can report printer conditions without disabling the connection.
/// Always await [`Self::disconnect`] when you finish, including after errors.
pub struct Printer<
    T,
    D: Driver,
    #[cfg(feature = "tokio")] C: Clock = TokioClock,
    #[cfg(not(feature = "tokio"))] C: Clock,
> {
    pub(crate) session: Session<T>,
    pub(crate) driver: D,
    pub(crate) clock: C,
}

#[cfg(feature = "tokio")]
impl<T: Transport, D: Driver> Printer<T, D, TokioClock> {
    /// Create a printer from a transport and its matching driver using Tokio time.
    ///
    /// This constructor does not connect, subscribe, or send printer commands.
    /// For communication, connect and subscribe the transport first as described
    /// by [`Transport`]. Offline pixel conversion and preview use [`crate::Raster`].
    #[must_use]
    pub fn new(transport: T, driver: D) -> Self {
        Self::with_clock(transport, driver, TokioClock::default())
    }
}

impl<T: Transport, D: Driver, C: Clock> Printer<T, D, C> {
    /// Create a printer using the supplied clock.
    ///
    /// This constructor does not connect, subscribe, or send printer commands.
    /// For communication, connect and subscribe the transport first as described
    /// by [`Transport`]. Offline pixel conversion and preview use [`crate::Raster`].
    #[must_use]
    pub fn with_clock(transport: T, driver: D, clock: C) -> Self {
        Self {
            session: Session::new(transport, driver.decoder()),
            driver,
            clock,
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
        let mut operation = self
            .session
            .begin(self.driver, OperationKind::Cancel, &self.clock)?;
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
        self.session.disconnect(&self.clock).await
    }
    /// Read the printer's state, including named conditions.
    ///
    /// Return a recent notification when available. While the printer is paused,
    /// return the latest known status. Otherwise, request a fresh status.
    /// A reported printer condition leaves the connection usable.
    /// An unknown state never implies readiness.
    ///
    /// # Errors
    /// Communication failures, cancellation, and malformed replies disable the connection.
    pub async fn status(&mut self) -> Result<PrinterState> {
        let mut operation = self
            .session
            .begin(self.driver, OperationKind::Observe, &self.clock)?;
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
        let mut operation = self
            .session
            .begin(self.driver, OperationKind::Observe, &self.clock)?;
        let result = D::device_info(&mut operation).await;
        operation.finish(result).await
    }
    async fn move_paper(&mut self, rows: u16, retract: bool) -> Result<PrintReport> {
        self.session.ensure_usable()?;
        let movement = self.driver.movement(rows, retract)?;
        let mut operation = self
            .session
            .begin(self.driver, OperationKind::Submit, &self.clock)?;
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
