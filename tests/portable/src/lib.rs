#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use alloc::vec::Vec;
use core::future::{Future, poll_fn};
#[cfg(not(target_family = "wasm"))]
use core::task::Poll;
use embassy_time::{Instant, Timer};
use meowprint::{
    Clock, Error, PixelFormat, PrintReport, Result, RowSource, Transport, TransportEvent,
    WriteChannel,
};
#[cfg(any(target_family = "wasm", target_os = "none"))]
use meowprint::{Mx10, Mx10Options, Printer};

#[cfg(any(target_family = "wasm", target_os = "none"))]
use alloc::rc::Rc;
#[cfg(not(any(target_family = "wasm", target_os = "none")))]
use alloc::sync::Arc;
#[cfg(any(target_family = "wasm", target_os = "none"))]
use core::cell::RefCell;
#[cfg(not(any(target_family = "wasm", target_os = "none")))]
use std::sync::Mutex;

/// Embassy-backed elapsed-time clock for a consumer runtime.
#[derive(Clone)]
pub struct EmbassyClock {
    #[cfg(any(target_family = "wasm", target_os = "none"))]
    origin: Rc<Instant>,
    #[cfg(not(any(target_family = "wasm", target_os = "none")))]
    origin: Instant,
}

impl Default for EmbassyClock {
    fn default() -> Self {
        Self {
            #[cfg(not(any(target_family = "wasm", target_os = "none")))]
            origin: Instant::now(),
            #[cfg(any(target_family = "wasm", target_os = "none"))]
            origin: Rc::new(Instant::now()),
        }
    }
}

impl Clock for EmbassyClock {
    fn now(&self) -> core::time::Duration {
        core::time::Duration::from_micros(
            Instant::now()
                .as_micros()
                .saturating_sub(self.origin.as_micros()),
        )
    }

    async fn sleep_until(&self, deadline: core::time::Duration) {
        if self.now() >= deadline {
            return;
        }
        let micros = deadline.as_nanos().saturating_add(999) / 1000;
        let micros = micros.min(u64::MAX as u128) as u64;
        let at = self.origin.as_micros().saturating_add(micros);
        Timer::at(Instant::from_micros(at)).await;
    }

