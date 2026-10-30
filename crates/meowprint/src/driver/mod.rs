//! Built-in printer drivers and their print controls.
//!
//! Pass a driver value such as [`Gt01`] or [`Mxw01::default()`] to
//! `Device::connect` or [`Printer::with_clock`](crate::Printer::with_clock).
//! Each driver has a matching options type, such as [`Gt01Options`].
//! Choose a driver that matches your printer firmware. A Bluetooth name or
//! service alone does not prove compatibility.
//!
//! All drivers print at a width of 384 dots.
//! [`CommonRaw`], [`TinyRle`], and [`PrefixedTiny`] support firmware variants that
//! do not have a dedicated model driver. See the [protocol reference](crate::protocol)
//! for the known associations between models and firmware.
//!
//! # Supported pixel formats
//!
//! | Driver | [`PixelFormat::Mono`] | [`PixelFormat::Gray4`] |
//! | --- | --- | --- |
//! | [`Gt01`] | Yes | No |
//! | [`Mx10`] | Yes | No |
//! | [`CommonRaw`] | Yes | No |
//! | [`TinyRle`] | Yes | No |
//! | [`PrefixedTiny`] | Yes | No |
//! | [`X6h`] | Yes | Yes |
//! | [`V5g`] | Yes | Yes |
//! | [`Mxw01`] | Yes | No |
//!
//! Query [`Driver::supports`] for pixel-format capability, then use
//! [`Driver::validate`] to check a specific format and set of controls.
//! A supported format may still be incompatible with a selected encoding.
//! Both checks work without a connection or Bluetooth access.
//!
//! ```
//! use meowprint::{Driver, Gt01, PixelFormat, X6h, X6hOptions};
//!
//! assert!(Gt01.supports(PixelFormat::Mono));
//! assert!(!Gt01.supports(PixelFormat::Gray4));
//! assert!(X6h.supports(PixelFormat::Gray4));
//! X6h.validate(&X6hOptions::default(), PixelFormat::Gray4)?;
//! # Ok::<(), meowprint::Error>(())
//! ```
//!
//! The CLI's `meowprint drivers` command lists these capabilities without
//! discovery: `grayscale true` means Mono and Gray4; `grayscale false` means Mono only.
//!
//! # Controls and defaults
//!
//! Start with `Default::default()` and change the fields you need.
//! Each options type documents its supported ranges. For example, this changes
//! the GT01 final feed while keeping its other defaults:
//!
//! ```
//! use meowprint::Gt01Options;
//!
//! let options = Gt01Options { feed: 48, ..Default::default() };
//! ```
//!
//! Encoding selects how the driver compresses pixels. `None` uses the default
//! for the pixel format. For optional density, `None` keeps the existing printer
//! density, including a value set by an earlier job.
//!
//! | Driver | Speed divisor | Final feed | Quality or concentration | Default encoding |
//! | --- | --- | --- | --- | --- |
//! | [`Gt01`] | 30 | 96 rows | Quality 3 | Raw mono |
//! | [`Mx10`] | 30 | 128 blank rows | Quality 3 | Raw mono |
//! | [`CommonRaw`] | 30 | 96 rows | Quality 3 | Raw mono |
//! | [`TinyRle`] | 30 | 96 rows | Quality 3 | RLE mono |
//! | [`PrefixedTiny`] | 30 | 96 rows | Quality 3 | RLE mono |
//! | [`X6h`] | 30 | 96 rows | Concentration 3 | LZO rows |
//! | [`V5g`] | 10 | 48 rows | Quality 3 | Raw mono or LZO Gray4 bands |
//! | [`Mxw01`] | Fixed by firmware | Not configurable | Intensity 93 | Raw mono |
//!
//! For drivers other than MXW01, defaults are energy 12000, feed speed 25, and image mode.
//! A smaller speed divisor means faster paper movement. Zero feed suppresses
//! the final feed. MXW01 adds blank rows to print at least 90 rows per job.
//! These controls use printer-specific values, not calibrated heat or speed units.
//!
//! [`Driver::validate`] accepts or rejects controls without contacting a printer.
//! [`Printer::print`](crate::Printer::print) validates them before reading rows or writing.

