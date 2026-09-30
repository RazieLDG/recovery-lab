//! Fixture and mock-contract tests. The HTTP mock tests lifecycle/reporting;
//! real TCP fault behavior is exercised separately in real_toxiproxy.rs.
#![cfg(test)]

mod common;

use common::*;
use recovery_lab::fixture::{wait_healthy, AppConfig, Backend, Mode, ReferenceApp};
use serde_json::{json, Value};
use std::{fs, thread, time::Duration};
use tempfile::TempDir;

fn healthy(app: &ReferenceApp) {
    wait_healthy(&format!("{}/healthz", app.url()), Duration::from_secs(3)).unwrap();
}
fn app_at(backend_url: String, mode: Mode) -> ReferenceApp {
    let app = ReferenceApp::start(AppConfig {
        backend_url,
        mode,
        ..AppConfig::default()
    })
    .unwrap();
    healthy(&app);
    app
}
struct Rig {
    proxy: MockToxiproxy,
    backend: Backend,
    directory: TempDir,
    run_number: usize,
}
impl Rig {
    fn new() -> Self {
        let backend = Backend::start("127.0.0.1", 0).unwrap();
        let proxy = MockToxiproxy::start(&backend.url());
        Self {
            proxy,
            backend,
            directory: tempfile::tempdir().unwrap(),
            run_number: 0,
        }
    }
    fn app(&self, mode: Mode) -> ReferenceApp {
        app_at(format!("{}/work", self.proxy.proxy_url()), mode)
    }
    fn config(&self, app: &ReferenceApp, kind: &str) -> Value {
        scenario(self.proxy.url(), &format!("{}/healthz", app.url()), kind)
    }
    fn run(&mut self, config: &Value) -> (i32, String, CliRun) {
        self.run_number += 1;
        let mut run = CliRun::launch_with_temp(
            &self.directory.path().join(self.run_number.to_string()),
            &self.directory.path().join("process-tmp"),
            config,
        );
        let (code, output) = run.finish();
        (code, output, run)
    }
}

#[test]
fn fixture_backend_and_retry_app_are_healthy() {
    let backend = Backend::start("127.0.0.1", 0).unwrap();
    let app = app_at(format!("{}/work", backend.url()), Mode::Retry);
    let state = request_json(&format!("{}/state", app.url()), "GET", None);
    assert_eq!(state["healthy"], true);
    assert!(state["successes"].as_u64().unwrap() > 0);
}

#[test]
fn fixture_retry_recovers_and_latch_remains_failed() {
    for mode in [Mode::Retry, Mode::Latch] {
        let rig = Rig::new();
        let app = rig.app(mode);
        let endpoint = format!("{}/proxies/reference", rig.proxy.url());
        request_json(&endpoint, "POST", Some(&json!({"enabled":false})));
        wait_until(
            Duration::from_secs(2),
            "application never observed the dependency outage",
            || app.state().failures > 0,
        );
        request_json(&endpoint, "POST", Some(&json!({"enabled":true})));
        if matches!(mode, Mode::Retry) {
            healthy(&app);
        } else {
            thread::sleep(Duration::from_millis(100));
            assert!(app.state().latched);
            assert!(!app.state().healthy);
        }
    }
}

#[test]
fn mock_all_faults_recover_and_restore_proxy() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    for kind in ["outage", "latency", "reset_peer"] {
        let before = rig.proxy.snapshot();
        let failures_before = app.state().failures;
        let (code, output, run) = rig.run(&rig.config(&app, kind));
        assert_eq!(code, 0, "{kind}: {output}");
        assert_eq!(rig.proxy.snapshot(), before);
        let report = check_reports(&run, false);
        assert_eq!(report["cleanup"], "succeeded");
        assert!(app.state().failures > failures_before);
        healthy(&app);
    }
}

#[test]
fn mock_nonrecoverable_app_fails_but_proxy_is_restored() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Latch);
    let before = rig.proxy.snapshot();
    let (code, output, run) = rig.run(&rig.config(&app, "outage"));
    assert_eq!(code, 1, "{output}");
    assert_eq!(rig.proxy.snapshot(), before);
    assert!(app.state().latched);
    check_reports(&run, true);
}

fn user_toxic() -> Value {
    json!({"name":"user-owned", "type":"latency", "stream":"downstream",
        "toxicity":1.0, "attributes":{"latency":1,"jitter":0}})
}

