//! Commands, controls, queries, and submission for the common wire family.
#[cfg(feature = "bluetooth")]
use super::Endpoints;
use super::framing::{Decoder, EncodedCommand, Frame, encode};
use super::{Flow, Movement, Observation, Operations, RasterEncoding, Settings};
use crate::session::{Operation, REPLY_TIMEOUT, ReplyKind};
use crate::{
    Completion, DeviceInfo, Driver, Error, PixelFormat, PrinterCondition, PrinterState,
    PrinterStatus, Raster, Result, Transport, WriteChannel, encoding, error::invalid,
};
use std::time::Duration;

/// Monochrome compression choices for [`Gt01`], [`TinyRle`], and [`PrefixedTiny`].
///
/// Leave the driver's `encoding` as `None` to use its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonoEncoding {
    /// Send uncompressed rows.
    Raw,
    /// Compress repeated pixels with run-length encoding (RLE).
    ///
    /// If compression makes a row larger, send that row without compression.
    Rle,
}
impl From<MonoEncoding> for RasterEncoding {
    fn from(value: MonoEncoding) -> Self {
        match value {
            MonoEncoding::Raw => Self::Raw,
            MonoEncoding::Rle => Self::Rle,
        }
    }
}
/// Compression choices for [`X6h`].
///
/// Leave [`X6hOptions::encoding`] as `None` to use LZO for either pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X6hEncoding {
    /// Uncompressed monochrome rows.
    Raw,
    /// Run-length encoded monochrome rows.
    Rle,
    /// LZO compression for monochrome or four-bit grayscale.
    Lzo,
}
impl From<X6hEncoding> for RasterEncoding {
    fn from(value: X6hEncoding) -> Self {
        match value {
            X6hEncoding::Raw => Self::Raw,
            X6hEncoding::Rle => Self::Rle,
            X6hEncoding::Lzo => Self::Lzo,
        }
    }
}
/// Compression choices for [`V5g`].
///
/// Leave [`V5gOptions::encoding`] as `None` to select the default for the pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V5gEncoding {
    /// Uncompressed monochrome rows.
    Raw,
    /// LZO compression for groups of four-bit grayscale rows.
    LzoBands,
    /// Experimental zlib compression for either pixel format. Untested on hardware.
    ZlibExperimental,
}
impl From<V5gEncoding> for RasterEncoding {
    fn from(value: V5gEncoding) -> Self {
        match value {
            V5gEncoding::Raw => Self::Raw,
            V5gEncoding::LzoBands => Self::LzoBands,
            V5gEncoding::ZlibExperimental => Self::ZlibExperimental,
        }
    }
}

