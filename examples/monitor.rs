//! Run with `cargo run --example monitor`.
//!
//! This executable consumes only public library APIs. Its synthetic probe does
//! not touch network configuration. The host application owns every action.

use recovery_lab::monitor::{
    ApplicationProbe, Monitor, MonitorConfig, MonitorEvent, Observation, ProbeFailure,
};
use std::error::Error;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

#[derive(Debug, Default, PartialEq, Eq)]
struct ApplicationState {
    work_enabled: bool,
    pause_actions: usize,
    resume_actions: usize,
    graceful_stop_observed: bool,
}

async fn run_demo() -> Result<ApplicationState, Box<dyn Error + Send + Sync>> {
    let dependency_healthy = Arc::new(AtomicBool::new(true));
    let probe_state = Arc::clone(&dependency_healthy);
    let probe = ApplicationProbe::new(move || {
        let healthy = probe_state.load(Ordering::Acquire);
        async move {
            if healthy {
                Observation::Healthy
            } else {
                Observation::Unhealthy(ProbeFailure::Application(
                    "synthetic dependency is unavailable".into(),
                ))
            }
        }
    });
    let (monitor, mut events) = Monitor::start(
        probe,
        MonitorConfig {
            interval: Duration::from_millis(20),
            probe_timeout: Duration::from_millis(100),
            loss_debounce: Duration::from_millis(40),
            recovery_stability: Duration::from_millis(60),
            event_capacity: 8,
        },
    )?;
    let mut application = ApplicationState::default();
    let deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(deadline);
    loop {
        let next = tokio::select! {
            event = events.recv() => event,
            _ = &mut deadline => return Err(io::Error::new(io::ErrorKind::TimedOut, "demo did not complete").into()),
        };
        match next {
            Ok(MonitorEvent::InitialStatus { observation }) => {
                application.work_enabled = observation.is_healthy();
                println!("Initial dependency health: {observation:?}");
                // Drive the demonstration, not an actual network fault.
                dependency_healthy.store(false, Ordering::Release);
            }
            Ok(MonitorEvent::HealthLost { failure }) => {
                println!("Confirmed health loss: {failure:?}");
                // Reconcile with the latest confirmed state: queued events may
                // be older, especially after a slow receiver has lagged.
                if monitor
                    .current_status()
                    .is_some_and(|state| !state.is_healthy())
                {
                    application.work_enabled = false;
                    application.pause_actions += 1;
                    println!("Host action: pause dependency-backed work");
                }
                dependency_healthy.store(true, Ordering::Release);
            }
            Ok(MonitorEvent::Recovered) => {
                if monitor
                    .current_status()
                    .is_some_and(|state| state.is_healthy())
                {
                    application.work_enabled = true;
                    application.resume_actions += 1;
                    println!("Host action: resume dependency-backed work");
                    break;
                }
            }
            Ok(MonitorEvent::Stopped) => {
                return Err(io::Error::other("monitor stopped before recovery").into());
            }
            Err(RecvError::Lagged(count)) => {
                eprintln!("Missed {count} events; resynchronizing current state");
                if let Some(state) = monitor.current_status() {
                    application.work_enabled = state.is_healthy();
                }
                // Missed transition-specific actions cannot be reconstructed.
                // A real application should reconcile its desired state here.
            }
            Err(RecvError::Closed) => {
                return Err(io::Error::other("monitor task closed unexpectedly").into());
            }
        }
    }
    monitor.shutdown().await?;
    application.graceful_stop_observed = matches!(events.recv().await, Ok(MonitorEvent::Stopped));
    println!("Monitor stopped; final host state: {application:?}");
    Ok(application)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    run_demo().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_embedding_api_drives_host_actions_and_shutdown() {
        assert_eq!(
            run_demo().await.unwrap(),
            ApplicationState {
                work_enabled: true,
                pause_actions: 1,
                resume_actions: 1,
                graceful_stop_observed: true,
            }
        );
    }
}