#[test]
fn mock_rejects_preexisting_toxic_without_changing_it() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    rig.proxy.state.lock().unwrap().proxy["toxics"]
        .as_array_mut()
        .unwrap()
        .push(user_toxic());
    let before = rig.proxy.snapshot();
    let (code, output, run) = rig.run(&rig.config(&app, "latency"));
    assert_eq!(code, 2, "{output}");
    assert_eq!(rig.proxy.snapshot(), before);
    assert!(rig.proxy.events().is_empty());
    check_reports(&run, true);
}

#[test]
fn mock_existing_report_destinations_rejected_without_mutation() {
    let rig = Rig::new();
    let app = rig.app(Mode::Retry);
    for name in ["report.json", "report.xml"] {
        let directory = rig.directory.path().join(name.replace('.', "-"));
        fs::create_dir(&directory).unwrap();
        let existing = directory.join(name);
        fs::write(&existing, "previous evidence must survive").unwrap();
        let before = rig.proxy.snapshot();
        let mut run = CliRun::launch(&directory, &rig.config(&app, "outage"));
        let (code, output) = run.finish();
        assert_eq!(code, 2, "{output}");
        assert_eq!(
            fs::read_to_string(existing).unwrap(),
            "previous evidence must survive"
        );
        assert!(rig.proxy.events().is_empty());
        assert_eq!(rig.proxy.snapshot(), before);
    }
}

#[test]
fn mock_fault_window_too_short_rejected_without_mutation() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    for duration in [150, 174] {
        let mut config = rig.config(&app, "outage");
        config["fault"]["duration_ms"] = json!(duration);
        let before = rig.proxy.snapshot();
        let (code, output, run) = rig.run(&config);
        assert_eq!(code, 2, "{output}");
        assert!(rig.proxy.events().is_empty());
        assert_eq!(rig.proxy.snapshot(), before);
        check_reports(&run, true);
    }
}

#[test]
fn mock_fault_window_exact_minimum_is_accepted() {
    let mut rig = Rig::new();
    // Bypass the dependency: accepted configuration, then failed assertion.
    let mut config = scenario(
        rig.proxy.url(),
        &format!("{}/healthz", rig.backend.url()),
        "outage",
    );
    config["fault"]["duration_ms"] = json!(
        config["request_timeout_ms"].as_u64().unwrap()
            + config["poll_interval_ms"].as_u64().unwrap()
    );
    let before = rig.proxy.snapshot();
    let (code, output, run) = rig.run(&config);
    assert_eq!(code, 1, "{output}");
    assert!(!rig.proxy.events().is_empty());
    assert_eq!(rig.proxy.snapshot(), before);
    check_reports(&run, true);
}

#[test]
fn mock_xml_noncharacters_are_sanitized_in_junit() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    let name = "xml <&\"'\u{fffe}\u{ffff}";
    let mut config = rig.config(&app, "outage");
    config["name"] = json!(name);
    let (code, output, run) = rig.run(&config);
    assert_eq!(code, 0, "{output}");
    assert_eq!(check_reports(&run, false)["scenario"], name);
    assert_eq!(
        parse_junit(&run.junit_path).testcase_names,
        vec![name.replace(['\u{fffe}', '\u{ffff}'], "\u{fffd}")]
    );
}

#[test]
fn mock_late_healthy_response_does_not_pass_deadline() {
    let mut rig = Rig::new();
    let late = HttpServer::start(|_| {
        thread::sleep(Duration::from_millis(120));
        HttpReply::Json(200, json!({"ok":true}))
    });
    let mut config = scenario(
        rig.proxy.url(),
        &format!("{}/healthz", late.url()),
        "outage",
    );
    config["preflight_timeout_ms"] = json!(60);
    let before = rig.proxy.snapshot();
    let (code, output, run) = rig.run(&config);
    assert_eq!(code, 1, "{output}");
    assert!(rig.proxy.events().is_empty());
    assert_eq!(rig.proxy.snapshot(), before);
    assert_eq!(check_reports(&run, true)["cleanup"], "not_needed");
}

#[test]
fn mock_unknown_field_rejected_without_mutation() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    let mut config = rig.config(&app, "outage");
    config["duraton_ms"] = json!(100);
    let before = rig.proxy.snapshot();
    let (code, output, _) = rig.run(&config);
    assert_eq!(code, 2, "{output}");
    assert!(rig.proxy.events().is_empty());
    assert_eq!(rig.proxy.snapshot(), before);
}

#[test]
fn mock_unknown_fault_field_rejected_without_mutation() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    let mut config = rig.config(&app, "outage");
    config["fault"]["duraton_ms"] = json!(100);
    let before = rig.proxy.snapshot();
    let (code, output, _) = rig.run(&config);
    assert_eq!(code, 2, "{output}");
    assert!(rig.proxy.events().is_empty());
    assert_eq!(rig.proxy.snapshot(), before);
}

