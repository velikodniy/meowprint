//! Human-readable device information without protocol fields.

use crate::args::QueryArg;
use meowprint::{DeviceInfo, Driver, Printer, PrinterState, Result, Transport};

pub async fn read<T: Transport, D: Driver>(
    printer: &mut Printer<T, D>,
    query: QueryArg,
) -> Result<String> {
    Ok(match query {
        QueryArg::Status => format!("State: {}", format_state(&printer.status().await?)),
        QueryArg::Info => format_info(&printer.device_info().await?),
    })
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

fn format_state(state: &PrinterState) -> String {
    match state {
        // Keep the CLI's shorter labels; the library formats conditions consistently.
        PrinterState::Printing => "printing".to_owned(),
        PrinterState::Unknown => "unknown".to_owned(),
        _ => state.to_string(),
    }
}
