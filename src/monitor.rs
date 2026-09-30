//! Passive, asynchronous observation of dependency or application health.
//!
//! This module never injects faults or reconnects an application. A failed HTTP
//! probe describes that endpoint as seen by this process; it does **not** prove
//! that the host, the network, or the Internet is disconnected. Applications can
//! react to the typed events using their own recovery policy.
//!
//! [`Monitor::start`] returns an already-subscribed, bounded broadcast receiver,
//! so its initial event cannot be lost to a start/subscribe race. A slow receiver
//! gets [`tokio::sync::broadcast::error::RecvError::Lagged`], not an unbounded
//! backlog. Additional [`Monitor::subscribe`] receivers see future events only.
//! After lag, consumers must not assume they saw every transition. Consult
//! [`Monitor::current_status`] for the latest confirmed state; the missing event
//! history cannot be reconstructed. Actions should be idempotent and reconcile
//! against current status rather than assume queued events are still current.
//!
//! Probes run sequentially. Every check has a deadline, and shutdown cancels the
//! pending check by dropping its future. Like other async Rust cancellation,
//! this requires cooperative futures: custom probes must not block a runtime
//! thread or leave detached work running. A timeout cannot preempt a blocking
//! `poll`, and it cannot cancel tasks or threads spawned by a custom probe.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;

/// The future returned by a [`Probe`]. It is cancelled when dropped.
pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = Observation> + Send + 'a>>;

/// An asynchronous health check whose future is safe to cancel by dropping.
///
/// Implementations must yield rather than perform blocking I/O or CPU work.
/// They should not create detached tasks, threads, or independent retry loops:
/// the monitor can only bound and cancel the future returned by `check`.
/// Checks must be observational; do not put reconnection or fault injection in
/// a probe. React to monitor events separately in application-owned code.
pub trait Probe: Send + Sync + 'static {
    fn check(&self) -> ProbeFuture<'_>;
}

/// The outcome of one probe, before transition debounce is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Healthy,
    Unhealthy(ProbeFailure),
}

impl Observation {
    /// Whether this individual observation was healthy.
    pub fn is_healthy(&self) -> bool {
        matches!(self, Self::Healthy)
    }
}

/// Why a dependency or application check was unsuccessful.
///
/// None of these variants establishes host-wide or Internet connectivity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeFailure {
    /// The monitor's per-check deadline elapsed.
    Timeout,
    /// An HTTP endpoint returned a non-2xx status, including redirects.
    HttpStatus(u16),
    /// HTTP transport, DNS, or TLS failed. The endpoint URL is omitted.
    Transport(String),
    /// An application-specific check failed.
    Application(String),
}

/// Health transitions confirmed by sampled observations.
///
/// Initial status is emitted immediately after the first bounded check. Later
/// transitions require observations remaining in the new state for their
/// configured window. Unchanged states do not emit repeated events. Sampling
/// cannot establish what happened between checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorEvent {
    /// The first observation, even if it is unhealthy or times out.
    InitialStatus { observation: Observation },
    /// An initially or previously healthy target remained observed unhealthy
    /// for the configured loss debounce. This is not proof of a network outage.
    HealthLost { failure: ProbeFailure },
    /// A previously unhealthy target remained observed healthy for the recovery
    /// stability window. Never emitted for initial success; it does not mean the
    /// application's own connection has been reestablished.
    Recovered,
    /// Graceful shutdown completed. Dropping/aborting the owner does not promise
    /// this event. It can also be lost by a lagging receiver.
    Stopped,
}

