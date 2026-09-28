//! Built-in printer drivers and their print controls.
//!
//! Pass a driver value such as [`Gt01`] or [`Mxw01::default()`] to
//! `Device::connect` or [`Printer::new`](crate::Printer::new).
//! Each driver has a matching options type, such as [`Gt01Options`].
//! Choose a driver that matches your printer firmware. A Bluetooth name or
//! service alone does not prove compatibility.
//!
//! All drivers print at a width of 384 dots. [`X6h`] and [`V5g`] also accept
//! four-bit grayscale. The other drivers accept monochrome pixels only.
//! [`CommonRaw`], [`TinyRle`], and [`PrefixedTiny`] support firmware variants that
//! do not have a dedicated model driver. See the [protocol reference](crate::protocol)
//! for the known associations between models and firmware.
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
//! [`Printer::prepare`](crate::Printer::prepare) also performs this validation.

mod common;
pub(crate) mod framing;
mod mxw01;

use crate::session::Operation;
use crate::{
    Completion, DeviceInfo, PixelFormat, PrinterState, PrinterStatus, Raster, Result, Transport,
    error::invalid,
};
use framing::{Decoder, EncodedCommand, Frame};
use std::time::Duration;

pub use common::Mode as PrintMode;
pub use common::{
    CommonRaw, CommonRawOptions, Gt01, Gt01Options, MonoEncoding, Mx10, Mx10Options, PrefixedTiny,
    PrefixedTinyOptions, TinyRle, TinyRleOptions, V5g, V5gEncoding, V5gOptions, X6h, X6hEncoding,
    X6hOptions,
};
pub use mxw01::{Mxw01, Mxw01Options, ReplyFormat as Mxw01ReplyFormat};

/// Resolved private representation of packed pixels.
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

/// A built-in firmware driver with its own print controls.
///
/// Use a built-in driver such as [`Gt01`] or [`Mxw01::default()`].
/// Applications cannot implement this trait. To supply another connection type,
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
    /// Return whether packed four-bit grayscale is supported.
    fn supports_gray4(self) -> bool;
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
    pub status: Option<PrinterStatus>,
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
    #[cfg(feature = "bluetooth")]
    pub endpoints: Endpoints,
}
#[cfg(feature = "bluetooth")]
#[derive(Clone, Copy)]
pub(crate) struct Endpoints {
    pub services: &'static [u16],
    pub control: u16,
    pub raster: u16,
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
/// Internal driver operations, sealed to keep wire types out of the public API.
pub(crate) trait Operations: Sized + Send + Sync {
    type Job: Send + Sync;
    fn settings(&self) -> Settings;
    fn decoder(&self) -> Decoder;
    fn prepare(&self, raster: &Raster, options: &<Self as Driver>::Options) -> Result<Self::Job>
    where
        Self: Driver;
    fn submit<T: Transport>(
        operation: &mut Operation<'_, T, Self>,
        job: &Self::Job,
        submitted: &mut usize,
    ) -> impl Future<Output = Result<Completion>> + Send
    where
        Self: Driver;
    fn status<T: Transport>(
        operation: &mut Operation<'_, T, Self>,
    ) -> impl Future<Output = Result<PrinterStatus>> + Send
    where
        Self: Driver;
    fn device_info<T: Transport>(
        operation: &mut Operation<'_, T, Self>,
    ) -> impl Future<Output = Result<DeviceInfo>> + Send
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

fn readable_text(data: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(data).ok()?.trim_end_matches('\0');
    (!text.is_empty() && !text.chars().any(char::is_control)).then(|| text.to_owned())
}
