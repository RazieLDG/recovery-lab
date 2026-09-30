//! A dependency HTTP server and an application that polls that dependency.
//!
//! Both handles start immediately and shut down their worker threads on drop.
//! Bind to port zero to let the operating system choose a free test port.

use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Response, Server, StatusCode};

pub type FixtureResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
const MAX_DEPENDENCY_BODY: u64 = 65_536;

fn loopback_host(host: &str) -> FixtureResult<IpAddr> {
    let address: IpAddr = host.parse().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture host must be a literal loopback IP",
        )
    })?;
    if !address.is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "fixture host must be a literal loopback IP",
        )
        .into());
    }
    Ok(address)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Retry,
    Latch,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Retry => "retry",
            Self::Latch => "latch",
        })
    }
}

impl FromStr for Mode {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "retry" => Ok(Self::Retry),
            "latch" => Ok(Self::Latch),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mode must be retry or latch",
            )),
        }
    }
}

/// Configuration for a polling application. Defaults use an ephemeral listener
/// and the demo's proxied dependency at `127.0.0.1:18082`.
#[derive(Clone, Debug)]
pub struct AppConfig {
    pub backend_url: String,
    pub mode: Mode,
    pub host: String,
    pub port: u16,
    pub request_timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            backend_url: "http://127.0.0.1:18082/work".into(),
            mode: Mode::Retry,
            host: "127.0.0.1".into(),
            port: 0,
            request_timeout: Duration::from_millis(100),
            poll_interval: Duration::from_millis(25),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AppState {
    pub healthy: bool,
    pub latched: bool,
    pub mode: Mode,
    pub attempts: u64,
    pub successes: u64,
    pub failures: u64,
    pub last_error: Option<String>,
}

impl AppState {
    fn new(mode: Mode) -> Self {
        Self {
            healthy: false,
            latched: false,
            mode,
            attempts: 0,
            successes: 0,
            failures: 0,
            last_error: None,
        }
    }