mod common;
pub(crate) mod framing;
mod mxw01;

use alloc::vec::Vec;
use alloc::{borrow::ToOwned, string::String};
use core::{future::Future, time::Duration};

use crate::{
    Clock, Completion, DeviceInfo, PixelFormat, PrinterState, Raster, Result, RowSource, Transport,
    error::invalid,
};
use crate::{session::Operation, stream::RowReader};
pub use common::Mode as PrintMode;
pub use common::{
    CommonRaw, CommonRawOptions, Gt01, Gt01Options, MonoEncoding, Mx10, Mx10Options, PrefixedTiny,
    PrefixedTinyOptions, TinyRle, TinyRleOptions, V5g, V5gEncoding, V5gOptions, X6h, X6hEncoding,
    X6hOptions,
};
use framing::{Decoder, EncodedCommand, Frame};
pub use mxw01::{Mxw01, Mxw01Options, ReplyFormat as Mxw01ReplyFormat};

/// Compression for packed raster rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RasterEncoding {
    /// Uncompressed monochrome pixels.
    Raw,
    /// Monochrome run-length encoding with a raw fallback for larger rows.
    Rle,
    /// LZO compression of individual rows.
    Lzo,
    /// LZO compression of groups of up to twenty grayscale rows.
    LzoBands,
    /// Experimental zlib rows, without hardware validation.
    ZlibExperimental,
}

/// A built-in driver for a printer firmware, with its print controls.
///
/// Use a built-in driver such as [`Gt01`] or [`Mxw01::default()`].
/// Applications cannot implement this trait. To use another connection type,
/// implement [`Transport`].
#[expect(
    private_bounds,
    reason = "The private operations contract keeps wire types out of the public API."
)]
pub trait Driver: Operations + Copy + 'static {
    /// Print controls accepted by this driver, with defaults supplied by `Default`.
    type Options: Default + Send + Sync;
    /// Return the driver name used by the command-line tool, such as `gt01`.
    fn name(self) -> &'static str;
    /// Return the driver's Bluetooth endpoint identifiers.
    ///
    /// This is advanced API for implementing a BLE backend. Ordinary callers
    /// should use `Device::connect`; this metadata does not perform discovery
    /// or validate a discovered service.
    fn ble_profile(self) -> BleProfile {
        self.settings().endpoints
    }
    /// Return the print head width in dots.
    fn printable_width(self) -> u32 {
        self.settings().width
    }
    /// Return the maximum image height in rows.
    ///
    /// Use this limit before allocating pixels. All built-in drivers accept up
    /// to 32,768 rows.
    fn max_height(self) -> u32 {
        Raster::max_height(self.printable_width())
    }
    /// Return whether this driver supports the given packed pixel format.
    ///
    /// All built-in drivers accept [`PixelFormat::Mono`]; only [`X6h`] and [`V5g`]
    /// also accept [`PixelFormat::Gray4`]. This query requires no printer connection.
    /// It does not check the selected controls or encoding; use [`Self::validate`]
    /// for that combination.
    fn supports(self, format: PixelFormat) -> bool;
    /// Determine whether the driver accepts these controls and pixel format.
    ///
    /// This method does not contact the printer or establish hardware compatibility.
    ///
    /// # Errors
    /// Reject out-of-range controls, an unsupported format, or an encoding that
    /// does not support the selected format.
    fn validate(self, options: &Self::Options, format: PixelFormat) -> Result<()>;
    /// Determine whether the driver accepts forward movement by `rows` dot rows.
    ///
    /// This method does not contact the printer.
    ///
    /// # Errors
    /// Reject an unsupported movement or zero rows.
    fn validate_feed(self, rows: u16) -> Result<()> {
        self.movement(rows, false).map(|_| ())
    }
    /// Determine whether the driver accepts a retraction request for `rows`.
    ///
    /// This method does not contact the printer. See [`Printer::retract`](crate::Printer::retract)
    /// for the limits of this command.
    ///
    /// # Errors
    /// Reject an unsupported movement or zero rows.
    fn validate_retract(self, rows: u16) -> Result<()> {
        self.movement(rows, true).map(|_| ())
    }
    /// Determine whether the driver supports [`Printer::cancel`](crate::Printer::cancel).
    ///
    /// This method does not contact the printer. Active operations on every driver
    /// can use a [`Cancellation`](crate::Cancellation) handle.
    ///
    /// # Errors
    /// Reject drivers other than MXW01.
    fn validate_cancel(self) -> Result<()> {
        self.cancel_command()?
            .ok_or_else(|| invalid("This driver has no documented cancel command."))?;
        Ok(())
    }
}

