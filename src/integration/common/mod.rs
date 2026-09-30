#![allow(dead_code)]

use quick_xml::{events::Event, Reader};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub fn client() -> Client {
    Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

pub fn request_json(url: &str, method: &str, body: Option<&Value>) -> Value {
    let mut request = client().request(method.parse().unwrap(), url);
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request
        .send()
        .unwrap_or_else(|error| panic!("{method} {url}: {error}"));
    let status = response.status();
    let bytes = response.bytes().unwrap();
    assert!(
        status.is_success(),
        "{method} {url}: {status} {}",
        String::from_utf8_lossy(&bytes)
    );
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    }
}

pub fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn scenario(api_url: &str, health_url: &str, kind: &str) -> Value {
    let mut fault = json!({"kind": kind, "duration_ms": 500});
    if kind == "latency" {
        fault["latency_ms"] = json!(400);
    }
    json!({"name":"fixture-recovery", "toxiproxy_url":api_url, "proxy":"reference",
        "health_url":health_url, "fault":fault, "request_timeout_ms":150,
        "recovery_timeout_ms":1500, "poll_interval_ms":25, "preflight_timeout_ms":1500})
}

/// Killing/reaping on drop also handles assertion failures in cancellation tests.
pub struct CliRun {
    pub child: Child,
    pub json_path: PathBuf,
    pub junit_path: PathBuf,
    exit_code: Option<i32>,
}
impl CliRun {
    pub fn launch(directory: &Path, config: &Value) -> Self {
        Self::launch_with_temp(directory, &directory.join("process-tmp"), config)
    }

    pub fn launch_with_temp(directory: &Path, temp: &Path, config: &Value) -> Self {
        fs::create_dir_all(directory).unwrap();
        fs::create_dir_all(temp).unwrap();
        let config_path = directory.join("scenario.json");
        let json_path = directory.join("report.json");
        let junit_path = directory.join("report.xml");
        fs::write(&config_path, serde_json::to_vec(config).unwrap()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_recovery-lab"))
            .env("TMPDIR", temp)
            .env("TMP", temp)
            .env("TEMP", temp)
            .arg("run")
            .arg(config_path)
            .arg("--json")
            .arg(&json_path)
            .arg("--junit")
            .arg(&junit_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch Cargo-built recovery-lab");
        Self {
            child,
            json_path,
            junit_path,
            exit_code: None,
        }
    }

    pub fn launch_fixture(directory: &Path, arguments: &[&str]) -> Self {
        let temp = directory.join("process-tmp");
        fs::create_dir_all(&temp).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_recovery-fixture"))
            .args(arguments)
            .env("TMPDIR", &temp)
            .env("TMP", &temp)
            .env("TEMP", &temp)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch Cargo-built recovery-fixture");
        Self {
            child,
            json_path: directory.join("report.json"),
            junit_path: directory.join("report.xml"),
            exit_code: None,
        }
    }

    pub fn finish(&mut self) -> (i32, String) {
        let deadline = Instant::now() + Duration::from_secs(12);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("CLI hung beyond 12 seconds: {}", self.output());
            }
            thread::sleep(Duration::from_millis(10));
        };
        let code = status.code().unwrap_or(-1);
        self.exit_code = Some(code);
        (code, self.output())
    }

    fn output(&mut self) -> String {
        let mut output = String::new();
        if let Some(mut stdout) = self.child.stdout.take() {
            stdout.read_to_string(&mut output).unwrap();
        }
        if let Some(mut stderr) = self.child.stderr.take() {
            stderr.read_to_string(&mut output).unwrap();
        }
        output
    }

