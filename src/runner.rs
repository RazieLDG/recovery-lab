//! Explicit, bounded fault-injection experiments for dedicated local test proxies.
//!
//! This module requires the `fault-injection` feature. Merely linking the crate,
//! loading configuration, or using the passive monitor never runs an experiment.
//! All proxy mutations originate from an explicit [`run_blocking`] or
//! [`run_blocking_with_events`] call. These APIs are blocking; use a blocking
//! worker thread from async applications.

use reqwest::blocking::Client;
use reqwest::{redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{error::Error, fmt};

/// Maximum encoded JSON scenario size, in bytes.
pub const MAX_SCENARIO_BYTES: u64 = 65536;
const MAX_API_BODY: u64 = 65536;
/// Explicit opt-in configuration for a local Toxiproxy fault experiment.
///
/// Loading or validating a scenario never contacts or mutates a proxy. The
/// blocking runner validates again before performing any network operations.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    pub toxiproxy_url: String,
    pub proxy: String,
    pub health_url: String,
    pub fault: Fault,
    pub request_timeout_ms: u64,
    pub recovery_timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub preflight_timeout_ms: u64,
}
/// The single bounded local fault applied by an explicitly started experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fault {
    Outage { duration_ms: u64 },
    Latency { duration_ms: u64, latency_ms: u64 },
    ResetPeer { duration_ms: u64 },
}
impl Fault {
    /// Duration of the fault observation window, in milliseconds.
    pub fn duration_ms(&self) -> u64 {
        match self {
            Self::Outage { duration_ms }
            | Self::Latency { duration_ms, .. }
            | Self::ResetPeer { duration_ms } => *duration_ms,
        }
    }
}
#[derive(Debug, Deserialize)]
struct Proxy {
    name: String,
    listen: String,
    upstream: String,
    enabled: bool,
    #[serde(default)]
    toxics: Vec<Value>,
}
/// One timestamped experiment event. No target URLs or response bodies are stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub elapsed_ms: u128,
    pub phase: String,
    pub detail: String,
}

/// Completed experiment evidence, using the CLI's version-1 JSON schema.
///
/// `exit_code` uses the same values as the CLI: 0 passed, 1 failed assertion,
/// 2 operational/configuration error, 3 cleanup failure, and 130 cancellation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u8,
    pub scenario: String,
    pub outcome: String,
    pub exit_code: i32,
    pub cleanup: String,
    pub elapsed_ms: u128,
    pub events: Vec<Event>,
}

impl Report {
    fn pending(scenario: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            scenario: scenario.into(),
            outcome: "error".into(),
            exit_code: 2,
            cleanup: "not_needed".into(),
            elapsed_ms: 0,
            events: Vec::new(),
        }
    }

    fn finish(&mut self, code: i32, elapsed: Duration) {
        self.exit_code = code;
        self.outcome = match code {
            0 => "passed",
            1 => "failed",
            130 => "cancelled",
            _ => "error",
        }
        .into();
        self.elapsed_ms = elapsed.as_millis();
    }

    /// Construct an error report for an adapter failure before a run starts.
    /// This is useful for scenario-loading, signal-handler, or CLI errors.
    pub fn error(
        scenario: impl Into<String>,
        detail: impl Into<String>,
        elapsed: Duration,
    ) -> Self {
        let mut report = Self::pending(scenario);
        report.events.push(Event {
            elapsed_ms: elapsed.as_millis(),
            phase: "error".into(),
            detail: detail.into(),
        });
        report.finish(2, elapsed);
        report
    }

    /// Serialize this report as the existing pretty-printed JSON report format.
    pub fn to_json_pretty(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec_pretty(self)
    }

    /// Render this report as a single-test JUnit XML document.
    pub fn to_junit(&self) -> String {
        let failure = if self.exit_code == 0 {
            String::new()
        } else {
            let tag = if self.exit_code == 1 {
                "failure"
            } else {
                "error"
            };
            format!(
                "<{tag} message=\"{}\">{}</{tag}>",
                xml(&self.outcome),
                xml(&self
                    .events
                    .iter()
                    .map(|e| format!("{}: {}", e.phase, e.detail))
                    .collect::<Vec<_>>()
                    .join("\n"))
            )
        };
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"recovery-lab\" tests=\"1\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\"><testcase name=\"{}\" time=\"{:.3}\">{}</testcase></testsuite>\n", u8::from(self.exit_code == 1), u8::from(self.exit_code != 0 && self.exit_code != 1), self.elapsed_ms as f64 / 1000.0, xml(&self.scenario), self.elapsed_ms as f64 / 1000.0, failure)
    }
}