/// Timing and bounded event-buffer settings for a [`Monitor`].
///
/// All durations must be nonzero and at most one day. The event capacity must be
/// between 1 and 65,536. These limits are checked before spawning any task.
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Delay from completion of one check to the start of the next. The first
    /// check starts immediately; checks never overlap or catch up in bursts.
    pub interval: Duration,
    /// Maximum duration of one cooperative probe future.
    pub probe_timeout: Duration,
    /// Minimum elapsed time between the first unhealthy sample and another
    /// unhealthy sample confirming loss. A healthy sample resets this window.
    pub loss_debounce: Duration,
    /// Minimum elapsed time between the first healthy sample and another
    /// healthy sample confirming recovery. A failed sample resets this window.
    pub recovery_stability: Duration,
    /// Bounded broadcast-buffer capacity. Tokio may round this to a power of 2.
    pub event_capacity: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            probe_timeout: Duration::from_secs(3),
            loss_debounce: Duration::from_secs(2),
            recovery_stability: Duration::from_secs(2),
            event_capacity: 64,
        }
    }
}

impl MonitorConfig {
    pub fn validate(&self) -> Result<(), MonitorError> {
        const MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
        for (name, duration) in [
            ("interval", self.interval),
            ("probe_timeout", self.probe_timeout),
            ("loss_debounce", self.loss_debounce),
            ("recovery_stability", self.recovery_stability),
        ] {
            if duration.is_zero() || duration > MAX_DURATION {
                return Err(MonitorError::InvalidConfig(name));
            }
        }
        if !(1..=65_536).contains(&self.event_capacity) {
            return Err(MonitorError::InvalidConfig("event_capacity"));
        }
        Ok(())
    }
}

/// A monitor could not start. No background task is created on these errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorError {
    /// The named configuration field is outside its documented bounds.
    InvalidConfig(&'static str),
    /// Start was called outside a Tokio runtime. This does not detect a runtime
    /// whose time driver is disabled; enabling time is a caller precondition.
    RuntimeUnavailable,
}

impl fmt::Display for MonitorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(field) => write!(f, "invalid monitor configuration: {field}"),
            Self::RuntimeUnavailable => write!(f, "a Tokio runtime is required to start a monitor"),
        }
    }
}

impl Error for MonitorError {}

/// Adapts an application's asynchronous health check into a [`Probe`].
///
/// Capture shared application state in the closure and clone any owned handles
/// needed by its returned future. This adapter does not spawn worker tasks.
pub struct ApplicationProbe<F> {
    check: F,
}

impl<F> ApplicationProbe<F> {
    pub fn new(check: F) -> Self {
        Self { check }
    }
}

impl<F, Fut> Probe for ApplicationProbe<F>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Observation> + Send + 'static,
{
    fn check(&self) -> ProbeFuture<'_> {
        Box::pin((self.check)())
    }
}

/// An HTTP(S) GET probe using normal certificate/hostname verification.
///
/// Every 2xx response is healthy; all other statuses are unhealthy. Redirects
/// are not followed. The response body is not downloaded or interpreted. Use
/// [`ApplicationProbe`] for body validation or application-level semantics.
/// Local and remote endpoints are accepted; the caller selects the endpoint.
///
/// A client is reused across checks. Monitor timeout/cancellation also bounds
/// waiting for DNS, a connection, TLS, and response headers. The method itself
/// is an ordinary async future; call it through a monitor for that bound.
pub struct HttpProbe {
    client: reqwest::Client,
    endpoint: reqwest::Url,
}

impl HttpProbe {
    pub fn new(endpoint: &str) -> Result<Self, HttpProbeError> {
        let endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| HttpProbeError::InvalidEndpoint("expected an absolute HTTP(S) URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
            return Err(HttpProbeError::InvalidEndpoint(
                "expected an absolute HTTP(S) URL",
            ));
        }
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(HttpProbeError::InvalidEndpoint(
                "credentials in URLs are not supported",
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(HttpProbeError::Client)?;
        Ok(Self { client, endpoint })
    }
}

impl Probe for HttpProbe {
    fn check(&self) -> ProbeFuture<'_> {
        Box::pin(async move {
            match self.client.get(self.endpoint.clone()).send().await {
                Ok(response) if response.status().is_success() => Observation::Healthy,
                Ok(response) => {
                    Observation::Unhealthy(ProbeFailure::HttpStatus(response.status().as_u16()))
                }
                Err(error) => {
                    Observation::Unhealthy(ProbeFailure::Transport(error.without_url().to_string()))
                }
            }
        })
    }
}

/// Creating the built-in HTTP probe failed before any request was sent.
#[derive(Debug)]
pub enum HttpProbeError {
    InvalidEndpoint(&'static str),
    Client(reqwest::Error),
}

impl fmt::Display for HttpProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEndpoint(reason) => write!(f, "invalid probe endpoint: {reason}"),
            Self::Client(error) => write!(f, "could not create HTTP probe client: {error}"),
        }
    }
}

