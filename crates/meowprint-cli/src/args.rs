//! Command-line options and content selection.
use clap::{Args, Parser, Subcommand, ValueEnum};
use meowprint::{
    CommonRaw, CommonRawOptions, Driver, Gt01, Gt01Options, MonoEncoding, Mx10, Mx10Options, Mxw01,
    Mxw01Options, Mxw01ReplyFormat as ReplyFormat, PrefixedTiny, PrefixedTinyOptions,
    PrintMode as Mode, TinyRle, TinyRleOptions, V5g, V5gEncoding, V5gOptions, X6h, X6hEncoding,
    X6hOptions,
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Render and print on Bluetooth thermal printers")]
/// Command-line arguments.
pub struct Cli {
    #[command(subcommand)]
    /// Command to run.
    pub command: Command,
}
#[derive(Subcommand)]
/// Commands supported by the CLI.
pub enum Command {
    /// Find devices without sending printer commands.
    Scan {
        /// Include devices that do not match known printer names or services.
        #[arg(long)]
        all: bool,
        /// Scan duration in seconds.
        #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u64).range(1..=60))]
        seconds: u64,
    },
    /// Read discovered services without sending printer commands.
    Inspect {
        /// Exact device name or platform identifier.
        #[arg(long)]
        device: String,
    },
    /// List the available reference drivers.
    Drivers,
    /// Save exact rendered pixels without Bluetooth access.
    Preview {
        /// Output PNG path.
        #[arg(short, long)]
        output: PathBuf,
        /// Driver used to render the preview.
        #[arg(long, value_enum)]
        driver: DriverArg,
        #[command(subcommand)]
        content: Content,
    },
    /// Render content and print it using the selected driver.
    Print {
        #[command(flatten)]
        connection: Connection,
        #[command(flatten)]
        controls: Controls,
        /// Save a PNG preview before Bluetooth discovery.
        #[arg(long)]
        preview: Option<PathBuf>,
        #[command(subcommand)]
        content: Content,
    },
    /// Read printer status or device information.
    Query {
        #[command(flatten)]
        connection: Connection,
        #[arg(value_enum)]
        query: QueryArg,
    },
    /// Move paper forward by a number of dot rows.
    Feed {
        #[command(flatten)]
        connection: Connection,
        /// Distance in dot rows.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        rows: u16,
    },
    /// Send the observed A0 paper-retract command. Direction and units are unverified.
    Retract {
        #[command(flatten)]
        connection: Connection,
        /// Number of rows to request.
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
        rows: u16,
    },
    /// Attempt the observed MXW01 AC cancellation command, then disconnect.
    Cancel {
        #[command(flatten)]
        connection: Connection,
    },
}
#[derive(Args)]
/// Printer selection and connection options.
pub struct Connection {
    #[arg(long)]
    /// Exact device name or platform identifier.
    pub device: String,
    #[arg(long, value_enum)]
    /// Firmware driver used to communicate with the printer.
    pub driver: DriverArg,
    #[arg(long, value_enum)]
    /// MXW01 notification format. Defaults to with-crc.
    pub mxw01_reply_format: Option<ReplyFormatArg>,
}
impl Connection {
    /// Select the MXW01 notification format, or return an error if used with another driver.
    pub fn reply_format(&self) -> anyhow::Result<ReplyFormat> {
        if self.mxw01_reply_format.is_some() && !matches!(self.driver, DriverArg::Mxw01) {
            anyhow::bail!("The MXW01 reply format requires the mxw01 driver.");
        }
        Ok(match self.mxw01_reply_format {
            Some(ReplyFormatArg::WithoutCrc) => ReplyFormat::WithoutCrc,
            _ => ReplyFormat::WithCrc,
        })
    }
}
#[derive(Args, Clone, Default)]
/// Print controls. Defaults depend on the selected driver.
pub struct Controls {
    /// Common thermal energy in 16-bit units. Default: 12000.
    #[arg(long, conflicts_with = "intensity")]
    energy: Option<u16>,
    /// MXW01 print intensity in byte units. Default: 93.
    #[arg(long)]
    intensity: Option<u8>,
    /// Common motor speed divisor. Smaller values are faster. Default: 30 (10 for V5G).
    #[arg(long)]
    speed: Option<u8>,
    /// Total final paper movement in dot rows. Zero suppresses final feed.
    #[arg(long)]
    feed: Option<u16>,
    /// Final feed speed divisor. Default: 25.
    #[arg(long)]
    feed_speed: Option<u8>,
    /// Common print quality, from 1 to 5. Default: 3.
    #[arg(long, conflicts_with = "concentration")]
    quality: Option<u8>,
    /// X6h print concentration: 1 is darkest, 3 is normal, 5 is lightest.
    #[arg(long)]
    concentration: Option<u8>,
    /// Density range: 0–200 for X6h, 1–200 for V5G.
    #[arg(long)]
    density: Option<u8>,
    /// Print mode for the selected firmware.
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,
    /// Select an encoding. Zlib and V5G band encoding require hardware testing.
    #[arg(long, value_enum)]
    encoding: Option<EncodingArg>,
}
impl Controls {
    fn reject_unused(&self) -> anyhow::Result<()> {
        for (flag, present) in [
            ("energy", self.energy.is_some()),
            ("intensity", self.intensity.is_some()),
            ("speed", self.speed.is_some()),
            ("feed", self.feed.is_some()),
            ("feed-speed", self.feed_speed.is_some()),
            ("quality", self.quality.is_some()),
            ("concentration", self.concentration.is_some()),
            ("density", self.density.is_some()),
            (
                "mode",
                self.mode
                    .is_some_and(|mode| !matches!(mode, ModeArg::Image)),
            ),
            (
                "encoding",
                self.encoding
                    .is_some_and(|encoding| !matches!(encoding, EncodingArg::Raw)),
            ),
        ] {
            if present {
                anyhow::bail!("--{flag} is not supported by this driver.");
            }
        }
        Ok(())
    }
}
/// Convert CLI controls to the selected driver's options.
pub trait CliDriver: Driver {
    fn options(controls: Controls) -> anyhow::Result<Self::Options>;
}
macro_rules! options {
    ($driver:ty, $options:ident; $($field:ident),*; $($mode:ident)?; $($converted:ident => $convert:expr),* $(,)?) => {
        impl CliDriver for $driver {
            fn options(mut controls: Controls) -> anyhow::Result<Self::Options> {
                let defaults = $options::default();
                let options = $options {
                    $($field: controls.$field.take().map_or(defaults.$field, Into::into),)*
                    $($mode: mode(controls.$mode.take()),)?
                    $($converted: ($convert)(controls.$converted.take())?,)*
                };
                controls.reject_unused()?;
                Ok(options)
            }
        }
    };
}
const fn mode(value: Option<ModeArg>) -> Mode {
    match value {
        None | Some(ModeArg::Image) => Mode::Image,
        Some(ModeArg::Text) => Mode::Text,
        Some(ModeArg::Tattoo) => Mode::Tattoo,
        Some(ModeArg::Label) => Mode::Label,
    }
}
fn mono_encoding(value: Option<EncodingArg>) -> anyhow::Result<Option<MonoEncoding>> {
    value
        .map(|value| match value {
            EncodingArg::Raw => Ok(MonoEncoding::Raw),
            EncodingArg::Rle => Ok(MonoEncoding::Rle),
            _ => anyhow::bail!("This driver supports raw and RLE encoding only."),
        })
        .transpose()
}
fn row_encoding(value: Option<EncodingArg>) -> anyhow::Result<Option<X6hEncoding>> {
    value
        .map(|value| match value {
            EncodingArg::Raw => Ok(X6hEncoding::Raw),
            EncodingArg::Rle => Ok(X6hEncoding::Rle),
            EncodingArg::Lzo => Ok(X6hEncoding::Lzo),
            _ => anyhow::bail!("X6h supports raw, RLE, and LZO encoding only."),
        })
        .transpose()
}
fn band_encoding(value: Option<EncodingArg>) -> anyhow::Result<Option<V5gEncoding>> {
    value
        .map(|value| match value {
            EncodingArg::Raw => Ok(V5gEncoding::Raw),
            EncodingArg::LzoBands => Ok(V5gEncoding::LzoBands),
            EncodingArg::ZlibExperimental => Ok(V5gEncoding::ZlibExperimental),
            _ => anyhow::bail!("V5G supports raw, LZO bands, and experimental zlib encoding only."),
        })
        .transpose()
}
options!(Gt01, Gt01Options; energy, speed, feed, feed_speed, quality; mode; encoding => mono_encoding);
options!(TinyRle, TinyRleOptions; energy, speed, feed, feed_speed, quality; mode; encoding => mono_encoding);
options!(PrefixedTiny, PrefixedTinyOptions; energy, speed, feed, feed_speed, quality; mode; encoding => mono_encoding);
options!(Mx10, Mx10Options; energy, speed, feed, feed_speed, quality; mode;);
options!(CommonRaw, CommonRawOptions; energy, speed, feed, feed_speed, quality; mode;);
options!(X6h, X6hOptions; energy, speed, feed, feed_speed, concentration, density; mode; encoding => row_encoding);
options!(V5g, V5gOptions; energy, speed, feed, feed_speed, quality, density; mode; encoding => band_encoding);
options!(Mxw01, Mxw01Options; intensity;;);
#[derive(Subcommand)]
/// Content types the CLI can render.
pub enum Content {
    /// Render an image file.
    Image {
        /// Source image file.
        path: PathBuf,
        /// Monochrome conversion method.
        #[arg(long, value_enum, default_value_t = DitherArg::Ostromoukhov)]
        dither: DitherArg,
        /// Render sixteen gray levels for X6h or V5G.
        #[arg(long)]
        grayscale: bool,
    },
    /// Render literal text or a UTF-8 text file.
    Text(TextArgs),
    /// Render a QR code.
    Qr {
        /// Data to encode in the QR code.
        content: String,
    },
}
#[derive(Args)]
/// Text content, font, and dimensions.
pub struct TextArgs {
    #[arg(required_unless_present = "file", conflicts_with = "file")]
    /// Literal text when no file is selected.
    pub content: Option<String>,
    #[arg(long)]
    /// UTF-8 text file path.
    pub file: Option<PathBuf>,
    /// TrueType or OpenType font path. Defaults to the embedded Roboto Regular font.
    #[arg(long)]
    pub font: Option<PathBuf>,
    #[arg(long, default_value_t = 28.0)]
    /// Font size in dots.
    pub size: f32,
    #[arg(long, default_value_t = 12)]
    /// Margin on all sides in dots.
    pub margin: u32,
}
#[derive(Clone, Copy, ValueEnum)]
/// Available monochrome conversion methods.
pub enum DitherArg {
    Ostromoukhov,
    Stucki,
    FloydSteinberg,
    Threshold,
}
macro_rules! drivers {
    ($format:ident; $($name:ident => $driver:expr),* $(,)?) => {
        /// Names of the built-in drivers.
        #[derive(Clone, Copy, ValueEnum)]
        pub enum DriverArg { $($name),* }
        impl DriverArg {
            /// Execute a command with this driver.
            pub async fn execute(self, command: Command) -> anyhow::Result<()> {
                let $format = match &command {
                    Command::Print { connection, .. } | Command::Query { connection, .. }
                    | Command::Feed { connection, .. } | Command::Retract { connection, .. }
                    | Command::Cancel { connection } => connection.reply_format()?,
                    _ => ReplyFormat::default(),
                };
                match self { $(Self::$name => crate::execute(command, $driver).await),* }
            }
        }
    };
}
drivers!(reply_format;
    Gt01 => Gt01, Mx10 => Mx10, CommonRaw => CommonRaw, TinyRle => TinyRle,
    PrefixedTiny => PrefixedTiny, X6h => X6h, V5g => V5g,
    Mxw01 => Mxw01 { reply_format },
);
#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Image,
    Text,
    Tattoo,
    Label,
}
#[derive(Clone, Copy, ValueEnum)]
/// MXW01 notification formats.
pub enum ReplyFormatArg {
    WithCrc,
    WithoutCrc,
}
#[derive(Clone, Copy, ValueEnum)]
enum EncodingArg {
    Raw,
    Rle,
    Lzo,
    LzoBands,
    ZlibExperimental,
}
#[derive(Clone, Copy, ValueEnum)]
/// Printer queries.
pub enum QueryArg {
    Status,
    #[value(alias = "version")]
    Info,
}
