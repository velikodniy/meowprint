//! Private communication state and one operation's exclusive access to it.
use crate::driver::{
    Flow, Movement, Observation,
    framing::{Decoder, EncodedCommand, Frame},
};
use crate::{
    Cancellation, Driver, Error, PrinterCondition, PrinterState, PrinterStatus, Result, Transport,
    TransportEvent, WriteChannel,
};
use std::time::Duration;
use tokio::time::{Instant, timeout, timeout_at};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const PAUSE_TIMEOUT: Duration = Duration::from_secs(15);
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_TIME: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Ready,
    Disabled,
    Closed,
}

pub struct Session<T> {
    pub(crate) transport: T,
    decoder: Decoder,
    state: ConnectionState,
    paused_since: Option<Instant>,
    last_resume: Option<Instant>,
    last_ready_position: u64,
    expected_reply: Option<Request>,
    cancellation: Cancellation,
    prefixed_start_sent: bool,
    status: Option<PrinterStatus>,
}

#[derive(Clone, Copy)]
pub enum ReplyKind {
    Command(u8),
    Status,
}
impl ReplyKind {
    const fn matches(self, frame: &Frame, observation: &Observation) -> bool {
        match self {
            Self::Command(command) => frame.command == command,
            Self::Status => observation.status.is_some(),
        }
    }
}
pub struct Response {
    pub(crate) frame: Frame,
    status: Option<PrinterStatus>,
}
enum Request {
    Armed(ReplyKind),
    Waiting {
        kind: ReplyKind,
        after: u64,
        reply: Option<Response>,
    },
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Observe,
    Submit,
    Cancel,
}