// Only this driver uses the resolved common-family controls.
struct Controls {
    energy: u16,
    speed: u8,
    feed: u16,
    feed_speed: u8,
    quality: u8,
    density: Option<u8>,
    mode: Mode,
    encoding: Option<RasterEncoding>,
}
#[derive(Clone, Copy)]
enum Feed {
    Split,
    Single,
    BlankRows,
}
#[derive(Clone, Copy)]
enum Quality {
    Quality,
    Concentration,
}
#[derive(Clone, Copy)]
struct Spec {
    name: &'static str,
    chunk_size: usize,
    pacing_ms: u64,
    max_payload: usize,
    default_feed: u16,
    feed: Feed,
    quality: Quality,
    density_first: bool,
    lattice: bool,
    end_speed: bool,
    end_queries: usize,
    speed: u8,
    mono_encoding: RasterEncoding,
    gray_encoding: Option<RasterEncoding>,
    prefix: Option<(u8, &'static [u8])>,
}
impl Spec {
    const fn encoding(self, format: PixelFormat) -> RasterEncoding {
        if matches!(format, PixelFormat::Gray4)
            && let Some(encoding) = self.gray_encoding
        {
            return encoding;
        }
        self.mono_encoding
    }
    const fn settings(self) -> Settings {
        Settings {
            width: 384,
            chunk_size: self.chunk_size,
            pacing: Duration::from_millis(self.pacing_ms),
            finish_control_frame: false,
            prefix: self.prefix,
            #[cfg(feature = "bluetooth")]
            endpoints: Endpoints {
                services: &[0xae30, 0xaf30],
                control: 0xae01,
                raster: 0xae01,
                notify: 0xae02,
            },
        }
    }
    fn validate(self, options: &Controls, format: PixelFormat) -> Result<RasterEncoding> {
        if format == PixelFormat::Gray4 && self.gray_encoding.is_none() {
            return Err(invalid("This driver does not support four-bit grayscale."));
        }
        for speed in [options.speed, options.feed_speed] {
            validate_speed(speed)?;
        }
        match self.quality {
            Quality::Quality => validate_quality(options.quality)?,
            Quality::Concentration => validate_concentration(options.quality)?,
        }
        if let Some(density) = options.density {
            let minimum = u8::from(self.density_first);
            if !(minimum..=200).contains(&density) {
                return Err(invalid(format!(
                    "Density must be from {minimum} through 200."
                )));
            }
        }
        let encoding = options.encoding.unwrap_or_else(|| self.encoding(format));
        if (matches!(encoding, RasterEncoding::Raw | RasterEncoding::Rle)
            && format != PixelFormat::Mono)
            || (encoding == RasterEncoding::LzoBands && format != PixelFormat::Gray4)
        {
            return Err(invalid("The encoding does not fit the pixel format."));
        }
        Ok(encoding)
    }
    fn commands(self, commands: Vec<Command<'_>>) -> Result<Vec<EncodedCommand>> {
        commands
            .into_iter()
            .map(|command| command.prepare(self.max_payload))
            .collect()
    }
    fn feed(self, rows: u16, split: bool) -> Result<Movement> {
        if matches!(self.feed, Feed::BlankRows) {
            return Ok(Movement::BlankRows {
                frame: Command::RawRow(&[0; 48]).encode(self.max_payload)?,
                count: rows,
            });
        }
        let distances = if split {
            [rows / 2, rows - rows / 2]
        } else {
            [rows, 0]
        };
        Ok(Movement::Commands(
            self.commands(
                distances
                    .into_iter()
                    .filter(|&rows| rows != 0)
                    .map(Command::Feed)
                    .collect(),
            )?,
        ))
    }
    fn movement(self, rows: u16, retract: bool) -> Result<Movement> {
        if rows == 0 || (retract && matches!(self.feed, Feed::BlankRows)) {
            return Err(invalid(
                "The paper movement is not supported by this driver, or its row count is zero.",
            ));
        }
        if retract {
            Ok(Movement::Commands(
                self.commands(vec![Command::Retract(rows)])?,
            ))
        } else {
            self.feed(rows, false)
        }
    }
    fn prepare(self, raster: &Raster, options: &Controls) -> Result<Job> {
        let encoding = self.validate(options, raster.format())?;
        let mut start = Vec::new();
        if self.density_first
            && let Some(density) = options.density
        {
            start.push(Command::Density(density));
        }
        start.push(Command::Query(Query::State));
        start.push(if matches!(self.quality, Quality::Concentration) {
            Command::Concentration(options.quality)
        } else {
            Command::Quality(options.quality)
        });
        if self.lattice {
            start.push(Command::LatticeStart);
        }
        start.push(Command::Energy(options.energy));
        start.push(
            if matches!(self.quality, Quality::Concentration)
                && raster.format() == PixelFormat::Gray4
            {
                Command::GrayscaleMode(options.mode)
            } else {
                Command::Mode(options.mode)
            },
        );
        start.push(Command::Speed(options.speed));
        if !self.density_first
            && let Some(density) = options.density
        {
            start.push(Command::Density(density));
        }
        let feed_speed = options.feed_speed;
        let mut end = Vec::new();
        if self.end_speed {
            end.push(Command::Speed(feed_speed));
        }
        if self.lattice {
            end.push(Command::LatticeEnd);
        }
        end.extend(std::iter::repeat_n(
            Command::Query(Query::State),
            self.end_queries,
        ));
        let band_rows = if encoding == RasterEncoding::LzoBands {
            20
        } else {
            1
        };
        Ok(Job {
            start: self.commands(start)?,
            before_feed: self.commands(vec![Command::Speed(feed_speed)])?,
            feed: self.feed(options.feed, matches!(self.feed, Feed::Split))?,
            end: self.commands(end)?,
            frames: raster
                .packed()
                .chunks(raster.row_bytes() * band_rows)
                .map(|raw| row_frame(raw, raster.format(), encoding, self.max_payload))
                .collect::<Result<_>>()?,
            blank: row_frame(
                &vec![0; raster.row_bytes()],
                raster.format(),
                encoding,
                self.max_payload,
            )?,
        })
    }
}
pub struct Job {
    start: Vec<EncodedCommand>,
    before_feed: Vec<EncodedCommand>,
    feed: Movement,
    end: Vec<EncodedCommand>,
    frames: Vec<Vec<u8>>,
    blank: Vec<u8>,
}
async fn submit<T: Transport, D: Driver>(
    operation: &mut Operation<'_, T, D>,
    job: &Job,
    submitted: &mut usize,
) -> Result<Completion> {
    operation.wait_ready().await?;
    operation.send_commands(&job.start, submitted).await?;
    operation
        .send(WriteChannel::Control, &job.blank, submitted)
        .await?;
    for frame in &job.frames {
        operation
            .send(WriteChannel::Control, frame, submitted)
            .await?;
    }
    operation
        .send(WriteChannel::Control, &job.blank, submitted)
        .await?;
    operation.send_commands(&job.before_feed, submitted).await?;
    operation.send_movement(&job.feed, submitted).await?;
    let marker = operation.send_commands(&job.end, submitted).await?;
    operation.bounded_drain().await?;
    Ok(if operation.ready_after(marker) {
        Completion::ReadyAfterEnd
    } else {
        Completion::TimedDrain
    })
}
fn status(frame: &Frame) -> Result<PrinterStatus> {
    let state = match frame.payload.as_slice() {
        [] => return Err(Error::InvalidReply("printer status")),
        &[flags] => common_state(flags),
        _ => PrinterState::Unknown,
    };
    Ok(PrinterStatus {
        state,
        battery_percent: None,
        temperature_celsius: None,
    })
}
fn device_info(frame: &Frame) -> Result<DeviceInfo> {
    if frame.payload.is_empty() {
        return Err(Error::InvalidReply("device information"));
    }
    Ok(DeviceInfo {
        description: super::readable_text(&frame.payload),
        firmware_version: None,
    })
}
fn observe(frame: &Frame) -> Result<Observation> {
    if frame.direction != 1 {
        return Ok(Observation::default());
    }
    let status = if matches!(frame.command, 0xa3 | 0xae) {
        Some(status(frame)?)
    } else {
        None
    };
    let flow = match (frame.command, frame.payload.as_slice()) {
        (0xa3 | 0xae, [flags]) if flags & 0x90 != 0 => Some(Flow::Pause),
        (0xae, [0]) => Some(Flow::Resume),
        _ => None,
    };
    Ok(Observation {
        status,
        flow,
        reply: true,
    })
}
fn common_state(flags: u8) -> PrinterState {
    if flags == 0 {
        return PrinterState::Ready;
    }
    let mut conditions: Vec<_> = [
        (0x01, PrinterCondition::OutOfPaper),
        (0x02, PrinterCondition::CoverOpen),
        (0x04, PrinterCondition::Overheated),
        (0x08, PrinterCondition::LowBattery),
        (0x10, PrinterCondition::Paused),
        (0x80, PrinterCondition::Busy),
    ]
    .into_iter()
    .filter_map(|(mask, condition)| (flags & mask != 0).then_some(condition))
    .collect();
    if flags & 0x60 != 0 {
        conditions.push(PrinterCondition::Unknown);
    }
    PrinterState::Conditions(conditions)
}

fn row_frame(
    raw: &[u8],
    format: PixelFormat,
    encoding: RasterEncoding,
    limit: usize,
) -> Result<Vec<u8>> {
    match encoding {
        RasterEncoding::Raw => Command::RawRow(raw).encode(limit),
        RasterEncoding::Rle => {
            let packed = encoding::rle(raw);
            if packed.len() > raw.len() {
                Command::RawRow(raw).encode(limit)
            } else {
                Command::RleRow(&packed).encode(limit)
            }
        }
        RasterEncoding::Lzo | RasterEncoding::LzoBands | RasterEncoding::ZlibExperimental => {
            let payload = if encoding == RasterEncoding::ZlibExperimental {
                encoding::zlib_payload(raw)?
            } else {
                encoding::lzo_payload(raw)?
            };
            match format {
                PixelFormat::Mono => Command::CompressedMono(&payload),
                PixelFormat::Gray4 => Command::CompressedGray(&payload),
            }
            .encode(limit)
        }
    }
}

macro_rules! driver {
    ($name:ident, $options:ident, $spec:expr, $quality:ident, $(#[$field_doc:meta])* $quality_type:ty;
        $( $(#[$doc:meta])* $field:ident: $ty:ty ),* $(,)?) => {
        #[doc = concat!("The `", stringify!($name), "` firmware driver.")]
        ///
        /// See the [driver reference](crate::driver) for supported formats and defaults.
        #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
        pub struct $name;
        #[doc = concat!("Print controls accepted by [`", stringify!($name), "`].")]
        ///
        /// Use `..Default::default()` to keep defaults for fields you do not set.
        /// See the [driver reference](crate::driver) for default speed and final feed.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $options {
            /// Thermal energy, from 0 through 65535. Default 12000.
            ///
            /// Values use the printer's own scale. The accepted range is not a safe range for every printer.
            pub energy: u16,
            /// Print speed divisor, from 4 through 255. Smaller values are faster.
            pub speed: u8,
            /// Final paper feed in dot rows. Zero suppresses the final feed.
            pub feed: u16,
            /// Final feed speed divisor, from 4 through 255. Smaller values are faster. Default 25.
            pub feed_speed: u8,
            $(#[$field_doc])*
            pub $quality: $quality_type,
            /// Firmware print mode. Defaults to [`PrintMode::Image`](crate::PrintMode::Image).
            pub mode: Mode,
            $( $(#[$doc])* pub $field: $ty, )*
        }
        impl Default for $options {
            fn default() -> Self {
                Self {
                    energy: 12000,
                    speed: ($spec).speed,
                    feed: ($spec).default_feed,
                    feed_speed: 25,
                    $quality: 3,
                    mode: Mode::default(),
                    $($field: None,)*
                }
            }
        }
        impl Driver for $name {
            type Options = $options;
            fn name(self) -> &'static str { ($spec).name }
            fn supports_gray4(self) -> bool { ($spec).gray_encoding.is_some() }
            fn validate(self, options: &$options, format: PixelFormat) -> Result<()> {
                ($spec).validate(&options.controls(), format).map(|_| ())
            }
        }
        impl Operations for $name {
            type Job = Job;
            fn settings(&self) -> Settings { ($spec).settings() }
            fn decoder(&self) -> Decoder { Decoder::new(HEADER, ($spec).max_payload, true) }
            fn prepare(&self, raster: &Raster, options: &$options) -> Result<Job> { ($spec).prepare(raster, &options.controls()) }
            async fn submit<T: Transport>(operation: &mut Operation<'_, T, Self>, job: &Job, submitted: &mut usize) -> Result<Completion> {
                submit(operation, job, submitted).await
            }
            async fn status<T: Transport>(operation: &mut Operation<'_, T, Self>) -> Result<PrinterStatus> {
                let command = Command::Query(Query::State).prepare(($spec).max_payload)?;
                operation.status(&command).await
            }
            async fn device_info<T: Transport>(operation: &mut Operation<'_, T, Self>) -> Result<DeviceInfo> {
                let command = Command::Query(Query::DeviceInfo).prepare(($spec).max_payload)?;
                device_info(&operation.exchange(&command, ReplyKind::Command(command.command), REPLY_TIMEOUT, &mut 0).await?.frame)
            }
            fn observe(&self, frame: &Frame) -> Result<Observation> { observe(frame) }
            fn blocks_submission(&self, frame: &Frame, state: &PrinterState) -> bool {
                // A3 reply formats vary by firmware. Only recognized A3 faults block
                // submission; unknown A3 replies provide no evidence of readiness.
                state.blocks_submission()
                    && !(frame.command == 0xa3 && *state == PrinterState::Unknown)
            }
            fn movement(&self, rows: u16, retract: bool) -> Result<Movement> { ($spec).movement(rows, retract) }
        }
    };
}
const BASE: Spec = Spec {
    name: "gt01",
    chunk_size: 120,
    pacing_ms: 25,
    max_payload: 255,
    default_feed: 96,
    feed: Feed::Split,
    quality: Quality::Quality,
    density_first: false,
    lattice: true,
    end_speed: true,
    end_queries: 1,
    speed: 30,
    mono_encoding: RasterEncoding::Raw,
    gray_encoding: None,
    prefix: None,
};

driver!(Gt01, Gt01Options, BASE, quality,
    /// Print quality from 1 through 5. Default 3.
    u8;
/// Pixel compression. `None` selects the driver default.
    encoding: Option<MonoEncoding>
);
impl Gt01Options {
    fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.quality,
            density: None,
            mode: self.mode,
            encoding: self.encoding.map(Into::into),
        }
    }
}

driver!(Mx10, Mx10Options, Spec {
    name: "mx10",
    chunk_size: 63,
    pacing_ms: 6,
    default_feed: 128,
    feed: Feed::BlankRows,
    ..BASE
}, quality,
    /// Print quality from 1 through 5. Default 3.
    u8;

);
impl Mx10Options {
    const fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.quality,
            density: None,
            mode: self.mode,
            encoding: None,
        }
    }
}

driver!(CommonRaw, CommonRawOptions, Spec {
    name: "common-raw",
    chunk_size: 63,
    pacing_ms: 6,
    ..BASE
}, quality,
    /// Print quality from 1 through 5. Default 3.
    u8;

);
impl CommonRawOptions {
    const fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.quality,
            density: None,
            mode: self.mode,
            encoding: None,
        }
    }
}