    fn observe(&mut self, result: Result<(), String>) {
        self.attempts += 1;
        match result {
            Ok(()) => {
                self.successes += 1;
                self.healthy = !self.latched;
                self.last_error = None;
            }
            Err(error) => {
                self.failures += 1;
                self.latched |= self.mode == Mode::Latch;
                self.healthy = false;
                self.last_error = Some(error);
            }
        }
    }
}

struct RunningHttp {
    server: Option<Arc<Server>>,
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RunningHttp {
    fn start<F>(host: &str, port: u16, handler: F) -> FixtureResult<Self>
    where
        F: Fn(&str) -> (u16, Value) + Send + 'static,
    {
        let address = SocketAddr::new(loopback_host(host)?, port);
        let server = Arc::new(Server::http(address)?);
        let address = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| io::Error::other("fixture did not bind a TCP listener"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_server = Arc::clone(&server);
        let worker_stop = Arc::clone(&stop);
        let worker = thread::Builder::new()
            .name("recovery-fixture-http".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    let request = match worker_server.recv_timeout(Duration::from_millis(25)) {
                        Ok(Some(request)) => request,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    let (status, value) = if request.method() == &Method::Get {
                        handler(request.url())
                    } else {
                        (405, json!({"error": "method not allowed"}))
                    };
                    let response = Response::from_string(value.to_string())
                        .with_status_code(StatusCode(status))
                        .with_header(
                            Header::from_bytes("Content-Type", "application/json")
                                .expect("constant valid HTTP header"),
                        );
                    // Clients timing out or closing during a fault are expected.
                    let _ = request.respond(response);
                }
            })?;
        Ok(Self {
            server: Some(server),
            address,
            stop,
            worker: Some(worker),
        })
    }

    fn url(&self) -> String {
        format!("http://{}", self.address)
    }

    fn close(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(server) = &self.server {
            server.unblock();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.server.take();
    }
}

impl Drop for RunningHttp {
    fn drop(&mut self) {
        self.close();
    }
}

/// A small HTTP dependency serving `/work` and `/healthz` with status 200.
pub struct Backend {
    http: RunningHttp,
}

impl Backend {
    pub fn start(host: &str, port: u16) -> FixtureResult<Self> {
        Ok(Self {
            http: RunningHttp::start(host, port, |path| match path {
                "/work" | "/healthz" => (200, json!({"ok": true, "service": "backend"})),
                _ => (404, json!({"error": "not found"})),
            })?,
        })
    }

    pub fn url(&self) -> String {
        self.http.url()
    }

    pub fn address(&self) -> SocketAddr {
        self.http.address
    }

    pub fn close(&mut self) {
        self.http.close();
    }
}

/// A background dependency poller whose health follows successful requests.
/// In latch mode the first failed request makes health permanently fail.
pub struct ReferenceApp {
    http: RunningHttp,
    state: Arc<Mutex<AppState>>,
    stop: Option<mpsc::Sender<()>>,
    poller: Option<JoinHandle<()>>,
}

impl ReferenceApp {
    pub fn start(config: AppConfig) -> FixtureResult<Self> {
        let allowed = Duration::from_millis(1)..=Duration::from_secs(10);
        if !allowed.contains(&config.request_timeout) || !allowed.contains(&config.poll_interval) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "timeouts and polling intervals must be from 1 to 10000 milliseconds",
            )
            .into());
        }
        let backend_url = reqwest::Url::parse(&config.backend_url)?;
        let host = backend_url.host_str().unwrap_or_default();
        loopback_host(host.trim_matches(['[', ']']))?;
        if backend_url.scheme() != "http"
            || !backend_url.username().is_empty()
            || backend_url.password().is_some()
            || backend_url.query().is_some()
            || backend_url.fragment().is_some()
            || backend_url.port() == Some(0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "fixture backend must use HTTP without credentials, query, fragment or port zero",
            )
            .into());
        }
        // Local test traffic must not escape through environment proxy settings.
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(config.request_timeout)
            .build()?;
        let state = Arc::new(Mutex::new(AppState::new(config.mode)));
        let http_state = Arc::clone(&state);
        let http = RunningHttp::start(&config.host, config.port, move |path| {
            let snapshot = http_state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            match path {
                "/healthz" | "/health" => {
                    (if snapshot.healthy { 200 } else { 503 }, json!(snapshot))
                }
                "/state" => (200, json!(snapshot)),
                _ => (404, json!({"error": "not found"})),
            }
        })?;
        let (stop, stopped) = mpsc::channel();
        let poll_state = Arc::clone(&state);
        let poller = thread::Builder::new()
            .name("recovery-fixture-poller".into())
            .spawn(move || loop {
                if stopped.try_recv() != Err(mpsc::TryRecvError::Empty) {
                    break;
                }
                let result = poll_backend(&client, backend_url.clone());
                poll_state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .observe(result);
                if stopped.recv_timeout(config.poll_interval)
                    != Err(mpsc::RecvTimeoutError::Timeout)
                {
                    break;
                }
            })?;
        Ok(Self {
            http,
            state,
            stop: Some(stop),
            poller: Some(poller),
        })
    }

    pub fn url(&self) -> String {
        self.http.url()
    }

    pub fn address(&self) -> SocketAddr {
        self.http.address
    }

    pub fn state(&self) -> AppState {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Stop polling and join all fixture workers. Safe to call more than once.
    pub fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(poller) = self.poller.take() {
            let _ = poller.join();
        }
        self.http.close();
    }
}

impl Drop for ReferenceApp {
    fn drop(&mut self) {
        self.close();
    }
}

fn poll_backend(client: &Client, url: reqwest::Url) -> Result<(), String> {
    let response = client.get(url).send().map_err(|error| {
        if error.is_timeout() {
            "dependency request timed out".to_owned()
        } else {
            "dependency request failed".to_owned()
        }
    })?;
    let status = response.status();
    // Consume the body as well as headers: downstream latency can delay either.
    // Discard it without retaining an unbounded body in memory.
    let received = io::copy(&mut response.take(MAX_DEPENDENCY_BODY + 1), &mut io::sink())
        .map_err(|_| "dependency response body failed or timed out".to_owned())?;
    if received > MAX_DEPENDENCY_BODY {
        return Err("dependency body exceeds 64 KiB".into());
    }
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", status.as_u16()))
    }
}

