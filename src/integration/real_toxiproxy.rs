//! Real TCP fault integration. Opt in explicitly; a missing executable fails:
//! TOXIPROXY_BIN=/path/to/toxiproxy-server cargo test --test real_toxiproxy -- --ignored --test-threads=1
#![cfg(test)]

mod common;

use common::*;
use recovery_lab::fixture::{wait_healthy, AppConfig, Backend, Mode, ReferenceApp};
use serde_json::{json, Value};
use std::time::Duration;
use tempfile::TempDir;

struct RealRig {
    proxy: ToxiproxyProcess,
    _backend: Backend,
    directory: TempDir,
    proxy_port: u16,
    before: Value,
    unrelated_before: Value,
}
impl RealRig {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::start("127.0.0.1", 0).unwrap();
        let proxy = ToxiproxyProcess::start(&directory.path().join("toxiproxy.log"));
        let proxy_port = create_proxy(&proxy.url, "reference", &backend.url());
        create_proxy(&proxy.url, "unrelated", &backend.url());
        request_json(
            &format!("{}/proxies/unrelated/toxics", proxy.url),
            "POST",
            Some(&json!({
                "name":"user-owned", "type":"latency", "stream":"downstream", "toxicity":1.0,
                "attributes":{"latency":1,"jitter":0}
            })),
        );
        let before = request_json(&format!("{}/proxies/reference", proxy.url), "GET", None);
        let unrelated_before =
            request_json(&format!("{}/proxies/unrelated", proxy.url), "GET", None);
        Self {
            proxy,
            _backend: backend,
            directory,
            proxy_port,
            before,
            unrelated_before,
        }
    }
    fn app(&self, mode: Mode) -> ReferenceApp {
        let app = ReferenceApp::start(AppConfig {
            backend_url: format!("http://127.0.0.1:{}/work", self.proxy_port),
            mode,
            ..AppConfig::default()
        })
        .unwrap();
        healthy(&app);
        app
    }
    fn assert_restored(&self) {
        assert_eq!(
            request_json(
                &format!("{}/proxies/reference", self.proxy.url),
                "GET",
                None
            ),
            self.before
        );
        assert_eq!(
            request_json(
                &format!("{}/proxies/unrelated", self.proxy.url),
                "GET",
                None
            ),
            self.unrelated_before,
            "cleanup must not change another proxy or its user-owned toxic"
        );
    }
}
fn healthy(app: &ReferenceApp) {
    wait_healthy(&format!("{}/healthz", app.url()), Duration::from_secs(3)).unwrap();
}
fn create_proxy(api_url: &str, name: &str, backend_url: &str) -> u16 {
    // The external Toxiproxy process has to bind this port itself. Retry the
    // unavoidable release-to-bind race rather than relying on fixed ports.
    for attempt in 0..3 {
        let port = free_port();
        let response = client()
            .post(format!("{api_url}/proxies"))
            .json(&json!({
                "name":name, "listen":format!("127.0.0.1:{port}"),
                "upstream":backend_url.trim_start_matches("http://"), "enabled":true
            }))
            .send()
            .unwrap();
        if response.status().is_success() {
            return port;
        }
        let status = response.status();
        let body = response.text().unwrap();
        assert!(
            attempt < 2 && body.to_lowercase().contains("address already in use"),
            "proxy creation failed: {status} {body}"
        );
    }
    unreachable!()
}
fn exercise(kind: &str, mode: Mode) {
    let rig = RealRig::new();
    let app = rig.app(mode);
    let config = scenario(&rig.proxy.url, &format!("{}/healthz", app.url()), kind);
    let mut run = CliRun::launch(rig.directory.path(), &config);
    let (code, output) = run.finish();
    let expected = if matches!(mode, Mode::Latch) { 1 } else { 0 };
    assert_eq!(code, expected, "{kind}: {output}");
    rig.assert_restored();
    assert_eq!(check_reports(&run, expected != 0)["cleanup"], "succeeded");
    assert!(
        app.state().failures > 0,
        "application must actually observe a dependency failure"
    );
    if matches!(mode, Mode::Latch) {
        assert!(app.state().latched);
    } else {
        healthy(&app);
    }
}