/// Independent effects of one validated frame.
#[derive(Default)]
pub(crate) struct Observation {
    pub status: Option<PrinterState>,
    pub flow: Option<Flow>,
    pub reply: bool,
}
pub(crate) enum Flow {
    Pause,
    Resume,
}
#[derive(Clone, Copy)]
pub(crate) struct Settings {
    pub width: u32,
    pub chunk_size: usize,
    pub pacing: Duration,
    pub finish_control_frame: bool,
    pub prefix: Option<(u8, &'static [u8])>,
    pub endpoints: BleProfile,
}

/// Bluetooth endpoint identifiers used by an advanced BLE backend.
///
/// All identifiers are Bluetooth-base 16-bit UUIDs. `services` lists
/// alternatives, not a priority order or proof of compatibility. A compatible
/// endpoint set must be found within one service; control and raster may be the
/// same characteristic. Both writes require `WRITE_WITHOUT_RESPONSE`, and
/// notifications require `NOTIFY`. Multiple fully compatible services are
/// ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BleProfile {
    /// Alternative service UUIDs to inspect.
    pub services: &'static [u16],
    /// Control characteristic UUID.
    pub control: u16,
    /// Raster characteristic UUID.
    pub raster: u16,
    /// Notification characteristic UUID.
    pub notify: u16,
}
impl Settings {
    pub const fn pacing(self, size: usize) -> Duration {
        if size <= 20 {
            Duration::from_millis(20)
        } else {
            self.pacing
        }
    }
}
pub(crate) enum Movement {
    Commands(Vec<EncodedCommand>),
    BlankRows { frame: Vec<u8>, count: u16 },
}
macro_rules! declare_operations {
    ([$($future_bounds:tt)*]) => {
        /// Driver-specific submission, queries, and reply handling.
        pub(crate) trait Operations: Sized + Send + Sync {
            type Stream: Send;
            fn settings(&self) -> Settings;
            fn decoder(&self) -> Decoder;
            fn stream(
                &self,
                height: u32,
                format: PixelFormat,
                options: &<Self as Driver>::Options,
            ) -> Result<Self::Stream>
            where
                Self: Driver;
            fn submit<T: Transport, S: RowSource, C: Clock>(
                operation: &mut Operation<'_, T, Self, C>,
                stream: &mut Self::Stream,
                rows: &mut RowReader<'_, S>,
                submitted: &mut usize,
            ) -> impl Future<Output = Result<Completion>> $($future_bounds)*
            where
                Self: Driver;
            fn status<T: Transport, C: Clock>(
                operation: &mut Operation<'_, T, Self, C>,
            ) -> impl Future<Output = Result<PrinterState>> $($future_bounds)*
            where
                Self: Driver;
            fn device_info<T: Transport, C: Clock>(
                operation: &mut Operation<'_, T, Self, C>,
            ) -> impl Future<Output = Result<DeviceInfo>> $($future_bounds)*
            where
                Self: Driver;
            fn observe(&self, frame: &Frame) -> Result<Observation>;
            fn blocks_submission(&self, _frame: &Frame, state: &PrinterState) -> bool {
                state.blocks_submission()
            }
            fn movement(&self, _rows: u16, _retract: bool) -> Result<Movement> {
                Err(invalid(
                    "This driver does not support standalone paper movement.",
                ))
            }
            fn cancel_command(&self) -> Result<Option<EncodedCommand>> {
                Ok(None)
            }
        }
    };
}

#[cfg(not(any(target_family = "wasm", target_os = "none")))]
declare_operations!([+ Send]);
#[cfg(any(target_family = "wasm", target_os = "none"))]
declare_operations!([]);

fn readable_text(data: &[u8]) -> Option<String> {
    let text = core::str::from_utf8(data).ok()?.trim_end_matches('\0');
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_owned())
}
