//! Print a blank row with GT01 firmware. Pass a device identifier as the first argument.
use meowprint::{Gt01, Gt01Options, PixelFormat, bluetooth::Bluetooth};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let selector = std::env::args()
        .nth(1)
        .ok_or("Supply a device identifier.")?;
    let driver = Gt01;
    let bluetooth = Bluetooth::new().await?;
    let device = bluetooth.find(&selector, Duration::from_secs(8)).await?;
    let mut printer = device.connect(driver).await?;
    let result = async {
        printer
            .prepare(vec![0; 48], PixelFormat::Mono, &Gt01Options::default())?
            .print()
            .await
    }
    .await;
    let disconnected = printer.disconnect().await;
    println!("Completion evidence: {:?}", result?.completion);
    disconnected?;
    Ok(())
}
