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
use meowprint::{Gt01, PixelFormat, Raster, bluetooth::Bluetooth};
use std::time::Duration;

#[tokio::main]
async fn main() -> meowprint::Result<()> {
    let bluetooth = Bluetooth::new().await?;
    let device = bluetooth.find("GT01", Duration::from_secs(8)).await?;
    let mut printer = device.connect(Gt01).await?;
    let row_bytes = (printer.printable_width() / 8) as usize;

    let result = async {
        let raster = Raster::new(printer.printable_width(), vec![0xff; row_bytes * 8], PixelFormat::Mono)?;
        printer
            .print(&mut raster.rows(), &Default::default())
            .await
    }
    .await;
    // Attempt to disconnect even if pixel validation or printing fails.
    let disconnected = printer.disconnect().await;
    let report = result?;
    disconnected?;
    println!("Completion evidence: {:?}", report.completion);
    Ok(())
}
```

`Bluetooth::find` accepts an exact device name or a platform device identifier. Use `Bluetooth::scan` to list devices. If several devices share a name, use an identifier to select one.

Discovery requires a powered Bluetooth adapter. On Android, adapters with an unknown initial power state may also be scanned. Adapters reported as powered off are rejected. A state-query failure does not prevent discovery through another eligible adapter; if none are eligible, the query error is returned. Enable Bluetooth and grant the application Bluetooth access before discovery.

## Supply and print rows

`Printer::print(&mut source, &options).await` reads packed rows from a `RowSource` with fixed width, height, and pixel format. Each successful `read_row` call fills the entire destination buffer. The library requests exactly the declared number of rows. It does not read an extra row to check for end-of-input, validate trailing data, or restart the source. The source or its caller handles document framing and input deadlines. Without a source deadline, a row read can remain pending until cancellation or a printer fault.

While a row read is pending, other Tokio tasks can supply input or request cancellation, including when transport events are continuously available.

For buffered pixels, construct `Raster::new(width, bytes, format)` and pass `&mut raster.rows()` to `print`. The cursor borrows pixels without cloning or consuming them. Each `raster.rows()` call starts at the first row. The print future borrows the printer, source, and controls until it completes; await it before starting another operation.

Each row must fill `Printer::printable_width()` dots. All built-in drivers use a width of 384 dots. Store rows from top to bottom, with no headers or gaps:

| Format | Pixel order within each byte | Pixel values | Bytes per 384-dot row |
| --- | --- | --- | --- |
| `PixelFormat::Mono` | Bit 0 is the leftmost pixel | 0 is white, 1 is black | 48 |
| `PixelFormat::Gray4` | The low four bits hold the left pixel | 0 is white, 15 is black | 192 |

Sources declare a height from 1 through 32,768 rows. For a `Raster`, the byte count determines height and must contain complete rows. Before allocating pixels, use `Driver::printable_width()` and `Driver::max_height()` to obtain the limits without a connection.

Pass `&Default::default()` for the driver defaults. To change controls such as energy or final paper feed, use the options type for your driver. This example changes the GT01 final feed to 48 rows:

```rust
use meowprint::Gt01Options;

let options = Gt01Options {
    feed: 48,
    ..Default::default()
};
```

Pass `&options` as the last argument to `print`. The [driver reference](https://docs.rs/meowprint/latest/meowprint/driver/index.html) lists supported controls, ranges, and defaults. For drivers with an optional `density`, `None` keeps the existing printer density, including a value set by an earlier job.

### Supported pixel formats

| Driver | `Mono` | `Gray4` |
| --- | --- | --- |
| `Gt01` | Yes | No |
| `Mx10` | Yes | No |
| `CommonRaw` | Yes | No |
| `TinyRle` | Yes | No |
| `PrefixedTiny` | Yes | No |
| `X6h` | Yes | Yes |
| `V5g` | Yes | Yes |
| `Mxw01` | Yes | No |

Use `Driver::supports(format)` to query pixel-format capability without connecting:

```rust
use meowprint::{Driver, Gt01, PixelFormat, X6h, X6hOptions};

assert!(Gt01.supports(PixelFormat::Mono));
assert!(!Gt01.supports(PixelFormat::Gray4));
assert!(X6h.supports(PixelFormat::Gray4));

// Check the actual format together with the selected controls and encoding.
X6h.validate(&X6hOptions::default(), PixelFormat::Gray4)?;
```

`supports(format)` describes the driver capability, not every encoding it offers. `Driver::validate(&options, format)` returns an error for an unsupported format, incompatible encoding, or invalid controls. Neither method performs printer I/O. `Printer::print` also validates this combination before reading source rows or writing to the printer.

From the CLI, run `meowprint drivers` without a printer: `grayscale true` means Mono and Gray4; `grayscale false` means Mono only. These are driver capabilities, not a guarantee that a similarly named device has compatible firmware.

## Convert images and preview pixels

Enable the optional `image` feature for buffered image conversion and raster previews. Add `image` to load files in your application:

```toml
meowprint = { version = "0.1", features = ["image"] }
image = { version = "0.25", default-features = false, features = ["png"] }
```

Convert an `image::DynamicImage` once, optionally preview it, then submit borrowed rows:

```rust
use meowprint::{Dither, Driver, Gt01, ImageConversion, Position, Raster};

