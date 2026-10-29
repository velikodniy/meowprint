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

Discovery requires a powered adapter. For backends without an initial power-state query, Meowprint permits an initial `Unknown` state and lets scanning report failures. Explicit powered-off states are still rejected. If an adapter's state query fails, discovery can use other eligible adapters; if none are eligible, the query error is returned. Enable Bluetooth and grant the application Bluetooth access before discovery.

## Supply and print rows

`Printer::print(&mut source, &options).await` submits a `RowSource` with fixed width, height, and pixel format. Each successful `read_row` fills the entire packed-row destination. The library reads exactly the declared height: no EOF probe, trailing-data validation, implicit replay, or source resumption. The source or document owner handles framing and input deadlines. Without its own deadline, input can remain pending until cancellation or a printer fault.

While input is pending, notification handling cooperatively yields so other Tokio tasks can supply rows or request cancellation, even when a custom transport continuously returns ready events.

For buffered pixels, construct `Raster::new(width, bytes, format)` and pass `&mut raster.rows()` to `print`. The cursor borrows pixels without cloning or consuming them. A fresh `raster.rows()` explicitly starts again. The print future borrows the printer, source, and controls until it settles; finish it before starting another operation.

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

`ImageConversion::Mono(Dither::default())` uses Ostromoukhov diffusion. Stucki, Floyd–Steinberg, and Threshold are also available. Use `ImageConversion::Mono(Dither::Threshold)` for images already black and white. Choose `ImageConversion::Gray4` instead to round luminance to 16 levels without monochrome dithering, for X6h or V5G. The library handles quantization and low-nibble-left packing; callers do not need a separate Gray4 packing loop. Use `Raster::new` when the input is already packed.

`raster.preview()` returns an `image::GrayImage` of the exact packed pixels, excluding driver-added blank rows and paper feed. Editing the preview does not change the raster or predict physical print quality. No printer, transport, or Tokio runtime is needed; see the [offline preview example](examples/preview.rs).

The [image-loading example](examples/load_image.rs) opens a PNG, uses `Driver::supports(format)` to choose Gray4 or monochrome dithering, validates the driver controls, and saves the packed-pixel preview:

```sh
cargo run --locked -p meowprint --no-default-features --features image \
  --example load_image -- input.png preview.png
```

It selects X6h by default and preserves the input dimensions; resize images wider than the print head before conversion. PNG decoding and encoding are enabled by a development dependency for this example, not by the library's `image` feature.

Raster width must fill whole bytes (a multiple of eight for Mono or two for Gray4); rows must be complete and nonempty, with at most 32,768 rows and 16,777,216 pixels. Source images may have odd widths; positioning leaves the remaining output pixels white. Construction validates these limits before allocating conversion buffers. Image conversion produces a buffered raster, not a lazy streaming adapter; Gray4 quantizes directly into that buffer without a grayscale scratch image. Raster validation does not validate all future compression: a late encoding error can occur after earlier rows were submitted.

Use `width()`, `height()`, `format()`, and `packed()` to inspect pixels; `into_packed()` consumes the raster and moves out its original buffer. `rows()` creates a forward-only borrowed source. Sources that generate rows or read another medium can implement `RowSource` without a page-sized buffer.

`Dither::rows(width)` creates reusable `RowDither` state without the `image` feature. `apply_row(&mut row)` accepts luminance (0 black, 255 white), producing only 0 or 255. A width mismatch leaves both pixels and state unchanged. `reset()` starts a new page without reallocating. Threshold allocates no error storage; diffusion retains three error rows.

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

A successful operation leaves the connection available for another job or query. `PrintReport::completion` describes how the operation finished. Some drivers wait for a fixed interval, while MXW01 waits for a completion message. Neither result guarantees the appearance of the printed paper.

Invalid source metadata or controls are rejected before row reads or transport I/O and leave the connection usable. After submission begins, source and compression failures disable the connection, even before the first write. Later failures can leave partial output. Communication failures, cancellation, and dropping an active operation also disable further use. A genuine printer condition detected before a write can leave it usable; a source returning the same error cannot. Use `Printer::is_usable()` to decide whether to reconnect.

Always await `Printer::disconnect()` when you finish, including after errors. Previously submitted rows can still print. Meowprint does not retry failed operations automatically.

To stop an active operation, obtain `Printer::cancellation()` before starting printing. Call `cancel()` on that handle from another task, and continue awaiting the operation until it returns. Cancellation permanently disables that connection and cannot recall rows already sent.

## Custom connections and further reference

Native Bluetooth support uses the default `bluetooth` feature. To supply another connection, implement `Transport` and pass it with a driver to `Printer::new`. Disable default features to exclude native Bluetooth dependencies:

```toml
meowprint = { version = "0.1", default-features = false }
```

Meowprint requires `std` in every supported configuration; `default-features = false` removes native Bluetooth, not the standard library. There is no `no_std` feature. The minimal production dependencies still include Tokio, `lzokay` with its `std` feature, and `thiserror` with its default `std` feature.

Printer communication requires Tokio, but the library's production dependency does not require its multithread runtime. The [custom transport example](examples/custom_transport.rs) uses a current-thread runtime, a generated eight-row source, and an in-memory connection. Offline raster conversion and preview need no runtime, but still require `std`.

Submission retains at most one image row or a 20-row Gray4 band (3,840 input bytes), plus framing/control and codec state. LZO owns a large fixed dictionary; bounded memory is not a guarantee that a particular microcontroller can fit it. The library still uses `std`, Tokio, and `Send`; this API does not implement a network adapter or establish firmware/hardware compatibility.

Advanced BLE backends can obtain `BleProfile` through `Driver::ble_profile()` without enabling native Bluetooth. Its 16-bit Bluetooth-base UUIDs describe alternative services and control/raster/notification endpoints, not compatibility proof or service priority. Backend setup must find all endpoints within one compatible service, require `WRITE_WITHOUT_RESPONSE` for both writes and `NOTIFY` for notifications, reject ambiguous fully compatible services, and subscribe before creating `Printer`. Control and raster may share a characteristic. Ordinary native callers continue using `Device::connect(driver)`.

See the [API documentation](https://docs.rs/meowprint) for paper movement, cancellation, and individual controls. The [protocol reference](https://docs.rs/meowprint/latest/meowprint/protocol/index.html) records firmware research for driver development and custom transports.
