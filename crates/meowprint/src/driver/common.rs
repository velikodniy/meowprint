//! Commands, controls, queries, and submission for the common wire family.
use super::framing::{Decoder, EncodedCommand, Frame, encode, finish_into};
use super::{BleProfile, Flow, Movement, Observation, Operations, RasterEncoding, Settings};
use crate::session::{Operation, REPLY_TIMEOUT, ReplyKind};
use crate::stream::RowReader;
use crate::{
    Completion, DeviceInfo, Driver, Error, PixelFormat, PrinterCondition, PrinterState, Result,
    RowSource, Transport, WriteChannel, encoding, error::invalid,
};
use flate2::{Compress, Compression};
use lzokay::compress::Dict;
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
            endpoints: BleProfile {
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
    fn commands<'a>(
        self,
        commands: impl IntoIterator<Item = Command<'a>>,
    ) -> Result<Vec<EncodedCommand>> {
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
                    .map(Command::Feed),
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
            Ok(Movement::Commands(self.commands([Command::Retract(rows)])?))
        } else {
            self.feed(rows, false)
        }
    }
    fn stream(self, format: PixelFormat, options: &Controls) -> Result<Stream> {
        let encoding = self.validate(options, format)?;
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
            if matches!(self.quality, Quality::Concentration) && format == PixelFormat::Gray4 {
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
        let mut end = Vec::new();
        if self.end_speed {
            end.push(Command::Speed(options.feed_speed));
        }
        if self.lattice {
            end.push(Command::LatticeEnd);
        }
        end.extend(std::iter::repeat_n(
            Command::Query(Query::State),
            self.end_queries,
        ));
        let row_bytes = 384 / if format == PixelFormat::Mono { 8 } else { 2 };
        let mut encoder = RowEncoder::new(format, row_bytes, encoding, self.max_payload)?;
        let blank = encoder.encode(1)?.to_vec();
        Ok(Stream {
            start: self.commands(start)?,
            before_feed: self.commands([Command::Speed(options.feed_speed)])?,
            feed: self.feed(options.feed, matches!(self.feed, Feed::Split))?,
            end: self.commands(end)?,
            blank,
            encoder,
        })
    }
}
pub struct Stream {
    start: Vec<EncodedCommand>,
    before_feed: Vec<EncodedCommand>,
    feed: Movement,
    end: Vec<EncodedCommand>,
    blank: Vec<u8>,
    encoder: RowEncoder,
}
async fn submit<T: Transport, D: Driver, S: RowSource>(
    operation: &mut Operation<'_, T, D>,
    stream: &mut Stream,
    rows: &mut RowReader<'_, S>,
    submitted: &mut usize,
) -> Result<Completion> {
    operation.wait_ready().await?;
    let count = rows.fill(operation, stream.encoder.input_mut()).await?;
    let frame = stream.encoder.encode(count)?;
    operation.send_commands(&stream.start, submitted).await?;
    operation
        .send(WriteChannel::Control, &stream.blank, submitted)
        .await?;
    operation
        .send(WriteChannel::Control, frame, submitted)
        .await?;
    loop {
        let count = rows.fill(operation, stream.encoder.input_mut()).await?;
        if count == 0 {
            break;
        }
        let frame = stream.encoder.encode(count)?;
        operation
            .send(WriteChannel::Control, frame, submitted)
            .await?;
    }
    operation
        .send(WriteChannel::Control, &stream.blank, submitted)
        .await?;
    operation
        .send_commands(&stream.before_feed, submitted)
        .await?;
    operation.send_movement(&stream.feed, submitted).await?;
    let marker = operation.send_commands(&stream.end, submitted).await?;
    operation.bounded_drain().await?;
    Ok(if operation.ready_after(marker) {
        Completion::ReadyAfterEnd
    } else {
        Completion::TimedDrain
    })
}
fn status(frame: &Frame) -> Result<PrinterState> {
    Ok(match frame.payload.as_slice() {
        [] => return Err(Error::InvalidReply("printer status")),
        &[flags] => common_state(flags),
        _ => PrinterState::Unknown,
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

enum Codec {
    Raw,
    Rle,
    Lzo(Box<Dict>),
    Zlib(Compress),
}

struct RowEncoder {
    format: PixelFormat,
    row_bytes: usize,
    max_payload: usize,
    input: Vec<u8>,
    frame: Vec<u8>,
    codec: Codec,
}

impl RowEncoder {
    fn new(
        format: PixelFormat,
        row_bytes: usize,
        encoding: RasterEncoding,
        max_payload: usize,
    ) -> Result<Self> {
        let band_rows = if encoding == RasterEncoding::LzoBands {
            20
        } else {
            1
        };
        let compressed = matches!(
            encoding,
            RasterEncoding::Lzo | RasterEncoding::LzoBands | RasterEncoding::ZlibExperimental
        );
        let payload_capacity = if compressed { max_payload } else { row_bytes };
        if payload_capacity > max_payload || (compressed && max_payload < 4) {
            return Err(invalid(
                "The frame exceeds the selected driver payload limit.",
            ));
        }
        let codec = match encoding {
            RasterEncoding::Raw => Codec::Raw,
            RasterEncoding::Rle => Codec::Rle,
            RasterEncoding::Lzo | RasterEncoding::LzoBands => Codec::Lzo(Dict::new()),
            RasterEncoding::ZlibExperimental => {
                Codec::Zlib(Compress::new(Compression::default(), true))
            }
        };
        Ok(Self {
            format,
            row_bytes,
            max_payload,
            input: if matches!(codec, Codec::Raw) {
                Vec::new()
            } else {
                vec![0; row_bytes * band_rows]
            },
            frame: vec![0; payload_capacity + 8],
            codec,
        })
    }

    fn input_mut(&mut self) -> &mut [u8] {
        if matches!(self.codec, Codec::Raw) {
            &mut self.frame[6..6 + self.row_bytes]
        } else {
            &mut self.input
        }
    }

    fn encode(&mut self, rows: usize) -> Result<&[u8]> {
        let raw_len = self.row_bytes * rows;
        let mut rle = false;
        let mut payload_len = match &mut self.codec {
            Codec::Raw => raw_len,
            Codec::Rle => {
                let raw = &self.input[..raw_len];
                if let Some(count) = encoding::rle_into(raw, &mut self.frame[6..6 + raw_len]) {
                    rle = true;
                    count
                } else {
                    self.frame[6..6 + raw_len].copy_from_slice(raw);
                    raw_len
                }
            }
            Codec::Lzo(dictionary) => encoding::lzo_into(
                &self.input[..raw_len],
                &mut self.frame[10..6 + self.max_payload],
                dictionary,
            )?,
            Codec::Zlib(compressor) => encoding::zlib_into(
                &self.input[..raw_len],
                &mut self.frame[10..6 + self.max_payload],
                compressor,
            )?,
        };
        if matches!(self.codec, Codec::Lzo(_) | Codec::Zlib(_)) {
            self.frame[6..8].copy_from_slice(
                &u16::try_from(raw_len)
                    .map_err(|_| invalid("The uncompressed block exceeds 65535 bytes."))?
                    .to_le_bytes(),
            );
            self.frame[8..10].copy_from_slice(
                &u16::try_from(payload_len)
                    .map_err(|_| invalid("The compressed block exceeds 65535 bytes."))?
                    .to_le_bytes(),
            );
            payload_len += 4;
        }
        let payload = &self.frame[6..6 + payload_len];
        let command = match &self.codec {
            Codec::Rle if rle => Command::RleRow(payload),
            Codec::Raw | Codec::Rle => Command::RawRow(payload),
            Codec::Lzo(_) | Codec::Zlib(_) => match self.format {
                PixelFormat::Mono => Command::CompressedMono(payload),
                PixelFormat::Gray4 => Command::CompressedGray(payload),
            },
        }
        .code();
        let length = finish_into(
            HEADER,
            command,
            0,
            payload_len,
            self.max_payload,
            &mut self.frame,
        )?;
        Ok(&self.frame[..length])
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
            fn supports(self, format: PixelFormat) -> bool {
                match format {
                    PixelFormat::Mono => true,
                    PixelFormat::Gray4 => ($spec).gray_encoding.is_some(),
                }
            }
            fn validate(self, options: &$options, format: PixelFormat) -> Result<()> {
                ($spec).validate(&options.controls(), format).map(|_| ())
            }
        }
        impl Operations for $name {
            type Stream = Stream;
            fn settings(&self) -> Settings { ($spec).settings() }
            fn decoder(&self) -> Decoder { Decoder::new(HEADER, ($spec).max_payload, true) }
            // Common-family framing does not declare a job height; MXW01 requires it.
            fn stream(&self, _height: u32, format: PixelFormat, options: &$options) -> Result<Stream> { ($spec).stream(format, &options.controls()) }
            async fn submit<T: Transport, S: RowSource>(operation: &mut Operation<'_, T, Self>, stream: &mut Stream, rows: &mut RowReader<'_, S>, submitted: &mut usize) -> Result<Completion> { submit(operation, stream, rows, submitted).await }
            async fn status<T: Transport>(operation: &mut Operation<'_, T, Self>) -> Result<PrinterState> {
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

/// Queries supported by the common family. Reply layouts vary by firmware.
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

/// An outgoing common-family command with its control value or raster payload.
///
/// Encoding checks payload limits and control ranges, not raster contents,
/// job order, or driver compatibility. Streaming uses [`Self::code`] to frame
/// an already encoded payload in place.
#[derive(Debug, Clone, Copy)]
enum Command<'a> {
    /// Probable retract distance in rows. Direction and units remain observed.
    Retract(u16),
    /// Feed distance in dot rows.
    Feed(u16),
    /// One packed monochrome row, leftmost pixel in bit zero.
    RawRow(&'a [u8]),
    /// One RLE monochrome row, with color in bit seven and run length below it.
    RleRow(&'a [u8]),
    /// Compressed monochrome bytes with little-endian raw and compressed lengths.
    CompressedMono(&'a [u8]),
    /// Compressed grayscale bytes with little-endian raw and compressed lengths.
    CompressedGray(&'a [u8]),
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
    /// Extended density from zero through 200, prefixed by `01`.
    Density(u8),
}
impl Command<'_> {
    /// Return the wire command byte without encoding or validating the payload.
    const fn code(self) -> u8 {
        match self {
            Self::Retract(_) => 0xa0,
            Self::Feed(_) => 0xa1,
            Self::RawRow(_) => 0xa2,
            Self::Query(query) => query.command(),
            Self::Quality(_) | Self::Concentration(_) => 0xa4,
            Self::LatticeStart | Self::LatticeEnd => 0xa6,
            Self::Energy(_) => 0xaf,
            Self::Speed(_) => 0xbd,
            Self::Mode(_) | Self::GrayscaleMode(_) => 0xbe,
            Self::RleRow(_) => 0xbf,
            Self::CompressedMono(_) => 0xce,
            Self::CompressedGray(_) => 0xcf,
            Self::Density(_) => 0xf2,
        }
    }

    /// Encode a complete frame within the driver's payload limit.
    ///
    /// # Errors
    /// Reject invalid control values or an oversized payload.
    fn encode(self, max_payload: usize) -> Result<Vec<u8>> {
        Ok(self.prepare(max_payload)?.bytes)
    }

    fn prepare(self, max_payload: usize) -> Result<EncodedCommand> {
        let scalar;
        let pair;
        let command = self.code();
        let payload: &[u8] = match self {
            Self::Retract(value) | Self::Feed(value) | Self::Energy(value) => {
                pair = value.to_le_bytes();
                &pair
            }
            Self::Query(_) => {
                scalar = [0];
                &scalar
            }
            Self::Quality(value) => {
                validate_quality(value)?;
                scalar = [0x30 + value];
                &scalar
            }
            Self::Concentration(value) => {
                validate_concentration(value)?;
                scalar = [value];
                &scalar
            }
            Self::Speed(value) => {
                validate_speed(value)?;
                scalar = [value];
                &scalar
            }
            Self::Density(value) => {
                if value > 200 {
                    return Err(invalid("Density cannot exceed 200."));
                }
                pair = [1, value];
                &pair
            }
            Self::Mode(mode) => {
                scalar = [mode as u8];
                &scalar
            }
            Self::GrayscaleMode(mode) => {
                pair = [mode as u8, 1];
                &pair
            }
            Self::LatticeStart => &LATTICE_START,
            Self::LatticeEnd => &LATTICE_END,
            Self::RawRow(payload)
            | Self::RleRow(payload)
            | Self::CompressedMono(payload)
            | Self::CompressedGray(payload) => payload,
        };
        let bytes = encode(HEADER, command, 0, payload, max_payload)?;
        Ok(EncodedCommand { command, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn high_entropy_bytes(length: usize) -> Vec<u8> {
        let mut state = 0x937a_4d81_u32;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    #[test]
    fn lzo_rows_and_bands_round_trip_all_requested_heights() -> Result<()> {
        for (format, encoding, band) in [
            (PixelFormat::Mono, RasterEncoding::Lzo, 1),
            (PixelFormat::Gray4, RasterEncoding::Lzo, 1),
            (PixelFormat::Gray4, RasterEncoding::LzoBands, 20),
        ] {
            let row_bytes = if format == PixelFormat::Mono { 48 } else { 192 };
            let max_payload = if encoding == RasterEncoding::LzoBands {
                4096
            } else {
                255
            };
            let mut encoder = RowEncoder::new(format, row_bytes, encoding, max_payload)?;
            for height in [1, 19, 20, 21, 39, 40, 41] {
                let expected = high_entropy_bytes(row_bytes * height);
                let mut decoded = Vec::new();
                let mut offset = 0;
                while offset < height {
                    let count = band.min(height - offset);
                    let input_len = count * row_bytes;
                    encoder.input_mut()[..input_len]
                        .copy_from_slice(&expected[offset * row_bytes..][..input_len]);
                    let frame = encoder.encode(count)?.to_vec();
                    let frame = Decoder::new(HEADER, max_payload, true)
                        .feed_positioned(&frame)
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
                    let raw_len = u16::from_le_bytes([frame.payload[0], frame.payload[1]]) as usize;
                    let compressed_len =
                        u16::from_le_bytes([frame.payload[2], frame.payload[3]]) as usize;
                    assert_eq!(raw_len, input_len);
                    assert_eq!(compressed_len, frame.payload.len() - 4);
                    let mut output = vec![0; raw_len];
                    assert_eq!(
                        lzokay::decompress::decompress(&frame.payload[4..], &mut output)
                            .map_err(|error| Error::Compression(error.to_string()))?,
                        raw_len
                    );
                    decoded.extend(output);
                    offset += count;
                }
                assert_eq!(decoded, expected);
            }
        }
        Ok(())
    }

    #[test]
    fn rle_equal_size_stays_rle_and_expanding_data_falls_back_to_raw() -> Result<()> {
        let mut encoder = RowEncoder::new(PixelFormat::Mono, 48, RasterEncoding::Rle, 255)?;
        let equal: Vec<u8> = (0..48).map(|i| if i % 2 == 0 { 0xff } else { 0 }).collect();
        encoder.input_mut().copy_from_slice(&equal);
        let frame = encoder.encode(1)?.to_vec();
        let frame = Decoder::new(HEADER, 255, true)
            .feed_positioned(&frame)
            .remove(0)
            .1;
        assert_eq!(frame.command, 0xbf);
        assert_eq!(frame.payload.len(), equal.len());
        let raw: Vec<u8> = (0..48)
            .map(|i| if i % 2 == 0 { 0x55 } else { 0xaa })
            .collect();
        encoder.input_mut().copy_from_slice(&raw);
        let frame = encoder.encode(1)?.to_vec();
        let frame = Decoder::new(HEADER, 255, true)
            .feed_positioned(&frame)
            .remove(0)
            .1;
        assert_eq!(frame.command, 0xa2);
        assert_eq!(frame.payload, raw);
        Ok(())
    }

    #[test]
    fn zlib_rows_round_trip_with_reused_compressor() -> Result<()> {
        for format in [PixelFormat::Mono, PixelFormat::Gray4] {
            let row_bytes = if format == PixelFormat::Mono { 48 } else { 192 };
            let mut encoder =
                RowEncoder::new(format, row_bytes, RasterEncoding::ZlibExperimental, 4096)?;
            for height in [1, 19, 20, 21, 39, 40, 41] {
                let expected = high_entropy_bytes(row_bytes * height);
                let mut decoded = Vec::new();
                for (row_index, expected_row) in expected.chunks_exact(row_bytes).enumerate() {
                    encoder.input_mut()[..row_bytes].copy_from_slice(expected_row);
                    let bytes = encoder.encode(1)?.to_vec();
                    let frame = Decoder::new(HEADER, 4096, true)
                        .feed_positioned(&bytes)
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
                        u16::from_le_bytes([frame.payload[0], frame.payload[1]]) as usize,
                        row_bytes
                    );
                    let compressed =
                        u16::from_le_bytes([frame.payload[2], frame.payload[3]]) as usize;
                    assert_eq!(compressed, frame.payload.len() - 4);
                    let mut output = Vec::new();
                    flate2::read::ZlibDecoder::new(&frame.payload[4..])
                        .read_to_end(&mut output)
                        .map_err(|error| Error::Compression(error.to_string()))?;
                    assert_eq!(output, expected_row, "row {row_index}");
                    decoded.extend(output);
                }
                assert_eq!(decoded, expected);
            }
        }
        Ok(())
    }
}
