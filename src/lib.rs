//! Passive application-health monitoring, with explicitly enabled recovery tests.

pub mod monitor;

#[cfg(feature = "fault-injection")]
pub mod fixture;

#[cfg(feature = "fault-injection")]
pub mod runner;