driver!(TinyRle, TinyRleOptions, Spec {
    name: "tiny-rle",
    chunk_size: 83,
    pacing_ms: 6,
    mono_encoding: RasterEncoding::Rle,
    ..BASE
}, quality,
    /// Print quality from 1 through 5. Default 3.
    u8;
/// Pixel compression. `None` selects the driver default.
    encoding: Option<MonoEncoding>
);
impl TinyRleOptions {
    fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.quality,
            density: None,
            mode: self.mode,
            encoding: self.encoding.map(Into::into),
        }
    }
}

driver!(PrefixedTiny, PrefixedTinyOptions, Spec {
    name: "prefixed-tiny",
    chunk_size: 83,
    pacing_ms: 6,
    mono_encoding: RasterEncoding::Rle,
    prefix: Some((0xa3,
    &[0x12])),
    ..BASE
}, quality,
    /// Print quality from 1 through 5. Default 3.
    u8;
/// Pixel compression. `None` selects the driver default.
    encoding: Option<MonoEncoding>
);
impl PrefixedTinyOptions {
    fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.quality,
            density: None,
            mode: self.mode,
            encoding: self.encoding.map(Into::into),
        }
    }
}

driver!(X6h, X6hOptions, Spec {
    name: "x6h",
    pacing_ms: 20,
    feed: Feed::Single,
    quality: Quality::Concentration,
    lattice: false,
    mono_encoding: RasterEncoding::Lzo,
    gray_encoding: Some(RasterEncoding::Lzo),
    ..BASE
}, concentration,
    /// Print darkness: 1 is darkest, 3 is normal, and 5 is lightest. Only these values are accepted. Default 3.
    u8;
/// Density from 0 through 200. `None` keeps the existing printer density.
    density: Option<u8>,
/// Pixel compression. `None` selects LZO for either pixel format.
    encoding: Option<X6hEncoding>
);
impl X6hOptions {
    fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.concentration,
            density: self.density,
            mode: self.mode,
            encoding: self.encoding.map(Into::into),
        }
    }
}