/// A scenario could not be read, decoded, or safely validated.
#[derive(Debug)]
pub enum ConfigError {
    Io {
        operation: &'static str,
        source: io::Error,
    },
    TooLarge,
    InvalidJson {
        line: usize,
        column: usize,
    },
    InvalidValue(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation: "open",
                source,
            } => write!(f, "cannot open scenario: {source}"),
            Self::Io { .. } => f.write_str("cannot read scenario"),
            Self::TooLarge => f.write_str("scenario exceeds 64 KiB"),
            Self::InvalidJson { line, column } => {
                write!(f, "invalid scenario JSON at line {line}, column {column}")
            }
            Self::InvalidValue(message) => f.write_str(message),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Reason an experiment could not run normally.
#[derive(Debug)]
pub enum RunErrorKind {
    Configuration(ConfigError),
    Runtime(String),
}

impl fmt::Display for RunErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Configuration(error) => error.fmt(f),
            Self::Runtime(message) => f.write_str(message),
        }
    }
}

/// Operational/configuration failure with all evidence gathered so far.
///
/// Failed recovery assertions, cancellation, and an unconfirmed cleanup are
/// completed runs returned as `Ok(Report)`; inspect the report's `exit_code`.
#[derive(Debug)]
pub struct RunError {
    pub kind: RunErrorKind,
    pub report: Box<Report>,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}

impl Error for RunError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.kind {
            RunErrorKind::Configuration(error) => Some(error),
            RunErrorKind::Runtime(_) => None,
        }
    }
}

struct RunContext<'a> {
    report: Report,
    on_event: &'a mut dyn FnMut(&Event),
}

impl RunContext<'_> {
    fn event(&mut self, start: Instant, phase: &str, detail: &str) {
        self.report.events.push(Event {
            elapsed_ms: start.elapsed().as_millis(),
            phase: phase.into(),
            detail: detail.into(),
        });
        (self.on_event)(self.report.events.last().expect("just pushed event"));
    }
}

