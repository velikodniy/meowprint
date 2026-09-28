//! The MXW01 firmware driver, controls, and protocol behavior.
#[cfg(feature = "bluetooth")]
use super::Endpoints;
use super::framing::{Decoder, EncodedCommand, Frame, encode};
use super::{Observation, Operations, Settings};
use crate::session::{Operation, REPLY_TIMEOUT, ReplyKind};
use crate::{
    Completion, DeviceInfo, Driver, Error, PixelFormat, PrinterCondition, PrinterState,
    PrinterStatus, Raster, Result, Transport, WriteChannel, error::invalid,
};
use std::time::Duration;

/// The MXW01 driver for monochrome printing.
///
/// Use [`Self::default`] for firmware that includes a checksum in its replies.
/// If your firmware omits that checksum, select
/// [`Mxw01ReplyFormat::WithoutCrc`](super::Mxw01ReplyFormat::WithoutCrc).
/// The library does not detect the reply format automatically.
///
/// MXW01 supports intensity control and reports print completion. It does not
/// support explicit paper feed or retraction. Jobs shorter than 90 rows receive
/// additional blank rows.
///
/// ```
/// use meowprint::{Printer, Transport, Mxw01, Mxw01ReplyFormat};
/// fn connect_transport<T: Transport>(transport: T) -> Printer<T, Mxw01> {
///     Printer::new(transport, Mxw01 { reply_format: Mxw01ReplyFormat::WithoutCrc })
/// }
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Mxw01 {
    /// Whether printer replies include a checksum. The default requires one.
    pub reply_format: ReplyFormat,
}
/// Print controls accepted by [`Mxw01`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mxw01Options {
    /// Print intensity, from 0 through 255. Default 93.
    ///
    /// Values use the printer's own scale, not a calibrated heat measurement.
    pub intensity: u8,
}
impl Default for Mxw01Options {
    fn default() -> Self {
        Self { intensity: 0x5d }
    }
}
pub struct Job {
    data: Vec<u8>,
    intensity: EncodedCommand,
    status: EncodedCommand,
    start: EncodedCommand,
    flush: EncodedCommand,
}
const MAX_PAYLOAD: usize = 255;
impl Driver for Mxw01 {
    type Options = Mxw01Options;
    fn name(self) -> &'static str {
        "mxw01"
    }
    fn supports_gray4(self) -> bool {
        false
    }
    fn validate(self, _options: &Mxw01Options, format: PixelFormat) -> Result<()> {
        if format != PixelFormat::Mono {
            return Err(invalid("This driver does not support four-bit grayscale."));
        }
        Ok(())
    }
}
impl Operations for Mxw01 {
    type Job = Job;
    fn settings(&self) -> Settings {
        Settings {
            width: 384,
            chunk_size: 120,
            pacing: Duration::from_millis(25),
            finish_control_frame: true,
            prefix: None,
            #[cfg(feature = "bluetooth")]
            endpoints: Endpoints {
                services: &[0xae30, 0xaf30],
                control: 0xae01,
                raster: 0xae03,
                notify: 0xae02,
            },
        }
    }
    fn decoder(&self) -> Decoder {
        Decoder::new(
            HEADER,
            MAX_PAYLOAD,
            self.reply_format == ReplyFormat::WithCrc,
        )
    }
    fn prepare(&self, raster: &Raster, options: &Mxw01Options) -> Result<Job> {
        self.validate(options, raster.format())?;
        let total_rows = u16::try_from(raster.height() + 2)
            .map_err(|_| invalid("The padded raster exceeds the 16-bit row count."))?
            .max(90);
        let mut data = vec![0; raster.row_bytes()];
        data.extend_from_slice(raster.packed());
        data.resize(usize::from(total_rows) * raster.row_bytes(), 0);
        Ok(Job {
            data,
            intensity: Command::Intensity(options.intensity).prepare(MAX_PAYLOAD)?,
            status: Command::Query(Query::Status).prepare(MAX_PAYLOAD)?,
            start: Command::Start(total_rows).prepare(MAX_PAYLOAD)?,
            flush: Command::Flush.prepare(MAX_PAYLOAD)?,
        })
    }
    async fn submit<T: Transport>(
        operation: &mut Operation<'_, T, Self>,
        job: &Job,
        submitted: &mut usize,
    ) -> Result<Completion> {
        operation.send_command(&job.intensity, submitted).await?;
        let status = operation.request_status(&job.status, submitted).await?;
        if status.state != PrinterState::Ready {
            return Err(Error::PrinterUnavailable(status.state));
        }
        let start = operation
            .exchange(
                &job.start,
                ReplyKind::Command(0xa9),
                REPLY_TIMEOUT,
                submitted,
            )
            .await?
            .frame;
        match start.payload.first().copied() {
            Some(0) => {}
            Some(flag) => return Err(Error::StartRejected { code: flag }),
            None => return Err(Error::InvalidReply("MXW01 start")),
        }
        operation
            .send(WriteChannel::Raster, &job.data, submitted)
            .await?;
        operation
            .exchange(
                &job.flush,
                ReplyKind::Command(0xaa),
                Duration::from_secs(30),
                submitted,
            )
            .await?;
        Ok(Completion::PrinterComplete)
    }
    async fn status<T: Transport>(operation: &mut Operation<'_, T, Self>) -> Result<PrinterStatus> {
        let command = Command::Query(Query::Status).prepare(MAX_PAYLOAD)?;
        operation.status(&command).await
    }
    async fn device_info<T: Transport>(
        operation: &mut Operation<'_, T, Self>,
    ) -> Result<DeviceInfo> {
        let command = Command::Query(Query::Version).prepare(MAX_PAYLOAD)?;
        let frame = operation
            .exchange(
                &command,
                ReplyKind::Command(command.command),
                REPLY_TIMEOUT,
                &mut 0,
            )
            .await?
            .frame;
        if frame.payload.is_empty() {
            return Err(Error::InvalidReply("device information"));
        }
        let firmware_version = frame
            .payload
            .get(..frame.payload.len().saturating_sub(2))
            .and_then(super::readable_text);
        Ok(DeviceInfo {
            description: None,
            firmware_version,
        })
    }
    fn observe(&self, frame: &Frame) -> Result<Observation> {
        let status = if frame.command == 0xa1 {
            Some(Status::from_frame(frame)?.snapshot())
        } else {
            None
        };
        Ok(Observation {
            status,
            flow: None,
            reply: true,
        })
    }
    fn cancel_command(&self) -> Result<Option<EncodedCommand>> {
        Command::Cancel.prepare(MAX_PAYLOAD).map(Some)
    }
}

