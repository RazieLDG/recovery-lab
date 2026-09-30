//! Standalone Rust fixture runner for manual and automated recovery experiments.

use recovery_lab::fixture::{AppConfig, Backend, FixtureResult, Mode, ReferenceApp};
use reqwest::{blocking::Client, redirect::Policy, Url};
use serde_json::json;
use std::collections::HashSet;
use std::env;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::mpsc;
use std::time::Duration;

const HELP: &str = "Usage:
  recovery-fixture backend [--host 127.0.0.1] [--port 18081]
  recovery-fixture app [--host 127.0.0.1] [--port 18080]
      [--backend http://127.0.0.1:18082/work] [--mode retry|latch]
      [--request-timeout-ms 100] [--poll-interval-ms 25]
  recovery-fixture proxy [--api http://127.0.0.1:8474] [--name reference]
      [--listen 127.0.0.1:18082] [--upstream 127.0.0.1:18081]

Port zero selects an ephemeral port. The bound URL is printed as JSON.
All hosts must be literal loopback IPs; timeout and poll values are 1-10000 ms.
Ctrl-C or SIGTERM cleanly stops the server and dependency poller.";

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("recovery-fixture: {error}");
        std::process::exit(2);
    }
}

fn run() -> FixtureResult<()> {
    let mut args = env::args().skip(1);
    let Some(service) = args.next() else {
        return Err(invalid(HELP).into());
    };
    if service == "--help" || service == "-h" {
        println!("{HELP}");
        return Ok(());
    }
    if service == "proxy" {
        return create_proxy(args);
    }
    if service != "backend" && service != "app" {
        return Err(invalid(format!("unknown service {service:?}\n{HELP}")).into());
    }
    let mut config = AppConfig {
        port: if service == "backend" { 18081 } else { 18080 },
        ..AppConfig::default()
    };
    let mut seen = HashSet::new();
    while let Some(option) = args.next() {
        if option == "--help" || option == "-h" {
            println!("{HELP}");
            return Ok(());
        }
        if !seen.insert(option.clone()) {
            return Err(invalid(format!("duplicate option {option}")).into());
        }
        let valid = option == "--host"
            || option == "--port"
            || (service == "app"
                && matches!(
                    option.as_str(),
                    "--backend" | "--mode" | "--request-timeout-ms" | "--poll-interval-ms"
                ));
        if !valid {
            return Err(invalid(format!("unknown option {option:?} for {service}")).into());
        }
        let value = args
            .next()
            .ok_or_else(|| invalid(format!("missing value for {option}")))?;
        match option.as_str() {
            "--host" => config.host = value,
            "--port" => {
                config.port = value
                    .parse()
                    .map_err(|_| invalid("port must be an integer from 0 to 65535"))?
            }
            "--backend" => config.backend_url = value,
            "--mode" => config.mode = value.parse::<Mode>()?,
            "--request-timeout-ms" | "--poll-interval-ms" => {
                let milliseconds = value
                    .parse::<u64>()
                    .ok()
                    .filter(|value| (1..=10_000).contains(value))
                    .ok_or_else(|| {
                        invalid(format!("{option} must be an integer from 1 to 10000"))
                    })?;
                let duration = Duration::from_millis(milliseconds);
                if option == "--request-timeout-ms" {
                    config.request_timeout = duration;
                } else {
                    config.poll_interval = duration;
                }
            }
            _ => unreachable!("options validated above"),
        }
    }
    let (stop, stopped) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = stop.send(());
    })?;
    if service == "backend" {
        let backend = Backend::start(&config.host, config.port)?;
        println!(
            "{}",
            json!({"service": "backend", "url": backend.url(), "mode": null})
        );
        io::stdout().flush()?;
        stopped.recv()?;
    } else {
        let mode = config.mode;
        let app = ReferenceApp::start(config)?;
        println!(
            "{}",
            json!({"service": "app", "url": app.url(), "mode": mode})
        );
        io::stdout().flush()?;
        stopped.recv()?;
    }
    Ok(())
}

fn validate_api(value: &str) -> FixtureResult<Url> {
    let url = Url::parse(value)?;
    let loopback = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|address| address.is_loopback());
    if url.scheme() != "http"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.port() == Some(0)
    {
        return Err(invalid("API must be an HTTP origin with a literal loopback IP, no credentials, path, query or fragment").into());
    }
    Ok(url)
}

fn validate_socket(value: &str, label: &str) -> FixtureResult<SocketAddr> {
    let address: SocketAddr = value
        .parse()
        .map_err(|_| invalid(format!("{label} must be a literal loopback IP and port")))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(invalid(format!(
            "{label} must be a literal loopback IP and nonzero port"
        ))
        .into());
    }
    Ok(address)
}

fn validate_name(name: &str) -> FixtureResult<()> {
    if name.is_empty()
        || name.len() > 80
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(invalid(
            "proxy name must be 1-80 ASCII letters, digits, underscores or hyphens",
        )
        .into());
    }
    Ok(())
}