#[test]
#[ignore = "requires installed Toxiproxy; set TOXIPROXY_BIN and run --ignored"]
fn real_outage_recovers() {
    exercise("outage", Mode::Retry);
}

#[test]
#[ignore = "requires installed Toxiproxy; set TOXIPROXY_BIN and run --ignored"]
fn real_latency_recovers() {
    exercise("latency", Mode::Retry);
}

#[test]
#[ignore = "requires installed Toxiproxy; set TOXIPROXY_BIN and run --ignored"]
fn real_reset_peer_recovers() {
    exercise("reset_peer", Mode::Retry);
}

#[test]
#[ignore = "requires installed Toxiproxy; set TOXIPROXY_BIN and run --ignored"]
fn real_nonrecoverable_app_fails() {
    exercise("outage", Mode::Latch);
}

#[cfg(unix)]
fn exercise_cancellation(kind: &str, signal: nix::sys::signal::Signal) {
    let rig = RealRig::new();
    let app = rig.app(Mode::Retry);
    let mut config = scenario(&rig.proxy.url, &format!("{}/healthz", app.url()), kind);
    config["fault"]["duration_ms"] = json!(10000);
    let mut run = CliRun::launch(rig.directory.path(), &config);
    wait_for_fault(&rig.proxy.url);
    run.signal(signal);
    let (code, output) = run.finish();
    assert_eq!(code, 130, "{output}");
    rig.assert_restored();
    assert_eq!(check_reports(&run, true)["cleanup"], "succeeded");
    healthy(&app);
}

#[cfg(unix)]
#[test]
#[ignore = "requires installed Toxiproxy and POSIX signals; run --ignored"]
fn real_sigint_restores_connectivity() {
    exercise_cancellation("outage", nix::sys::signal::Signal::SIGINT);
}

#[cfg(unix)]
#[test]
#[ignore = "requires installed Toxiproxy and POSIX signals; run --ignored"]
fn real_sigterm_removes_latency_toxic() {
    exercise_cancellation("latency", nix::sys::signal::Signal::SIGTERM);
}

#[test]
#[ignore = "requires installed Toxiproxy; set TOXIPROXY_BIN and run --ignored"]
fn real_fixture_proxy_helper_creates_without_overwriting() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::start("127.0.0.1", 0).unwrap();
    let proxy = ToxiproxyProcess::start(&directory.path().join("toxiproxy.log"));
    let listen = format!("127.0.0.1:{}", free_port());
    let upstream = backend.url().trim_start_matches("http://").to_owned();
    let mut run = CliRun::launch_fixture(
        directory.path(),
        &[
            "proxy",
            "--api",
            &proxy.url,
            "--name",
            "helper-test",
            "--listen",
            &listen,
            "--upstream",
            &upstream,
        ],
    );
    let (code, output) = run.finish();
    assert_eq!(code, 0, "{output}");
    let endpoint = format!("{}/proxies/helper-test", proxy.url);
    let expected = request_json(&endpoint, "GET", None);
    assert_eq!(expected["name"], "helper-test");
    assert_eq!(expected["listen"], listen);
    assert_eq!(expected["upstream"], upstream);
    assert_eq!(expected["enabled"], true);
    assert_eq!(expected["toxics"], json!([]));
    assert_eq!(
        request_json(&format!("http://{listen}/work"), "GET", None)["ok"],
        true
    );

    // Change requested parameters so an accidental upsert would be observable.
    let conflicting_listen = format!("127.0.0.1:{}", free_port());
    let mut conflict = CliRun::launch_fixture(
        directory.path(),
        &[
            "proxy",
            "--api",
            &proxy.url,
            "--name",
            "helper-test",
            "--listen",
            &conflicting_listen,
            "--upstream",
            &upstream,
        ],
    );
    let (code, output) = conflict.finish();
    assert_eq!(code, 2, "{output}");
    assert_eq!(
        request_json(&endpoint, "GET", None),
        expected,
        "existing proxy must survive a conflicting helper request unchanged"
    );
    assert_eq!(
        request_json(&format!("http://{listen}/work"), "GET", None)["ok"],
        true
    );
}
