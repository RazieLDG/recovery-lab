//! Passive async dependency-health monitoring and opt-in local recovery tests.
//!
//! # Experimental, active development
//!
//! Recovery Lab is under heavy development and has no production-readiness
//! assurance. Before 1.0, new minor versions may introduce breaking APIs; patch
//! versions are intended for compatible fixes. The tested minimum Rust version
//! is 1.98.1. Linux x86_64 is the currently validated platform.
//!
//! Default features are empty: [`monitor`] is passive and does not inject faults
//! or reconnect your application. The `fault-injection` feature adds the blocking
//! recovery runner, CLI and local reference fixture. Applications own every
//! response to a health event. Endpoint health is not proof of host-wide network
//! connectivity or an application's completed reconnect.
//!
//! # Embedding quickstart
//!
//! This helper is called from a Tokio runtime with its time driver enabled.
//! The host sends the stop signal and reconciles its own desired work state.
//! The example is compile-tested and does not contact the example endpoint in
//! documentation tests.
//!
//! ```no_run
//! use recovery_lab::monitor::{HttpProbe, Monitor, MonitorConfig, MonitorEvent};
//! use tokio::sync::{broadcast::error::RecvError, oneshot};
//!
//! async fn observe_dependency(
//!     mut stop: oneshot::Receiver<()>,
//! ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
//!     let (monitor, mut events) = Monitor::start(
//!         HttpProbe::new("https://example.com/health")?,
//!         MonitorConfig::default(),
//!     )?;
//!     loop {
//!         tokio::select! {
//!             _ = &mut stop => break,
//!             event = events.recv() => match event {
//!                 Ok(MonitorEvent::Stopped) | Err(RecvError::Closed) => break,
//!                 Ok(event) => {
//!                     // InitialStatus, HealthLost or Recovered: application
//!                     // actions belong here, outside the monitoring task.
//!                     println!("Health event: {event:?}");
//!                 }
//!                 Err(RecvError::Lagged(skipped)) => {
//!                     // Lost event history cannot be reconstructed. Reconcile
//!                     // desired state from the latest confirmed snapshot.
//!                     eprintln!("Missed {skipped} events");
//!                 }
//!             }
//!         }
//!         if let Some(confirmed) = monitor.current_status() {
//!             let application_work_enabled = confirmed.is_healthy();
//!             println!("Host's desired work state: {application_work_enabled}");
//!         }
//!     }
//!     monitor.shutdown().await?;
//!     Ok(())
//! }
//! ```
//!
//! Initial success is not a recovery event. Loss and recovery require configured
//! sampled stability windows. The bounded receiver reports lag explicitly.
//! Custom probes must yield and support cancellation by dropping their future;
//! blocking polls or independently spawned work cannot be forcibly cancelled.
//! See [`monitor`] for the full lifecycle and timing contract.

pub mod monitor;

#[cfg(feature = "fault-injection")]
pub mod fixture;

#[cfg(feature = "fault-injection")]
pub mod runner;