/// Wait for a 200 health response, bounded by a monotonic overall deadline.
pub fn wait_healthy(url: &str, timeout: Duration) -> FixtureResult<()> {
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .build()?;
    let started = Instant::now();
    while started.elapsed() < timeout {
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        if let Ok(response) = client
            .get(url)
            .timeout(remaining.min(Duration::from_millis(250)))
            .send()
        {
            if response.status().as_u16() == 200 && started.elapsed() < timeout {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(25).min(timeout.saturating_sub(started.elapsed())));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!("service did not become healthy: {url}"),
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_recovers_but_latch_stays_unhealthy() {
        for mode in [Mode::Retry, Mode::Latch] {
            let mut state = AppState::new(mode);
            state.observe(Ok(()));
            assert!(state.healthy);
            state.observe(Err("unavailable".into()));
            assert!(!state.healthy);
            state.observe(Ok(()));
            assert_eq!(state.healthy, mode == Mode::Retry);
            assert_eq!(state.latched, mode == Mode::Latch);
            assert_eq!((state.attempts, state.successes, state.failures), (3, 2, 1));
            assert_eq!(state.last_error, None);
        }
    }

    #[test]
    fn rejects_invalid_mode_and_out_of_bounds_intervals() {
        assert!("other".parse::<Mode>().is_err());
        for config in [
            AppConfig {
                request_timeout: Duration::ZERO,
                ..AppConfig::default()
            },
            AppConfig {
                poll_interval: Duration::ZERO,
                ..AppConfig::default()
            },
            AppConfig {
                request_timeout: Duration::from_millis(10_001),
                ..AppConfig::default()
            },
            AppConfig {
                poll_interval: Duration::from_millis(10_001),
                ..AppConfig::default()
            },
            AppConfig {
                request_timeout: Duration::from_nanos(1),
                ..AppConfig::default()
            },
        ] {
            assert!(ReferenceApp::start(config).is_err());
        }
    }

    #[test]
    fn rejects_remote_hosts_and_unsafe_dependency_urls() {
        for host in ["0.0.0.0", "::", "192.0.2.1", "localhost"] {
            assert!(Backend::start(host, 0).is_err(), "{host}");
            assert!(ReferenceApp::start(AppConfig {
                host: host.into(),
                ..AppConfig::default()
            })
            .is_err());
        }
        for url in [
            "http://192.0.2.1/work",
            "http://localhost/work",
            "https://127.0.0.1/work",
            "http://user:password@127.0.0.1/work",
            "http://127.0.0.1/work?secret=value",
            "http://127.0.0.1/work#fragment",
            "http://127.0.0.1:0/work",
        ] {
            assert!(
                ReferenceApp::start(AppConfig {
                    backend_url: url.into(),
                    ..AppConfig::default()
                })
                .is_err(),
                "{url}"
            );
        }
    }

    #[test]
    fn backend_and_application_serve_health_and_state() {
        let backend = Backend::start("127.0.0.1", 0).unwrap();
        let app = ReferenceApp::start(AppConfig {
            backend_url: format!("{}/work", backend.url()),
            ..AppConfig::default()
        })
        .unwrap();
        wait_healthy(&format!("{}/healthz", app.url()), Duration::from_secs(2)).unwrap();
        let client = Client::builder().no_proxy().build().unwrap();
        let state: AppState = client
            .get(format!("{}/state", app.url()))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert!(state.healthy);
        assert!(state.successes > 0);
        assert_eq!(state.failures, 0);
        assert_eq!(
            client
                .get(format!("{}/missing", backend.url()))
                .send()
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn close_interrupts_long_poll_interval_and_releases_listener() {
        let backend = Backend::start("127.0.0.1", 0).unwrap();
        let mut app = ReferenceApp::start(AppConfig {
            backend_url: format!("{}/work", backend.url()),
            poll_interval: Duration::from_secs(10),
            ..AppConfig::default()
        })
        .unwrap();
        wait_healthy(&format!("{}/healthz", app.url()), Duration::from_secs(2)).unwrap();
        let address = app.address();
        let started = Instant::now();
        app.close();
        app.close();
        assert!(started.elapsed() < Duration::from_secs(1));
        // tiny_http's own accept thread releases its listener asynchronously.
        let deadline = Instant::now() + Duration::from_secs(1);
        while std::net::TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_ok() {
            assert!(
                Instant::now() < deadline,
                "closed fixture still accepts connections"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn oversized_dependency_body_is_unhealthy() {
        let backend = RunningHttp::start("127.0.0.1", 0, |_| {
            (
                200,
                json!({"oversized": "x".repeat(MAX_DEPENDENCY_BODY as usize)}),
            )
        })
        .unwrap();
        let app = ReferenceApp::start(AppConfig {
            backend_url: format!("{}/work", backend.url()),
            ..AppConfig::default()
        })
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.state().attempts == 0 {
            assert!(Instant::now() < deadline, "dependency was never polled");
            thread::sleep(Duration::from_millis(10));
        }
        let state = app.state();
        assert!(!state.healthy);
        assert_eq!(state.successes, 0);
        assert_eq!(
            state.last_error.as_deref(),
            Some("dependency body exceeds 64 KiB")
        );
    }
}
