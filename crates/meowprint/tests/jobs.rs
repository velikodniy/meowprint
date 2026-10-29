//! Streaming source contracts and independently observed wire boundaries.
#[cfg(feature = "image")]
use meowprint::Raster;
use meowprint::{
    Error, Gt01, Gt01Options, Mx10, Mx10Options, Mxw01, PixelFormat, Printer, Result, RowSource,
    Transport, TransportEvent, V5g, V5gOptions, WriteChannel,
};
use std::{
    collections::VecDeque,
    future::{Future, poll_fn},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
};

#[derive(Default)]
struct Counts {
    metadata: [AtomicUsize; 3],
    rows: AtomicUsize,
    io: AtomicUsize,
    bytes: AtomicUsize,
    frames: AtomicUsize,
    early_write: AtomicBool,
}
struct Generated {
    width: u32,
    height: u32,
    format: PixelFormat,
    counts: Arc<Counts>,
}
impl Generated {
    fn mono(height: u32, counts: &Arc<Counts>) -> Self {
        Self {
            width: 384,
            height,
            format: PixelFormat::Mono,
            counts: Arc::clone(counts),
        }
    }
}
const fn pixel(row: usize, column: usize) -> u8 {
    row.to_le_bytes()[0]
        .wrapping_mul(37)
        .wrapping_add(column.to_le_bytes()[0].wrapping_mul(11))
}
impl RowSource for Generated {
    fn width(&self) -> u32 {
        self.counts.metadata[0].fetch_add(1, Ordering::SeqCst);
        self.width
    }
    fn height(&self) -> u32 {
        self.counts.metadata[1].fetch_add(1, Ordering::SeqCst);
        self.height
    }
    fn format(&self) -> PixelFormat {
        self.counts.metadata[2].fetch_add(1, Ordering::SeqCst);
        self.format
    }
    fn read_row(&mut self, output: &mut [u8]) -> impl Future<Output = Result<()>> + Send {
        let row = self.counts.rows.fetch_add(1, Ordering::SeqCst);
        for (column, value) in output.iter_mut().enumerate() {
            *value = pixel(row, column);
        }
        std::future::ready(Ok(()))
    }
}
struct CountingTransport(Arc<Counts>);
impl Transport for CountingTransport {
    fn write(&mut self, _: WriteChannel, bytes: &[u8]) -> impl Future<Output = Result<()>> + Send {
        poll_fn(move |_| {
            self.0.io.fetch_add(1, Ordering::SeqCst);
            self.0.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
            Poll::Ready(Ok(()))
        })
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        self.0.io.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    fn disconnect(&mut self) -> impl Future<Output = Result<()>> + Send {
        poll_fn(|_| {
            self.0.io.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        })
    }
}

#[tokio::test]
async fn invalid_metadata_and_options_do_not_read_rows_or_touch_transport() {
    let counts = Arc::new(Counts::default());
    let mut printer = Printer::new(CountingTransport(Arc::clone(&counts)), Gt01);
    for (width, height, format) in [
        (376, 1, PixelFormat::Mono),
        (384, 0, PixelFormat::Mono),
        (384, 32769, PixelFormat::Mono),
        (384, 1, PixelFormat::Gray4),
    ] {
        let mut source = Generated {
            width,
            height,
            format,
            counts: Arc::clone(&counts),
        };
        assert!(matches!(
            printer.print(&mut source, &Gt01Options::default()).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(printer.is_usable());
    }
    let mut source = Generated::mono(1, &counts);
    assert!(matches!(
        printer
            .print(
                &mut source,
                &Gt01Options {
                    speed: 0,
                    ..Default::default()
                }
            )
            .await,
        Err(Error::InvalidInput(_))
    ));
    assert!(printer.is_usable());
    assert_eq!(counts.rows.load(Ordering::SeqCst), 0);
    assert_eq!(counts.io.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn metadata_is_captured_once_on_first_poll_and_next_page_stays_unread() -> Result<()> {
    let counts = Arc::new(Counts::default());
    let mut source = Generated::mono(3, &counts);
    let mut printer = Printer::new(CountingTransport(Arc::clone(&counts)), Gt01);
    let options = Gt01Options {
        feed: 0,
        ..Default::default()
    };
    drop(printer.print(&mut source, &options));
    assert!(printer.is_usable());
    for count in &counts.metadata {
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
    assert_eq!(counts.io.load(Ordering::SeqCst), 0);
    assert_eq!(counts.rows.load(Ordering::SeqCst), 0);
    let mut print = Box::pin(printer.print(&mut source, &options));
    std::future::poll_fn(|cx| {
        assert!(print.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    for count in &counts.metadata {
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    let report = print.await?;
    for count in &counts.metadata {
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    assert_eq!(counts.rows.load(Ordering::SeqCst), 3);
    assert_eq!(report.image_rows, 3);
    assert_eq!(report.bytes_submitted, counts.bytes.load(Ordering::SeqCst));
    let mut next_page = [0; 48];
    source.read_row(&mut next_page).await?;
    for (column, byte) in next_page.into_iter().enumerate() {
        assert_eq!(byte, pixel(3, column));
    }
    Ok(())
}

// This transport retains only one partial frame, not a page or a write log.
struct CheckingTransport {
    counts: Arc<Counts>,
    pending: Vec<u8>,
    height: usize,
}
impl Transport for CheckingTransport {
    fn max_write_size(&self) -> usize {
        63
    }
    fn write(
        &mut self,
        channel: WriteChannel,
        bytes: &[u8],
    ) -> impl Future<Output = Result<()>> + Send {
        poll_fn(move |_| {
            assert_eq!(channel, WriteChannel::Control);
            if self.counts.rows.load(Ordering::SeqCst) < self.height {
                self.counts.early_write.store(true, Ordering::SeqCst);
            }
            self.counts.bytes.fetch_add(bytes.len(), Ordering::SeqCst);
            self.pending.extend_from_slice(bytes);
            while self.pending.len() >= 6 {
                let length = usize::from(u16::from_le_bytes([self.pending[4], self.pending[5]]));
                if self.pending.len() < length + 8 {
                    break;
                }
                assert_eq!(&self.pending[..2], &[0x51, 0x78]);
                if self.pending[2] == 0xa2 {
                    let row = self.counts.frames.fetch_add(1, Ordering::SeqCst);
                    let payload = &self.pending[6..6 + length];
                    assert_eq!(payload.len(), 48);
                    if row == 0 || row == self.height + 1 {
                        assert_eq!(payload, &[0; 48]);
                    } else {
                        for (column, &value) in payload.iter().enumerate() {
                            assert_eq!(value, pixel(row - 1, column));
                        }
                    }
                }
                self.pending.drain(..length + 8);
            }
            assert!(self.pending.len() < 64);
            Poll::Ready(Ok(()))
        })
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        std::future::pending().await
    }
    fn disconnect(&mut self) -> impl Future<Output = Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}
#[tokio::test(start_paused = true)]
async fn generated_maximum_height_streams_ordered_rows_before_production_finishes() -> Result<()> {
    let counts = Arc::new(Counts::default());
    let mut source = Generated::mono(32768, &counts);
    let transport = CheckingTransport {
        counts: Arc::clone(&counts),
        pending: Vec::new(),
        height: 32768,
    };
    let mut printer = Printer::new(transport, V5g);
    let report = printer
        .print(
            &mut source,
            &V5gOptions {
                feed: 0,
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(report.image_rows, 32768);
    assert_eq!(counts.rows.load(Ordering::SeqCst), 32768);
    assert_eq!(counts.frames.load(Ordering::SeqCst), 32770);
    assert!(counts.early_write.load(Ordering::SeqCst));
    assert_eq!(report.bytes_submitted, counts.bytes.load(Ordering::SeqCst));
    Ok(())
}

#[derive(Default)]
struct Record {
    writes: Vec<(WriteChannel, Vec<u8>)>,
    controls: Vec<(u8, Vec<u8>)>,
}
fn record(shared: &Arc<Mutex<Record>>) -> Result<MutexGuard<'_, Record>> {
    shared
        .lock()
        .map_err(|_| Error::Transport("test record poisoned".into()))
}
fn reply(command: u8, payload: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0x22, 0x21, command, 3, payload.len().to_le_bytes()[0], 0];
    bytes.extend_from_slice(payload);
    let crc = payload.iter().fold(0u8, |mut crc, &byte| {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 == 0 {
                crc << 1
            } else {
                (crc << 1) ^ 7
            };
        }
        crc
    });
    bytes.extend_from_slice(&[crc, 0xff]);
    bytes
}
struct RecordingTransport {
    limit: usize,
    mxw01: bool,
    shared: Arc<Mutex<Record>>,
    pending: Vec<u8>,
    replies: VecDeque<TransportEvent>,
}
impl Transport for RecordingTransport {
    fn max_write_size(&self) -> usize {
        self.limit
    }
    fn write(
        &mut self,
        channel: WriteChannel,
        bytes: &[u8],
    ) -> impl Future<Output = Result<()>> + Send {
        poll_fn(move |_| {
            assert!(bytes.len() <= self.limit);
            record(&self.shared)?.writes.push((channel, bytes.to_vec()));
            if channel == WriteChannel::Control {
                self.pending.extend_from_slice(bytes);
                while self.pending.len() >= 6 {
                    let length =
                        usize::from(u16::from_le_bytes([self.pending[4], self.pending[5]]));
                    if self.pending.len() < length + 8 {
                        break;
                    }
                    let command = self.pending[2];
                    record(&self.shared)?
                        .controls
                        .push((command, self.pending[6..6 + length].to_vec()));
                    if self.mxw01 {
                        let response = match command {
                            0xa1 => Some(reply(0xa1, &[0; 13])),
                            0xa9 => Some(reply(0xa9, &[0])),
                            0xad => Some(reply(0xaa, &[0])),
                            _ => None,
                        };
                        if let Some(bytes) = response {
                            self.replies.push_back(TransportEvent::Notification(bytes));
                        }
                    }
                    self.pending.drain(..length + 8);
                }
            }
            Poll::Ready(Ok(()))
        })
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        if let Some(event) = self.replies.pop_front() {
            return Ok(event);
        }
        std::future::pending().await
    }
    fn disconnect(&mut self) -> impl Future<Output = Result<()>> + Send {
        std::future::ready(Ok(()))
    }
}
fn recording(limit: usize, mxw01: bool, shared: &Arc<Mutex<Record>>) -> RecordingTransport {
    RecordingTransport {
        limit,
        mxw01,
        shared: Arc::clone(shared),
        pending: Vec::new(),
        replies: VecDeque::new(),
    }
}
#[tokio::test(start_paused = true)]
async fn mx10_limits_preserve_image_padding_and_zero_or_default_feed() -> Result<()> {
    for limit in [20, 63] {
        for feed in [0, Mx10Options::default().feed] {
            let shared = Arc::new(Mutex::new(Record::default()));
            let mut printer = Printer::new(recording(limit, false, &shared), Mx10);
            let counts = Arc::new(Counts::default());
            let mut source = Generated::mono(3, &counts);
            printer
                .print(
                    &mut source,
                    &Mx10Options {
                        feed,
                        ..Default::default()
                    },
                )
                .await?;
            let record = record(&shared)?;
            let frames: Vec<_> = record
                .controls
                .iter()
                .filter(|(command, _)| *command == 0xa2)
                .collect();
            assert_eq!(frames.len(), 3 + 2 + usize::from(feed));
            for (index, (_, payload)) in frames.iter().enumerate() {
                if (1..=3).contains(&index) {
                    for (column, &value) in payload.iter().enumerate() {
                        assert_eq!(value, pixel(index - 1, column));
                    }
                } else {
                    assert_eq!(payload.as_slice(), &[0; 48]);
                }
            }
            assert!(
                record
                    .writes
                    .iter()
                    .all(|(channel, bytes)| *channel == WriteChannel::Control
                        && bytes.len() <= limit)
            );
            drop(record);
            assert_eq!(counts.rows.load(Ordering::SeqCst), 3);
        }
    }
    Ok(())
}
#[tokio::test(start_paused = true)]
async fn mxw01_announces_padding_and_preserves_continuous_chunk_boundaries() -> Result<()> {
    for limit in [20, 120] {
        for height in [1, 87, 88, 89, 90, 32768] {
            let shared = Arc::new(Mutex::new(Record::default()));
            let mut printer = Printer::new(recording(limit, true, &shared), Mxw01::default());
            let counts = Arc::new(Counts::default());
            let mut source = Generated::mono(height, &counts);
            let report = printer
                .print(&mut source, &meowprint::Mxw01Options::default())
                .await?;
            let record = record(&shared)?;
            let total_rows = (height as usize + 2).max(90);
            let start = record
                .controls
                .iter()
                .find(|(command, _)| *command == 0xa9)
                .ok_or_else(|| Error::Transport("missing start command".into()))?;
            assert_eq!(
                usize::from(u16::from_le_bytes([start.1[0], start.1[1]])),
                total_rows
            );
            let chunks: Vec<_> = record
                .writes
                .iter()
                .filter(|(channel, _)| *channel == WriteChannel::Raster)
                .map(|(_, bytes)| bytes)
                .collect();
            let total_bytes = total_rows * 48;
            assert_eq!(chunks.len(), total_bytes.div_ceil(limit));
            let mut position = 0;
            for chunk in chunks {
                assert_eq!(chunk.len(), limit.min(total_bytes - position));
                for &value in chunk {
                    let row = position / 48;
                    let expected = if row == 0 || row > height as usize {
                        0
                    } else {
                        pixel(row - 1, position % 48)
                    };
                    assert_eq!(value, expected, "height={height}, byte={position}");
                    position += 1;
                }
            }
            assert_eq!(position, total_bytes);
            assert_eq!(counts.rows.load(Ordering::SeqCst), height as usize);
            assert_eq!(report.image_rows, height);
            assert_eq!(
                report.bytes_submitted,
                record.writes.iter().map(|(_, bytes)| bytes.len()).sum()
            );
        }
    }
    Ok(())
}

#[cfg(feature = "image")]
#[test]
fn raster_preview_preserves_bits_nibbles_and_all_gray_levels() -> Result<()> {
    let mut bytes = vec![0; 96];
    bytes[0] = 0x81;
    bytes[48] = 0x02;
    let raster = Raster::new(384, bytes, PixelFormat::Mono)?;
    let mut preview = raster.preview();
    assert_eq!(
        &preview.as_raw()[..8],
        &[0, 255, 255, 255, 255, 255, 255, 0]
    );
    assert_eq!(preview.get_pixel(1, 1)[0], 0);
    preview.put_pixel(0, 0, image::Luma([255]));
    assert_eq!(raster.preview().get_pixel(0, 0)[0], 0);
    let pixels: Vec<u8> = (0u8..192)
        .map(|index| {
            let left = index.wrapping_mul(2) % 16;
            left | ((left + 1) << 4)
        })
        .collect();
    let gray = Raster::new(384, pixels, PixelFormat::Gray4)?;
    assert_eq!(
        &gray.preview().as_raw()[..16],
        &[
            255, 238, 221, 204, 187, 170, 153, 136, 119, 102, 85, 68, 51, 34, 17, 0
        ]
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn mxw01_source_failure_discards_unsent_suffix_without_padding_or_flush() -> Result<()> {
    struct FailsSecond {
        reads: usize,
    }
    impl RowSource for FailsSecond {
        fn width(&self) -> u32 {
            384
        }
        fn height(&self) -> u32 {
            2
        }
        fn format(&self) -> PixelFormat {
            PixelFormat::Mono
        }
        fn read_row(&mut self, output: &mut [u8]) -> impl Future<Output = Result<()>> + Send {
            self.reads += 1;
            if self.reads == 2 {
                return std::future::ready(Err(Error::InvalidInput("source failure".into())));
            }
            output.fill(0x81);
            std::future::ready(Ok(()))
        }
    }
    for limit in [20, 120] {
        let shared = Arc::new(Mutex::new(Record::default()));
        let mut printer = Printer::new(recording(limit, true, &shared), Mxw01::default());
        let mut source = FailsSecond { reads: 0 };
        let result = printer
            .print(&mut source, &meowprint::Mxw01Options::default())
            .await;
        assert!(matches!(result, Err(Error::InvalidInput(_))));
        assert!(!printer.is_usable());
        assert_eq!(source.reads, 2);
        let record = record(&shared)?;
        assert!(record.controls.iter().all(|(command, _)| *command != 0xad));
        let accepted: Vec<_> = record
            .writes
            .iter()
            .filter(|(channel, _)| *channel == WriteChannel::Raster)
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect();
        drop(record);
        let mut expected = vec![0; 48];
        expected.extend_from_slice(&[0x81; 48]);
        assert_eq!(accepted, expected[..96 / limit * limit]);
    }
    Ok(())
}

#[tokio::test]
async fn mxw01_zero_write_limit_fails_before_reading_input() {
    let shared = Arc::new(Mutex::new(Record::default()));
    let counts = Arc::new(Counts::default());
    let mut source = Generated::mono(1, &counts);
    let mut printer = Printer::new(recording(0, true, &shared), Mxw01::default());
    assert!(matches!(
        printer
            .print(&mut source, &meowprint::Mxw01Options::default())
            .await,
        Err(Error::Transport(_))
    ));
    assert_eq!(counts.rows.load(Ordering::SeqCst), 0);
    assert!(!printer.is_usable());
}
