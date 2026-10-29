//! Preview a packed raster without Bluetooth access or a Tokio runtime.
use meowprint::{Driver, PixelFormat, Raster, Result};

fn main() -> Result<()> {
    let driver = meowprint::Gt01;
    let width = driver.printable_width();
    let raster = Raster::new(width, vec![0x81; (width / 8) as usize], PixelFormat::Mono)?;
    let preview = raster.preview();
    assert_eq!(preview.dimensions(), (width, 1));
    assert_eq!(preview.get_pixel(0, 0)[0], 0);
    assert_eq!(preview.get_pixel(1, 0)[0], 255);
    Ok(())
}