impl Error for HttpProbeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::InvalidEndpoint(_) => None,
        }
    }
}

/// Owns one monitoring task. Dropping this owner aborts it.
///
/// Keep the owner alive while receiving events. [`Self::shutdown`] cancels any
/// pending probe and waits for the task and its future to be dropped. It returns
/// a [`JoinError`] if application probe code panicked or the task was aborted.
/// Drop requests an abort but cannot wait for the runtime to process it; no
/// `Stopped` event is promised on Drop. Probe futures must remain cooperative.
#[must_use = "dropping the monitor aborts its background task"]
pub struct Monitor {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    // A receiver lets us create future-only subscriptions without holding a
    // Sender alive. Receivers therefore observe Closed if the task panics.
    subscription: broadcast::Receiver<MonitorEvent>,
    status: watch::Receiver<Option<Observation>>,
}

impl Monitor {
    /// Start in a Tokio runtime with time enabled. The returned receiver is
    /// subscribed before the task is spawned. No probe runs on validation error.
    ///
    /// Only the presence of a runtime is checked here. If its time driver is
    /// disabled, the monitoring task can panic; `shutdown` reports that panic
    /// as a `JoinError`. Use a time-enabled runtime such as `#[tokio::main]`.
    pub fn start<P: Probe>(
        probe: P,
        config: MonitorConfig,
    ) -> Result<(Self, broadcast::Receiver<MonitorEvent>), MonitorError> {
        config.validate()?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| MonitorError::RuntimeUnavailable)?;
        let (events, receiver) = broadcast::channel(config.event_capacity);
        let subscription = receiver.resubscribe();
        let (status_sender, status) = watch::channel(None);
        let (shutdown, cancellation) = oneshot::channel();
        let task = runtime.spawn(run(probe, config, events, status_sender, cancellation));
        Ok((
            Self {
                shutdown: Some(shutdown),
                task: Some(task),
                subscription,
                status,
            },
            receiver,
        ))
    }

    /// Subscribe to future events only, with no replay or new initial status.
    /// Each receiver independently reports lag if it misses buffered events.
    pub fn subscribe(&self) -> broadcast::Receiver<MonitorEvent> {
        self.subscription.resubscribe()
    }

    /// Latest confirmed state, or `None` until the first check completes.
    ///
    /// Updated before each initial/transition event is emitted, never for raw
    /// samples still inside a debounce window. Use this to reconcile state after
    /// `RecvError::Lagged`; it cannot reconstruct missed event history. Queued
    /// events can describe an older state, so reconcile application actions
    /// against this snapshot when freshness matters.
    pub fn current_status(&self) -> Option<Observation> {
        self.status.borrow().clone()
    }

    /// Cancel the pending check, emit `Stopped`, and await task completion.
    ///
    /// If this shutdown future is itself dropped, the owner's Drop still
    /// aborts the task. No detached task is created during shutdown.
    pub async fn shutdown(mut self) -> Result<(), JoinError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let result = match self.task.as_mut() {
            Some(task) => task.await,
            None => Ok(()),
        };
        self.task.take();
        result
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run<P: Probe>(
    probe: P,
    config: MonitorConfig,
    events: broadcast::Sender<MonitorEvent>,
    status: watch::Sender<Option<Observation>>,
    mut cancellation: oneshot::Receiver<()>,
) {
    let mut tracker = HealthTracker::default();
    loop {
        let observation = tokio::select! {
            biased;
            _ = &mut cancellation => break,
            result = tokio::time::timeout(config.probe_timeout, probe.check()) => {
                result.unwrap_or(Observation::Unhealthy(ProbeFailure::Timeout))
            }
        };
        if let Some(event) = tracker.observe(observation.clone(), Instant::now(), &config) {
            status.send_replace(Some(observation));
            // Bounded, synchronous send never waits for subscribers or runs
            // application callbacks. Lag is reported by the receiver.
            let _ = events.send(event);
        }
        tokio::select! {
            biased;
            _ = &mut cancellation => break,
            _ = tokio::time::sleep(config.interval) => {}
        }
    }
    let _ = events.send(MonitorEvent::Stopped);
}