    async fn yield_now(&self) {
        #[cfg(target_family = "wasm")]
        {
            gloo_timers::future::TimeoutFuture::new(0).await;
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let mut yielded = false;
            poll_fn(|cx| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub rows: usize,
    pub frames: usize,
    pub bytes: usize,
    pub disconnects: usize,
    pub early_frame: bool,
}

#[derive(Default)]
struct Record {
    snapshot: Snapshot,
}

#[derive(Clone)]
pub struct RecordHandle(Shared<Record>);

#[cfg(any(target_family = "wasm", target_os = "none"))]
type Shared<T> = Rc<RefCell<T>>;
#[cfg(not(any(target_family = "wasm", target_os = "none")))]
type Shared<T> = Arc<Mutex<T>>;

impl RecordHandle {
    fn new() -> Self {
        Self(share(Record::default()))
    }

    fn update(&self, f: impl FnOnce(&mut Record)) {
        #[cfg(any(target_family = "wasm", target_os = "none"))]
        f(&mut self.0.borrow_mut());
        #[cfg(not(any(target_family = "wasm", target_os = "none")))]
        f(&mut self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner));
    }

    pub fn snapshot(&self) -> Snapshot {
        #[cfg(any(target_family = "wasm", target_os = "none"))]
        {
            self.0.borrow().snapshot
        }
        #[cfg(not(any(target_family = "wasm", target_os = "none")))]
        {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .snapshot
        }
    }
}

#[cfg(any(target_family = "wasm", target_os = "none"))]
fn share<T>(value: T) -> Shared<T> {
    Rc::new(RefCell::new(value))
}
#[cfg(not(any(target_family = "wasm", target_os = "none")))]
fn share<T>(value: T) -> Shared<T> {
    Arc::new(Mutex::new(value))
}

pub struct GeneratedRows {
    record: RecordHandle,
    next: usize,
}

impl RowSource for GeneratedRows {
    fn width(&self) -> u32 {
        384
    }
    fn height(&self) -> u32 {
        8
    }
    fn format(&self) -> PixelFormat {
        PixelFormat::Mono
    }
    async fn read_row(&mut self, output: &mut [u8]) -> Result<()> {
        let row = self.next;
        for (column, value) in output.iter_mut().enumerate() {
            *value = pixel(row, column);
        }
        self.next += 1;
        let rows = self.next;
        self.record.update(|r| r.snapshot.rows = rows);
        Ok(())
    }
}

const fn pixel(row: usize, column: usize) -> u8 {
    row.to_le_bytes()[0]
        .wrapping_mul(37)
        .wrapping_add(column.to_le_bytes()[0].wrapping_mul(11))
}

/// Create the common eight-row print, bounded frame parser, and observation handle.
pub fn stream_parts() -> (RecordingTransport, GeneratedRows, RecordHandle) {
    let record = RecordHandle::new();
    (
        RecordingTransport {
            record: record.clone(),
            pending: Vec::new(),
        },
        GeneratedRows {
            record: record.clone(),
            next: 0,
        },
        record,
    )
}

pub struct RecordingTransport {
    record: RecordHandle,
    pending: Vec<u8>,
}

impl Transport for RecordingTransport {
    fn max_write_size(&self) -> usize {
        63
    }

    fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> impl Future<Output = Result<()>> {
        #[cfg(target_family = "wasm")]
        let wait = async {
            wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(
                &wasm_bindgen::JsValue::UNDEFINED,
            ))
            .await
            .map_err(|_| Error::Transport("JS transport promise failed".into()))?;
            Ok(())
        };
        #[cfg(not(target_family = "wasm"))]
        let wait = async { Ok(()) };
        async move {
            wait.await?;
            assert_eq!(channel, WriteChannel::Control);
            assert!(bytes.len() <= self.max_write_size());
            self.record.update(|r| r.snapshot.bytes += bytes.len());
            self.pending.extend_from_slice(bytes);
            loop {
                if self.pending.len() < 6 {
                    break;
                }
                let length = usize::from(u16::from_le_bytes([self.pending[4], self.pending[5]]));
                assert!(length <= 48);
                if self.pending.len() < length + 8 {
                    break;
                }
                assert_eq!(&self.pending[..2], &[0x51, 0x78]);
                if self.pending[2] == 0xa2 {
                    let payload = &self.pending[6..6 + length];
                    assert_eq!(payload.len(), 48);
                    let mut observed = 0;
                    self.record.update(|r| {
                        observed = r.snapshot.frames;
                        if (1..=8).contains(&observed) && r.snapshot.rows < 8 {
                            r.snapshot.early_frame = true;
                        }
                        r.snapshot.frames += 1;
                    });
                    if observed == 0 || observed == 9 {
                        assert!(payload.iter().all(|&x| x == 0));
                    } else {
                        for (column, &value) in payload.iter().enumerate() {
                            assert_eq!(value, pixel(observed - 1, column));
                        }
                    }
                }
                self.pending.drain(..length + 8);
            }
            assert!(self.pending.len() < 64);
            Ok(())
        }
    }

    async fn event(&mut self) -> Result<TransportEvent> {
        core::future::pending().await
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.record.update(|r| r.snapshot.disconnects += 1);
        Ok(())
    }
}

pub fn verify_stream(report: &PrintReport, record: &RecordHandle) -> Result<()> {
    let snapshot = record.snapshot();
    if report.image_rows != 8
        || snapshot.rows != 8
        || snapshot.frames != 10
        || !snapshot.early_frame
        || snapshot.disconnects != 0
        || snapshot.bytes != report.bytes_submitted
    {
        return Err(Error::InvalidInput(
            "portable stream accounting mismatch".into(),
        ));
    }
    Ok(())
}

/// Concrete local-target paths ensure the no_std consumer compiles real operations.
#[cfg(any(target_family = "wasm", target_os = "none"))]
pub async fn local_print_and_cleanup() -> Result<()> {
    let (transport, mut rows, record) = stream_parts();
    let mut printer = Printer::with_clock(transport, Mx10, EmbassyClock::default());
    let report = printer
        .print(
            &mut rows,
            &Mx10Options {
                feed: 0,
                ..Default::default()
            },
        )
        .await?;
    verify_stream(&report, &record)?;
    printer.disconnect().await
}

#[cfg(any(target_family = "wasm", target_os = "none"))]
pub async fn local_cancel_and_cleanup() -> Result<()> {
    let (transport, _rows, record) = stream_parts();
    let clock = EmbassyClock::default();
    let mut printer = Printer::with_clock(transport, Mx10, clock.clone());
    let cancellation = printer.cancellation();
    let stop = async {
        clock
            .sleep_until(clock.now() + core::time::Duration::from_millis(1))
            .await;
        cancellation.cancel();
    };
    let mut stop = core::pin::pin!(stop);
    let mut stopped = false;
    let mut pending = PendingRows;
    let options = Mx10Options {
        feed: 0,
        ..Default::default()
    };
    let result = {
        let mut print = core::pin::pin!(printer.print(&mut pending, &options));
        poll_fn(|cx| {
            if !stopped && stop.as_mut().poll(cx).is_ready() {
                stopped = true;
            }
            print.as_mut().poll(cx)
        })
        .await
    };
    let cleanup = printer.disconnect().await;
    assert!(!printer.is_usable());
    assert_eq!(record.snapshot().disconnects, 1);
    match result {
        Err(Error::Cancelled) => cleanup,
        _ => Err(Error::InvalidInput(
            "pending source was not cancelled".into(),
        )),
    }
}

#[cfg(any(target_family = "wasm", target_os = "none"))]
pub async fn local_blocked_write_timeout_and_cleanup() -> Result<()> {
    let (transport, mut rows, _) = stream_parts();
    let mut printer =
        Printer::with_clock(BlockedTransport(transport), Mx10, EmbassyClock::default());
    let result = printer
        .print(
            &mut rows,
            &Mx10Options {
                feed: 0,
                ..Default::default()
            },
        )
        .await;
    let cleanup = printer.disconnect().await;
    match result {
        Err(Error::Timeout(_)) => cleanup,
        _ => Err(Error::InvalidInput("blocked write did not time out".into())),
    }
}

#[cfg(any(target_family = "wasm", target_os = "none"))]
struct PendingRows;
#[cfg(any(target_family = "wasm", target_os = "none"))]
impl RowSource for PendingRows {
    fn width(&self) -> u32 {
        384
    }
    fn height(&self) -> u32 {
        1
    }
    fn format(&self) -> PixelFormat {
        PixelFormat::Mono
    }
    async fn read_row(&mut self, _: &mut [u8]) -> Result<()> {
        core::future::pending().await
    }
}

#[cfg(any(target_family = "wasm", target_os = "none"))]
struct BlockedTransport<T>(T);
#[cfg(any(target_family = "wasm", target_os = "none"))]
impl<T: Transport> Transport for BlockedTransport<T> {
    fn max_write_size(&self) -> usize {
        self.0.max_write_size()
    }
    async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()> {
        let _ = (channel, bytes);
        core::future::pending().await
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        self.0.event().await
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.0.disconnect().await
    }
}
