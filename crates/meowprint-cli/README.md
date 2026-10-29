# Meowprint CLI

Meowprint is a command-line tool for cat printers: small Bluetooth thermal printers.
Print images, text, and QR codes, or save them as PNG previews.
This package provides the `meowprint` command.

## Install

Use Rust 1.88 or later.
On macOS, install the Xcode Command Line Tools and allow Bluetooth access for your terminal.
On Linux, install the D-Bus development packages and `pkg-config`.
From the repository root, install the command:

```sh
cargo install --path crates/meowprint-cli --locked
```

## Preview and print

Replace `input.jpg` with the path to your image.
Save a PNG preview for the MX10 driver:

```sh
meowprint preview --driver mx10 -o preview.png image input.jpg
```

Previews are expanded from the packed raster without Bluetooth access. The CLI renders and packs the full page before connecting; `print --preview` saves that exact raster before discovery or submission.

Find your printer and list the available drivers:

```sh
meowprint scan
meowprint drivers
```

`meowprint drivers` needs no Bluetooth access. Every listed driver supports Mono.
Its `grayscale true` entries (X6h and V5g) also support Gray4 through `image --grayscale`;
`grayscale false` means Mono only. See the [supported-format table](../meowprint/README.md#supported-pixel-formats)
for all drivers and the library's capability checks.

The `--device` value selects a printer by name or identifier from the scan.
The `--driver` value selects the commands for its firmware.
For an MX10 with matching firmware, print an image, text, or a QR code:

```sh
meowprint print --device MX10 --driver mx10 image input.jpg
meowprint print --device MX10 --driver mx10 text 'Hello from Meowprint!'
meowprint print --device MX10 --driver mx10 qr 'https://example.com'
```

Text uses the embedded Roboto Regular font on all platforms.
Roboto supports Latin, Greek, and Cyrillic text.
Use `text --font path/to/font.ttf` to select another TrueType or OpenType font.

Run `meowprint --help` for all commands and `meowprint print --help` for print controls.
The CLI renders, validates, and packs input before opening a Bluetooth connection. During printing, the library submits rows incrementally rather than compressing the complete page in advance. A later input or compression failure can occur after earlier rows were sent; the print outcome is then unknown, so do not retry automatically.
See the [protocol reference](https://docs.rs/meowprint/latest/meowprint/protocol/index.html) for firmware compatibility.

## Read printer information

For an MXW01 with matching firmware, read its status or device information:

```sh
meowprint query --device MXW01 --driver mxw01 status
meowprint query --device MXW01 --driver mxw01 info
```

The output uses names such as `ready`, `out of paper`, and `overheated`.
The CLI marks missing information as `unknown`.
Status includes battery and temperature fields.
Both remain `unknown` until the firmware units are established.
`version` remains an alias for `info`.

Licensed under MIT.
The embedded [Roboto font](assets/fonts/README.md) uses the SIL Open Font License 1.1.
