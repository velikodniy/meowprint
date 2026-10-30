//! Runtime timing and cooperative scheduling.

use core::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};

macro_rules! declare_clock {
    ([$($supertraits:tt)*] [$($future_bounds:tt)*]) => {
        /// Monotonic timing and cooperative scheduling for a printer connection.
        ///
        /// Applications using a custom executor supply this boundary with
        /// [`crate::Printer::with_clock`]. The clock must support deadlines at least
        /// 30 seconds ahead without wrapping. Native targets require `Send + Sync`
        /// and `Send` futures; WASM and `target_os = "none"` targets allow local
        /// clock values and futures.
        pub trait Clock $($supertraits)* {
            /// Return elapsed monotonic time from this clock's arbitrary, fixed origin.
            /// Deadlines passed to [`Self::sleep_until`] use the same origin, not wall time.
            fn now(&self) -> Duration;

            /// Wait until an absolute deadline, registering a wakeup without blocking.
            ///
            /// Expired deadlines complete immediately. Other sleeps must not complete
            /// early and must be safe to drop and recreate without losing timer progress.
            fn sleep_until(&self, deadline: Duration) -> impl Future<Output = ()> $($future_bounds)*;

            /// Give the runtime's input and timer producers a scheduling opportunity.
            ///
            /// This must be cancellation-safe and finish after that opportunity, without
            /// waiting for unrelated I/O. Browsers must yield to a browser task (for
            /// example `setTimeout(0)`), not merely a microtask or a self-wake.
            fn yield_now(&self) -> impl Future<Output = ()> $($future_bounds)*;
        }
    };
}

#[cfg(not(any(target_family = "wasm", target_os = "none")))]
declare_clock!([: Send + Sync] [+ Send]);
#[cfg(any(target_family = "wasm", target_os = "none"))]
declare_clock!([] []);

/// Tokio timing with a fixed monotonic origin.
///
/// Construction is possible outside a runtime. Waiting requires Tokio's time
/// driver; paused Tokio time is respected.
#[cfg(feature = "tokio")]
#[derive(Debug)]
pub struct TokioClock {
    origin: tokio::time::Instant,
}

#[cfg(feature = "tokio")]
impl Default for TokioClock {
    fn default() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

#[cfg(feature = "tokio")]
impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
    async fn sleep_until(&self, deadline: Duration) {
        tokio::time::sleep_until(self.origin + deadline).await;
    }
    async fn yield_now(&self) {
        tokio::task::yield_now().await;
    }
}

/// Work wins a ready/expired race. Zero-deadline probes never construct a timer.
pub async fn timeout_at<C: Clock, F: Future>(
    clock: &C,
    deadline: Duration,
    future: F,
) -> core::result::Result<F::Output, ()> {
    let mut future = pin!(future);
    let mut sleep = pin!(async { clock.sleep_until(deadline).await });
    poll_fn(|cx| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(output));
        }
        if clock.now() >= deadline || sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(()));
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    struct Expired(AtomicUsize);
    impl Clock for Expired {
        fn now(&self) -> Duration {
            Duration::from_secs(5)
        }
        fn sleep_until(&self, _: Duration) -> impl Future<Output = ()> + Send {
            self.0.fetch_add(1, Ordering::Relaxed);
            core::future::pending()
        }
        async fn yield_now(&self) {}
    }

    #[test]
    fn expired_probe_polls_work_without_constructing_sleep() {
        let clock = Expired(AtomicUsize::new(0));
        let waker = std::task::Waker::noop();
        let mut cx = core::task::Context::from_waker(waker);
        let mut ready = pin!(timeout_at(&clock, Duration::ZERO, core::future::ready(42)));
        assert_eq!(ready.as_mut().poll(&mut cx), Poll::Ready(Ok(42)));
        let mut pending = pin!(timeout_at(
            &clock,
            Duration::ZERO,
            core::future::pending::<()>()
        ));
        assert_eq!(pending.as_mut().poll(&mut cx), Poll::Ready(Err(())));
        assert_eq!(clock.0.load(Ordering::Relaxed), 0);
        assert_eq!(clock.now(), Duration::from_secs(5));
    }
}