fn create_proxy(mut args: impl Iterator<Item = String>) -> FixtureResult<()> {
    let mut api = "http://127.0.0.1:8474".to_owned();
    let mut name = "reference".to_owned();
    let mut listen = "127.0.0.1:18082".to_owned();
    let mut upstream = "127.0.0.1:18081".to_owned();
    let mut seen = HashSet::new();
    while let Some(option) = args.next() {
        if option == "--help" || option == "-h" {
            println!("{HELP}");
            return Ok(());
        }
        if !seen.insert(option.clone()) {
            return Err(invalid(format!("duplicate option {option}")).into());
        }
        let target = match option.as_str() {
            "--api" => &mut api,
            "--name" => &mut name,
            "--listen" => &mut listen,
            "--upstream" => &mut upstream,
            _ => return Err(invalid(format!("unknown proxy option {option:?}")).into()),
        };
        *target = args
            .next()
            .ok_or_else(|| invalid(format!("missing value for {option}")))?;
    }
    let api = validate_api(&api)?;
    validate_name(&name)?;
    let listen = validate_socket(&listen, "listen")?.to_string();
    let upstream = validate_socket(&upstream, "upstream")?.to_string();
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    // Toxiproxy's POST /proxies creates only; it returns a conflict for an
    // existing name. Never call the endpoint that updates an existing proxy.
    let response = client
        .post(api.join("proxies")?)
        .json(&json!({"name": name, "listen": listen, "upstream": upstream, "enabled": true}))
        .send()?;
    let status = response.status();
    let mut body = Vec::new();
    response.take(65_537).read_to_end(&mut body)?;
    if body.len() > 65_536 {
        return Err(invalid("Toxiproxy returned an oversized response").into());
    }
    if !status.is_success() {
        return Err(invalid(format!(
            "proxy creation failed with HTTP {} (existing proxies are never overwritten)",
            status.as_u16()
        ))
        .into());
    }
    let created: serde_json::Value = serde_json::from_slice(&body)?;
    if created.get("name").and_then(|value| value.as_str()) != Some(name.as_str())
        || created.get("listen").and_then(|value| value.as_str()) != Some(listen.as_str())
        || created.get("upstream").and_then(|value| value.as_str()) != Some(upstream.as_str())
        || created.get("enabled").and_then(|value| value.as_bool()) != Some(true)
    {
        return Err(invalid("Toxiproxy did not confirm the requested proxy configuration").into());
    }
    println!(
        "{}",
        json!({"service": "proxy", "name": name, "listen": listen, "upstream": upstream, "enabled": true})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_setup_accepts_only_literal_loopback_origins() {
        for valid in ["http://127.0.0.1:8474", "http://[::1]:8474/"] {
            assert!(validate_api(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "http://localhost:8474",
            "http://192.0.2.1:8474",
            "http://0.0.0.0:8474",
            "https://127.0.0.1:8474",
            "http://127.0.0.1:8474/proxies",
            "http://user@127.0.0.1:8474",
            "http://127.0.0.1:8474?x=1",
            "http://127.0.0.1:8474#fragment",
            "http://127.0.0.1:0",
        ] {
            assert!(validate_api(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn proxy_setup_rejects_remote_targets_and_unsafe_names() {
        for value in ["127.0.0.1:18081", "[::1]:18081"] {
            assert!(validate_socket(value, "address").is_ok());
        }
        for value in [
            "0.0.0.0:18081",
            "192.0.2.1:18081",
            "localhost:18081",
            "127.0.0.1:0",
        ] {
            assert!(validate_socket(value, "address").is_err());
        }
        assert!(validate_name("reference_1-ok").is_ok());
        for value in ["", "../proxy", "proxy name", "прокси", &"x".repeat(81)] {
            assert!(validate_name(value).is_err());
        }
    }

    #[test]
    fn proxy_setup_creates_only_and_reports_conflicts() {
        for status in [201, 409] {
            let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
            let url = format!("http://{}", server.server_addr());
            let worker = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .expect("proxy request was never sent");
                assert_eq!(request.method(), &tiny_http::Method::Post);
                assert_eq!(request.url(), "/proxies");
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let config: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(config["name"], "reference");
                assert_eq!(config["listen"], "127.0.0.1:18082");
                assert_eq!(config["upstream"], "127.0.0.1:18081");
                assert_eq!(config["enabled"], true);
                request
                    .respond(tiny_http::Response::from_string(body).with_status_code(status))
                    .unwrap();
            });
            let result = create_proxy(vec!["--api".into(), url].into_iter());
            worker.join().unwrap();
            if status == 201 {
                assert!(result.is_ok());
            } else {
                assert!(result.unwrap_err().to_string().contains("HTTP 409"));
            }
        }
    }
}
