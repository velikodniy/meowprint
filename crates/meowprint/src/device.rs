//! Information returned by printer operations.

/// Information reported by the printer firmware.
///
/// Missing or uninterpretable fields are `None`. Other printer operations remain available.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Device text when available. The text can include a firmware version.
    pub description: Option<String>,
    /// Firmware version when the library can identify it separately.
    pub firmware_version: Option<String>,
}

/// The state reported by a printer.
///
/// An unknown state does not mean that the printer is ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrinterState {
    /// The printer reports no condition that prevents a new job.
    Ready,
    /// The printer reports an active print. Currently reported only by MXW01.
    Printing,
    /// One or more conditions prevent readiness.
    Conditions(Vec<PrinterCondition>),
    /// The library does not know the reported state.
    Unknown,
}

/// A condition reported by the printer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrinterCondition {
    /// The printer needs paper.
    OutOfPaper,
    /// The printer cover is open.
    CoverOpen,
    /// The print head is too hot.
    Overheated,
    /// The battery is low.
    LowBattery,
    /// The printer requests a pause or reports a full buffer.
    Paused,
    /// The printer is busy.
    Busy,
    /// The printer reports a condition whose meaning is unknown.
    Unknown,
}

/// The printer's status at the time of its last report.
///
/// Missing or undocumented readings are `None`. A successful request can still
/// return [`PrinterState::Unknown`] if the library cannot interpret the reply.
/// See [`Printer::status`](crate::Printer::status) for when a new report is requested.
#[derive(Debug, Clone, PartialEq)]
pub struct PrinterStatus {
    /// Readiness, activity, or reported conditions.
    pub state: PrinterState,
    /// Battery level from 0 through 100 percent, or `None` when unavailable.
    ///
    /// Current drivers return `None` because the firmware's battery scale is unknown.
    pub battery_percent: Option<u8>,
    /// Print head temperature in degrees Celsius, or `None` when unavailable.
    ///
    /// Current drivers return `None` because the firmware's temperature units are unknown.
    pub temperature_celsius: Option<f32>,
}

impl PrinterState {
    pub(crate) fn blocks_submission(&self) -> bool {
        match self {
            Self::Ready | Self::Printing => false,
            Self::Unknown => true,
            Self::Conditions(conditions) => conditions.iter().any(|condition| {
                !matches!(condition, PrinterCondition::Paused | PrinterCondition::Busy)
            }),
        }
    }
}
impl std::fmt::Display for PrinterState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => formatter.write_str("ready"),
            Self::Printing => formatter.write_str("already printing"),
            Self::Unknown => formatter.write_str("unknown state"),
            Self::Conditions(conditions) => {
                for (index, condition) in conditions.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(", ")?;
                    }
                    write!(formatter, "{condition}")?;
                }
                Ok(())
            }
        }
    }
}
impl std::fmt::Display for PrinterCondition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::OutOfPaper => "out of paper",
            Self::CoverOpen => "cover open",
            Self::Overheated => "overheated",
            Self::LowBattery => "low battery",
            Self::Paused => "paused",
            Self::Busy => "busy",
            Self::Unknown => "unknown condition",
        })
    }
}