driver!(V5g, V5gOptions, Spec {
    name: "v5g",
    pacing_ms: 20,
    max_payload: 4096,
    default_feed: 48,
    feed: Feed::Single,
    density_first: true,
    end_speed: false,
    end_queries: 2,
    speed: 10,
    gray_encoding: Some(RasterEncoding::LzoBands),
    ..BASE
}, quality,
    /// Print quality from 1 through 5. Default 3.
    u8;
/// Density from 1 through 200. `None` keeps the existing printer density.
    density: Option<u8>,
/// Pixel compression. `None` selects raw monochrome or LZO grayscale.
    encoding: Option<V5gEncoding>
);
impl V5gOptions {
    fn controls(self) -> Controls {
        Controls {
            energy: self.energy,
            speed: self.speed,
            feed: self.feed,
            feed_speed: self.feed_speed,
            quality: self.quality,
            density: self.density,
            mode: self.mode,
            encoding: self.encoding.map(Into::into),
        }
    }
}

/// The payload of [`Command::LatticeStart`]. Its internal meaning is unknown.
const LATTICE_START: [u8; 11] = [
    0xaa, 0x55, 0x17, 0x38, 0x44, 0x5f, 0x5f, 0x5f, 0x44, 0x38, 0x2c,
];
/// The payload of [`Command::LatticeEnd`]. Its internal meaning is unknown.
const LATTICE_END: [u8; 11] = [0xaa, 0x55, 0x17, 0, 0, 0, 0, 0, 0, 0, 0x17];

