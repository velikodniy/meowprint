//! Connected command operations, cancellation, and printer session cleanup.

use crate::{args::QueryArg, content::RenderedContent, query_output};
use anyhow::{Context, Result};
use meowprint::{Driver, PrintReport, Printer, Transport};
use std::{io, path::PathBuf};

/// Own a connected printer session and its source of cancellation.
pub struct Session<T, D: Driver, S> {
    printer: Printer<T, D>,
    signal: S,
}

impl<T: Transport, D: Driver, S: Future<Output = io::Result<()>>> Session<T, D, S> {
    /// Use an existing connection and a cancellation signal for one command.
    pub const fn new(printer: Printer<T, D>, signal: S) -> Self {
        Self { printer, signal }
    }

    /// Prepare and submit a print job, then close the printer session.
    pub async fn print(
        self,
        content: RenderedContent<D>,
        preview: Option<PathBuf>,
    ) -> Result<PrintReport> {
        self.run(
            async move |printer| {
                let job = content.prepare(printer)
                    .context("Cannot prepare the job. No print data was submitted.")?;
                if let Some(path) = preview {
                    job.preview()
                        .save_with_format(path, image::ImageFormat::Png)
                        .context("Cannot save the preview. No print data was submitted.")?;
                }
                eprintln!("Sending print data...");
                job.print().await.context("The job did not complete. Submitted rows have an unknown outcome. Do not retry automatically.")
            },
            "The job was submitted, but disconnect failed. Do not reprint automatically.",
        )
        .await
    }

    /// Read printer status or device information, then close the printer session.
    pub async fn query(self, query: QueryArg) -> Result<String> {
        self.run(
            async move |printer| Ok(query_output::read(printer, query).await?),
            "The query succeeded, but disconnect failed.",
        )
        .await
    }

    /// Submit paper movement, then close the printer session.
    pub async fn movement(self, rows: u16, retract: bool) -> Result<PrintReport> {
        self.run(
            async move |printer| {
                Ok(if retract {
                    printer.retract(rows).await?
                } else {
                    printer.feed(rows).await?
                })
            },
            "Paper movement was submitted, but disconnect failed. Do not repeat automatically.",
        )
        .await
    }

    async fn run<R>(
        mut self,
        operation: impl AsyncFnOnce(&mut Printer<T, D>) -> Result<R>,
        disconnect_context: &'static str,
    ) -> Result<R> {
        let cancellation = self.printer.cancellation();
        let result = {
            let operation = operation(&mut self.printer);
            tokio::pin!(operation);
            tokio::select! {
                result = &mut operation => result,
                signal = self.signal => {
                    cancellation.cancel();
                    // Await the active write before cancellation and cleanup.
                    let result = operation.await;
                    signal.context("Cannot listen for Ctrl-C. Submission stopped.")
                        .and(result)
                }
            }
        };
        let disconnected = self.printer.disconnect().await;
        let output = result?;
        disconnected.context(disconnect_context)?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Content;
    use async_trait::async_trait;
    use meowprint::{Error, Gt01, Gt01Options, TransportEvent, WriteChannel};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    #[derive(Default)]
    struct Record {
        writes: AtomicUsize,
        finished_writes: AtomicUsize,
        disconnects: AtomicUsize,
    }

    #[derive(Default)]
    struct Script {
        record: Arc<Record>,
        reply: bool,
        fail_write: bool,
        fail_disconnect: bool,
        write_delay: Duration,
    }

    #[async_trait]
    impl Transport for Script {
        fn max_write_size(&self) -> usize {
            120
        }

        async fn write(&mut self, _: WriteChannel, bytes: &[u8]) -> meowprint::Result<()> {
            self.record.writes.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.write_delay).await;
            self.record.finished_writes.fetch_add(1, Ordering::SeqCst);
            if self.fail_write {
                return Err(Error::Transport("operation failed".into()));
            }
            // Reply to the common-family status query.
            self.reply = bytes.starts_with(&[0x51, 0x78, 0xa3]);
            Ok(())
        }

        async fn event(&mut self) -> meowprint::Result<TransportEvent> {
            if std::mem::take(&mut self.reply) {
                return Ok(TransportEvent::Notification(vec![
                    0x51, 0x78, 0xa3, 1, 1, 0, 0, 0, 0xff,
                ]));
            }
            std::future::pending().await
        }

        async fn disconnect(&mut self) -> meowprint::Result<()> {
            self.record.disconnects.fetch_add(1, Ordering::SeqCst);
            if self.fail_disconnect {
                return Err(Error::Transport("disconnect failed".into()));
            }
            Ok(())
        }
    }

