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
//! use meowprint::{Gt01, PixelFormat, Raster, bluetooth::Bluetooth};
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
//!         let raster = Raster::new(printer.printable_width(), vec![0xff; row_bytes * 8], PixelFormat::Mono)?;
//!         printer
//!             .print(&mut raster.rows(), &Default::default())
//!             .await
//!     }
//!     .await;
//!     // Attempt to disconnect even if pixel validation or printing fails.
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
//! Native Bluetooth uses Tokio. Custom transports can use their own [`Clock`]
//! and executor. Raster conversion and preview need no runtime.
//!
//! # Supply image rows
//!
//! [`Printer::print`] accepts a mutable [`RowSource`] and typed driver controls.
//! It reads exactly the declared height, without probing EOF. Each row fills
//! [`Printer::printable_width`] dots. All built-in drivers use 384 dots and accept
//! heights from 1 through 32,768 rows. Only [`X6h`] and [`V5g`] support four-bit
//! grayscale. See [`PixelFormat`] for darkness polarity and byte layout.
//!
//! Buffered callers use [`Raster::new`] and submit [`Raster::rows`], borrowing
//! the same pixels without cloning or consuming them. Generated or external
//! sources can implement [`RowSource`] without retaining a page. The source owns
//! input deadlines and document framing; no extra row is read to detect EOF.
//! A deadline-free source can remain pending until cancellation or a printer fault.
//!
//! Pass `&Default::default()` or matching controls such as [`Gt01Options`].
//! The print future exclusively borrows the printer, source, and controls until
//! it completes. Rows are compressed and submitted incrementally.
//!
//! Enable the `image` feature to convert an `image::DynamicImage` to Mono or Gray4:
//!
//! ```toml
//! meowprint = { version = "0.1", features = ["image"] }
//! image = { version = "0.25", default-features = false, features = ["png"] }
//! ```
//!
//! ```no_run
//! # #[cfg(feature = "image")]
//! # async fn example<T: meowprint::Transport, D: meowprint::Driver, C: meowprint::Clock>(printer: &mut meowprint::Printer<T, D, C>, image: &image::DynamicImage) -> meowprint::Result<()> {
//! use meowprint::{Dither, ImageConversion, Position, Raster};
//!
//! let raster = Raster::from_image(
//!     image,
//!     printer.printable_width(),
//!     Position::Center,
//!     ImageConversion::Mono(Dither::default()),
//! )?;
//! let preview = raster.preview();
//! let report = printer.print(&mut raster.rows(), &Default::default()).await?;
//! # Ok(())
//! # }
//! ```
//!
//! Resize images wider than the print head before preparation.
//! `Position` controls horizontal alignment. `ImageConversion::Mono(dither)`
//! represents shades as black and white dots. Use `ImageConversion::Gray4` for
//! 16-level grayscale on X6h or V5g, without monochrome dithering.
//! Both conversions composite transparency on white and leave unused columns white.
//! The preview shows the prepared pixels, excluding blank rows and paper feed
//! added by the driver. It does not predict physical print quality.
//!
//! Meowprint itself does not enable image file formats. The `png` feature above
//! enables PNG files. Enable other formats in your application's `image` dependency as needed.
//! [`Raster`] conversion and preview require neither a printer nor a runtime.
//! Conversion buffers the image; it is not a streaming image adapter.
//! [`Dither::rows`] provides reusable scanline dithering without the `image` feature.
//!
//! # Queries, completion, and cleanup
//!
//! [`Printer::status`] returns [`PrinterState`] with named states and conditions.
//! [`Printer::device_info`] returns device text and a firmware version when available;
//! missing or unknown fields are `None`. An unknown state never implies readiness.
//! [`Printer::feed`] moves paper forward on supported drivers.
//!
//! After a successful print or query, you can reuse the same printer.
//! [`PrintReport::completion`] describes how printing finished. Some drivers wait
//! for a fixed interval, while MXW01 waits for a completion message.
//! Neither result proves that every row reached the paper or guarantees print quality.
//!
//! Invalid source metadata or controls leave the connection usable without reading
//! rows or performing I/O. Source and compression failures after submission begins
//! disable it, including first-row failures before writes. Later failures may leave
//! partial output. Communication failures, cancellation, and dropping an active
//! operation also disable the connection. A condition reported by the printer before
//! any write can leave it usable. After errors, use [`Printer::is_usable`].
//! Always await [`Printer::disconnect`] when you finish, including after errors.
//! Previously submitted rows can still print. Meowprint does not retry automatically.
//!
//! To stop an active operation, obtain [`Printer::cancellation`] before starting
//! printing. Request cancellation through that handle and continue awaiting the
//! operation until it returns. See [`Cancellation`] for details.
//!
//! # Custom connections
//!
//! Native Bluetooth uses the default `bluetooth` feature and `Device::connect`.
//! Custom connections implement [`Transport`] and select a constructor:
//!
//! | Consumer | Features with defaults disabled | Constructor |
//! | --- | --- | --- |
//! | Native Tokio transport | `tokio` | `Printer::new(transport, driver)` |
//! | Bare-metal or browser runtime | none | `Printer::with_clock(transport, driver, clock)` |
//! | Custom runtime with image conversion | `image` | `Printer::with_clock(transport, driver, clock)` |
//!
//! With no features, the library supports full `no_std + alloc` streaming printing:
//! transports, drivers, codecs, packed rasters, scanline dithering, and cancellation.
//! The heap is always required; applications supply an allocator, executor, [`Clock`],
//! and connected transport. No board HAL, BLE backend, or Web Bluetooth backend is
//! supplied. `std` does not imply a runtime; `image` requires `std` but not Tokio.
//!
//! On browser and bare-metal targets, [`Transport`], [`RowSource`], and [`Clock`]
//! may use local, non-`Send` futures. Native targets retain `Send` futures and
//! `Send + Sync` clocks. Without pointer atomics, cancellation stays on one executor,
//! not across threads or interrupts. Browser clocks must yield to browser tasks.
//!
//! Codec availability does not guarantee RAM fit: LZO's dictionary alone is about
//! 455 KB. Raw Mono avoids that dictionary. Making this library `no_std` does not
//! remove std, ESP-IDF, or allocations from firmware that still uses them.
//! Custom BLE backends can use [`Driver::ble_profile`] without native Bluetooth.
//! Before creating a printer, discover, validate, and subscribe to a compatible
//! service as described by [`BleProfile`] and [`Transport`].
//! For native Bluetooth connections, use `Device::connect`.
//! The [protocol reference](protocol) records firmware research for driver
//! development and custom transports.

#![cfg_attr(feature = "image", doc = concat!(
    "\n# Offline preview\n\nThis example packs pixels without Bluetooth access or a Tokio runtime.\n\n```\n",
    include_str!("../examples/preview.rs"),
    "\n```\n",
))]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(all(test, not(feature = "std")))]
extern crate std;

#[cfg(feature = "bluetooth")]
pub mod bluetooth;
#[doc = include_str!("../docs/protocol.md")]
pub mod protocol {}
mod clock;
mod device;
mod dither;
pub mod driver;
mod encoding;
mod error;
#[cfg(feature = "image")]
mod imaging;
pub mod printer;
mod raster;
mod session;
mod stream;

pub use clock::Clock;
#[cfg(feature = "tokio")]
pub use clock::TokioClock;
pub use device::{DeviceInfo, PrinterCondition, PrinterState};
pub use dither::{Dither, RowDither};
pub use driver::*;
pub use error::{Error, Result};
#[cfg(feature = "image")]
pub use imaging::{ImageConversion, Position};
pub use printer::{
    Cancellation, Completion, PrintReport, Printer, RowSource, Transport, TransportEvent,
    WriteChannel,
};
pub use raster::{PixelFormat, Raster, RasterRows};