let raster = Raster::from_image(
    &image,
    Gt01.printable_width(),
    Position::Center,
    ImageConversion::Mono(Dither::default()),
)?;
let preview = raster.preview();
let report = printer.print(&mut raster.rows(), &Default::default()).await?;
```

Your application loads image files, draws text, and resizes images. `Raster::from_image` rejects images wider than the print head and keeps the original size. `Position` controls horizontal alignment; unused columns stay white. Transparent pixels are composited on white.

`ImageConversion::Mono(Dither::default())` uses Ostromoukhov dithering. Stucki, Floyd–Steinberg, and Threshold are also available. Use `ImageConversion::Mono(Dither::Threshold)` for images already black and white. For X6h or V5G grayscale output, `ImageConversion::Gray4` rounds luminance to 16 levels and packs pixels in the Gray4 format. Use `Raster::new` when the input is already packed.

`raster.preview()` returns an `image::GrayImage` of the exact packed pixels, excluding driver-added blank rows and paper feed. Editing the preview does not change the raster or predict physical print quality. No printer, transport, or Tokio runtime is needed; see the [offline preview example](examples/preview.rs).

The [image-loading example](examples/load_image.rs) opens a PNG, uses `Driver::supports(format)` to choose Gray4 or monochrome dithering, validates the driver controls, and saves the packed-pixel preview:

```sh
cargo run --locked -p meowprint --no-default-features --features image \
  --example load_image -- input.png preview.png
```

It selects X6h by default and preserves the input dimensions; resize images wider than the print head before conversion. PNG decoding and encoding are enabled by a development dependency for this example, not by the library's `image` feature.

Raster width must be a multiple of eight for Mono or two for Gray4. Rows must be complete and nonempty, with at most 32,768 rows and 16,777,216 pixels. Source images may have odd widths; unused output columns remain white. Dimension limits are checked before allocating conversion buffers.

Image conversion returns a buffered `Raster`. Compression occurs during printing, so an encoding error can occur after earlier rows have been submitted.

Use `width()`, `height()`, `format()`, and `packed()` to inspect pixels. `into_packed()` consumes the raster and returns its packed pixels without copying. `rows()` creates a forward-only borrowed source. Sources that generate rows or read another medium can implement `RowSource` without buffering a complete page.

`Dither::rows(width)` creates a reusable `RowDither` without the `image` feature. `apply_row(&mut row)` accepts luminance values from 0 (black) through 255 (white) and produces only 0 or 255. A width mismatch leaves both pixels and state unchanged. `reset()` starts a new image without reallocating.

With the `image` feature, `Dither::apply(&mut image)` processes only the declared image dimensions. Empty images and any trailing bytes in an oversized backing buffer remain unchanged.

Meowprint itself does not enable image file formats. The `png` feature above enables PNG files. Enable other formats in your application's `image` dependency as needed. To use image preparation without native Bluetooth dependencies, set `default-features = false` alongside `features = ["image"]` on `meowprint`.

## Read printer information

`Printer::status()` returns a `PrinterState` with named states and conditions such as ready, out of paper, or overheated. `Printer::device_info()` returns optional device text and firmware version fields; missing or uninterpretable fields are `None`. An unknown state never implies readiness.

```no_run
use meowprint::{Driver, Printer, PrinterState, Transport};

async fn show_status<T: Transport, D: Driver>(printer: &mut Printer<T, D>) -> meowprint::Result<()> {
    let status = printer.status().await?;
    if status == PrinterState::Ready {
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

A successful operation leaves the connection available for another job or query. `PrintReport::completion` describes how the operation finished. Some drivers wait for a fixed interval, while MXW01 waits for a completion message. Neither result guarantees physical print quality.

Invalid source metadata or controls are rejected before row reads or transport I/O and leave the connection usable. Once printing starts, source and compression errors disable the connection, even before the first write. Errors after submission can leave partial output. Communication errors, cancellation, and dropping an active operation also disable the connection. A condition reported by the printer before any write can leave the connection usable; an error returned by `read_row` cannot. Use `Printer::is_usable()` to decide whether to reconnect.

Always await `Printer::disconnect()` when you finish, including after errors. Previously submitted rows can still print. Meowprint does not retry failed operations automatically.

To stop an active operation, obtain `Printer::cancellation()` before starting printing. Call `cancel()` on that handle from another task, and continue awaiting the operation until it returns. Cancellation permanently disables that connection and cannot recall rows already sent.

## Custom connections and further reference

Native Bluetooth support uses the default `bluetooth` feature. To supply another connection, implement `Transport` and pass it with a driver to `Printer::new`. Disable default features to exclude native Bluetooth dependencies:

```toml
meowprint = { version = "0.1", default-features = false }
```

Meowprint requires `std`, even with `default-features = false`. Disabling default features removes native Bluetooth support; it does not provide `no_std` support.

Printer communication requires a Tokio runtime; a current-thread runtime is sufficient. See the [custom transport example](examples/custom_transport.rs) for a complete printing and disconnect sequence. Offline raster conversion and preview require `std` but no runtime.

Custom BLE backends can obtain `BleProfile` through `Driver::ble_profile()` without enabling native Bluetooth. Its 16-bit Bluetooth-base UUIDs identify alternative services and control, raster, and notification characteristics. They do not establish compatibility or service priority. Backend setup must find all endpoints within one compatible service, require `WRITE_WITHOUT_RESPONSE` for both write characteristics and `NOTIFY` for notifications, reject multiple compatible services, and subscribe before creating a `Printer`. Control and raster may share a characteristic. For native Bluetooth connections, use `Device::connect(driver)`.

See the [API documentation](https://docs.rs/meowprint) for paper movement, cancellation, and individual controls. The [protocol reference](https://docs.rs/meowprint/latest/meowprint/protocol/index.html) records firmware research for driver development and custom transports.