fn local_url(raw: &str, api: bool) -> Result<Url, String> {
    let u = Url::parse(raw).map_err(|_| "invalid URL")?;
    if u.scheme() != "http"
        || !u.username().is_empty()
        || u.password().is_some()
        || u.fragment().is_some()
        || u.query().is_some()
    {
        return Err("URLs must use HTTP without credentials, query or fragment".into());
    }
    let ip = u
        .host_str()
        .unwrap_or("")
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .map_err(|_| "URLs require a literal loopback IP (use 127.0.0.1, not localhost)")?;
    if !ip.is_loopback() {
        return Err("only loopback targets are permitted".into());
    }
    if api && u.path() != "/" {
        return Err("Toxiproxy URL must have no path".into());
    }
    Ok(u)
}
fn local_socket(s: &str) -> bool {
    s.parse::<SocketAddr>()
        .map(|a| a.ip().is_loopback() && a.port() != 0)
        .unwrap_or(false)
}
fn validate(s: &Scenario) -> Result<(), String> {
    if s.name.is_empty() || s.name.len() > 120 || s.name.chars().any(char::is_control) {
        return Err("name must be 1–120 bytes without control characters".into());
    }
    if s.proxy.is_empty()
        || s.proxy.len() > 80
        || !s
            .proxy
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("proxy must be 1–80 ASCII letters, digits, hyphens or underscores".into());
    }
    local_url(&s.toxiproxy_url, true)?;
    local_url(&s.health_url, false)?;
    for (n, v, min, max) in [
        ("request_timeout_ms", s.request_timeout_ms, 10, 10000),
        ("recovery_timeout_ms", s.recovery_timeout_ms, 10, 300000),
        ("preflight_timeout_ms", s.preflight_timeout_ms, 10, 60000),
        ("poll_interval_ms", s.poll_interval_ms, 10, 10000),
        ("fault.duration_ms", s.fault.duration_ms(), 10, 60000),
    ] {
        if !(min..=max).contains(&v) {
            return Err(format!("{n} must be in {min}..={max}"));
        }
    }
    if s.fault.duration_ms() < s.request_timeout_ms + s.poll_interval_ms {
        return Err(
            "fault.duration_ms must be at least request_timeout_ms + poll_interval_ms".into(),
        );
    }
    if let Fault::Latency { latency_ms, .. } = s.fault {
        if !(1..=60000).contains(&latency_ms) {
            return Err("latency_ms must be in 1..=60000".into());
        }
    }
    Ok(())
}
impl Scenario {
    /// Check all safety constraints without network activity or side effects.
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate(self).map_err(ConfigError::InvalidValue)
    }

    /// Decode and validate bounded scenario JSON, without network activity.
    pub fn from_json(bytes: &[u8]) -> Result<Self, ConfigError> {
        if bytes.len() as u64 > MAX_SCENARIO_BYTES {
            return Err(ConfigError::TooLarge);
        }
        let scenario: Self =
            serde_json::from_slice(bytes).map_err(|error| ConfigError::InvalidJson {
                line: error.line(),
                column: error.column(),
            })?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// Read at most 64 KiB and validate a JSON scenario, without network activity.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let file = fs::File::open(path).map_err(|source| ConfigError::Io {
            operation: "open",
            source,
        })?;
        let mut bytes = Vec::new();
        file.take(MAX_SCENARIO_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| ConfigError::Io {
                operation: "read",
                source,
            })?;
        Self::from_json(&bytes)
    }
}

/// Run one fault experiment synchronously, with no logging or signal handlers.
///
/// Calling this function can mutate the selected local test proxy. It validates
/// the scenario, checks the dedicated proxy's safety,
/// applies the fault, attempts restoration, and returns the completed report.
/// Set `cancel` to `true` from another thread to request cancellation. In-flight
/// requests finish or time out before cleanup. Cleanup failure takes priority
/// over cancellation. The caller owns the token; it is never reset here.
///
/// This function blocks and must run on a dedicated blocking thread when used
/// by an asynchronous application (for example, via `tokio::task::spawn_blocking`).
/// For passive connectivity observation, use the separate monitor API.
pub fn run_blocking(scenario: &Scenario, cancel: &AtomicBool) -> Result<Report, RunError> {
    run_blocking_with_events(scenario, cancel, |_| {})
}

/// Run a blocking fault experiment and observe its events as they occur.
///
/// The callback executes synchronously on the calling thread and should return
/// promptly: a blocking callback delays cancellation and fault restoration. It
/// receives the same events, in order, as the returned report. A callback panic
/// propagates to the caller; when panic unwinding is enabled, the cleanup guard
/// attempts bounded, best-effort restoration during unwinding. An abort cannot
/// run that cleanup.
/// This function installs no process-wide handlers and writes no report files.
/// See [`run_blocking`] for mutation, cancellation, and blocking requirements.
pub fn run_blocking_with_events(
    scenario: &Scenario,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(&Event),
) -> Result<Report, RunError> {
    let start = Instant::now();
    let mut context = RunContext {
        report: Report::pending(&scenario.name),
        on_event: &mut on_event,
    };
    let result = scenario
        .validate()
        .map_err(RunErrorKind::Configuration)
        .and_then(|_| {
            if cancelled(cancel) {
                Ok(130)
            } else {
                run(scenario, cancel, &mut context, start).map_err(RunErrorKind::Runtime)
            }
        });
    match result {
        Ok(code) => {
            context.report.finish(code, start.elapsed());
            Ok(context.report)
        }
        Err(kind) => {
            context.event(start, "error", &kind.to_string());
            context.report.finish(2, start.elapsed());
            Err(RunError {
                kind,
                report: Box::new(context.report),
            })
        }
    }
}