    #[cfg(unix)]
    pub fn signal(&self, signal: nix::sys::signal::Signal) {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.child.id() as i32), signal).unwrap();
    }
}
impl Drop for CliRun {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub struct Junit {
    pub testcase_names: Vec<String>,
    pub failed: bool,
}
pub fn parse_junit(path: &Path) -> Junit {
    let xml = fs::read_to_string(path).expect("JUnit report missing");
    // XML 1.0 forbids these code points even when an event parser accepts them.
    assert!(xml.chars().all(|c| matches!(c, '\t' | '\n' | '\r')
        || ('\u{20}'..='\u{d7ff}').contains(&c)
        || ('\u{e000}'..='\u{fffd}').contains(&c)
        || c >= '\u{10000}'));
    let mut reader = Reader::from_str(&xml);
    let mut names = Vec::new();
    let mut root = None;
    let mut failed = false;
    loop {
        match reader.read_event().expect("JUnit must be well-formed XML") {
            Event::Start(element) | Event::Empty(element) => {
                let name = element.name().as_ref().to_vec();
                if root.is_none() {
                    root = Some(name.clone());
                }
                if name == b"failure" || name == b"error" {
                    failed = true;
                }
                for attribute in element.attributes() {
                    let attribute = attribute.expect("valid JUnit attribute");
                    let value = attribute
                        .decode_and_unescape_value(reader.decoder())
                        .unwrap();
                    if name == b"testcase" && attribute.key.as_ref() == b"name" {
                        names.push(value.into_owned());
                    }
                }
            }
            Event::Text(text) => {
                text.xml_content().unwrap();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    assert!(matches!(
        root.as_deref(),
        Some(b"testsuite") | Some(b"testsuites")
    ));
    assert!(
        !names.is_empty(),
        "JUnit report must contain a named testcase"
    );
    Junit {
        testcase_names: names,
        failed,
    }
}

pub fn check_reports(run: &CliRun, failed: bool) -> Value {
    let report: Value =
        serde_json::from_slice(&fs::read(&run.json_path).expect("JSON report missing")).unwrap();
    assert!(report.is_object());
    assert_eq!(report["schema_version"], 1);
    assert!(
        ["passed", "failed", "error", "cancelled"].contains(&report["outcome"].as_str().unwrap())
    );
    assert!(["not_needed", "succeeded", "failed"].contains(&report["cleanup"].as_str().unwrap()));
    let code = report["exit_code"].as_i64().unwrap();
    assert!([0, 1, 2, 3, 130].contains(&code));
    assert_eq!(
        Some(code as i32),
        run.exit_code,
        "JSON exit code must agree with the process"
    );
    assert_eq!(code != 0, failed);
    assert!(report["elapsed_ms"].is_u64());
    let events = report["events"].as_array().unwrap();
    let mut previous = 0;
    for event in events {
        let elapsed = event["elapsed_ms"].as_u64().unwrap();
        assert!(elapsed >= previous, "event timeline must be monotonic");
        previous = elapsed;
        assert!(event["phase"].is_string());
        assert!(event["detail"].is_string());
    }
    assert_eq!(
        parse_junit(&run.junit_path).failed,
        failed,
        "JUnit result must agree with process status"
    );
    report
}

pub fn wait_until(timeout: Duration, message: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if predicate() {
            return;
        }
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}
pub fn wait_for_fault(api_url: &str) -> Value {
    let mut result = Value::Null;
    wait_until(
        Duration::from_secs(4),
        "fault was never observed in the proxy API",
        || {
            result = request_json(&format!("{api_url}/proxies/reference"), "GET", None);
            result["enabled"] == false || !result["toxics"].as_array().unwrap().is_empty()
        },
    );
    result
}

/// Bounded HTTP-only test server. This is not a TCP fault injection engine.
/// Each response closes its connection; Drop replies intentionally close without
/// a status line, reproducing an applied mutation whose response is lost.
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub body: Value,
}
pub enum HttpReply {
    Json(u16, Value),
    Bytes(u16, Vec<u8>),
    Drop,
}
pub struct HttpServer {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl HttpServer {
    pub fn start(handler: impl Fn(HttpRequest) -> HttpReply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = handler.clone();
                        workers.push(thread::spawn(move || serve(stream, &*handler)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }
    pub fn url(&self) -> &str {
        &self.url
    }
}
impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn serve(mut stream: TcpStream, handler: &dyn Fn(HttpRequest) -> HttpReply) {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(length) => bytes.extend_from_slice(&buffer[..length]),
        }
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        if bytes.len() > 64 * 1024 {
            return;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let mut first = headers.lines().next().unwrap().split_whitespace();
    let method = first.next().unwrap_or_default().to_owned();
    let path = first.next().unwrap_or_default().to_owned();
    let length: usize = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse().unwrap())
        .unwrap_or(0);
    if length > 1024 * 1024 {
        return;
    }
    while bytes.len() < header_end + length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(length) => bytes.extend_from_slice(&buffer[..length]),
        }
    }
    let body = if length == 0 {
        json!({})
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
    };
    let (status, body) = match handler(HttpRequest { method, path, body }) {
        HttpReply::Json(status, body) => (status, serde_json::to_vec(&body).unwrap()),
        HttpReply::Bytes(status, body) => (status, body),
        HttpReply::Drop => return,
    };
    let header = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&body);
}

#[derive(Default)]
pub struct MockState {
    pub proxy: Value,
    pub events: Vec<Value>,
    pub fail_delete: bool,
    pub fail_mutation_once: bool,
}
pub struct MockToxiproxy {
    pub state: Arc<Mutex<MockState>>,
    api: HttpServer,
    forwarder: HttpServer,
}
impl MockToxiproxy {
    pub fn start(backend_url: &str) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            proxy: json!({"name":"reference", "listen":"127.0.0.1:0", "upstream":backend_url.trim_start_matches("http://"), "enabled":true, "toxics":[]}),
            ..MockState::default()
        }));
        let forwarding = state.clone();
        let backend = backend_url.to_owned();
        let upstream = client();
        let forwarder = HttpServer::start(move |request| {
            let proxy = forwarding.lock().unwrap().proxy.clone();
            let toxics = proxy["toxics"].as_array().unwrap();
            if proxy["enabled"] == false || toxics.iter().any(|toxic| toxic["type"] == "reset_peer")
            {
                return HttpReply::Drop;
            }
            let latency: u64 = toxics
                .iter()
                .filter(|toxic| toxic["type"] == "latency")
                .map(|toxic| toxic["attributes"]["latency"].as_u64().unwrap_or(0))
                .sum();
            thread::sleep(Duration::from_millis(latency));
            match upstream.get(format!("{backend}{}", request.path)).send() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    match response.bytes() {
                        Ok(body) => HttpReply::Bytes(status, body.to_vec()),
                        Err(_) => HttpReply::Json(502, json!({"error":"backend unavailable"})),
                    }
                }
                Err(_) => HttpReply::Json(502, json!({"error":"backend unavailable"})),
            }
        });
        state.lock().unwrap().proxy["listen"] =
            json!(forwarder.url().trim_start_matches("http://"));
        let api_state = state.clone();
        let api = HttpServer::start(move |request| {
            let mut state = api_state.lock().unwrap();
            match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/proxies") => HttpReply::Json(200, json!({"reference":state.proxy})),
                ("GET", "/proxies/reference") => HttpReply::Json(200, state.proxy.clone()),
                ("GET", "/proxies/reference/toxics") => {
                    HttpReply::Json(200, state.proxy["toxics"].clone())
                }
                ("GET", "/version") => {
                    HttpReply::Json(200, json!({"version":"mock-contract-only"}))
                }
                ("POST", "/proxies/reference") => {
                    state.events.push(json!(["update", request.body]));
                    for (key, value) in request.body.as_object().unwrap() {
                        state.proxy[key] = value.clone();
                    }
                    HttpReply::Json(200, state.proxy.clone())
                }
                ("POST", "/proxies/reference/toxics") => {
                    if state.proxy["toxics"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|toxic| toxic["name"] == request.body["name"])
                    {
                        return HttpReply::Json(409, json!({"error":"toxic already exists"}));
                    }
                    let mut toxic = json!({"stream":"downstream", "toxicity":1.0});
                    for (key, value) in request.body.as_object().unwrap() {
                        toxic[key] = value.clone();
                    }
                    state.proxy["toxics"]
                        .as_array_mut()
                        .unwrap()
                        .push(toxic.clone());
                    state.events.push(json!(["add", toxic]));
                    if state.fail_mutation_once {
                        state.fail_mutation_once = false;
                        HttpReply::Drop
                    } else {
                        HttpReply::Json(200, toxic)
                    }
                }
                ("DELETE", path) if path.starts_with("/proxies/reference/toxics/") => {
                    let name = path.trim_start_matches("/proxies/reference/toxics/");
                    state.events.push(json!(["delete", name]));
                    if state.fail_delete {
                        return HttpReply::Json(500, json!({"error":"forced cleanup failure"}));
                    }
                    let toxics = state.proxy["toxics"].as_array_mut().unwrap();
                    let before = toxics.len();
                    toxics.retain(|toxic| toxic["name"] != name);
                    if toxics.len() != before {
                        HttpReply::Bytes(204, Vec::new())
                    } else {
                        HttpReply::Json(404, json!({"error":"not found"}))
                    }
                }
                _ => HttpReply::Json(404, json!({"error":"not found"})),
            }
        });
        Self {
            state,
            api,
            forwarder,
        }
    }
    pub fn url(&self) -> &str {
        self.api.url()
    }
    pub fn proxy_url(&self) -> &str {
        self.forwarder.url()
    }
    pub fn snapshot(&self) -> Value {
        self.state.lock().unwrap().proxy.clone()
    }
    pub fn events(&self) -> Vec<Value> {
        self.state.lock().unwrap().events.clone()
    }
}

