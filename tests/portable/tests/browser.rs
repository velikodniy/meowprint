#![cfg(target_family = "wasm")]

use core::{
    future::{self, poll_fn},
    task::Poll,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    time::Duration,
};

use embassy_time::Timer;
use meowprint::{
    Clock, Error, Mx10, Mx10Options, PixelFormat, Printer, Result, RowSource, Transport,
    TransportEvent, WriteChannel,
};
use meowprint_portable::{
    EmbassyClock, GeneratedRows, RecordingTransport, stream_parts, verify_stream,
};
use wasm_bindgen_futures::spawn_local;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn streamed_print_uses_local_state_and_browser_promises() -> Result<()> {
    let (inner, mut source, record) = stream_parts();
    let mut printer = Printer::with_clock(inner, Mx10, EmbassyClock::default());
    let report = printer
        .print(
            &mut source,
            &Mx10Options {
                feed: 0,
                ..Default::default()
            },
        )
        .await?;
    verify_stream(&report, &record)?;
    assert!(printer.is_usable());
    assert_eq!(report.completion, meowprint::Completion::TimedDrain);
    printer.disconnect().await?;
    assert_eq!(record.snapshot().disconnects, 1);
    Ok(())
}

struct PendingRows(GeneratedRows);
impl RowSource for PendingRows {
    fn width(&self) -> u32 {
        self.0.width()
    }
    fn height(&self) -> u32 {
        self.0.height()
    }
    fn format(&self) -> PixelFormat {
        self.0.format()
    }
    async fn read_row(&mut self, _output: &mut [u8]) -> Result<()> {
        future::pending().await
    }
}

#[wasm_bindgen_test]
async fn pending_row_cancellation_finishes_with_awaited_cleanup() -> Result<()> {
    let (inner, generated, record) = stream_parts();
    let mut printer = Printer::with_clock(inner, Mx10, EmbassyClock::default());
    let cancellation = printer.cancellation();
    spawn_local(async move {
        Timer::after(embassy_time::Duration::from_millis(20)).await;
        cancellation.cancel();
    });
    let mut source = PendingRows(generated);
    let result = printer
        .print(
            &mut source,
            &Mx10Options {
                feed: 0,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!printer.is_usable());
    printer.disconnect().await?;
    assert_eq!(record.snapshot().disconnects, 1);
    Ok(())
}

struct BlockedWrite {
    inner: RecordingTransport,
    writes: Rc<Cell<usize>>,
    disconnects: Rc<Cell<usize>>,
}
impl Transport for BlockedWrite {
    fn max_write_size(&self) -> usize {
        self.inner.max_write_size()
    }
    async fn write(&mut self, _channel: WriteChannel, _bytes: &[u8]) -> Result<()> {
        self.writes.set(self.writes.get() + 1);
        future::pending().await
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        self.inner.event().await
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.disconnects.set(self.disconnects.get() + 1);
        self.inner.disconnect().await
    }
}

#[wasm_bindgen_test]
async fn pending_write_times_out_without_replay_and_disconnects_once() -> Result<()> {
    let (inner, mut source, _) = stream_parts();
    let clock = EmbassyClock::default();
    let started = clock.now();
    let writes = Rc::new(Cell::new(0));
    let disconnects = Rc::new(Cell::new(0));
    let mut printer = Printer::with_clock(
        BlockedWrite {
            inner,
            writes: Rc::clone(&writes),
            disconnects: Rc::clone(&disconnects),
        },
        Mx10,
        clock.clone(),
    );
    let result = printer
        .print(
            &mut source,
            &Mx10Options {
                feed: 0,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(Error::Timeout("transport write"))));
    assert!(clock.now().saturating_sub(started) >= Duration::from_secs(5));
    assert_eq!(writes.get(), 1);
    assert_eq!(disconnects.get(), 1);
    assert!(!printer.is_usable());
    Ok(())
}

struct BurstTransport {
    inner: RecordingTransport,
    enabled: Rc<Cell<bool>>,
    events: Rc<RefCell<VecDeque<TransportEvent>>>,
}
impl Transport for BurstTransport {
    fn max_write_size(&self) -> usize {
        self.inner.max_write_size()
    }
    async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()> {
        self.inner.write(channel, bytes).await
    }
    async fn event(&mut self) -> Result<TransportEvent> {
        poll_fn(|cx| {
            if !self.enabled.get() {
                return Poll::Pending;
            }
            match self.events.borrow_mut().pop_front() {
                Some(event) => Poll::Ready(Ok(event)),
                None => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        })
        .await
    }
    async fn disconnect(&mut self) -> Result<()> {
        self.inner.disconnect().await
    }
}

struct BurstRows {
    generated: GeneratedRows,
    enabled: Rc<Cell<bool>>,
    events: Rc<RefCell<VecDeque<TransportEvent>>>,
    reads: Rc<Cell<usize>>,
}
impl RowSource for BurstRows {
    fn width(&self) -> u32 {
        self.generated.width()
    }
    fn height(&self) -> u32 {
        self.generated.height()
    }
    fn format(&self) -> PixelFormat {
        self.generated.format()
    }
    async fn read_row(&mut self, _output: &mut [u8]) -> Result<()> {
        self.reads.set(self.reads.get() + 1);
        if !self.enabled.replace(true) {
            let mut events = self.events.borrow_mut();
            events.extend((0..1024).map(|_| {
                TransportEvent::Notification(vec![0x51, 0x78, 0xae, 1, 1, 0, 0, 0, 0xff])
            }));
            events.push_back(TransportEvent::Disconnected);
        }
        future::pending().await
    }
}

#[wasm_bindgen_test]
async fn ready_notification_burst_yields_to_timer_cancellation_before_sentinel() -> Result<()> {
    let (inner, generated, record) = stream_parts();
    let enabled = Rc::new(Cell::new(false));
    let events = Rc::new(RefCell::new(VecDeque::new()));
    let reads = Rc::new(Cell::new(0));
    let transport = BurstTransport {
        inner,
        enabled: Rc::clone(&enabled),
        events: Rc::clone(&events),
    };
    let mut source = BurstRows {
        generated,
        enabled: Rc::clone(&enabled),
        events: Rc::clone(&events),
        reads: Rc::clone(&reads),
    };
    let mut printer = Printer::with_clock(transport, Mx10, EmbassyClock::default());
    let cancellation = printer.cancellation();
    // Before the first row read, preflight has no event to consume.
    assert!(events.borrow().is_empty());
    let timer_enabled = Rc::clone(&enabled);
    spawn_local(async move {
        while !timer_enabled.get() {
            Timer::after(embassy_time::Duration::from_millis(1)).await;
        }
        Timer::after(embassy_time::Duration::from_millis(20)).await;
        cancellation.cancel();
    });
    let result = printer
        .print(
            &mut source,
            &Mx10Options {
                feed: 0,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!printer.is_usable());
    assert_eq!(record.snapshot().disconnects, 1);
    assert_eq!(
        reads.get(),
        1,
        "the pending row future must be retained across notifications"
    );
    assert!(
        events
            .borrow()
            .iter()
            .any(|event| matches!(event, TransportEvent::Disconnected))
    );
    Ok(())
}
