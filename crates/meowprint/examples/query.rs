//! Read an MXW01 firmware version. Pass a device identifier as the first argument.
use meowprint::{Mxw01, bluetooth::Bluetooth};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let selector = std::env::args()
        .nth(1)
        .ok_or("Supply a device identifier.")?;
    let driver = Mxw01::default();
    let bluetooth = Bluetooth::new().await?;
    let device = bluetooth.find(&selector, Duration::from_secs(8)).await?;
    let mut printer = device.connect(driver).await?;
    let reply = printer.device_info().await;
    let disconnected = printer.disconnect().await;
    if let Some(version) = reply?.firmware_version {
        println!("Firmware version: {version}");
    } else {
        println!("The printer replied, but its firmware version is unknown.");
    }
    disconnected?;
    Ok(())
}
