//! Print images on small Bluetooth thermal printers.
//!
//! Meowprint provides device discovery, printing, status queries, and paper feed
//! controls. It accepts packed pixels, or decoded images with the optional
//! `image` feature. Your application loads image files, draws text, and resizes
//! images to fit the paper.
//!
//! A [`Driver`] tells Meowprint how to communicate with your printer.
//! Choose a driver that matches its firmware, such as [`Gt01`] or [`Mx10`].
//! A Bluetooth name alone does not prove compatibility. The [driver reference](driver)
//! lists supported formats and controls.
//!
//! # Quick start
//!
//! Add `meowprint` and Tokio to your application:
//!
//! ```toml
//! [dependencies]
//! meowprint = "0.1"
//! tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
//! ```
//!
//! This example prints a black stripe, 384 dots wide and eight rows tall.
//! It assumes that a nearby device named `GT01` supports the [`Gt01`] driver.
//!
//! ```no_run
//! # #[cfg(feature = "bluetooth")]
//! # mod example {
//! use meowprint::{Gt01, PixelFormat, bluetooth::Bluetooth};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() -> meowprint::Result<()> {
//!     let bluetooth = Bluetooth::new().await?;
//!     let device = bluetooth.find("GT01", Duration::from_secs(8)).await?;
//!     let mut printer = device.connect(Gt01).await?;
//!     let row_bytes = (printer.printable_width() / 8) as usize;
//!
//!     let result = async {
//!         printer
//!             .prepare(vec![0xff; row_bytes * 8], PixelFormat::Mono, &Default::default())?
//!             .print()
//!             .await
//!     }
//!     .await;
//!     // Attempt to disconnect even if preparation or printing fails.
//!     let disconnected = printer.disconnect().await;
//!     let report = result?;
//!     disconnected?;
//!     println!("Completion evidence: {:?}", report.completion);
//!     Ok(())
//! }
//! # }
//! ```
//!
//! Bluetooth access requires a powered adapter and host permission.
//! On macOS, allow Bluetooth access for your application or terminal.
//! On Linux, install the D-Bus development packages and `pkg-config` before building.
//! Communication requires a Tokio runtime. Preparation and preview do not.
//!
//! # Prepare an image
//!
//! [`Printer::prepare`] accepts packed pixels and a [`PixelFormat`].
//! Each row must fill [`Printer::printable_width`] dots. The byte count determines
//! the height. All built-in drivers use a width of 384 dots and accept heights
//! from 1 through 32,768 rows. Only [`X6h`] and [`V5g`] support four-bit grayscale.
//! See [`PixelFormat`] for the byte layout.
//!
//! Pass `&Default::default()` for the driver defaults, or supply the matching
//! options type, such as [`Gt01Options`]. Preparation does not communicate with
//! the printer. The returned [`PrintJob`] keeps exclusive access to the printer
//! until you print or discard the job.
//!
//! Enable the `image` feature to convert an `image::DynamicImage` to black and white:
//!
//! ```toml
//! meowprint = { version = "0.1", features = ["image"] }
//! image = { version = "0.25", default-features = false, features = ["png"] }
//! ```
//!
//! ```no_run
//! # #[cfg(feature = "image")]
//! # async fn example<T: meowprint::Transport, D: meowprint::Driver>(printer: &mut meowprint::Printer<T, D>, image: &image::DynamicImage) -> meowprint::Result<()> {
//! use meowprint::{Dither, Position};
//!
//! let job = printer.prepare_from_image(
//!     image,
//!     Position::Center,
//!     Dither::default(),
//!     &Default::default(),
//! )?;
//! let preview = job.preview();
//! let report = job.print().await?;
//! # Ok(())
//! # }
//! ```
//!
//! Resize images wider than the print head before preparation.
//! `Position` controls horizontal alignment. `Dither` converts shades to patterns
//! of black and white dots. Transparency is combined with a white background.
//! Unused columns stay white.
//! The preview shows the prepared pixels, excluding blank rows and paper feed
//! added by the driver. It does not predict physical print quality.
//!
//! Meowprint itself does not enable image file formats. The `png` feature above
//! enables PNG files. Enable other formats in your application's `image` dependency as needed.
//! Use [`NoOpTransport`] to prepare and preview images without a connection.
//!
//! # Queries, completion, and cleanup
//!
//! [`Printer::status`] returns named states and conditions. [`Printer::device_info`]
//! returns device text and a firmware version when available. Missing or unknown
//! readings are `None`. An unknown state does not mean that the printer is ready.
//! [`Printer::feed`] moves paper forward on supported drivers.
//!
//! After a successful print or query, you can reuse the same printer.
//! [`PrintReport::completion`] describes how printing finished. Some drivers wait
//! for a fixed interval, while MXW01 waits for a completion message.
//! Neither result guarantees the appearance of the printed paper.
//!
//! Preparation errors leave the connection usable. Communication failures,
//! cancellation, and dropping an active operation disable further use.
//! A printer condition detected before a write can leave the connection usable.
//! After an error, use [`Printer::is_usable`] to decide whether to reconnect.
//! Always await [`Printer::disconnect`] when you finish, including after errors.
//! Previously submitted rows can still print. Meowprint does not retry automatically.
//!
//! To stop an active operation, obtain [`Printer::cancellation`] before preparing
//! the job. Request cancellation through that handle and continue awaiting the
//! operation until it returns. See [`Cancellation`] for details.
//!
//! # Custom connections
//!
//! Native Bluetooth support uses the default `bluetooth` feature.
//! To supply another connection, implement [`Transport`] and pass it with a driver
//! to [`Printer::new`]. Disable default features to exclude native Bluetooth dependencies:
//!
//! ```toml
//! meowprint = { version = "0.1", default-features = false }
//! ```
//!
//! The `image` feature works with or without `bluetooth`.
//! The [protocol reference](protocol) records firmware research for driver
//! development and custom transports.

#![cfg_attr(feature = "image", doc = concat!(
    "\n# Offline preview\n\nThis example prepares a job without Bluetooth access or a Tokio runtime.\n\n```\n",
    include_str!("../examples/preview.rs"),
    "\n```\n",
))]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[cfg(feature = "bluetooth")]
pub mod bluetooth;
#[doc = include_str!("../docs/protocol.md")]
pub mod protocol {}
mod device;
pub mod driver;
mod encoding;
mod error;
#[cfg(feature = "image")]
mod imaging;
mod job;
pub mod printer;
mod raster;
mod session;

pub use device::{DeviceInfo, PrinterCondition, PrinterState, PrinterStatus};
pub use driver::*;
pub use error::{Error, Result};
#[cfg(feature = "image")]
pub use imaging::{Dither, Position};
pub use printer::{
    Cancellation, Completion, NoOpTransport, PrintJob, PrintReport, Printer, Transport,
    TransportEvent, WriteChannel,
};
pub use raster::PixelFormat;
pub(crate) use raster::Raster;