pub struct ToxiproxyProcess {
    pub url: String,
    child: Child,
}
impl ToxiproxyProcess {
    pub fn start(log_path: &Path) -> Self {
        let binary = std::env::var_os("TOXIPROXY_BIN").unwrap_or_else(|| "toxiproxy-server".into());
        let mut last_error = String::new();
        // External processes cannot inherit our reserved listener. Retry a port
        // selection if another process wins the bind race.
        for _ in 0..3 {
            let port = free_port();
            let log = File::create(log_path).unwrap();
            let child = Command::new(&binary).args(["-host", "127.0.0.1", "-port", &port.to_string()])
                .stdout(Stdio::from(log.try_clone().unwrap())).stderr(Stdio::from(log))
                .spawn().unwrap_or_else(|error| panic!("real Toxiproxy is required for this explicitly requested test; set TOXIPROXY_BIN to an installed executable: {error}"));
            let mut server = Self {
                url: format!("http://127.0.0.1:{port}"),
                child,
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if server.child.try_wait().unwrap().is_some() {
                    break;
                }
                if client()
                    .get(format!("{}/proxies", server.url))
                    .send()
                    .is_ok_and(|response| response.status().is_success())
                {
                    return server;
                }
                thread::sleep(Duration::from_millis(25));
            }
            drop(server);
            last_error = fs::read_to_string(log_path).unwrap();
        }
        panic!("Toxiproxy startup failed after three port selections: {last_error}");
    }
}
impl Drop for ToxiproxyProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