#[test]
fn mock_remote_targets_rejected_by_default() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    // Both remote API and remote health targets must be rejected locally.
    for remote_api in [true, false] {
        let mut config = rig.config(&app, "outage");
        if remote_api {
            config["toxiproxy_url"] = json!("http://192.0.2.1:8474");
        } else {
            config["health_url"] = json!("http://192.0.2.1:8080/healthz");
        }
        let before = rig.proxy.snapshot();
        let (code, output, _) = rig.run(&config);
        assert_eq!(code, 2, "{output}");
        assert!(rig.proxy.events().is_empty());
        assert_eq!(rig.proxy.snapshot(), before);
    }
}

#[test]
fn mock_preexisting_disabled_proxy_is_rejected() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    rig.proxy.state.lock().unwrap().proxy["enabled"] = json!(false);
    let before = rig.proxy.snapshot();
    let (code, output, run) = rig.run(&rig.config(&app, "outage"));
    assert_eq!(code, 2, "{output}");
    assert_eq!(rig.proxy.snapshot(), before);
    assert!(rig.proxy.events().is_empty());
    check_reports(&run, true);
}

#[test]
fn mock_mutation_response_loss_still_cleans_up() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    let before = rig.proxy.snapshot();
    rig.proxy.state.lock().unwrap().fail_mutation_once = true;
    let (code, output, run) = rig.run(&rig.config(&app, "latency"));
    assert_eq!(code, 2, "{output}");
    assert_eq!(rig.proxy.snapshot(), before);
    assert_eq!(check_reports(&run, true)["cleanup"], "succeeded");
    let events = rig.proxy.events();
    assert!(events.iter().any(|event| event[0] == "add"));
    assert!(events.iter().any(|event| event[0] == "delete"));
}

#[test]
fn mock_no_observable_fault_is_inconclusive_failure() {
    let mut rig = Rig::new();
    let config = scenario(
        rig.proxy.url(),
        &format!("{}/healthz", rig.backend.url()),
        "outage",
    );
    let before = rig.proxy.snapshot();
    let (code, output, run) = rig.run(&config);
    assert_eq!(code, 1, "{output}");
    assert_eq!(rig.proxy.snapshot(), before);
    check_reports(&run, true);
}

#[test]
fn mock_cleanup_failure_has_dedicated_exit() {
    let mut rig = Rig::new();
    let app = rig.app(Mode::Retry);
    rig.proxy.state.lock().unwrap().fail_delete = true;
    let (code, output, run) = rig.run(&rig.config(&app, "latency"));
    assert_eq!(code, 3, "{output}");
    assert!(!rig.proxy.snapshot()["toxics"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(check_reports(&run, true)["cleanup"], "failed");
}

#[cfg(unix)]
#[test]
fn mock_sigint_restores_outage() {
    let rig = Rig::new();
    let app = rig.app(Mode::Retry);
    let mut config = rig.config(&app, "outage");
    config["fault"]["duration_ms"] = json!(10000);
    let before = rig.proxy.snapshot();
    let mut run = CliRun::launch(rig.directory.path(), &config);
    wait_for_fault(rig.proxy.url());
    run.signal(nix::sys::signal::Signal::SIGINT);
    let (code, output) = run.finish();
    assert_eq!(code, 130, "{output}");
    assert_eq!(rig.proxy.snapshot(), before);
    assert_eq!(check_reports(&run, true)["cleanup"], "succeeded");
    healthy(&app);
}

#[cfg(unix)]
#[test]
fn mock_sigint_removes_only_owned_toxic() {
    let rig = Rig::new();
    let app = rig.app(Mode::Retry);
    let mut config = rig.config(&app, "latency");
    config["fault"]["duration_ms"] = json!(10000);
    let mut expected = rig.proxy.snapshot();
    let mut run = CliRun::launch(rig.directory.path(), &config);
    wait_for_fault(rig.proxy.url());
    // Another actor can add a toxic after preflight. Cleanup owns only its toxic.
    let unrelated = user_toxic();
    request_json(
        &format!("{}/proxies/reference/toxics", rig.proxy.url()),
        "POST",
        Some(&unrelated),
    );
    expected["toxics"].as_array_mut().unwrap().push(unrelated);
    run.signal(nix::sys::signal::Signal::SIGINT);
    let (code, output) = run.finish();
    assert_eq!(code, 130, "{output}");
    assert_eq!(rig.proxy.snapshot(), expected);
    assert_eq!(check_reports(&run, true)["cleanup"], "succeeded");
    healthy(&app);
}
