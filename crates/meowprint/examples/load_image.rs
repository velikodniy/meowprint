//! Load a PNG and save the exact packed-pixel preview without Bluetooth or Tokio.
//!
//! Run with `cargo run -p meowprint --no-default-features --features image
//! --example load_image -- input.png preview.png`.
use meowprint::{Dither, Driver, ImageConversion, PixelFormat, Position, Raster, X6h};
use std::{env, error::Error};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let input = args
        .next()
        .ok_or("Usage: load_image <input.png> <preview.png>")?;
    let output = args
        .next()
        .ok_or("Usage: load_image <input.png> <preview.png>")?;

    // Choose the driver matching your firmware; Gt01, for example, selects Mono.
    let driver = X6h;
    let conversion = if driver.supports(PixelFormat::Gray4) {
        ImageConversion::Gray4
    } else {
        ImageConversion::Mono(Dither::default())
    };
    let image = image::open(input)?;
    // Keep the original size; images wider than the print head are rejected.
    let raster = Raster::from_image(
        &image,
        driver.printable_width(),
        Position::Center,
        conversion,
    )?;
    driver.validate(&meowprint::X6hOptions::default(), raster.format())?;
    raster.preview().save(&output)?;
    println!(
        "Saved {}x{} {:?} preview to {}",
        raster.width(),
        raster.height(),
        raster.format(),
        output.to_string_lossy(),
    );
    // To print after connecting, pass &mut raster.rows() to Printer::print.
    Ok(())
}