const HEADER: [u8; 2] = [0x51, 0x78];

fn validate_quality(value: u8) -> Result<()> {
    if !(1..=5).contains(&value) {
        return Err(invalid("Quality must be from 1 through 5."));
    }
    Ok(())
}

fn validate_concentration(value: u8) -> Result<()> {
    if ![1, 3, 5].contains(&value) {
        return Err(invalid("Concentration must be 1, 3, or 5."));
    }
    Ok(())
}

fn validate_speed(value: u8) -> Result<()> {
    if value < 4 {
        return Err(invalid("Speed must be from 4 through 255."));
    }
    Ok(())
}

/// A firmware print mode, selected through the driver's `mode` field.
///
/// The application supplies image pixels in every mode. Text mode does not
/// render text, and label mode does not create labels. The effect on printing
/// depends on the firmware.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Image mode, the default.
    #[default]
    Image = 0,
    /// Text mode.
    Text = 1,
    /// Observed tattoo mode.
    Tattoo = 2,
    /// Observed label mode.
    Label = 3,
}

/// A query encoded by [`Command::Query`] for the common protocol family.
///
/// Applications use [`Printer::status`](crate::Printer::status) and [`Printer::device_info`](crate::Printer::device_info).
/// Reply layouts differ by firmware.
/// Both queries are available on all common-family drivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Query {
    /// State or start handshake (`A3 00`).
    State,
    /// Probable firmware information (`A8 00`).
    DeviceInfo,
}
impl Query {
    /// Return the command byte used to match a reply.
    #[must_use]
    const fn command(self) -> u8 {
        match self {
            Self::State => 0xa3,
            Self::DeviceInfo => 0xa8,
        }
    }
}

