use reqwest::blocking::Client;
use reqwest::{redirect::Policy, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_SCENARIO: u64 = 65536;
const MAX_API_BODY: u64 = 65536;
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    toxiproxy_url: String,
    proxy: String,
    health_url: String,
    fault: Fault,
    request_timeout_ms: u64,
    recovery_timeout_ms: u64,
    poll_interval_ms: u64,
    preflight_timeout_ms: u64,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Fault {
    Outage { duration_ms: u64 },
    Latency { duration_ms: u64, latency_ms: u64 },
    ResetPeer { duration_ms: u64 },
}
impl Fault {
    fn duration(&self) -> u64 {
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
#[derive(Serialize)]
struct Event {
    elapsed_ms: u128,
    phase: String,
    detail: String,
}
#[derive(Serialize)]
struct Report {
    schema_version: u8,
    scenario: String,
    outcome: String,
    exit_code: i32,
    cleanup: String,
    elapsed_ms: u128,
    events: Vec<Event>,
}
impl Report {
    fn event(&mut self, start: Instant, phase: &str, detail: &str) {
        let elapsed_ms = start.elapsed().as_millis();
        eprintln!("{elapsed_ms:>7} ms  {phase}: {detail}");
        self.events.push(Event {
            elapsed_ms,
            phase: phase.into(),
            detail: detail.into(),
        });
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
        ("fault.duration_ms", s.fault.duration(), 10, 60000),
    ] {
        if !(min..=max).contains(&v) {
            return Err(format!("{n} must be in {min}..={max}"));
        }
    }
    if s.fault.duration() < s.request_timeout_ms + s.poll_interval_ms {
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
fn load(path: &Path) -> Result<Scenario, String> {
    let f = fs::File::open(path).map_err(|e| format!("cannot open scenario: {e}"))?;
    let mut bytes = Vec::new();
    f.take(MAX_SCENARIO + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read scenario")?;
    if bytes.len() as u64 > MAX_SCENARIO {
        return Err("scenario exceeds 64 KiB".into());
    }
    let s = serde_json::from_slice::<Scenario>(&bytes).map_err(|e| {
        format!(
            "invalid scenario JSON at line {}, column {}",
            e.line(),
            e.column()
        )
    })?;
    validate(&s)?;
    Ok(s)
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
fn run(s: &Scenario, c: &AtomicBool, report: &mut Report, start: Instant) -> Result<i32, String> {
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
        let deadline = Instant::now() + Duration::from_millis(s.fault.duration());
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
        report.cleanup = "failed".into();
        report.event(start, "restore", &e);
        return Ok(3);
    }
    report.cleanup = "succeeded".into();
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
fn reserve_report(path: &Option<PathBuf>) -> Result<Option<fs::File>, String> {
    path.as_ref()
        .map(|p| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(p)
                .map_err(|e| format!("cannot create report {}: {e}", p.display()))
        })
        .transpose()
}
fn write_report(file: &mut fs::File, bytes: &[u8]) -> Result<(), String> {
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("cannot write report: {e}"))
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        println!("recovery-lab run SCENARIO.json [--json PATH] [--junit PATH]\nOnly literal loopback HTTP targets are accepted. Output files must not exist.");
        return;
    }
    if args.len() < 2 || args[0] != "run" {
        eprintln!("usage: recovery-lab run SCENARIO.json [--json PATH] [--junit PATH]");
        std::process::exit(2);
    }
    let mut json_path = None;
    let mut junit_path = None;
    let mut i = 2;
    while i < args.len() {
        if i + 1 >= args.len() {
            eprintln!("missing option value");
            std::process::exit(2);
        }
        match args[i].as_str() {
            "--json" if json_path.is_none() => json_path = Some(PathBuf::from(&args[i + 1])),
            "--junit" if junit_path.is_none() => junit_path = Some(PathBuf::from(&args[i + 1])),
            _ => {
                eprintln!("unknown or duplicate option");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    let (mut json_file, mut junit_file) = match reserve_report(&json_path)
        .and_then(|j| reserve_report(&junit_path).map(|x| (j, x)))
    {
        Ok(files) => files,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let start = Instant::now();
    let c = Arc::new(AtomicBool::new(false));
    let cc = c.clone();
    let mut report = Report {
        schema_version: 1,
        scenario: "unloaded".into(),
        outcome: "error".into(),
        exit_code: 2,
        cleanup: "not_needed".into(),
        elapsed_ms: 0,
        events: Vec::new(),
    };
    let result = ctrlc::set_handler(move || cc.store(true, Ordering::SeqCst))
        .map_err(|_| "cannot install cancellation handler".to_string())
        .and_then(|_| load(Path::new(&args[1])))
        .and_then(|s| {
            report.scenario = s.name.clone();
            run(&s, &c, &mut report, start)
        });
    let mut code = match result {
        Ok(code) => code,
        Err(e) => {
            report.event(start, "error", &e);
            2
        }
    };
    report.exit_code = code;
    report.outcome = match code {
        0 => "passed",
        1 => "failed",
        130 => "cancelled",
        _ => "error",
    }
    .into();
    report.elapsed_ms = start.elapsed().as_millis();
    if let Some(file) = &mut json_file {
        if let Err(e) = write_report(
            file,
            &serde_json::to_vec_pretty(&report).expect("serializable report"),
        ) {
            eprintln!("{e}");
            if code != 3 {
                code = 2;
            }
            report.exit_code = code;
            report.outcome = "error".into();
        }
    }
    if let Some(file) = &mut junit_file {
        let failure = if report.exit_code == 0 {
            String::new()
        } else {
            let tag = if report.exit_code == 1 {
                "failure"
            } else {
                "error"
            };
            format!(
                "<{tag} message=\"{}\">{}</{tag}>",
                xml(&report.outcome),
                xml(&report
                    .events
                    .iter()
                    .map(|e| format!("{}: {}", e.phase, e.detail))
                    .collect::<Vec<_>>()
                    .join("\n"))
            )
        };
        let body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"recovery-lab\" tests=\"1\" failures=\"{}\" errors=\"{}\" time=\"{:.3}\"><testcase name=\"{}\" time=\"{:.3}\">{}</testcase></testsuite>\n",u8::from(report.exit_code==1),u8::from(report.exit_code!=0 && report.exit_code!=1),report.elapsed_ms as f64/1000.0,xml(&report.scenario),report.elapsed_ms as f64/1000.0,failure);
        if let Err(e) = write_report(file, body.as_bytes()) {
            eprintln!("{e}");
            if code != 3 {
                code = 2;
            }
            report.exit_code = code;
            report.outcome = "error".into();
        }
    }
    eprintln!("result: {} (exit {code})", report.outcome);
    std::process::exit(code);
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
}
