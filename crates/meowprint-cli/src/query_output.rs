//! Human-readable device information without protocol fields.

use crate::args::QueryArg;
use meowprint::{DeviceInfo, Driver, Printer, PrinterState, PrinterStatus, Result, Transport};

pub async fn read<T: Transport, D: Driver>(
    printer: &mut Printer<T, D>,
    query: QueryArg,
) -> Result<String> {
    Ok(match query {
        QueryArg::Status => format_status(&printer.status().await?),
        QueryArg::Info => format_info(&printer.device_info().await?),
    })
}

fn format_status(status: &PrinterStatus) -> String {
    format!(
        "State: {}\n{}\n{}",
        format_state(&status.state),
        format_reading("Battery", status.battery_percent, "%"),
        format_reading("Temperature", status.temperature_celsius, " °C")
    )
}

fn format_info(info: &DeviceInfo) -> String {
    let mut lines = Vec::new();
    if let Some(description) = &info.description {
        lines.push(format!(
            "Device information: {}",
            description.escape_debug()
        ));
    }
    if let Some(version) = &info.firmware_version {
        lines.push(format!("Firmware version: {}", version.escape_debug()));
    }
    if lines.is_empty() {
        "Device information: unknown (the printer replied, but its response is not understood)"
            .to_owned()
    } else {
        lines.join("\n")
    }
}

fn format_reading(label: &str, reading: Option<impl std::fmt::Display>, unit: &str) -> String {
    reading.map_or_else(
        || format!("{label}: unknown"),
        |reading| format!("{label}: {reading}{unit}"),
    )
}

fn format_state(state: &PrinterState) -> String {
    match state {
        // Keep the CLI's shorter labels; the library formats conditions consistently.
        PrinterState::Printing => "printing".to_owned(),
        PrinterState::Unknown => "unknown".to_owned(),
        _ => state.to_string(),
    }
}