struct Guard {
    client: Client,
    proxy_url: String,
    toxic_url: Option<String>,
    armed: bool,
    lock: PathBuf,
}
impl Guard {
    fn cleanup(&mut self) -> Result<(), String> {
        if !self.armed {
            return Ok(());
        }
        let mut last = String::new();
        for _ in 0..3 {
            let r = if let Some(url) = &self.toxic_url {
                self.client.delete(url).send()
            } else {
                self.client
                    .post(&self.proxy_url)
                    .json(&json!({"enabled":true}))
                    .send()
            };
            match r {
                Ok(resp)
                    if resp.status().is_success()
                        || (self.toxic_url.is_some() && resp.status().as_u16() == 404) =>
                {
                    self.armed = false;
                    return Ok(());
                }
                Ok(resp) => last = format!("cleanup returned HTTP {}", resp.status().as_u16()),
                Err(_) => last = "cleanup request failed or timed out".into(),
            }
        }
        Err(last)
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.cleanup();
        }
        if !self.armed {
            let _ = fs::remove_file(&self.lock);
        }
    }
}
fn api_json(client: &Client, url: &str) -> Result<Value, String> {
    let r = client
        .get(url)
        .send()
        .map_err(|_| "Toxiproxy request failed or timed out")?;
    if !r.status().is_success() {
        return Err(format!("Toxiproxy returned HTTP {}", r.status().as_u16()));
    }
    let mut bytes = Vec::new();
    r.take(MAX_API_BODY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Toxiproxy response read failed")?;
    if bytes.len() as u64 > MAX_API_BODY {
        return Err("Toxiproxy response too large".into());
    }
    serde_json::from_slice(&bytes).map_err(|_| "invalid Toxiproxy JSON".into())
}
fn healthy(client: &Client, url: &str, timeout: Duration) -> bool {
    client
        .get(url)
        .timeout(timeout)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}
fn cancelled(c: &AtomicBool) -> bool {
    c.load(Ordering::SeqCst)
}
fn sleep_until(deadline: Instant, c: &AtomicBool) {
    while !cancelled(c) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(20)));
    }
}
fn wait_health(s: &Scenario, client: &Client, deadline: Instant, c: &AtomicBool) -> bool {
    while !cancelled(c) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let ok = healthy(
            client,
            &s.health_url,
            (deadline - now).min(Duration::from_millis(s.request_timeout_ms)),
        );
        if ok && Instant::now() <= deadline && !cancelled(c) {
            return true;
        }
        sleep_until(
            deadline.min(Instant::now() + Duration::from_millis(s.poll_interval_ms)),
            c,
        );
    }
    false
}
fn run(
    s: &Scenario,
    c: &AtomicBool,
    report: &mut RunContext<'_>,
    start: Instant,
) -> Result<i32, String> {
    let api = Client::builder()
        .timeout(Duration::from_millis(s.request_timeout_ms))
        .redirect(Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| "cannot build HTTP client")?;
    let client = api.clone();
    let proxy_url = format!(
        "{}/proxies/{}",
        local_url(&s.toxiproxy_url, true)?
            .as_str()
            .trim_end_matches('/'),
        s.proxy
    );
    let mut h = std::collections::hash_map::DefaultHasher::new();
    proxy_url.hash(&mut h);
    let lock = std::env::temp_dir().join(format!("recovery-lab-{:x}.lock", h.finish()));
    let mut file = OpenOptions::new().write(true).create_new(true).open(&lock).map_err(|_| "proxy lock already exists or cannot be created; ensure no other run is active before removing the stale lock")?;
    writeln!(file, "pid={} proxy={}", std::process::id(), s.proxy)
        .map_err(|_| "cannot write lock")?;
    let mut guard = Guard {
        client: api.clone(),
        proxy_url: proxy_url.clone(),
        toxic_url: None,
        armed: false,
        lock,
    };
    let p: Proxy = serde_json::from_value(api_json(&api, &proxy_url)?)
        .map_err(|_| "invalid proxy description")?;
    if p.name != s.proxy || !local_socket(&p.listen) || !local_socket(&p.upstream) {
        return Err(
            "proxy name mismatch or listen/upstream is not a literal loopback socket".into(),
        );
    }
    if !p.enabled || !p.toxics.is_empty() {
        return Err(
            "proxy must be enabled with no pre-existing toxics; use a dedicated test proxy".into(),
        );
    }
    report.event(start, "preflight", "checking application health");
    if !wait_health(
        s,
        &client,
        Instant::now() + Duration::from_millis(s.preflight_timeout_ms),
        c,
    ) {
        report.event(
            start,
            "preflight",
            "application was not healthy within deadline",
        );
        return Ok(if cancelled(c) { 130 } else { 1 });
    }
    if cancelled(c) {
        return Ok(130);
    }
    let toxic_name = format!(
        "recovery_lab_{}_{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let payload = match s.fault {
        Fault::Outage { .. } => json!({"enabled":false}),
        Fault::Latency { latency_ms, .. } => {
            json!({"name":toxic_name,"type":"latency","stream":"downstream","toxicity":1.0,"attributes":{"latency":latency_ms,"jitter":0}})
        }
        Fault::ResetPeer { .. } => {
            json!({"name":toxic_name,"type":"reset_peer","stream":"downstream","toxicity":1.0,"attributes":{"timeout":0}})
        }
    };
    let inject_url = if matches!(s.fault, Fault::Outage { .. }) {
        proxy_url.clone()
    } else {
        guard.toxic_url = Some(format!("{proxy_url}/toxics/{toxic_name}"));
        format!("{proxy_url}/toxics")
    };
    // Arm before sending: a timeout may occur after the server applied the mutation.
    guard.armed = true;
    let injected = api
        .post(&inject_url)
        .json(&payload)
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    let mut outcome = 0;
    let mut observed = false;
    if !injected {
        report.event(
            start,
            "fault",
            "injection failed or timed out; restoring conservatively",
        );
        outcome = 2;
    } else {
        report.event(start, "fault", "injection acknowledged");
        let deadline = Instant::now() + Duration::from_millis(s.fault.duration_ms());
        while Instant::now() < deadline && !cancelled(c) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining < Duration::from_millis(s.request_timeout_ms) {
                sleep_until(deadline, c);
                break;
            }
            if !healthy(
                &client,
                &s.health_url,
                Duration::from_millis(s.request_timeout_ms),
            ) {
                observed = true;
            }
            sleep_until(
                deadline.min(Instant::now() + Duration::from_millis(s.poll_interval_ms)),
                c,
            );
        }
    }
    report.event(start, "restore", "removing only this run's fault");
    if let Err(e) = guard.cleanup() {
        report.report.cleanup = "failed".into();
        report.event(start, "restore", &e);
        return Ok(3);
    }
    report.report.cleanup = "succeeded".into();
    report.event(start, "restore", "connectivity restored");
    if cancelled(c) {
        return Ok(130);
    }
    if outcome != 0 {
        return Ok(outcome);
    }
    if !observed {
        report.event(
            start,
            "assertion",
            "no degraded health response observed during fault; test is inconclusive",
        );
        return Ok(1);
    }
    report.event(start, "recovery", "waiting for healthy HTTP status");
    let recovered = wait_health(
        s,
        &client,
        Instant::now() + Duration::from_millis(s.recovery_timeout_ms),
        c,
    );
    if cancelled(c) {
        return Ok(130);
    }
    if recovered {
        report.event(start, "assertion", "application recovered within deadline");
        Ok(0)
    } else {
        report.event(
            start,
            "assertion",
            "application did not recover within deadline",
        );
        Ok(1)
    }
}
fn xml(s: &str) -> String {
    s.chars()
        .map(|c| {
            if (c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
                || matches!(c, '\u{fffe}' | '\u{ffff}')
            {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_urls_only() {
        for u in [
            "http://example.com",
            "http://localhost:8474",
            "http://192.168.0.1",
            "https://127.0.0.1",
            "http://user@127.0.0.1",
            "http://127.0.0.1/?secret=1",
        ] {
            assert!(local_url(u, false).is_err(), "{u}");
        }
        assert!(local_url("http://127.0.0.1:8080/health", false).is_ok());
        assert!(local_url("http://[::1]:8080/health", false).is_ok());
    }
    #[test]
    fn socket_safety() {
        assert!(local_socket("127.0.0.1:9000"));
        assert!(!local_socket("0.0.0.0:9000"));
        assert!(!local_socket("localhost:9000"));
    }
    #[test]
    fn xml_escaping() {
        assert_eq!(xml("a<&\"'"), "a&lt;&amp;&quot;&apos;");
        assert_eq!(xml("\u{fffe}\u{ffff}\u{1}"), "\u{fffd}\u{fffd}\u{fffd}");
    }
    #[test]
    fn strict_fault_fields() {
        assert!(
            serde_json::from_str::<Fault>(r#"{"kind":"outage","duration_ms":10,"extra":1}"#)
                .is_err()
        );
    }

    fn example_scenario() -> Scenario {
        Scenario::from_json(include_bytes!("../examples/outage.json")).unwrap()
    }

    #[test]
    fn public_scenario_load_and_roundtrip() {
        let scenario = example_scenario();
        assert_eq!(scenario.fault.duration_ms(), 1500);
        let bytes = serde_json::to_vec(&scenario).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scenario.json");
        fs::write(&path, &bytes).unwrap();
        let loaded = Scenario::load(&path).unwrap();
        assert_eq!(loaded.name, scenario.name);
        assert_eq!(loaded.fault, scenario.fault);
        assert_eq!(serde_json::to_vec(&loaded).unwrap(), bytes);
    }

    #[test]
    fn public_config_errors_are_typed_and_bounded() {
        assert!(matches!(
            Scenario::from_json(b"{"),
            Err(ConfigError::InvalidJson { .. })
        ));
        assert!(matches!(
            Scenario::from_json(&vec![b' '; MAX_SCENARIO_BYTES as usize + 1]),
            Err(ConfigError::TooLarge)
        ));
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            Scenario::load(directory.path().join("absent.json")),
            Err(ConfigError::Io {
                operation: "open",
                ..
            })
        ));
        let mut scenario = example_scenario();
        scenario.health_url = "http://example.com/health".into();
        assert!(matches!(
            scenario.validate(),
            Err(ConfigError::InvalidValue(_))
        ));
    }

    #[test]
    fn public_runner_revalidates_programmatic_configuration() {
        let mut scenario = example_scenario();
        scenario.request_timeout_ms = 0;
        let mut observed = Vec::new();
        let error = run_blocking_with_events(&scenario, &AtomicBool::new(false), |event| {
            observed.push(event.clone())
        })
        .unwrap_err();
        assert!(matches!(
            error.kind,
            RunErrorKind::Configuration(ConfigError::InvalidValue(_))
        ));
        assert_eq!(error.report.exit_code, 2);
        assert_eq!(error.report.cleanup, "not_needed");
        assert_eq!(error.report.events, observed);
        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].phase, "error");
    }

    #[test]
    fn public_runner_honors_preexisting_cancellation_without_network() {
        let report = run_blocking(&example_scenario(), &AtomicBool::new(true)).unwrap();
        assert_eq!(report.exit_code, 130);
        assert_eq!(report.outcome, "cancelled");
        assert_eq!(report.cleanup, "not_needed");
        assert!(report.events.is_empty());
    }

    #[test]
    fn public_reports_keep_json_schema_and_junit_escaping() {
        let report = Report::error("sample <&\"", "failure <&", Duration::from_millis(12));
        let json: Value = serde_json::from_slice(&report.to_json_pretty().unwrap()).unwrap();
        assert_eq!(
            json,
            json!({
                "schema_version": 1, "scenario": "sample <&\"", "outcome": "error",
                "exit_code": 2, "cleanup": "not_needed", "elapsed_ms": 12,
                "events": [{ "elapsed_ms": 12, "phase": "error", "detail": "failure <&" }]
            })
        );
        assert!(report
            .to_junit()
            .contains("name=\"sample &lt;&amp;&quot;\""));
        assert!(report.to_junit().contains("error: failure &lt;&amp;"));
    }

    /// Minimal local HTTP contract fixture for exercising the library directly.
    struct LocalProxy {
        url: String,
        enabled: std::sync::Arc<AtomicBool>,
        mutations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        stop: std::sync::Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl LocalProxy {
        fn start() -> Self {
            use std::sync::atomic::AtomicUsize;
            use std::sync::Arc;
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_ip().unwrap();
            let enabled = Arc::new(AtomicBool::new(true));
            let mutations = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let thread_enabled = enabled.clone();
            let thread_mutations = mutations.clone();
            let thread_stop = stop.clone();
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    let Some(mut request) = server.recv_timeout(Duration::from_millis(10)).unwrap()
                    else {
                        continue;
                    };
                    let (status, body) = match (request.method().as_str(), request.url()) {
                        ("GET", "/proxies/reference") => (
                            200,
                            json!({
                                "name": "reference", "listen": address.to_string(),
                                "upstream": address.to_string(), "enabled": true, "toxics": []
                            })
                            .to_string(),
                        ),
                        ("POST", "/proxies/reference") => {
                            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
                            thread_enabled
                                .store(body["enabled"].as_bool().unwrap(), Ordering::SeqCst);
                            thread_mutations.fetch_add(1, Ordering::SeqCst);
                            (200, "{}".into())
                        }
                        ("GET", "/health") => (
                            if thread_enabled.load(Ordering::SeqCst) {
                                200
                            } else {
                                503
                            },
                            String::new(),
                        ),
                        _ => (404, String::new()),
                    };
                    request
                        .respond(tiny_http::Response::from_string(body).with_status_code(status))
                        .unwrap();
                }
            });
            Self {
                url: format!("http://{address}"),
                enabled,
                mutations,
                stop,
                thread: Some(thread),
            }
        }

        fn scenario(&self) -> Scenario {
            Scenario {
                name: "library contract".into(),
                toxiproxy_url: self.url.clone(),
                proxy: "reference".into(),
                health_url: format!("{}/health", self.url),
                fault: Fault::Outage { duration_ms: 400 },
                request_timeout_ms: 150,
                recovery_timeout_ms: 1000,
                poll_interval_ms: 25,
                preflight_timeout_ms: 1000,
            }
        }
    }

    impl Drop for LocalProxy {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn public_runner_delivers_events_and_restores_proxy() {
        let proxy = LocalProxy::start();
        let mut observed = Vec::new();
        let report =
            run_blocking_with_events(&proxy.scenario(), &AtomicBool::new(false), |event| {
                observed.push(event.clone())
            })
            .unwrap();
        assert_eq!(report.exit_code, 0);
        assert_eq!(report.cleanup, "succeeded");
        assert_eq!(report.events, observed);
        assert_eq!(
            observed
                .iter()
                .map(|event| event.phase.as_str())
                .collect::<Vec<_>>(),
            vec![
                "preflight",
                "fault",
                "restore",
                "restore",
                "recovery",
                "assertion"
            ]
        );
        assert!(proxy.enabled.load(Ordering::SeqCst));
        assert_eq!(proxy.mutations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn public_runner_cancellation_after_injection_still_restores() {
        let proxy = LocalProxy::start();
        let cancel = AtomicBool::new(false);
        let report = run_blocking_with_events(&proxy.scenario(), &cancel, |event| {
            if event.phase == "fault" {
                cancel.store(true, Ordering::SeqCst);
            }
        })
        .unwrap();
        assert_eq!(report.exit_code, 130);
        assert_eq!(report.cleanup, "succeeded");
        assert!(proxy.enabled.load(Ordering::SeqCst));
        assert_eq!(proxy.mutations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn public_runner_callback_panic_after_injection_still_restores() {
        let proxy = LocalProxy::start();
        let unrelated = LocalProxy::start();
        let scenario = proxy.scenario();
        let cancel = AtomicBool::new(false);
        let result = std::panic::catch_unwind(|| {
            run_blocking_with_events(&scenario, &cancel, |event| {
                if event.phase == "fault" && event.detail == "injection acknowledged" {
                    panic!("event observer panic");
                }
            })
        });
        let panic = result.expect_err("the observer panic must propagate to the caller");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"event observer panic"));
        assert!(proxy.enabled.load(Ordering::SeqCst));
        assert_eq!(proxy.mutations.load(Ordering::SeqCst), 2);
        assert!(unrelated.enabled.load(Ordering::SeqCst));
        assert_eq!(unrelated.mutations.load(Ordering::SeqCst), 0);
    }
}
