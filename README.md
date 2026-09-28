# Meowprint

Meowprint prints images, text, and QR codes on cat printers: small Bluetooth thermal printers.
Use the [command-line tool](crates/meowprint-cli/README.md) from your terminal or the [Rust library](crates/meowprint/README.md) in your application.
Built-in printer drivers include GT01, MX10, X6h, V5G, and MXW01.

## Quick start

Use Rust 1.88 or later.
On macOS, install the Xcode Command Line Tools and allow Bluetooth access for your terminal.
On Linux, install the D-Bus development packages and `pkg-config`.
From the repository root, install the `meowprint` command:

```sh
cargo install --path crates/meowprint-cli --locked
```

Create a PNG preview:

```sh
meowprint preview --driver mx10 -o preview.png text 'Hello from Meowprint!'
```

Find your printer and list the available drivers:

```sh
meowprint scan
meowprint drivers
```

A driver selects the commands for your printer firmware.
For an MX10 with matching firmware, print the text:

```sh
meowprint print --device MX10 --driver mx10 text 'Hello from Meowprint!'
```

See the [command-line guide](crates/meowprint-cli/README.md) for more examples and the [API documentation](https://docs.rs/meowprint) for library use.
The [protocol reference](crates/meowprint/docs/protocol.md) describes firmware behavior and compatibility.

## Development

Install [prek](https://prek.j178.dev/installation/) and the Rust components used by the hooks:

```sh
rustup component add rustfmt clippy
prek install
```

The hooks run Rust formatting, Clippy, `cargo check`, and common file checks before each commit.
Rust checks run when source files, manifests, or crate assets change.
Run all hooks manually with:

```sh
prek run --all-files
```