#[derive(Default)]
struct HealthTracker {
    stable_healthy: Option<bool>,
    candidate_since: Option<Instant>,
}

impl HealthTracker {
    fn observe(
        &mut self,
        observation: Observation,
        now: Instant,
        config: &MonitorConfig,
    ) -> Option<MonitorEvent> {
        let healthy = observation.is_healthy();
        let Some(stable) = self.stable_healthy else {
            self.stable_healthy = Some(healthy);
            return Some(MonitorEvent::InitialStatus { observation });
        };
        if stable == healthy {
            self.candidate_since = None;
            return None;
        }
        let since = self.candidate_since.get_or_insert(now);
        let window = if healthy {
            config.recovery_stability
        } else {
            config.loss_debounce
        };
        if now.duration_since(*since) < window {
            return None;
        }
        self.stable_healthy = Some(healthy);
        self.candidate_since = None;
        Some(match observation {
            Observation::Healthy => MonitorEvent::Recovered,
            Observation::Unhealthy(failure) => MonitorEvent::HealthLost { failure },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::broadcast::error::{RecvError, TryRecvError};

    fn config() -> MonitorConfig {
        MonitorConfig {
            interval: Duration::from_secs(1),
            probe_timeout: Duration::from_secs(2),
            loss_debounce: Duration::from_secs(2),
            recovery_stability: Duration::from_secs(2),
            event_capacity: 16,
        }
    }

    fn unhealthy() -> Observation {
        Observation::Unhealthy(ProbeFailure::Application("dependency unavailable".into()))
    }

    fn state_probe(healthy: &Arc<AtomicBool>) -> impl Probe {
        let healthy = Arc::clone(healthy);
        ApplicationProbe::new(move || {
            let value = healthy.load(Ordering::SeqCst);
            async move {
                if value {
                    Observation::Healthy
                } else {
                    unhealthy()
                }
            }
        })
    }

    async fn advance(seconds: u64) {
        // Advance one interval at a time, allowing the sequential monitor to
        // finish each check and schedule its next timer before advancing again.
        for _ in 0..seconds {
            tokio::time::advance(Duration::from_secs(1)).await;
            tokio::task::yield_now().await;
        }
    }

    fn assert_empty(events: &mut broadcast::Receiver<MonitorEvent>) {
        assert_eq!(events.try_recv(), Err(TryRecvError::Empty));
    }

    #[tokio::test(start_paused = true)]
    async fn initial_success_is_not_recovery_and_unchanged_health_is_quiet() {
        let healthy = Arc::new(AtomicBool::new(true));
        let (monitor, mut events) = Monitor::start(state_probe(&healthy), config()).unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            MonitorEvent::InitialStatus {
                observation: Observation::Healthy
            }
        );
        advance(20).await;
        assert_empty(&mut events);
        monitor.shutdown().await.unwrap();
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Stopped);
        assert_eq!(events.recv().await, Err(RecvError::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn loss_and_recovery_are_debounced_and_do_not_repeat() {
        let healthy = Arc::new(AtomicBool::new(true));
        let (monitor, mut events) = Monitor::start(state_probe(&healthy), config()).unwrap();
        events.recv().await.unwrap();
        healthy.store(false, Ordering::SeqCst);
        advance(2).await;
        assert_empty(&mut events);
        advance(1).await;
        assert_eq!(
            events.recv().await.unwrap(),
            MonitorEvent::HealthLost {
                failure: ProbeFailure::Application("dependency unavailable".into())
            }
        );
        advance(10).await;
        assert_empty(&mut events);
        healthy.store(true, Ordering::SeqCst);
        advance(2).await;
        assert_empty(&mut events);
        advance(1).await;
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Recovered);
        advance(10).await;
        assert_empty(&mut events);
        monitor.shutdown().await.unwrap();
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn initial_failure_only_emits_initial_status_then_stable_recovery() {
        let healthy = Arc::new(AtomicBool::new(false));
        let (monitor, mut events) = Monitor::start(state_probe(&healthy), config()).unwrap();
        assert_eq!(
            events.recv().await.unwrap(),
            MonitorEvent::InitialStatus {
                observation: unhealthy()
            }
        );
        advance(5).await;
        assert_empty(&mut events);
        healthy.store(true, Ordering::SeqCst);
        advance(2).await;
        assert_empty(&mut events);
        advance(1).await;
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Recovered);
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn flapping_resets_both_loss_and_recovery_windows() {
        let healthy = Arc::new(AtomicBool::new(true));
        let (monitor, mut events) = Monitor::start(state_probe(&healthy), config()).unwrap();
        events.recv().await.unwrap();
        for _ in 0..3 {
            healthy.store(false, Ordering::SeqCst);
            advance(2).await;
            healthy.store(true, Ordering::SeqCst);
            advance(1).await;
        }
        assert_empty(&mut events);
        healthy.store(false, Ordering::SeqCst);
        advance(3).await;
        assert!(matches!(
            events.recv().await.unwrap(),
            MonitorEvent::HealthLost { .. }
        ));
        for _ in 0..3 {
            healthy.store(true, Ordering::SeqCst);
            advance(2).await;
            healthy.store(false, Ordering::SeqCst);
            advance(1).await;
        }
        assert_empty(&mut events);
        healthy.store(true, Ordering::SeqCst);
        advance(3).await;
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Recovered);
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_receivers_report_lag_without_blocking_monitor() {
        let healthy = Arc::new(AtomicBool::new(true));
        let mut small = config();
        small.event_capacity = 2;
        let (monitor, mut slow) = Monitor::start(state_probe(&healthy), small).unwrap();
        let mut fast = monitor.subscribe();
        assert!(matches!(
            fast.recv().await.unwrap(),
            MonitorEvent::InitialStatus { .. }
        ));
        healthy.store(false, Ordering::SeqCst);
        advance(3).await;
        assert!(matches!(
            fast.recv().await.unwrap(),
            MonitorEvent::HealthLost { .. }
        ));
        healthy.store(true, Ordering::SeqCst);
        advance(3).await;
        assert_eq!(fast.recv().await.unwrap(), MonitorEvent::Recovered);
        assert_eq!(slow.recv().await, Err(RecvError::Lagged(1)));
        assert_eq!(monitor.current_status(), Some(Observation::Healthy));
        assert!(matches!(
            slow.recv().await.unwrap(),
            MonitorEvent::HealthLost { .. }
        ));
        assert_eq!(slow.recv().await.unwrap(), MonitorEvent::Recovered);
        monitor.shutdown().await.unwrap();
        assert_eq!(fast.recv().await.unwrap(), MonitorEvent::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn late_subscription_has_no_replayed_initial_event() {
        let healthy = Arc::new(AtomicBool::new(true));
        let (monitor, mut first) = Monitor::start(state_probe(&healthy), config()).unwrap();
        first.recv().await.unwrap();
        let mut late = monitor.subscribe();
        assert_empty(&mut late);
        monitor.shutdown().await.unwrap();
        assert_eq!(late.recv().await.unwrap(), MonitorEvent::Stopped);
        assert_eq!(late.recv().await, Err(RecvError::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_tracks_confirmed_state_not_unconfirmed_samples() {
        let healthy = Arc::new(AtomicBool::new(true));
        let (monitor, mut events) = Monitor::start(state_probe(&healthy), config()).unwrap();
        assert_eq!(monitor.current_status(), None);
        events.recv().await.unwrap();
        assert_eq!(monitor.current_status(), Some(Observation::Healthy));
        healthy.store(false, Ordering::SeqCst);
        advance(2).await;
        assert_eq!(monitor.current_status(), Some(Observation::Healthy));
        advance(1).await;
        assert!(matches!(
            events.recv().await.unwrap(),
            MonitorEvent::HealthLost { .. }
        ));
        assert_eq!(monitor.current_status(), Some(unhealthy()));
        healthy.store(true, Ordering::SeqCst);
        advance(2).await;
        assert_eq!(monitor.current_status(), Some(unhealthy()));
        advance(1).await;
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Recovered);
        assert_eq!(monitor.current_status(), Some(Observation::Healthy));
        monitor.shutdown().await.unwrap();
    }

    struct PendingProbe {
        started: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    struct DropCounter(Arc<AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Probe for PendingProbe {
        fn check(&self) -> ProbeFuture<'_> {
            Box::pin(async move {
                let _guard = DropCounter(Arc::clone(&self.dropped));
                self.started.fetch_add(1, Ordering::SeqCst);
                std::future::pending().await
            })
        }
    }

    fn pending_probe() -> (PendingProbe, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        (
            PendingProbe {
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            },
            started,
            dropped,
        )
    }

    #[tokio::test(start_paused = true)]
    async fn hung_probes_timeout_and_never_overlap() {
        let (probe, started, dropped) = pending_probe();
        let (monitor, mut events) = Monitor::start(probe, config()).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        advance(2).await;
        assert_eq!(
            events.recv().await.unwrap(),
            MonitorEvent::InitialStatus {
                observation: Observation::Unhealthy(ProbeFailure::Timeout)
            }
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        advance(1).await;
        assert_eq!(started.load(Ordering::SeqCst), 2);
        monitor.shutdown().await.unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_timeouts_confirm_loss_and_successes_confirm_recovery() {
        let healthy = Arc::new(AtomicBool::new(true));
        let state = Arc::clone(&healthy);
        let probe = ApplicationProbe::new(move || {
            let value = state.load(Ordering::SeqCst);
            async move {
                if value {
                    Observation::Healthy
                } else {
                    std::future::pending().await
                }
            }
        });
        let (monitor, mut events) = Monitor::start(probe, config()).unwrap();
        events.recv().await.unwrap();
        healthy.store(false, Ordering::SeqCst);
        advance(3).await;
        assert_empty(&mut events);
        assert_eq!(monitor.current_status(), Some(Observation::Healthy));
        advance(3).await;
        assert_eq!(
            events.recv().await.unwrap(),
            MonitorEvent::HealthLost {
                failure: ProbeFailure::Timeout
            }
        );
        healthy.store(true, Ordering::SeqCst);
        advance(2).await;
        assert_empty(&mut events);
        advance(1).await;
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Recovered);
        monitor.shutdown().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_cancels_pending_probe_without_waiting_for_deadline() {
        let (probe, started, dropped) = pending_probe();
        let (monitor, mut events) = Monitor::start(probe, config()).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        let before = Instant::now();
        monitor.shutdown().await.unwrap();
        assert_eq!(Instant::now(), before);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Stopped);
        assert_eq!(events.recv().await, Err(RecvError::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_owner_aborts_pending_probe_and_closes_events() {
        let (probe, started, dropped) = pending_probe();
        let (monitor, mut events) = Monitor::start(probe, config()).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        drop(monitor);
        tokio::task::yield_now().await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(events.recv().await, Err(RecvError::Closed));
        advance(10).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_shutdown_future_still_owns_and_aborts_task() {
        let (probe, started, dropped) = pending_probe();
        let (monitor, mut events) = Monitor::start(probe, config()).unwrap();
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        let mut shutdown = Box::pin(monitor.shutdown());
        // Poll once to issue cancellation, then cancel shutdown itself before
        // yielding to the monitor. Its owner must retain the JoinHandle.
        assert!(
            std::future::poll_fn(|context| {
                std::task::Poll::Ready(shutdown.as_mut().poll(context).is_pending())
            })
            .await
        );
        drop(shutdown);
        tokio::task::yield_now().await;
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(events.recv().await, Err(RecvError::Closed));
    }

    #[tokio::test(start_paused = true)]
    async fn probe_panic_closes_event_stream_and_is_reported_on_shutdown() {
        let probe = ApplicationProbe::new(|| async { panic!("test probe panic") });
        let (monitor, mut events) = Monitor::start(probe, config()).unwrap();
        assert_eq!(events.recv().await, Err(RecvError::Closed));
        assert!(monitor.shutdown().await.unwrap_err().is_panic());
    }

    #[test]
    fn rejects_zero_and_excessive_configuration_values() {
        for field in 0..4 {
            for duration in [Duration::ZERO, Duration::from_secs(86_401)] {
                let mut invalid = config();
                match field {
                    0 => invalid.interval = duration,
                    1 => invalid.probe_timeout = duration,
                    2 => invalid.loss_debounce = duration,
                    _ => invalid.recovery_stability = duration,
                }
                assert!(matches!(
                    invalid.validate(),
                    Err(MonitorError::InvalidConfig(_))
                ));
            }
        }
        for capacity in [0, 65_537, usize::MAX] {
            let mut invalid = config();
            invalid.event_capacity = capacity;
            assert_eq!(
                invalid.validate(),
                Err(MonitorError::InvalidConfig("event_capacity"))
            );
        }
        assert!(MonitorConfig::default().validate().is_ok());
    }

    #[test]
    fn starting_without_runtime_returns_error_instead_of_panicking() {
        let probe = ApplicationProbe::new(|| async { Observation::Healthy });
        assert!(matches!(
            Monitor::start(probe, config()),
            Err(MonitorError::RuntimeUnavailable)
        ));
    }

    #[tokio::test]
    async fn invalid_configuration_never_runs_a_probe() {
        let (probe, started, _) = pending_probe();
        let mut invalid = config();
        invalid.interval = Duration::ZERO;
        assert!(matches!(
            Monitor::start(probe, invalid),
            Err(MonitorError::InvalidConfig("interval"))
        ));
        tokio::task::yield_now().await;
        assert_eq!(started.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn http_endpoint_validation_allows_remote_https_and_rejects_bad_schemes() {
        assert!(HttpProbe::new("https://example.com/health").is_ok());
        assert!(HttpProbe::new("http://127.0.0.1:8080/health").is_ok());
        for endpoint in [
            "",
            "/health",
            "file:///tmp/health",
            "ftp://example.com/",
            "http://user:secret@example.com/",
        ] {
            assert!(HttpProbe::new(endpoint).is_err(), "accepted {endpoint}");
        }
    }

    #[tokio::test]
    async fn http_probe_observes_status_and_does_not_follow_redirects() {
        for status in [204, 503, 302] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_ip().unwrap();
            let thread = std::thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(request.method(), &tiny_http::Method::Get);
                assert_eq!(request.url(), "/health");
                request
                    .respond(
                        tiny_http::Response::empty(status).with_header(
                            tiny_http::Header::from_bytes(
                                "Location",
                                "http://127.0.0.1:1/unreachable",
                            )
                            .unwrap(),
                        ),
                    )
                    .unwrap();
            });
            let probe = HttpProbe::new(&format!("http://{address}/health")).unwrap();
            let observed = tokio::time::timeout(Duration::from_secs(5), probe.check())
                .await
                .unwrap();
            if status == 204 {
                assert_eq!(observed, Observation::Healthy);
            } else {
                assert_eq!(
                    observed,
                    Observation::Unhealthy(ProbeFailure::HttpStatus(status))
                );
            }
            thread.join().unwrap();
        }
    }

    // Bounded loopback server for exercising the real HTTP future. The owner
    // always signals and joins its worker, including when a test unwinds.
    struct TestHttpServer {
        stop: std::sync::mpsc::Sender<()>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for TestHttpServer {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn test_http_server(
        close_after_request: bool,
    ) -> (String, oneshot::Receiver<()>, TestHttpServer) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/health", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (stop, cancellation) = std::sync::mpsc::channel();
        let (accepted, received) = oneshot::channel();
        let worker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                if cancellation.try_recv().is_ok() || std::time::Instant::now() >= deadline {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => return,
                }
            };
            if stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .is_err()
            {
                return;
            }
            // Read a bounded request before signalling that the client is
            // waiting for response headers. Never send any response bytes.
            let mut request = Vec::new();
            let mut bytes = [0_u8; 1024];
            while request.len() <= 16_384 && !request.windows(4).any(|w| w == b"\r\n\r\n") {
                if std::time::Instant::now() >= deadline {
                    return;
                }
                match stream.read(&mut bytes) {
                    Ok(0) | Err(_) => return,
                    Ok(count) => request.extend_from_slice(&bytes[..count]),
                }
            }
            let _ = accepted.send(());
            if !close_after_request {
                let _ = cancellation.recv_timeout(Duration::from_secs(5));
            }
            let _ = stream.shutdown(Shutdown::Both);
        });
        (
            url,
            received,
            TestHttpServer {
                stop,
                worker: Some(worker),
            },
        )
    }

    #[tokio::test]
    async fn http_probe_deadline_bounds_server_that_never_responds() {
        let (url, accepted, server) = test_http_server(false);
        let mut bounded = config();
        bounded.probe_timeout = Duration::from_secs(1);
        bounded.interval = Duration::from_secs(60);
        let (monitor, mut events) = Monitor::start(HttpProbe::new(&url).unwrap(), bounded).unwrap();
        tokio::time::timeout(Duration::from_secs(2), accepted)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap(),
            MonitorEvent::InitialStatus {
                observation: Observation::Unhealthy(ProbeFailure::Timeout)
            }
        );
        tokio::time::timeout(Duration::from_secs(2), monitor.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Stopped);
        assert_eq!(events.recv().await, Err(RecvError::Closed));
        drop(server);
    }

    #[tokio::test]
    async fn shutdown_cancels_real_http_probe_before_its_deadline() {
        let (url, accepted, server) = test_http_server(false);
        let mut bounded = config();
        bounded.probe_timeout = Duration::from_secs(30);
        let (monitor, mut events) = Monitor::start(HttpProbe::new(&url).unwrap(), bounded).unwrap();
        tokio::time::timeout(Duration::from_secs(2), accepted)
            .await
            .unwrap()
            .unwrap();
        assert_empty(&mut events);
        assert_eq!(monitor.current_status(), None);
        tokio::time::timeout(Duration::from_secs(2), monitor.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(events.recv().await.unwrap(), MonitorEvent::Stopped);
        assert_eq!(events.recv().await, Err(RecvError::Closed));
        drop(server);
    }

    #[tokio::test]
    async fn http_transport_failure_event_omits_endpoint_and_query_secret() {
        let (url, accepted, server) = test_http_server(true);
        let secret = "QUERY_SECRET_MUST_NOT_LEAK";
        let endpoint = format!("{url}?token={secret}");
        let (monitor, mut events) =
            Monitor::start(HttpProbe::new(&endpoint).unwrap(), config()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), accepted)
            .await
            .unwrap()
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            MonitorEvent::InitialStatus {
                observation: Observation::Unhealthy(ProbeFailure::Transport(_))
            }
        ));
        let diagnostic = format!("{event:?}");
        assert!(!diagnostic.contains(secret));
        assert!(!diagnostic.contains(&url));
        monitor.shutdown().await.unwrap();
        drop(server);
    }
}
