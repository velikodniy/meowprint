//! The MXW01 firmware driver, controls, and protocol behavior.
use super::framing::{Decoder, EncodedCommand, Frame, encode};
use super::{BleProfile, Observation, Operations, Settings};
use crate::session::{Operation, REPLY_TIMEOUT, ReplyKind};
use crate::stream::RowReader;
use crate::{
    Clock, Completion, DeviceInfo, Driver, Error, PixelFormat, PrinterCondition, PrinterState,
    Result, RowSource, Transport, WriteChannel, error::invalid,
};
use alloc::vec;
use core::time::Duration;

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
/// use meowprint::{Clock, Printer, Transport, Mxw01, Mxw01ReplyFormat};
/// fn connect_transport<T: Transport, C: Clock>(transport: T, clock: C) -> Printer<T, Mxw01, C> {
///     Printer::with_clock(transport, Mxw01 { reply_format: Mxw01ReplyFormat::WithoutCrc }, clock)
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
pub struct Stream {
    intensity: EncodedCommand,
    status: EncodedCommand,
    start: EncodedCommand,
    flush: EncodedCommand,
    total_rows: u16,
    height: u32,
    row: [u8; 48],
    chunk: [u8; MAX_CHUNK_SIZE],
}
const MAX_PAYLOAD: usize = 255;
const MAX_CHUNK_SIZE: usize = 120;
impl Driver for Mxw01 {
    type Options = Mxw01Options;
    fn name(self) -> &'static str {
        "mxw01"
    }
    fn supports(self, format: PixelFormat) -> bool {
        match format {
            PixelFormat::Mono => true,
            PixelFormat::Gray4 => false,
        }
    }
    fn validate(self, _options: &Mxw01Options, format: PixelFormat) -> Result<()> {
        if format != PixelFormat::Mono {
            return Err(invalid("This driver does not support four-bit grayscale."));
        }
        Ok(())
    }
}
impl Operations for Mxw01 {
    type Stream = Stream;
    fn settings(&self) -> Settings {
        Settings {
            width: 384,
            chunk_size: MAX_CHUNK_SIZE,
            pacing: Duration::from_millis(25),
            finish_control_frame: true,
            prefix: None,
            endpoints: BleProfile {
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
    fn stream(&self, height: u32, format: PixelFormat, options: &Mxw01Options) -> Result<Stream> {
        self.validate(options, format)?;
        let total_rows = u16::try_from(
            height
                .checked_add(2)
                .ok_or_else(|| invalid("The padded raster exceeds the 16-bit row count."))?,
        )
        .map_err(|_| invalid("The padded raster exceeds the 16-bit row count."))?
        .max(90);
        Ok(Stream {
            intensity: Command::Intensity(options.intensity).prepare(MAX_PAYLOAD)?,
            status: Command::Query(Query::Status).prepare(MAX_PAYLOAD)?,
            start: Command::Start(total_rows).prepare(MAX_PAYLOAD)?,
            flush: Command::Flush.prepare(MAX_PAYLOAD)?,
            total_rows,
            height,
            row: [0; 48],
            chunk: [0; MAX_CHUNK_SIZE],
        })
    }
    async fn submit<T: Transport, S: RowSource, C: Clock>(
        operation: &mut Operation<'_, T, Self, C>,
        stream: &mut Stream,
        rows: &mut RowReader<'_, S>,
        submitted: &mut usize,
    ) -> Result<Completion> {
        let chunk_size = operation.chunk_size()?;
        rows.fill(operation, &mut stream.row).await?;
        operation.send_command(&stream.intensity, submitted).await?;
        let status = operation.request_status(&stream.status, submitted).await?;
        if status != PrinterState::Ready {
            return Err(Error::PrinterUnavailable(status));
        }
        let start = operation
            .exchange(
                &stream.start,
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
        let mut used = 0usize;
        append_chunk(
            operation,
            &mut stream.chunk[..chunk_size],
            &mut used,
            &[0; 48],
            submitted,
        )
        .await?;
        append_chunk(
            operation,
            &mut stream.chunk[..chunk_size],
            &mut used,
            &stream.row,
            submitted,
        )
        .await?;
        for _ in 1..stream.height {
            rows.fill(operation, &mut stream.row).await?;
            append_chunk(
                operation,
                &mut stream.chunk[..chunk_size],
                &mut used,
                &stream.row,
                submitted,
            )
            .await?;
        }
        for _ in 0..(usize::from(stream.total_rows) - stream.height as usize - 1) {
            append_chunk(
                operation,
                &mut stream.chunk[..chunk_size],
                &mut used,
                &[0; 48],
                submitted,
            )
            .await?;
        }
        if used != 0 {
            operation
                .send(WriteChannel::Raster, &stream.chunk[..used], submitted)
                .await?;
        }
        operation
            .exchange(
                &stream.flush,
                ReplyKind::Command(0xaa),
                Duration::from_secs(30),
                submitted,
            )
            .await?;
        Ok(Completion::PrinterComplete)
    }
    async fn status<T: Transport, C: Clock>(
        operation: &mut Operation<'_, T, Self, C>,
    ) -> Result<PrinterState> {
        let command = Command::Query(Query::Status).prepare(MAX_PAYLOAD)?;
        operation.status(&command).await
    }
    async fn device_info<T: Transport, C: Clock>(
        operation: &mut Operation<'_, T, Self, C>,
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
            Some(status(frame)?)
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
async fn append_chunk<T: Transport, C: Clock>(
    operation: &mut Operation<'_, T, Mxw01, C>,
    chunk: &mut [u8],
    used: &mut usize,
    bytes: &[u8; 48],
    submitted: &mut usize,
) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let count = (chunk.len() - *used).min(bytes.len() - offset);
        chunk[*used..*used + count].copy_from_slice(&bytes[offset..offset + count]);
        *used += count;
        offset += count;
        if *used == chunk.len() {
            operation
                .send(WriteChannel::Raster, chunk, submitted)
                .await?;
            *used = 0;
        }
    }
    Ok(())
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
/// Applications use [`Printer::status`](crate::Printer::status) and
/// [`Printer::device_info`](crate::Printer::device_info). Reply layouts vary
/// by firmware.
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
/// through [`WriteChannel::Raster`] after the printer
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

fn status(frame: &Frame) -> Result<PrinterState> {
    let data = &frame.payload;
    if frame.command != 0xa1 || data.len() < 13 || (data[12] != 0 && data.len() < 14) {
        return Err(Error::InvalidReply("MXW01 status"));
    }
    let error = (data[12] != 0).then(|| data[13]);
    Ok(match (error, data[6]) {
        (None, 0) => PrinterState::Ready,
        (None, 1) => PrinterState::Printing,
        (None, _) => PrinterState::Unknown,
        (Some(error), _) => PrinterState::Conditions(vec![match error {
            1 | 9 => PrinterCondition::OutOfPaper,
            4 => PrinterCondition::Overheated,
            8 => PrinterCondition::LowBattery,
            _ => PrinterCondition::Unknown,
        }]),
    })
}
