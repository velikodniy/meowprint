# Meowprint

Meowprint is a Rust library for cat printers: small Bluetooth thermal printers. It provides device discovery, monochrome and grayscale printing, image previews, status queries, and paper feed controls. Built-in drivers include GT01, MX10, X6h, V5G, and MXW01.

## Quick start

Use Rust 1.88 or later. On Linux, install the D-Bus development packages and `pkg-config`. On macOS, allow Bluetooth access for your application or terminal.

Add these dependencies to your `Cargo.toml`:

```toml
[dependencies]
meowprint = "0.1"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

A driver tells Meowprint how to communicate with your printer. Choose a [driver](https://docs.rs/meowprint/latest/meowprint/driver/index.html) that matches its firmware. A Bluetooth name alone does not prove compatibility.

This example uses the GT01 driver to print a black stripe, 384 dots wide and eight rows tall:

```no_run
use meowprint::{Gt01, PixelFormat, bluetooth::Bluetooth};
use std::time::Duration;

#[tokio::main]
async fn main() -> meowprint::Result<()> {
    let bluetooth = Bluetooth::new().await?;
    let device = bluetooth.find("GT01", Duration::from_secs(8)).await?;
    let mut printer = device.connect(Gt01).await?;
    let row_bytes = (printer.printable_width() / 8) as usize;

    let result = async {
        printer
            .prepare(vec![0xff; row_bytes * 8], PixelFormat::Mono, &Default::default())?
            .print()
            .await
    }
    .await;
    // Attempt to disconnect even if preparation or printing fails.
    let disconnected = printer.disconnect().await;
    let report = result?;
    disconnected?;
    println!("Completion evidence: {:?}", report.completion);
    Ok(())
}
```

`Bluetooth::find` accepts an exact device name or a platform device identifier. Use `Bluetooth::scan` to list devices. If several devices share a name, use an identifier to select one.

## Prepare and print pixels

`Printer::prepare(bytes, format, &options)` takes ownership of packed pixels and returns a `PrintJob`. Preparation does not communicate with the printer. Call `job.print().await` to print, or drop the job to discard it. Finish or discard the job before querying the printer or preparing another job.

Each row must fill `Printer::printable_width()` dots. All built-in drivers use a width of 384 dots. Store rows from top to bottom, with no headers or gaps:

| Format | Pixel order within each byte | Pixel values | Bytes per 384-dot row |
| --- | --- | --- | --- |
| `PixelFormat::Mono` | Bit 0 is the leftmost pixel | 0 is white, 1 is black | 48 |
| `PixelFormat::Gray4` | The low four bits hold the left pixel | 0 is white, 15 is black | 192 |

Only X6h and V5G accept `Gray4`. The byte count determines the image height. Images must contain complete rows, with a height from 1 through 32,768 rows. Before allocating pixels, use `Driver::printable_width()` and `Driver::max_height()` to obtain the limits without a connection.

Pass `&Default::default()` for the driver defaults. To change controls such as energy or final paper feed, use the options type for your driver. This example changes the GT01 final feed to 48 rows:

```rust
use meowprint::Gt01Options;

let options = Gt01Options {
    feed: 48,
    ..Default::default()
};
```

Pass `&options` as the last argument to `prepare`. The [driver reference](https://docs.rs/meowprint/latest/meowprint/driver/index.html) lists supported controls, ranges, and defaults. For drivers with an optional `density`, `None` keeps the existing printer density, including a value set by an earlier job.

## Prepare images and previews

Enable the optional `image` feature to convert decoded images and preview prepared jobs. Add `image` to load files in your application:

```toml
meowprint = { version = "0.1", features = ["image"] }
image = { version = "0.25", default-features = false, features = ["png"] }
```

After connecting, pass an `image::DynamicImage` to `prepare_from_image`:

```rust
use meowprint::{Dither, Position};

let job = printer.prepare_from_image(
    &image,
    Position::Center,
    Dither::default(),
    &Default::default(),
)?;
let preview = job.preview();
let report = job.print().await?;
```

Your application loads image files, draws text, and resizes images. Preparation rejects images wider than the print head and keeps the original image size. `Position` controls horizontal alignment, and unused columns stay white. Transparent pixels appear on a white background.

Preparation converts the image to black and white through dithering, which represents shades with patterns of dots. `Dither::default()` uses the Ostromoukhov method. Stucki, Floyd–Steinberg, and a fixed threshold are also available. Use `Dither::Threshold` for images that are already black and white. For packed four-bit grayscale, use `prepare`.

`job.preview()` returns an `image::GrayImage` of the prepared pixels. It excludes blank rows and paper feed added by the driver. Editing the preview does not change the job, and the preview does not predict physical print quality.

For an offline preview, create a printer with `Printer::new(NoOpTransport, Gt01)`. Preparation and preview need no Bluetooth connection or Tokio runtime. See the [offline preview example](examples/preview.rs).

Meowprint itself does not enable image file formats. The `png` feature above enables PNG files. Enable other formats in your application's `image` dependency as needed. To use image preparation without native Bluetooth dependencies, set `default-features = false` alongside `features = ["image"]` on `meowprint`.

## Read printer information

`Printer::status()` returns named states and conditions, such as ready, out of paper, or overheated. `Printer::device_info()` returns device text and a firmware version when available. Missing or unknown readings are `None`. An unknown state does not mean that the printer is ready.

```no_run
use meowprint::{Driver, Printer, PrinterState, Transport};

async fn show_status<T: Transport, D: Driver>(printer: &mut Printer<T, D>) -> meowprint::Result<()> {
    let status = printer.status().await?;
    if status.state == PrinterState::Ready {
        println!("The printer is ready.");
    }
    let info = printer.device_info().await?;
    if let Some(version) = info.firmware_version {
        println!("Firmware version: {version}");
    }
    Ok(())
}
```

Status can reflect a recent notification instead of a new request. While the printer is paused, `status()` returns the latest known status. See the [query example](examples/query.rs) for a complete connection and cleanup sequence.

## Completion and errors

A successful operation leaves the connection available for another job or query. `PrintReport::completion` describes how the operation finished. Some drivers wait for a fixed interval, while MXW01 waits for a completion message. Neither result guarantees the appearance of the printed paper.

Preparation errors leave the connection usable. Communication failures, cancellation, and dropping an active operation disable further use of the connection. A printer condition detected before a write can leave it usable. After an error, use `Printer::is_usable()` to decide whether to reconnect.

Always await `Printer::disconnect()` when you finish, including after errors. Previously submitted rows can still print. Meowprint does not retry failed operations automatically.

To stop an active operation, obtain `Printer::cancellation()` before preparing the job. Call `cancel()` on that handle from another task, and continue awaiting the operation until it returns. Cancellation permanently disables that connection and cannot recall rows already sent.

## Custom connections and further reference

Native Bluetooth support uses the default `bluetooth` feature. To supply another connection, implement `Transport` and pass it with a driver to `Printer::new`. Disable default features to exclude native Bluetooth dependencies:

```toml
meowprint = { version = "0.1", default-features = false }
```

Printer communication requires Tokio. The [custom transport example](examples/custom_transport.rs) demonstrates the interface with an in-memory connection. For offline preparation only, use `NoOpTransport`.

See the [API documentation](https://docs.rs/meowprint) for paper movement, cancellation, and individual controls. The [protocol reference](https://docs.rs/meowprint/latest/meowprint/protocol/index.html) records firmware research for driver development and custom transports.