/// Leaves the session disabled unless `finish` explicitly restores it.
/// No destructor needs to run asynchronous cleanup.
pub struct Operation<'a, T, D> {
    session: &'a mut Session<T>,
    driver: D,
    kind: OperationKind,
    write_attempted: bool,
    status_seen: bool,
}
impl<T: Transport> Session<T> {
    pub(crate) fn new(transport: T, decoder: Decoder) -> Self {
        Self {
            transport,
            decoder,
            state: ConnectionState::Ready,
            paused_since: None,
            last_resume: None,
            last_ready_position: 0,
            expected_reply: None,
            cancellation: Cancellation::new(),
            prefixed_start_sent: false,
            status: None,
        }
    }
    pub(crate) fn cancellation(&self) -> Cancellation {
        self.cancellation.clone()
    }
    pub(crate) fn is_usable(&self) -> bool {
        self.state == ConnectionState::Ready && !self.cancellation.is_cancelled()
    }
    pub(crate) fn ensure_usable(&self) -> Result<()> {
        if self.is_usable() {
            Ok(())
        } else {
            Err(Error::UnusableConnection)
        }
    }
    pub(crate) fn begin<D: Driver>(
        &mut self,
        driver: D,
        kind: OperationKind,
    ) -> Result<Operation<'_, T, D>> {
        self.ensure_usable()?;
        self.state = ConnectionState::Disabled;
        Ok(Operation {
            session: self,
            driver,
            kind,
            write_attempted: false,
            status_seen: false,
        })
    }
    pub(crate) async fn disconnect(&mut self) -> Result<()> {
        self.expected_reply = None;
        if self.state == ConnectionState::Closed {
            return Ok(());
        }
        self.state = ConnectionState::Disabled;
        timeout(WRITE_TIMEOUT, self.transport.disconnect())
            .await
            .map_err(|_| Error::Timeout("disconnect"))??;
        self.state = ConnectionState::Closed;
        Ok(())
    }
}
impl<T: Transport, D: Driver> Operation<'_, T, D> {
    pub(crate) async fn finish<R>(mut self, result: Result<R>) -> Result<R> {
        self.session.expected_reply = None;
        let safe_rejection =
            !self.write_attempted && matches!(result, Err(Error::PrinterUnavailable(_)));
        if self.kind != OperationKind::Cancel
            && !self.session.cancellation.is_cancelled()
            && (result.is_ok() || safe_rejection)
        {
            self.session.state = ConnectionState::Ready;
        } else {
            if matches!(result, Err(Error::Cancelled)) {
                let _ = self.send_cancel().await;
            }
            let cleanup = self.session.disconnect().await;
            if result.is_ok() {
                cleanup?;
            }
        }
        result
    }
    pub(crate) async fn cancel(&mut self) -> Result<()> {
        self.send_cancel().await
    }
    fn cancelled(&self) -> Result<()> {
        if self.session.cancellation.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    async fn send_cancel(&mut self) -> Result<()> {
        let Some(command) = self.driver.cancel_command()? else {
            return Ok(());
        };
        let bytes = command.bytes;
        let size = self.chunk_size()?;
        for chunk in bytes.chunks(size) {
            self.write_attempted = true;
            timeout(
                WRITE_TIMEOUT,
                self.session.transport.write(WriteChannel::Control, chunk),
            )
            .await
            .map_err(|_| Error::Timeout("cancel write"))??;
        }
        Ok(())
    }
    fn handle(&mut self, event: TransportEvent) -> Result<()> {
        let TransportEvent::Notification(bytes) = event else {
            return Err(Error::Disconnected);
        };
        let mut failure = None;
        for (position, frame) in self.session.decoder.feed_positioned(&bytes) {
            let observation = match self.driver.observe(&frame) {
                Ok(observation) => observation,
                Err(error) => {
                    // A malformed observation cannot become a reusable preflight
                    // rejection merely because a device condition preceded it.
                    failure = Some(error);
                    continue;
                }
            };
            if let Some(status) = &observation.status {
                self.session.status = Some(status.clone());
                self.status_seen = true;
                if self.kind == OperationKind::Submit && status.state.blocks_submission() {
                    failure.get_or_insert_with(|| Error::PrinterUnavailable(status.state.clone()));
                }
            }
            match observation.flow {
                Some(Flow::Pause) => {
                    self.session.paused_since.get_or_insert_with(Instant::now);
                }
                Some(Flow::Resume) => {
                    if self.session.paused_since.take().is_some() {
                        self.session.last_resume = Some(Instant::now());
                    }
                    self.session.last_ready_position = position;
                }
                None => {}
            }
            if let Some(Request::Waiting { kind, after, reply }) = &mut self.session.expected_reply
                && reply.is_none()
                && position > *after
                && observation.reply
                && kind.matches(&frame, &observation)
            {
                *reply = Some(Response {
                    frame,
                    status: observation.status,
                });
            }
        }
        // Finish interpreting this notification before returning a fault. A later
        // ready frame must not erase a fault, or leave the input half processed.
        failure.map_or(Ok(()), Err)
    }
    async fn event_until(&mut self, deadline: Instant) -> Result<Option<TransportEvent>> {
        tokio::select! {
            biased;
            () = self.session.cancellation.cancelled() => Err(Error::Cancelled),
            result = timeout_at(deadline, self.session.transport.event()) => result.map_or(Ok(None), |event| event.map(Some))
        }
    }
    async fn listen_until(&mut self, deadline: Instant) -> Result<()> {
        loop {
            match self.event_until(deadline).await? {
                Some(event) => self.handle(event)?,
                None => return Ok(()),
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }
    async fn drain_pending(&mut self) -> Result<()> {
        for _ in 0..256 {
            self.cancelled()?;
            match timeout_at(Instant::now(), self.session.transport.event()).await {
                Ok(event) => self.handle(event?)?,
                Err(_) => return Ok(()),
            }
        }
        Err(Error::Protocol(
            "The printer flooded the connection with notifications.",
        ))
    }
    pub(crate) async fn wait_ready(&mut self) -> Result<()> {
        loop {
            self.drain_pending().await?;
            let Some(since) = self.session.paused_since else {
                return Ok(());
            };
            if self.kind == OperationKind::Observe {
                return Err(Error::PrinterUnavailable(self.paused_status().state));
            }
            let deadline = since + PAUSE_TIMEOUT;
            let event = self
                .event_until(deadline)
                .await?
                .ok_or(Error::Timeout("printer pause"))?;
            self.handle(event)?;
            if self.session.paused_since.is_some() && Instant::now() >= deadline {
                return Err(Error::Timeout("printer pause"));
            }
        }
    }
    fn chunk_size(&self) -> Result<usize> {
        let size = self
            .session
            .transport
            .max_write_size()
            .min(self.driver.settings().chunk_size);
        if size == 0 {
            return Err(Error::Transport(
                "The transport reports a zero write limit.".into(),
            ));
        }
        Ok(size)
    }
    pub(crate) async fn send_command(
        &mut self,
        command: &EncodedCommand,
        submitted: &mut usize,
    ) -> Result<u64> {
        let prefixed;
        let bytes = if let Some((trigger, prefix)) = self.driver.settings().prefix
            && !self.session.prefixed_start_sent
            && command.command == trigger
        {
            let mut bytes = Vec::with_capacity(command.bytes.len() + prefix.len());
            bytes.extend_from_slice(prefix);
            bytes.extend_from_slice(&command.bytes);
            prefixed = bytes;
            &prefixed
        } else {
            &command.bytes
        };
        let result = self.send(WriteChannel::Control, bytes, submitted).await;
        if result.is_ok()
            && self
                .driver
                .settings()
                .prefix
                .is_some_and(|(trigger, _)| trigger == command.command)
        {
            self.session.prefixed_start_sent = true;
        }
        result
    }
    pub(crate) async fn send(
        &mut self,
        channel: WriteChannel,
        bytes: &[u8],
        submitted: &mut usize,
    ) -> Result<u64> {
        let size = self.chunk_size()?;
        self.wait_ready().await?;
        let mut marker = self.session.decoder.position();
        let chunks = bytes.chunks(size);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            // Some protocols must finish a control frame before sending a cancellation command.
            let finishing_control = self.driver.settings().finish_control_frame
                && channel == WriteChannel::Control
                && index != 0;
            if !finishing_control {
                self.wait_ready().await?;
            }
            marker = self.session.decoder.position();
            if channel == WriteChannel::Control
                && index + 1 == count
                && let Some(Request::Armed(kind)) = self.session.expected_reply
            {
                self.session.expected_reply = Some(Request::Waiting {
                    kind,
                    after: marker,
                    reply: None,
                });
            }
            self.write_attempted = true;
            timeout(WRITE_TIMEOUT, self.session.transport.write(channel, chunk))
                .await
                .map_err(|_| Error::Timeout("transport write"))??;
            *submitted += chunk.len();
            if self.driver.settings().finish_control_frame
                && channel == WriteChannel::Control
                && index + 1 < count
            {
                // Finish this control frame before observing cooperative cancellation.
                let deadline = Instant::now() + self.driver.settings().pacing(size);
                while let Ok(event) = timeout_at(deadline, self.session.transport.event()).await {
                    self.handle(event?)?;
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                continue;
            }
            self.cancelled()?;
            self.listen_until(Instant::now() + self.driver.settings().pacing(size))
                .await?;
        }
        Ok(marker)
    }
    pub(crate) async fn exchange(
        &mut self,
        command: &EncodedCommand,
        kind: ReplyKind,
        duration: Duration,
        submitted: &mut usize,
    ) -> Result<Response> {
        self.wait_ready().await?;
        self.session.expected_reply = Some(Request::Armed(kind));
        self.send_command(command, submitted).await?;
        let deadline = Instant::now() + duration;
        loop {
            self.cancelled()?;
            if let Some(Request::Waiting { reply, .. }) = &mut self.session.expected_reply
                && let Some(response) = reply.take()
            {
                self.session.expected_reply = None;
                return Ok(response);
            }
            let event = self
                .event_until(deadline)
                .await?
                .ok_or(Error::Timeout("printer reply"))?;
            self.handle(event)?;
            if Instant::now() >= deadline {
                return Err(Error::Timeout("printer reply"));
            }
        }
    }
    pub(crate) async fn status(&mut self, command: &EncodedCommand) -> Result<PrinterStatus> {
        self.drain_pending().await?;
        if self.session.paused_since.is_some() {
            return Ok(self.paused_status());
        }
        if self.status_seen
            && let Some(status) = &self.session.status
        {
            return Ok(status.clone());
        }
        self.request_status(command, &mut 0).await
    }
    pub(crate) async fn request_status(
        &mut self,
        command: &EncodedCommand,
        submitted: &mut usize,
    ) -> Result<PrinterStatus> {
        self.exchange(command, ReplyKind::Status, REPLY_TIMEOUT, submitted)
            .await?
            .status
            .ok_or(Error::InvalidReply("printer status"))
    }
    fn paused_status(&self) -> PrinterStatus {
        let mut status = self.session.status.clone().unwrap_or(PrinterStatus {
            state: PrinterState::Unknown,
            battery_percent: None,
            temperature_celsius: None,
        });
        if matches!(status.state, PrinterState::Ready | PrinterState::Unknown) {
            status.state = PrinterState::Conditions(vec![PrinterCondition::Paused]);
        }
        status
    }
    pub const fn ready_after(&self, marker: u64) -> bool {
        self.session.last_ready_position > marker
    }
    async fn drain(&mut self) -> Result<()> {
        let start = Instant::now();
        loop {
            let deadline = self.session.last_resume.unwrap_or(start).max(start) + DRAIN_TIME;
            self.listen_until(deadline).await?;
            self.wait_ready().await?;
            if Instant::now() >= self.session.last_resume.unwrap_or(start).max(start) + DRAIN_TIME {
                return Ok(());
            }
        }
    }
    pub(crate) async fn bounded_drain(&mut self) -> Result<()> {
        timeout(COMPLETION_TIMEOUT, self.drain())
            .await
            .map_err(|_| Error::Timeout("completion wait"))?
    }
    pub(crate) async fn send_movement(
        &mut self,
        movement: &Movement,
        submitted: &mut usize,
    ) -> Result<()> {
        match movement {
            Movement::Commands(commands) => {
                self.send_commands(commands, submitted).await?;
            }
            Movement::BlankRows { frame, count } => {
                for _ in 0..*count {
                    self.send(WriteChannel::Control, frame, submitted).await?;
                }
            }
        }
        Ok(())
    }
    pub(crate) async fn send_commands(
        &mut self,
        commands: &[EncodedCommand],
        submitted: &mut usize,
    ) -> Result<u64> {
        let mut marker = self.session.decoder.position();
        for command in commands {
            marker = self.send_command(command, submitted).await?;
        }
        Ok(marker)
    }
}

#[cfg(test)]
mod tests {
    //! Failure-oriented tests through the public printer operations.
    use crate::{
        Error, Gt01, PixelFormat, Printer, PrinterCondition, PrinterState, Result, Transport,
        TransportEvent, WriteChannel,
    };
    use async_trait::async_trait;
    use std::{collections::VecDeque, time::Duration};

    const A3_PAPER: &[u8] = &[0x51, 0x78, 0xa3, 1, 1, 0, 1, 7, 0xff];
    const AE_PAPER: &[u8] = &[0x51, 0x78, 0xae, 1, 1, 0, 1, 7, 0xff];
    const AE_PAUSE: &[u8] = &[0x51, 0x78, 0xae, 1, 1, 0, 0x10, 0x70, 0xff];

    #[tokio::test(start_paused = true)]
    async fn recognized_a3_fault_stops_printing() -> Result<()> {
        let mut transport = Script::default();
        transport.reply(1, A3_PAPER);
        let mut printer = Printer::new(transport, Gt01);
        let result = printer
            .prepare(
                vec![0xff; 48],
                PixelFormat::Mono,
                &crate::Gt01Options::default(),
            )?
            .print()
            .await;
        assert!(
            result.is_err(),
            "A recognized paper fault must stop submission"
        );
        assert_eq!(printer.session.transport.writes.len(), 1);
        assert!(!printer.is_usable());
        Ok(())
    }
    #[tokio::test(start_paused = true)]
    async fn status_returns_queued_conditions_without_io() -> Result<()> {
        for (bytes, condition) in [
            (AE_PAPER, PrinterCondition::OutOfPaper),
            (AE_PAUSE, PrinterCondition::Paused),
        ] {
            let mut transport = Script::default();
            transport.notify(Duration::ZERO, bytes);
            let mut printer = Printer::new(transport, Gt01);
            let started = tokio::time::Instant::now();
            assert_eq!(
                printer.status().await?.state,
                PrinterState::Conditions(vec![condition])
            );
            assert_eq!(tokio::time::Instant::now(), started);
            assert!(printer.is_usable());
            assert!(printer.session.transport.writes.is_empty());
            assert_eq!(printer.session.transport.disconnects, 0);
        }
        Ok(())
    }

    const A3_READY: &[u8] = &[0x51, 0x78, 0xa3, 1, 1, 0, 0, 0, 0xff];
    const AE_READY: &[u8] = &[0x51, 0x78, 0xae, 1, 1, 0, 0, 0, 0xff];

    fn common(command: u8, payload: &[u8]) -> Result<Vec<u8>> {
        crate::driver::framing::encode([0x51, 0x78], command, 1, payload, 255)
    }
    fn mxw(command: u8, payload: &[u8], crc: bool) -> Result<Vec<u8>> {
        let mut frame = crate::driver::framing::encode([0x22, 0x21], command, 3, payload, 255)?;
        if !crc {
            frame.remove(frame.len() - 2);
        }
        Ok(frame)
    }
    async fn print(printer: &mut Printer<Script, Gt01>) -> Result<crate::PrintReport> {
        printer
            .prepare(
                vec![0x81; 48],
                PixelFormat::Mono,
                &crate::Gt01Options::default(),
            )?
            .print()
            .await
    }

    #[tokio::test(start_paused = true)]
    async fn faults_are_semantic_and_later_ready_cannot_erase_them() -> Result<()> {
        for command in [0xa3, 0xae] {
            for flags in [1, 2, 4, 8, 0x20, 0x40, 0x13] {
                let fault = common(command, &[flags])?;
                let mut transport = Script::default();
                transport.reply(1, &[fault, AE_READY.to_vec()].concat());
                let mut printer = Printer::new(transport, Gt01);
                assert!(matches!(
                    print(&mut printer).await,
                    Err(Error::PrinterUnavailable(PrinterState::Conditions(_)))
                ));
                assert_eq!(printer.session.transport.attempts, 1);
                assert_eq!(printer.session.transport.disconnects, 1);
                assert!(!printer.is_usable());
            }
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn known_prewrite_fault_is_safe_for_print_and_paper_movement() -> Result<()> {
        for movement in [false, true] {
            let mut transport = Script::default();
            transport.notify(Duration::ZERO, &[AE_PAPER, AE_READY].concat());
            let mut printer = Printer::new(transport, Gt01);
            let result = if movement {
                printer.feed(10).await
            } else {
                print(&mut printer).await
            };
            assert!(matches!(result, Err(Error::PrinterUnavailable(_))));
            assert!(printer.is_usable());
            assert_eq!(printer.session.transport.attempts, 0);
            assert_eq!(printer.session.transport.disconnects, 0);
            // The complete notification was consumed, including its final ready state.
            print(&mut printer).await?;
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn status_snapshots_refresh_and_paused_reads_preserve_connection() -> Result<()> {
        let mut transport = Script::default();
        transport.notify(Duration::ZERO, AE_PAUSE);
        let mut printer = Printer::new(transport, Gt01);
        let paused = printer.status().await?;
        assert_eq!(printer.status().await?, paused);
        assert!(matches!(
            printer.device_info().await,
            Err(Error::PrinterUnavailable(_))
        ));
        assert!(printer.is_usable());
        assert_eq!(printer.session.transport.attempts, 0);
        printer.session.transport.notify(Duration::ZERO, AE_READY);
        assert_eq!(printer.status().await?.state, PrinterState::Ready);
        printer.session.transport.reply(1, A3_PAPER);
        assert_eq!(
            printer.status().await?.state,
            PrinterState::Conditions(vec![PrinterCondition::OutOfPaper])
        );
        printer.session.transport.reply(2, A3_READY);
        assert_eq!(printer.status().await?.state, PrinterState::Ready);
        assert_eq!(printer.session.transport.attempts, 2);
        assert!(printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn fresh_flow_status_can_answer_a_status_request() -> Result<()> {
        let mut transport = Script::default();
        transport.reply(1, AE_PAPER);
        let mut printer = Printer::new(transport, Gt01);
        assert_eq!(
            printer.status().await?.state,
            PrinterState::Conditions(vec![PrinterCondition::OutOfPaper])
        );
        assert!(printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn device_information_survives_unrelated_paper_conditions() -> Result<()> {
        let mut transport = Script::default();
        transport.notify(Duration::ZERO, AE_PAPER);
        transport.reply(1, &[A3_PAPER.to_vec(), common(0xa8, b"firmware")?].concat());
        let mut printer = Printer::new(transport, Gt01);
        assert_eq!(
            printer.device_info().await?.description.as_deref(),
            Some("firmware")
        );
        assert!(printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_status_is_never_ready_and_malformed_status_disables() -> Result<()> {
        let mut transport = Script::default();
        transport.reply(1, &common(0xa3, &[0, 0])?);
        transport.reply(2, &common(0xa3, &[])?);
        let mut printer = Printer::new(transport, Gt01);
        assert_eq!(printer.status().await?.state, PrinterState::Unknown);
        assert!(printer.is_usable());
        assert!(matches!(
            printer.status().await,
            Err(Error::InvalidReply(_))
        ));
        assert!(!printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_pause_does_not_extend_deadline() -> Result<()> {
        for flags in [0x10, 0x80] {
            let bytes = common(0xae, &[flags])?;
            let mut transport = Script::default();
            transport.notify(Duration::ZERO, &bytes);
            transport.notify(Duration::from_secs(14), &bytes);
            let mut printer = Printer::new(transport, Gt01);
            let start = tokio::time::Instant::now();
            assert!(matches!(
                print(&mut printer).await,
                Err(Error::Timeout("printer pause"))
            ));
            assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(15));
            assert_eq!(printer.session.transport.attempts, 0);
            assert!(!printer.is_usable());
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn resume_allows_submission_and_restarts_completion_drain() -> Result<()> {
        let mut printer = Printer::new(Script::default(), Gt01);
        print(&mut printer).await?;
        let last = printer.session.transport.attempts;
        let mut transport = Script::default();
        transport.notify(Duration::ZERO, AE_PAUSE);
        transport.notify(Duration::from_secs(1), AE_READY);
        transport.responses.insert(
            last,
            vec![
                (
                    Duration::from_secs(1),
                    crate::TransportEvent::Notification(AE_PAUSE.to_vec()),
                ),
                (
                    Duration::from_secs(2),
                    crate::TransportEvent::Notification(AE_READY.to_vec()),
                ),
            ],
        );
        let mut printer = Printer::new(transport, Gt01);
        let start = tokio::time::Instant::now();
        assert_eq!(
            print(&mut printer).await?.completion,
            crate::Completion::ReadyAfterEnd
        );
        assert!(tokio::time::Instant::now() - start >= Duration::from_secs(6));
        assert!(printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn completion_requires_eligible_ae_evidence() -> Result<()> {
        let mut baseline = Printer::new(Script::default(), Gt01);
        assert_eq!(
            print(&mut baseline).await?.completion,
            crate::Completion::TimedDrain
        );
        let last = baseline.session.transport.attempts;
        for (write, reply, expected) in [
            (1, AE_READY, crate::Completion::TimedDrain),
            (last, A3_READY, crate::Completion::TimedDrain),
            (last, AE_READY, crate::Completion::ReadyAfterEnd),
        ] {
            let mut transport = Script::default();
            transport.reply(write, reply);
            let mut printer = Printer::new(transport, Gt01);
            assert_eq!(print(&mut printer).await?.completion, expected);
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn fragmented_old_replies_cannot_satisfy_a_new_request() -> Result<()> {
        let old = common(0xa8, b"old")?;
        let fresh = common(0xa8, b"fresh")?;
        for split in 1..=old.len() {
            let mut transport = Script::default();
            transport.notify(Duration::ZERO, &old[..split]);
            transport.reply(1, &[old[split..].to_vec(), fresh.clone()].concat());
            let mut printer = Printer::new(transport, Gt01);
            assert_eq!(
                printer.device_info().await?.description.as_deref(),
                Some("fresh")
            );
        }
        let mut transport = Script::default();
        transport.notify(Duration::ZERO, &old[..1]);
        transport.reply(1, &old[1..]);
        let mut printer = Printer::new(transport, Gt01);
        assert!(matches!(
            printer.device_info().await,
            Err(Error::Timeout("printer reply"))
        ));
        assert!(!printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn replies_before_final_request_chunk_are_ineligible() -> Result<()> {
        for with_fresh in [false, true] {
            let mut transport = Script {
                limit: 1,
                ..Script::default()
            };
            transport.reply(1, &common(0xa8, b"early")?);
            if with_fresh {
                transport.reply(9, &common(0xa8, b"fresh")?);
            }
            let mut printer = Printer::new(transport, Gt01);
            let result = printer.device_info().await;
            if with_fresh {
                assert_eq!(result?.description.as_deref(), Some("fresh"));
            } else {
                assert!(matches!(result, Err(Error::Timeout("printer reply"))));
            }
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn decoder_retains_a_partial_frame_across_operations() -> Result<()> {
        let info = common(0xa8, b"old")?;
        let mut transport = Script::default();
        transport.notify(Duration::ZERO, &[AE_READY, &info[..1]].concat());
        let mut printer = Printer::new(transport, Gt01);
        assert_eq!(printer.status().await?.state, PrinterState::Ready);
        printer
            .session
            .transport
            .reply(1, &[info[1..].to_vec(), common(0xa8, b"new")?].concat());
        assert_eq!(
            printer.device_info().await?.description.as_deref(),
            Some("new")
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn mxw01_state_and_start_rejection_have_distinct_errors() -> Result<()> {
        for (state, error, start_code) in [
            (1, None, 0),
            (0, Some(1), 0),
            (0, Some(4), 0),
            (0, Some(8), 0),
            (2, None, 0),
            (0, None, 1),
        ] {
            let mut status = vec![0; 14];
            status[6] = state;
            if let Some(code) = error {
                status[12] = 1;
                status[13] = code;
            }
            let mut transport = Script::default();
            transport.reply(2, &mxw(0xa1, &status, true)?);
            transport.reply(3, &mxw(0xa9, &[start_code], true)?);
            let mut printer = Printer::new(transport, crate::Mxw01::default());
            let result = printer
                .prepare(
                    vec![0; 48],
                    PixelFormat::Mono,
                    &crate::Mxw01Options::default(),
                )?
                .print()
                .await;
            if start_code != 0 {
                assert!(matches!(result, Err(Error::StartRejected { code: 1 })));
            } else if state == 1 {
                assert!(matches!(
                    result,
                    Err(Error::PrinterUnavailable(PrinterState::Printing))
                ));
            } else if state == 2 {
                assert!(matches!(
                    result,
                    Err(Error::PrinterUnavailable(PrinterState::Unknown))
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(Error::PrinterUnavailable(PrinterState::Conditions(_)))
                ));
            }
            assert!(!printer.is_usable());
            assert!(
                printer
                    .session
                    .transport
                    .writes
                    .iter()
                    .all(|(channel, _)| *channel == crate::WriteChannel::Control)
            );
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn mxw01_requires_completion_after_flush_in_both_reply_formats() -> Result<()> {
        for crc in [false, true] {
            for fresh in [false, true] {
                let mut transport = Script::default();
                transport.reply(2, &mxw(0xa1, &[0; 13], crc)?);
                transport.reply(3, &mxw(0xa9, &[0], crc)?);
                transport.reply(if fresh { 40 } else { 39 }, &mxw(0xaa, &[0], crc)?);
                let driver = crate::Mxw01 {
                    reply_format: if crc {
                        crate::Mxw01ReplyFormat::WithCrc
                    } else {
                        crate::Mxw01ReplyFormat::WithoutCrc
                    },
                };
                let mut printer = Printer::new(transport, driver);
                let result = printer
                    .prepare(
                        vec![0; 48],
                        PixelFormat::Mono,
                        &crate::Mxw01Options::default(),
                    )?
                    .print()
                    .await;
                if fresh {
                    assert_eq!(result?.completion, crate::Completion::PrinterComplete);
                } else {
                    assert!(matches!(result, Err(Error::Timeout("printer reply"))));
                }
            }
        }
        Ok(())
    }

    async fn poll_once<F: Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
        let mut future = future;
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
    }
    #[tokio::test(start_paused = true)]
    async fn dropping_polled_operations_disables_until_explicit_cleanup() -> Result<()> {
        let mut printer = Printer::new(
            Script {
                block_at: Some(1),
                ..Script::default()
            },
            Gt01,
        );
        drop(printer.status());
        assert!(printer.is_usable());
        let mut future = Box::pin(printer.status());
        assert!(poll_once(future.as_mut()).await.is_pending());
        drop(future);
        assert!(!printer.is_usable());
        assert_eq!(printer.session.transport.disconnects, 0);
        printer.disconnect().await?;
        printer.disconnect().await?;
        assert_eq!(printer.session.transport.disconnects, 1);
        let mut printer = Printer::new(
            Script {
                block_at: Some(1),
                ..Script::default()
            },
            Gt01,
        );
        let mut future = Box::pin(print(&mut printer));
        assert!(poll_once(future.as_mut()).await.is_pending());
        drop(future);
        assert!(!printer.is_usable());
        printer.disconnect().await?;
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn failures_and_cleanup_preserve_the_primary_error_without_replay() -> Result<()> {
        for attempt in [1, 2, 5] {
            for cleanup_fails in [false, true] {
                let transport = Script {
                    fail_at: Some(attempt),
                    fail_disconnect: cleanup_fails,
                    limit: 3,
                    ..Script::default()
                };
                let mut printer = Printer::new(transport, Gt01);
                assert!(
                    matches!(print(&mut printer).await, Err(Error::Transport(message)) if message == "injected write failure")
                );
                assert_eq!(printer.session.transport.attempts, attempt);
                assert_eq!(printer.session.transport.writes.len(), attempt - 1);
                assert_eq!(printer.session.transport.disconnects, 1);
                assert!(!printer.is_usable());
                assert!(matches!(
                    print(&mut printer).await,
                    Err(Error::UnusableConnection)
                ));
                printer.session.transport.fail_disconnect = false;
                printer.disconnect().await?;
                assert_eq!(
                    printer.session.transport.disconnects,
                    if cleanup_fails { 2 } else { 1 }
                );
            }
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_writes_and_disconnects_are_bounded() -> Result<()> {
        let mut printer = Printer::new(
            Script {
                block_at: Some(1),
                block_disconnect: true,
                ..Script::default()
            },
            Gt01,
        );
        let start = tokio::time::Instant::now();
        assert!(matches!(
            print(&mut printer).await,
            Err(Error::Timeout("transport write"))
        ));
        assert_eq!(tokio::time::Instant::now() - start, Duration::from_secs(10));
        assert_eq!(printer.session.transport.attempts, 1);
        assert!(!printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_write_stops_further_common_submission() -> Result<()> {
        let mut printer = Printer::new(Script::default(), Gt01);
        printer.session.transport.cancel_at = Some((1, printer.cancellation()));
        assert!(matches!(print(&mut printer).await, Err(Error::Cancelled)));
        assert_eq!(printer.session.transport.writes.len(), 1);
        assert_eq!(printer.session.transport.disconnects, 1);
        assert!(!printer.is_usable());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn mxw01_finishes_control_frame_before_sending_cancellation() -> Result<()> {
        let mut printer = Printer::new(
            Script {
                limit: 3,
                ..Script::default()
            },
            crate::Mxw01::default(),
        );
        printer.session.transport.cancel_at = Some((1, printer.cancellation()));
        assert!(matches!(
            printer
                .prepare(
                    vec![0; 48],
                    PixelFormat::Mono,
                    &crate::Mxw01Options::default()
                )?
                .print()
                .await,
            Err(Error::Cancelled)
        ));
        let bytes: Vec<_> = printer
            .session
            .transport
            .writes
            .iter()
            .flat_map(|(_, bytes)| bytes.iter().copied())
            .collect();
        let intensity = crate::driver::framing::encode([0x22, 0x21], 0xa2, 0, &[0x5d], 255)?;
        let cancel = crate::driver::framing::encode([0x22, 0x21], 0xac, 0, &[0], 255)?;
        assert_eq!(bytes, [intensity, cancel].concat());
        assert_eq!(printer.session.transport.disconnects, 1);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn transport_limits_floods_and_disconnects_are_not_input_errors() -> Result<()> {
        let mut printer = Printer::new(
            Script {
                limit: 0,
                ..Script::default()
            },
            Gt01,
        );
        assert!(matches!(
            print(&mut printer).await,
            Err(Error::Transport(_))
        ));
        let mut transport = Script::default();
        for _ in 0..256 {
            transport.notify(Duration::ZERO, AE_READY);
        }
        let mut printer = Printer::new(transport, Gt01);
        assert!(matches!(printer.status().await, Err(Error::Protocol(_))));
        assert_eq!(printer.session.transport.attempts, 0);
        let mut transport = Script::default();
        transport.events.push_back((
            tokio::time::Instant::now(),
            crate::TransportEvent::Disconnected,
        ));
        let mut printer = Printer::new(transport, Gt01);
        assert!(matches!(printer.status().await, Err(Error::Disconnected)));
        assert_eq!(printer.session.transport.attempts, 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_waits_for_an_in_flight_write_to_finish() -> Result<()> {
        let delay = Duration::from_millis(100);
        let mut printer = Printer::new(
            Script {
                write_delay: delay,
                ..Script::default()
            },
            Gt01,
        );
        printer.session.transport.cancel_at = Some((1, printer.cancellation()));
        let start = tokio::time::Instant::now();
        assert!(matches!(print(&mut printer).await, Err(Error::Cancelled)));
        assert_eq!(printer.session.transport.writes.len(), 1);
        assert_eq!(tokio::time::Instant::now() - start, delay);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_interrupts_pause_completion_and_query_waits() -> Result<()> {
        for phase in 0..3 {
            let mut transport = Script::default();
            if phase == 0 {
                transport.notify(Duration::ZERO, AE_PAUSE);
            }
            let mut printer = Printer::new(transport, Gt01);
            let cancellation = printer.cancellation();
            let stop = async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                cancellation.cancel();
            };
            let operation = async {
                if phase == 2 {
                    printer.device_info().await.map(|_| ())
                } else {
                    print(&mut printer).await.map(|_| ())
                }
            };
            let (result, ()) = tokio::join!(operation, stop);
            assert!(matches!(result, Err(Error::Cancelled)));
            assert!(!printer.is_usable());
            assert_eq!(printer.session.transport.disconnects, 1);
            if phase == 0 {
                assert_eq!(printer.session.transport.attempts, 0);
            }
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn prefix_is_not_lost_when_rejected_before_its_first_write() -> Result<()> {
        use crate::driver::{Operations, framing::EncodedCommand};
        use crate::session::{OperationKind, Session};
        let mut transport = Script::default();
        transport.notify(Duration::ZERO, AE_PAPER);
        let driver = crate::PrefixedTiny;
        let mut session = Session::new(transport, driver.decoder());
        let command = EncodedCommand {
            command: 0xa3,
            bytes: crate::driver::framing::encode([0x51, 0x78], 0xa3, 0, &[0], 255)?,
        };
        let mut operation = session.begin(driver, OperationKind::Submit)?;
        let result = operation.send_command(&command, &mut 0).await;
        assert!(matches!(
            operation.finish(result).await,
            Err(Error::PrinterUnavailable(_))
        ));
        let mut operation = session.begin(driver, OperationKind::Submit)?;
        let result = operation.send_command(&command, &mut 0).await;
        operation.finish(result).await?;
        assert_eq!(session.transport.writes[0].1[0], 0x12);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_explicit_disconnect_keeps_the_session_disabled() -> Result<()> {
        let mut printer = Printer::new(
            Script {
                block_disconnect: true,
                ..Script::default()
            },
            Gt01,
        );
        let mut future = Box::pin(printer.disconnect());
        assert!(poll_once(future.as_mut()).await.is_pending());
        drop(future);
        assert!(!printer.is_usable());
        printer.session.transport.block_disconnect = false;
        printer.disconnect().await?;
        assert_eq!(printer.session.transport.disconnects, 2);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_status_after_a_prewrite_fault_cannot_restore_reuse() -> Result<()> {
        let mut transport = Script::default();
        transport.notify(
            Duration::ZERO,
            &[A3_PAPER.to_vec(), common(0xa3, &[])?].concat(),
        );
        let mut printer = Printer::new(transport, Gt01);
        assert!(matches!(
            print(&mut printer).await,
            Err(Error::InvalidReply(_))
        ));
        assert_eq!(printer.session.transport.attempts, 0);
        assert!(!printer.is_usable());
        Ok(())
    }

    /// Scheduled notifications and injected transport failures, without protocol emulation.
    struct Script {
        writes: Vec<(WriteChannel, Vec<u8>)>,
        attempts: usize,
        disconnects: usize,
        limit: usize,
        fail_at: Option<usize>,
        block_at: Option<usize>,
        write_delay: std::time::Duration,
        fail_disconnect: bool,
        block_disconnect: bool,
        cancel_at: Option<(usize, crate::Cancellation)>,
        responses: std::collections::BTreeMap<usize, Vec<(std::time::Duration, TransportEvent)>>,
        events: VecDeque<(tokio::time::Instant, TransportEvent)>,
    }
    impl Default for Script {
        fn default() -> Self {
            Self {
                writes: Vec::new(),
                attempts: 0,
                disconnects: 0,
                limit: 120,
                fail_at: None,
                block_at: None,
                write_delay: std::time::Duration::ZERO,
                fail_disconnect: false,
                block_disconnect: false,
                cancel_at: None,
                responses: std::collections::BTreeMap::default(),
                events: VecDeque::new(),
            }
        }
    }
    impl Script {
        fn notify(&mut self, delay: std::time::Duration, bytes: &[u8]) {
            self.events.push_back((
                tokio::time::Instant::now() + delay,
                TransportEvent::Notification(bytes.to_vec()),
            ));
        }
        fn reply(&mut self, write: usize, bytes: &[u8]) {
            self.responses.entry(write).or_default().push((
                std::time::Duration::ZERO,
                TransportEvent::Notification(bytes.to_vec()),
            ));
        }
    }
    #[async_trait]
    impl Transport for Script {
        fn max_write_size(&self) -> usize {
            self.limit
        }
        async fn write(&mut self, channel: WriteChannel, bytes: &[u8]) -> Result<()> {
            self.attempts += 1;
            if let Some((at, cancellation)) = &self.cancel_at
                && *at == self.attempts
            {
                cancellation.cancel();
            }
            if self.block_at == Some(self.attempts) {
                std::future::pending::<()>().await;
            }
            if self.fail_at == Some(self.attempts) {
                return Err(Error::Transport("injected write failure".into()));
            }
            if !self.write_delay.is_zero() {
                tokio::time::sleep(self.write_delay).await;
            }
            self.writes.push((channel, bytes.to_vec()));
            if let Some(events) = self.responses.remove(&self.attempts) {
                for (delay, event) in events {
                    self.events
                        .push_back((tokio::time::Instant::now() + delay, event));
                }
            }
            Ok(())
        }
        async fn event(&mut self) -> Result<TransportEvent> {
            if let Some((at, _)) = self.events.front() {
                tokio::time::sleep_until(*at).await;
                if let Some((_, event)) = self.events.pop_front() {
                    return Ok(event);
                }
            }
            std::future::pending().await
        }
        async fn disconnect(&mut self) -> Result<()> {
            self.disconnects += 1;
            if self.block_disconnect {
                std::future::pending::<()>().await;
            }
            if self.fail_disconnect {
                return Err(Error::Transport("injected cleanup failure".into()));
            }
            Ok(())
        }
    }
}