    fn session(script: Script) -> Session<Script, Gt01, impl Future<Output = io::Result<()>>> {
        Session::new(Printer::new(script, Gt01), std::future::pending())
    }

    fn content() -> Result<RenderedContent<Gt01>> {
        Content::Qr {
            content: "session-test".into(),
        }
        .render(Gt01, Gt01Options::default())
    }

    #[tokio::test(start_paused = true)]
    async fn connected_commands_disconnect_after_success() -> Result<()> {
        let script = Script::default();
        let record = Arc::clone(&script.record);
        let content = content()?;
        let (_, height) = content.dimensions();
        let report = session(script).print(content, None).await?;
        assert_eq!(report.image_rows, height);
        assert!(report.bytes_submitted > 0);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);

        let script = Script::default();
        let record = Arc::clone(&script.record);
        let output = session(script).query(QueryArg::Status).await?;
        assert!(output.contains("State: ready"), "{output}");
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);

        for retract in [false, true] {
            let script = Script::default();
            let record = Arc::clone(&script.record);
            let report = session(script).movement(1, retract).await?;
            assert_eq!(report.image_rows, 0);
            assert!(report.bytes_submitted > 0);
            assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn operation_failure_survives_disconnect_failure_without_replay() {
        let script = Script {
            fail_write: true,
            fail_disconnect: true,
            ..Script::default()
        };
        let record = Arc::clone(&script.record);
        let result = session(script).query(QueryArg::Status).await;
        let error = result.err().map(|error| format!("{error:#}"));
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("operation failed")),
            "{error:?}"
        );
        assert!(
            !error
                .as_deref()
                .is_some_and(|error| error.contains("disconnect failed"))
        );
        assert_eq!(record.writes.load(Ordering::SeqCst), 1);
        assert!(record.disconnects.load(Ordering::SeqCst) > 0);
    }

    #[tokio::test(start_paused = true)]
    async fn disconnect_failure_retains_the_successful_command_context() {
        let result = session(Script {
            fail_disconnect: true,
            ..Script::default()
        })
        .query(QueryArg::Status)
        .await;
        let error = result.err().map(|error| format!("{error:#}"));
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("The query succeeded, but disconnect failed.")),
            "{error:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn preparation_failure_disconnects_without_a_submission_warning() -> Result<()> {
        let script = Script::default();
        let record = Arc::clone(&script.record);
        let printer = Printer::new(script, Gt01);
        printer.cancellation().cancel();
        let result = Session::new(printer, std::future::pending::<io::Result<()>>())
            .print(content()?, None)
            .await;
        let error = result.err().map(|error| format!("{error:#}"));
        assert!(
            error
                .as_deref()
                .is_some_and(|error| error.contains("No print data was submitted.")),
            "{error:?}"
        );
        assert!(
            !error
                .as_deref()
                .is_some_and(|error| error.contains("unknown outcome"))
        );
        assert_eq!(record.writes.load(Ordering::SeqCst), 0);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn preview_save_failure_disconnects_before_submission() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("meowprint-preview-parent-{}", std::process::id()));
        std::fs::write(&path, [])?;
        let script = Script::default();
        let record = Arc::clone(&script.record);
        let result = session(script)
            .print(content()?, Some(path.join("preview.png")))
            .await;
        std::fs::remove_file(path)?;
        let error = result.err().map(|error| format!("{error:#}"));
        assert!(
            error.as_deref().is_some_and(
                |error| error.contains("Cannot save the preview. No print data was submitted.")
            ),
            "{error:?}"
        );
        assert_eq!(record.writes.load(Ordering::SeqCst), 0);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_and_signal_failure_await_the_active_write_and_disconnect() -> Result<()> {
        for fail_signal in [false, true] {
            let script = Script {
                write_delay: Duration::from_secs(1),
                ..Script::default()
            };
            let record = Arc::clone(&script.record);
            let signal = async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if fail_signal {
                    Err(io::Error::other("signal failed"))
                } else {
                    Ok(())
                }
            };
            let result = Session::new(Printer::new(script, Gt01), signal)
                .print(content()?, None)
                .await;
            let error = result.err();
            if fail_signal {
                assert!(
                    error.as_ref().is_some_and(|error| format!("{error:#}")
                        .contains("Cannot listen for Ctrl-C. Submission stopped.")),
                    "{error:?}"
                );
            } else {
                assert!(
                    matches!(
                        error
                            .as_ref()
                            .and_then(|error| error.downcast_ref::<Error>()),
                        Some(Error::Cancelled)
                    ),
                    "{error:?}"
                );
            }
            assert_eq!(record.writes.load(Ordering::SeqCst), 1);
            assert_eq!(record.finished_writes.load(Ordering::SeqCst), 1);
            assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        }
        Ok(())
    }
}
