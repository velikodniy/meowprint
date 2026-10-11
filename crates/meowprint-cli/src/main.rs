//! Content rendering and terminal access for Meowprint.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod args;
mod content;
mod error;
mod preview;
mod query_output;
mod render;
mod session;

use anyhow::Result as CliResult;
use args::{Cli, CliDriver, Command, Connection, Content, Controls, DriverArg, QueryArg};
use clap::{Parser, ValueEnum};
use error::Result;
use meowprint::{
    Driver, Printer,
    bluetooth::{BleTransport, Bluetooth, Device},
};
use std::{path::PathBuf, time::Duration};

async fn find_device(selector: &str) -> CliResult<Device> {
    let bluetooth = Bluetooth::new().await?;
    Ok(bluetooth.find(selector, Duration::from_secs(8)).await?)
}
async fn connect<D: Driver>(
    connection: &Connection,
    driver: D,
) -> CliResult<Printer<BleTransport, D>> {
    eprintln!("Finding printer {}...", connection.device);
    let started = std::time::Instant::now();
    let device = find_device(&connection.device).await?;
    eprintln!(
        "Found {} in {:.2}s. Connecting...",
        device.id,
        started.elapsed().as_secs_f64()
    );
    let started = std::time::Instant::now();
    let printer = device.connect(driver).await?;
    eprintln!("Connected in {:.2}s.", started.elapsed().as_secs_f64());
    Ok(printer)
}
async fn print<D: CliDriver>(
    connection: Connection,
    driver: D,
    controls: Controls,
    preview: Option<PathBuf>,
    content: Content,
) -> CliResult<()> {
    let started = std::time::Instant::now();
    let options = D::options(controls)?;
    let format = content.format();
    driver.validate(&options, format)?;
    eprintln!("Rendering...");
    let image = content.render(driver)?;
    eprintln!(
        "Rendered {} x {} dots in {:.2}s.",
        image.width(),
        image.height(),
        started.elapsed().as_secs_f64()
    );
    let printer = connect(&connection, driver).await?;
    let report = session::Session::new(printer, tokio::signal::ctrl_c())
        .print(image, format, options, preview)
        .await?;
    println!(
        "Submitted {} image rows ({} bytes) in {:.2}s. Completion evidence: {:?}. Physical output is not confirmed.",
        report.image_rows,
        report.bytes_submitted,
        started.elapsed().as_secs_f64(),
        report.completion
    );
    Ok(())
}
async fn query<D: Driver>(connection: Connection, driver: D, query: QueryArg) -> CliResult<()> {
    let printer = connect(&connection, driver).await?;
    let output = session::Session::new(printer, tokio::signal::ctrl_c())
        .query(query)
        .await?;
    println!("{output}");
    Ok(())
}

async fn movement<D: Driver>(
    connection: Connection,
    driver: D,
    rows: u16,
    retract: bool,
) -> CliResult<()> {
    if retract {
        driver.validate_retract(rows)?;
    } else {
        driver.validate_feed(rows)?;
    }
    let printer = connect(&connection, driver).await?;
    let report = session::Session::new(printer, tokio::signal::ctrl_c())
        .movement(rows, retract)
        .await?;
    println!(
        "Submitted {} bytes. Completion evidence: {:?}.",
        report.bytes_submitted, report.completion
    );
    Ok(())
}
async fn scan(all: bool, seconds: u64) -> CliResult<()> {
    let bluetooth = Bluetooth::new().await?;
    let devices = bluetooth.scan(Duration::from_secs(seconds)).await?;
    let mut count = 0;
    for device in devices
        .into_iter()
        .filter(|d| all || d.is_printer_candidate())
    {
        println!(
            "{}  {}  RSSI {:?}  services {:?}",
            device.id,
            device.name.as_deref().unwrap_or("(unnamed)"),
            device.rssi,
            device.advertised_services
        );
        count += 1;
    }
    if count == 0 {
        eprintln!("No devices matched. Turn on the printer and try meowprint scan --all.");
    }
    Ok(())
}
async fn inspect(selector: &str) -> CliResult<()> {
    let device = find_device(selector).await?;
    println!(
        "{}  {}\nManufacturer data: {:?}",
        device.id,
        device.name.as_deref().unwrap_or("(unnamed)"),
        device.manufacturer_data
    );
    let inspection = device.inspect().await?;
    println!("Maximum write: {} bytes", inspection.max_write_size);
    for service in inspection.services {
        println!("Service {}", service.uuid);
        for characteristic in service.characteristics {
            println!("  {} {:?}", characteristic.uuid, characteristic.properties);
        }
    }
    Ok(())
}
async fn execute<D: CliDriver>(command: Command, driver: D) -> CliResult<()> {
    match command {
        Command::Drivers => println!(
            "{}: {} dots, grayscale {}",
            driver.name(),
            driver.printable_width(),
            driver.supports_gray4()
        ),
        Command::Preview {
            output, content, ..
        } => {
            let format = content.format();
            driver.validate(&Default::default(), format)?;
            let image = content.render(driver)?;
            let (width, height) = image.dimensions();
            preview::render(driver, image, format)?
                .save_with_format(&output, image::ImageFormat::Png)?;
            println!("Saved {} ({} x {} dots).", output.display(), width, height);
        }
        Command::Print {
            connection,
            controls,
            preview,
            content,
        } => print(connection, driver, controls, preview, content).await?,
        Command::Query {
            connection,
            query: request,
        } => query(connection, driver, request).await?,
        Command::Feed { connection, rows } => movement(connection, driver, rows, false).await?,
        Command::Retract { connection, rows } => movement(connection, driver, rows, true).await?,
        Command::Cancel { connection } => {
            driver.validate_cancel()?;
            let mut printer = connect(&connection, driver).await?;
            printer.cancel().await?;
            println!("Cancellation submitted. Rows already sent can still print.");
        }
        Command::Scan { all, seconds } => scan(all, seconds).await?,
        Command::Inspect { device } => inspect(&device).await?,
    }
    Ok(())
}
#[tokio::main]
async fn main() -> CliResult<()> {
    let command = Cli::parse().command;
    let driver = match &command {
        Command::Scan { all, seconds } => return scan(*all, *seconds).await,
        Command::Inspect { device } => return inspect(device).await,
        Command::Drivers => {
            for driver in DriverArg::value_variants() {
                driver.execute(Command::Drivers).await?;
            }
            return Ok(());
        }
        Command::Preview { driver, .. } => *driver,
        Command::Print { connection, .. }
        | Command::Query { connection, .. }
        | Command::Feed { connection, .. }
        | Command::Retract { connection, .. }
        | Command::Cancel { connection } => connection.driver,
    };
    driver.execute(command).await
}
