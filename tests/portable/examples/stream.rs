use meowprint::Completion;
use meowprint_portable::{EmbassyClock, Snapshot, stream_parts, verify_stream};

fn main() -> meowprint::Result<()> {
    let task = async move {
        let (transport, mut rows, record) = stream_parts();
        let mut printer =
            meowprint::Printer::with_clock(transport, meowprint::Mx10, EmbassyClock::default());
        let report = printer
            .print(
                &mut rows,
                &meowprint::Mx10Options {
                    feed: 0,
                    ..Default::default()
                },
            )
            .await?;
        verify_stream(&report, &record)?;
        if report.completion != Completion::TimedDrain || !printer.is_usable() {
            return Err(meowprint::Error::InvalidInput(
                "portable print completion or connection state mismatch".into(),
            ));
        }
        printer.disconnect().await?;
        if printer.is_usable()
            || record.snapshot()
                != (Snapshot {
                    rows: 8,
                    frames: 10,
                    bytes: report.bytes_submitted,
                    disconnects: 1,
                    early_frame: true,
                })
        {
            return Err(meowprint::Error::InvalidInput(
                "portable cleanup accounting mismatch".into(),
            ));
        }
        Ok(())
    };
    let worker = std::thread::spawn(move || futures_lite::future::block_on(task));
    worker
        .join()
        .map_err(|_| meowprint::Error::InvalidInput("portable worker panicked".into()))??;
    println!("portable stream: ok");
    Ok(())
}