const HEADER: [u8; 2] = [0x22, 0x21];

/// Whether MXW01 replies include a CRC, a checksum for detecting transmission errors.
///
/// Choose the format that matches your firmware. The library does not detect it automatically.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ReplyFormat {
    /// Require a checksum in printer replies. This is the default.
    #[default]
    WithCrc,
    /// Accept printer replies that omit the checksum.
    WithoutCrc,
}

/// A query encoded by [`Command::Query`] for the MXW01 protocol family.
///
/// Applications use [`Printer::status`](crate::Printer::status) and [`Printer::device_info`](crate::Printer::device_info).
/// Only [`Self::Status`] has a field parser here, [`Status::from_frame`].
/// Other reply layouts remain specific to the firmware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    /// Observed status (`A1`).
    Status,
    /// Version and type (`B1`).
    Version,
}
impl Query {
    /// Return the command byte used to match replies.
    #[must_use]
    const fn command(self) -> u8 {
        match self {
            Self::Status => 0xa1,
            Self::Version => 0xb1,
        }
    }
}

/// One outgoing MXW01 control command, encoded by `prepare`.
///
/// These commands do not contain image bytes. MXW01 sends raw monochrome pixels
/// through [`WriteChannel::Raster`](crate::WriteChannel::Raster) after the printer
/// accepts the start command. [`Printer`](crate::Printer) supplies this sequence.
/// Encoding alone does not send commands or wait for replies.
#[derive(Debug, Clone, Copy)]
enum Command {
    /// Query observed device information.
    Query(Query),
    /// Darkness in native units. The reference default is `0x5d`.
    Intensity(u8),
    /// Start monochrome printing with this total row count, including padding.
    Start(u16),
    /// Flush after the last raster byte.
    Flush,
    /// Attempt cancellation with the tentative zero payload.
    Cancel,
}
impl Command {
    fn prepare(self, max_payload: usize) -> Result<EncodedCommand> {
        let scalar;
        let start;
        let (command, payload): (u8, &[u8]) = match self {
            Self::Query(query) => (query.command(), &[0]),
            Self::Intensity(value) => {
                scalar = [value];
                (0xa2, &scalar)
            }
            Self::Start(rows) => {
                if rows == 0 {
                    return Err(invalid("MXW01 needs at least one row."));
                }
                let [lo, hi] = rows.to_le_bytes();
                start = [lo, hi, 0x30, 0];
                (0xa9, &start)
            }
            Self::Flush => (0xad, &[0]),
            Self::Cancel => (0xac, &[0]),
        };
        let bytes = encode(HEADER, command, 0, payload, max_payload)?;
        Ok(EncodedCommand { command, bytes })
    }
}

/// An owned copy of the known fields in an MXW01 status reply.
///
/// Call [`Status::from_frame`] after a [`Query::Status`] request. The original
/// [`Frame`] retains all bytes, including fields whose meaning remains unknown.
/// This type does not borrow the frame or update when the printer state changes.
///
/// The print session requires both `state` and `flag` to be zero before starting
/// a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Status {
    /// Observed state. Zero means standby. Other values prevent a new job.
    state: u8,
    /// Overall status flag. Zero means no reported error.
    flag: u8,
    /// Error byte when the flag is nonzero.
    error: Option<u8>,
}
impl Status {
    fn snapshot(self) -> PrinterStatus {
        let state = match (self.error, self.state) {
            (None, 0) => PrinterState::Ready,
            (None, 1) => PrinterState::Printing,
            (None, _) => PrinterState::Unknown,
            (Some(error), _) => PrinterState::Conditions(vec![match error {
                1 | 9 => PrinterCondition::OutOfPaper,
                4 => PrinterCondition::Overheated,
                8 => PrinterCondition::LowBattery,
                _ => PrinterCondition::Unknown,
            }]),
        };
        PrinterStatus {
            state,
            battery_percent: None,
            temperature_celsius: None,
        }
    }

    /// Copy known status fields from their observed payload offsets.
    ///
    /// The frame must have command `A1` and at least 13 payload bytes.
    /// A nonzero flag requires a fourteenth byte for the error. The method does
    /// not test the frame family, direction, or checksum. Use a decoder for framing.
    ///
    /// # Errors
    /// Reject another command or a payload without the required offsets.
    fn from_frame(frame: &Frame) -> Result<Self> {
        let data = &frame.payload;
        if frame.command != 0xa1 || data.len() < 13 || (data[12] != 0 && data.len() < 14) {
            return Err(Error::InvalidReply("MXW01 status"));
        }
        Ok(Self {
            state: data[6],
            flag: data[12],
            error: (data[12] != 0).then(|| data[13]),
        })
    }
}
