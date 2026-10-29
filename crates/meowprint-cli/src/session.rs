//! Connected command operations, cancellation, and printer session cleanup.

use crate::{args::QueryArg, content::RenderedContent, query_output};
use anyhow::{Context, Result};
use meowprint::{Driver, PrintReport, Printer, Transport};
use std::io;

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

    /// Submit rendered raster rows, then close the printer session.
    pub async fn print(self, content: RenderedContent<D>) -> Result<PrintReport> {
        self.run(
            async move |printer| {
                let RenderedContent { raster, options } = content;
                let mut rows = raster.rows();
                eprintln!("Sending print data...");
                printer.print(&mut rows, &options).await.context("The print operation did not complete. Submitted rows have an unknown outcome. Do not retry automatically.")
            },
            "The print operation was submitted, but disconnect failed. Do not reprint automatically.",
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
    use crate::args::{Cli, CliDriver, Command as CliCommand, Content, Controls};
    use clap::Parser;
    use meowprint::{Error, Gt01, Gt01Options, TransportEvent, WriteChannel, X6h};
    use std::{
        future::poll_fn,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        task::Poll,
        time::Duration,
    };

    #[derive(Default)]
    struct Record {
        writes: AtomicUsize,
        finished_writes: AtomicUsize,
        disconnects: AtomicUsize,
        control_bytes: Mutex<Vec<u8>>,
    }

    #[derive(Default)]
    struct Script {
        record: Arc<Record>,
        reply: bool,
        fail_write: bool,
        fail_disconnect: bool,
        write_delay: Duration,
    }

    impl Transport for Script {
        fn max_write_size(&self) -> usize {
            120
        }
        async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> meowprint::Result<()> {
            self.record.writes.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.write_delay).await;
            self.record.finished_writes.fetch_add(1, Ordering::SeqCst);
            if self.fail_write {
                return Err(Error::Transport("operation failed".into()));
            }
            if channel == WriteChannel::Control {
                self.record
                    .control_bytes
                    .lock()
                    .map_err(|error| Error::Transport(error.to_string()))?
                    .extend_from_slice(bytes);
            }
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
        fn disconnect(&mut self) -> impl Future<Output = meowprint::Result<()>> + Send {
            poll_fn(|_| {
                assert_eq!(
                    self.record.writes.load(Ordering::SeqCst),
                    self.record.finished_writes.load(Ordering::SeqCst)
                );
                self.record.disconnects.fetch_add(1, Ordering::SeqCst);
                if self.fail_disconnect {
                    return Poll::Ready(Err(Error::Transport("disconnect failed".into())));
                }
                Poll::Ready(Ok(()))
            })
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

    fn command_payloads(bytes: &[u8], command: u8) -> Vec<&[u8]> {
        let mut payloads = Vec::new();
        let mut position = 0;
        while position < bytes.len() {
            assert!(position + 8 <= bytes.len());
            assert_eq!(&bytes[position..position + 2], &[0x51, 0x78]);
            let payload_len = usize::from(u16::from_le_bytes([
                bytes[position + 4],
                bytes[position + 5],
            ]));
            let end = position + payload_len + 8;
            assert!(end <= bytes.len());
            if bytes[position + 2] == command {
                payloads.push(&bytes[position + 6..position + 6 + payload_len]);
            }
            position = end;
        }
        payloads
    }

    fn parsed_print(args: &[&str]) -> Result<(Controls, Content)> {
        match Cli::try_parse_from(args)?.command {
            CliCommand::Print {
                controls, content, ..
            } => Ok((controls, content)),
            _ => anyhow::bail!("Expected a print command."),
        }
    }

    fn snapshot(record: &Record) -> Result<(usize, Vec<u8>)> {
        Ok((
            record.writes.load(Ordering::SeqCst),
            record
                .control_bytes
                .lock()
                .map_err(|error| anyhow::anyhow!("{error}"))?
                .clone(),
        ))
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_zero_controls_reach_printed_commands() -> Result<()> {
        let (controls, content) = parsed_print(&[
            "cli", "print", "--device", "p", "--driver", "gt01", "--feed", "0", "--energy", "0",
            "qr", "x",
        ])?;
        let options = Gt01::options(controls)?;
        let script = Script::default();
        let record = Arc::clone(&script.record);
        Session::new(Printer::new(script, Gt01), std::future::pending())
            .print(content.render(Gt01, options)?)
            .await?;
        let (_, bytes) = snapshot(&record)?;
        assert_eq!(command_payloads(&bytes, 0xaf), vec![&[0, 0][..]]);
        assert_eq!(command_payloads(&bytes, 0xa1), [] as [&[u8]; 0]);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);

        for (args, expected) in [
            (
                vec![
                    "cli", "print", "--device", "p", "--driver", "x6h", "qr", "x",
                ],
                None,
            ),
            (
                vec![
                    "cli",
                    "print",
                    "--device",
                    "p",
                    "--driver",
                    "x6h",
                    "--density",
                    "0",
                    "qr",
                    "x",
                ],
                Some(&[1, 0][..]),
            ),
        ] {
            let (controls, content) = parsed_print(&args)?;
            let driver = X6h;
            let options = X6h::options(controls)?;
            let script = Script::default();
            let record = Arc::clone(&script.record);
            Session::new(Printer::new(script, driver), std::future::pending())
                .print(content.render(driver, options)?)
                .await?;
            let (_, bytes) = snapshot(&record)?;
            let payloads = command_payloads(&bytes, 0xf2);
            match expected {
                None => assert_eq!(payloads, [] as [&[u8]; 0]),
                Some(payload) => assert_eq!(payloads, vec![payload]),
            }
            assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn connected_commands_disconnect_after_success() -> Result<()> {
        let baseline = content()?;
        let (_, height) = baseline.dimensions();
        let script = Script::default();
        let record = Arc::clone(&script.record);
        let report = session(script).print(baseline).await?;
        assert_eq!(report.image_rows, height);
        let expected = snapshot(&record)?;
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        let script = Script {
            fail_disconnect: true,
            ..Script::default()
        };
        let record = Arc::clone(&script.record);
        let err = session(script).print(content()?).await.err();
        assert!(
            matches!(err.as_ref().and_then(|error| error.downcast_ref::<Error>()), Some(Error::Transport(s)) if s == "disconnect failed")
        );
        assert_eq!(snapshot(&record)?, expected);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);

        let script = Script::default();
        let record = Arc::clone(&script.record);
        let output = session(script).query(QueryArg::Status).await?;
        assert_eq!(output, "State: ready");
        let expected = snapshot(&record)?;
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        let script = Script {
            fail_disconnect: true,
            ..Script::default()
        };
        let record = Arc::clone(&script.record);
        let err = session(script).query(QueryArg::Status).await.err();
        assert!(
            matches!(err.as_ref().and_then(|error| error.downcast_ref::<Error>()), Some(Error::Transport(s)) if s == "disconnect failed")
        );
        assert_eq!(snapshot(&record)?, expected);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        for retract in [false, true] {
            let script = Script::default();
            let record = Arc::clone(&script.record);
            let report = session(script).movement(1, retract).await?;
            assert_eq!(report.image_rows, 0);
            let expected = snapshot(&record)?;
            assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
            let script = Script {
                fail_disconnect: true,
                ..Script::default()
            };
            let record = Arc::clone(&script.record);
            let err = session(script).movement(1, retract).await.err();
            assert!(
                matches!(err.as_ref().and_then(|error| error.downcast_ref::<Error>()), Some(Error::Transport(s)) if s == "disconnect failed")
            );
            assert_eq!(snapshot(&record)?, expected);
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
        let error = session(script).query(QueryArg::Status).await.err();
        assert!(
            matches!(error.as_ref().and_then(|error| error.downcast_ref::<Error>()), Some(Error::Transport(s)) if s == "operation failed")
        );
        assert_eq!(record.writes.load(Ordering::SeqCst), 1);
        assert_eq!(record.disconnects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_print_disconnects_without_writing() -> Result<()> {
        let script = Script::default();
        let record = Arc::clone(&script.record);
        let printer = Printer::new(script, Gt01);
        printer.cancellation().cancel();
        let error = Session::new(printer, std::future::pending::<io::Result<()>>())
            .print(content()?)
            .await
            .err();
        assert!(matches!(
            error
                .as_ref()
                .and_then(|error| error.downcast_ref::<Error>()),
            Some(Error::UnusableConnection)
        ));
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
            let error = Session::new(Printer::new(script, Gt01), signal)
                .print(content()?)
                .await
                .err();
            if fail_signal {
                assert!(
                    error
                        .as_ref()
                        .and_then(|error| error.downcast_ref::<io::Error>())
                        .is_some_and(|error| error.kind() == io::ErrorKind::Other
                            && error.to_string() == "signal failed")
                );
            } else {
                assert!(matches!(
                    error
                        .as_ref()
                        .and_then(|error| error.downcast_ref::<Error>()),
                    Some(Error::Cancelled)
                ));
            }
            assert_eq!(record.writes.load(Ordering::SeqCst), 1);
            assert_eq!(record.finished_writes.load(Ordering::SeqCst), 1);
            assert_eq!(record.disconnects.load(Ordering::SeqCst), 1);
        }
        Ok(())
    }
}