/// One outgoing command in the common protocol family.
///
/// [`Self::encode`] produces a complete frame but does not transmit it.
/// Commands cover both basic and extended firmware. The encoder enforces wire
/// limits and individual control ranges, not driver compatibility or job order.
/// Use [`Printer`](crate::Printer) for complete print jobs and device queries.
///
/// Slice variants borrow bytes only during encoding. Compressed variants expect
/// payloads from [`crate::encoding`], including their length prefixes.
#[derive(Debug, Clone, Copy)]
enum Command<'a> {
    /// Probable retract distance in rows. Direction and units remain observed.
    Retract(u16),
    /// Feed distance in dot rows.
    Feed(u16),
    /// One packed monochrome row, leftmost pixel in bit zero.
    RawRow(&'a [u8]),
    /// A documented query.
    Query(Query),
    /// Quality from 1 through 5, encoded as `31` through `35`.
    Quality(u8),
    /// Concentration 1, 3, or 5. One is darkest, five is lightest.
    Concentration(u8),
    /// Start a lattice, the pair of commands surrounding a common-driver job.
    LatticeStart,
    /// End the lattice with its fixed payload.
    LatticeEnd,
    /// Thermal energy in native unsigned 16-bit units, little-endian.
    Energy(u16),
    /// Motor divisor from 4 through 255. Smaller values are faster.
    Speed(u8),
    /// Common one-byte print mode.
    Mode(Mode),
    /// Extended print mode for sixteen grayscale levels.
    GrayscaleMode(Mode),
    /// Run lengths, with color in bit seven and count in bits zero through six.
    RleRow(&'a [u8]),
    /// `CE` payload from the compression encoder, including both length prefixes.
    CompressedMono(&'a [u8]),
    /// `CF` row or band payload, including both length prefixes.
    CompressedGray(&'a [u8]),
    /// Extended density from zero through 200, prefixed by `01`.
    Density(u8),
}
impl Command<'_> {
    /// Encode a command within a selected driver's payload ceiling.
    ///
    /// The driver supplies the firmware payload limit.
    /// The encoder does not test raster widths or compressed payload contents.
    ///
    /// # Errors
    /// Reject invalid control values, invalid RLE runs, or an oversized payload.
    fn encode(self, max_payload: usize) -> Result<Vec<u8>> {
        Ok(self.prepare(max_payload)?.bytes)
    }

    fn prepare(self, max_payload: usize) -> Result<EncodedCommand> {
        let scalar;
        let pair;
        let (command, payload): (u8, &[u8]) = match self {
            Self::Retract(value) | Self::Feed(value) | Self::Energy(value) => {
                pair = value.to_le_bytes();
                (
                    match self {
                        Self::Retract(_) => 0xa0,
                        Self::Feed(_) => 0xa1,
                        _ => 0xaf,
                    },
                    &pair,
                )
            }
            Self::Query(query) => {
                scalar = [0];
                (query.command(), &scalar)
            }
            Self::Quality(value) => {
                validate_quality(value)?;
                scalar = [0x30 + value];
                (0xa4, &scalar)
            }
            Self::Concentration(value) => {
                validate_concentration(value)?;
                scalar = [value];
                (0xa4, &scalar)
            }
            Self::Speed(value) => {
                validate_speed(value)?;
                scalar = [value];
                (0xbd, &scalar)
            }
            Self::Density(value) => {
                if value > 200 {
                    return Err(invalid("Density cannot exceed 200."));
                }
                pair = [1, value];
                (0xf2, &pair)
            }
            Self::Mode(mode) => {
                scalar = [mode as u8];
                (0xbe, &scalar)
            }
            Self::GrayscaleMode(mode) => {
                pair = [mode as u8, 1];
                (0xbe, &pair)
            }
            Self::LatticeStart => (0xa6, &LATTICE_START),
            Self::LatticeEnd => (0xa6, &LATTICE_END),
            Self::RawRow(row) => (0xa2, row),
            Self::RleRow(row) => {
                if row.is_empty() || row.iter().any(|byte| byte.trailing_zeros() >= 7) {
                    return Err(invalid("RLE runs must contain 1 through 127 pixels."));
                }
                (0xbf, row)
            }
            Self::CompressedMono(data) => (0xce, data),
            Self::CompressedGray(data) => (0xcf, data),
        };
        let bytes = encode(HEADER, command, 0, payload, max_payload)?;
        Ok(EncodedCommand { command, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn lzo_round_trip<D: Driver + Operations<Job = Job>>(
        driver: D,
        format: PixelFormat,
        height: usize,
    ) -> TestResult {
        let row_size = if format == PixelFormat::Mono { 48 } else { 192 };
        let mut state = 23_u32;
        let data: Vec<_> = (0..row_size * height)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                state.to_be_bytes()[0]
            })
            .collect();
        let raster = Raster::new(driver.printable_width(), data.clone(), format)?;
        let job = driver.prepare(&raster, &Default::default())?;
        let mut parser = Decoder::new(HEADER, 4096, true);
        let mut decoded = Vec::new();
        for bytes in &job.frames {
            let frame = parser.feed_positioned(bytes).remove(0).1;
            assert_eq!(
                frame.command,
                if format == PixelFormat::Mono {
                    0xce
                } else {
                    0xcf
                }
            );
            let raw_len = usize::from(u16::from_le_bytes([frame.payload[0], frame.payload[1]]));
            let compressed_len =
                usize::from(u16::from_le_bytes([frame.payload[2], frame.payload[3]]));
            assert_eq!(compressed_len, frame.payload.len() - 4);
            let mut output = vec![0; raw_len];
            assert_eq!(
                lzokay::decompress::decompress(&frame.payload[4..], &mut output)?,
                raw_len
            );
            decoded.extend(output);
        }
        assert_eq!(decoded, data);
        assert_eq!(job.frames.len(), 3);
        Ok(())
    }
    #[test]
    fn lzo_rows_and_bands_round_trip() -> TestResult {
        lzo_round_trip(X6h, PixelFormat::Mono, 3)?;
        lzo_round_trip(X6h, PixelFormat::Gray4, 3)?;
        lzo_round_trip(V5g, PixelFormat::Gray4, 41)
    }
    #[test]
    fn zlib_frames_round_trip() -> TestResult {
        for format in [PixelFormat::Mono, PixelFormat::Gray4] {
            let raw = vec![0xa5; if format == PixelFormat::Mono { 48 } else { 192 }];
            let raster = Raster::new(V5g.printable_width(), raw.clone(), format)?;
            let job = V5g.prepare(
                &raster,
                &V5gOptions {
                    encoding: Some(V5gEncoding::ZlibExperimental),
                    ..Default::default()
                },
            )?;
            let frame = Decoder::new(HEADER, 4096, true)
                .feed_positioned(&job.frames[0])
                .remove(0)
                .1;
            assert_eq!(
                frame.command,
                if format == PixelFormat::Mono {
                    0xce
                } else {
                    0xcf
                }
            );
            assert_eq!(
                usize::from(u16::from_le_bytes([frame.payload[0], frame.payload[1]])),
                raw.len()
            );
            assert_eq!(
                usize::from(u16::from_le_bytes([frame.payload[2], frame.payload[3]])),
                frame.payload.len() - 4
            );
            let mut output = Vec::new();
            flate2::read::ZlibDecoder::new(&frame.payload[4..]).read_to_end(&mut output)?;
            assert_eq!(output, raw);
        }
        Ok(())
    }
}
