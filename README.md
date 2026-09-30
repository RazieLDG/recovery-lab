# Recovery Lab

A Rust CLI for testing whether an application recovers after a local network fault.

Recovery Lab checks HTTP health, applies one bounded Toxiproxy fault, observes degradation, restores connectivity, and asserts recovery before a deadline. It writes a timeline, JSON and JUnit reports with CI-friendly exit codes.

**MVP / pre-release:** local and CI use only. The CLI, reference application, mock server and integration tests are all Rust. Toxiproxy is the separately installed network-fault engine.

## Requirements

- Rust 1.98+; CI pins 1.98.1
- An official [Toxiproxy](https://github.com/Shopify/toxiproxy) 2.12.0 server binary for real network tests
- An isolated local test environment

## Quickstart

```sh
cargo build --locked
cargo test --locked
TOXIPROXY_BIN=/absolute/path/to/toxiproxy-server cargo test --locked --test real_toxiproxy -- --ignored --test-threads=1
```

The real tests start their own isolated loopback Toxiproxy, Rust backend and reference application, and stop them afterward. They cover outage, latency, reset-peer, a deliberately broken client, and signal cancellation. Supplying a missing or unusable binary fails the real suite; it cannot silently pass without testing the data plane.

Regular `cargo test` runs the Rust unit and fixture/mock tests. The explicitly ignored real tests are run by the third command and by CI with the official binary installed.

## Run a scenario against your application

Create a dedicated local Toxiproxy proxy and route your application's dependency through it. Then:

```sh
cargo run --locked --bin recovery-lab -- run examples/outage.json --json outage.json --junit outage.xml
```

Output files must not exist. The example expects proxy `reference` on the API at `127.0.0.1:8474`, and application health at `127.0.0.1:18080/healthz`. Adapt those values to your local app.

The health endpoint must reflect the affected dependency, not just process liveness. Recovery Lab requires at least one non-2xx response or request failure during the fault. An unrelated, permanently healthy endpoint produces an inconclusive failure rather than a misleading pass.

### Try the Rust reference application

Start an official Toxiproxy server with its API bound to `127.0.0.1:8474`. Run the following backend and application commands in separate terminals; run the proxy-creation command once after the backend starts:

```sh
cargo run --locked --bin recovery-fixture -- backend --port 18081
cargo run --locked --bin recovery-fixture -- proxy --api http://127.0.0.1:8474 --name reference --listen 127.0.0.1:18082 --upstream 127.0.0.1:18081
cargo run --locked --bin recovery-fixture -- app --backend http://127.0.0.1:18082/work --port 18080 --mode retry
```

Then run the scenario command above. Try `examples/latency.json` and `examples/reset.json` with fresh output paths. To demonstrate a failed recovery assertion, restart the app with `--mode latch`: it intentionally stays unhealthy after its first dependency failure. Restart a latching app before another test so its baseline is healthy.

The backend serves `/work` and `/healthz`. The app exposes `/healthz` and `/state` counters. The proxy helper only creates a new dedicated local proxy; it does not overwrite an existing one. Stop the manually started services when finished. Scenario execution restores its fault but does not own those services or delete the proxy.

## Scenario format

The three `examples/*.json` files are complete scenarios. JSON only, at most 64 KiB; unknown fields are rejected. Each run has exactly one fault and cannot execute arbitrary shell commands.

| Field | Meaning / bounds |
| --- | --- |
| `name` | 1–120 bytes, no control characters |
| `toxiproxy_url` | HTTP origin with a literal loopback IP |
| `proxy` | Existing proxy; 1–80 ASCII letters, digits, hyphens or underscores |
| `health_url` | Application HTTP endpoint with a literal loopback IP |
| `fault.kind` | `outage`, `latency`, or `reset_peer` |
| `fault.duration_ms` | 10–60,000; at least `request_timeout_ms + poll_interval_ms` |
| `fault.latency_ms` | Only latency; 1–60,000 |
| `request_timeout_ms` | Each network operation: 10–10,000 |
| `preflight_timeout_ms` | Baseline total: 10–60,000 |
| `recovery_timeout_ms` | Recovery total after restoration: 10–300,000 |
| `poll_interval_ms` | Between observations: 10–10,000 |

URLs cannot contain credentials, queries or fragments. Hostnames, HTTPS, redirects, environment HTTP proxies, remote addresses and wildcard proxy listeners are excluded. Use `127.0.0.1` or `[::1]`, not `localhost`. Both the selected proxy's listen and upstream sockets must also be literal loopback addresses. There is no remote override.

The selected proxy must be enabled and have no existing toxics. Reserve one proxy for each test.

### Fault semantics

- **Outage:** disables only the selected proxy, then re-enables it
- **Latency:** adds a uniquely named downstream toxic with `toxicity=1` and zero jitter, then removes only that toxic
- **Reset-peer:** adds a uniquely named downstream `reset_peer` toxic with `timeout=0`, then removes only that toxic

These use the [documented Toxiproxy API](https://github.com/Shopify/toxiproxy#http-api). Recovery Lab never calls global `/reset`, deletes an existing proxy, or edits its listen/upstream addresses.

The latency example exceeds the reference app's dependency timeout to create observable degradation. A latency that preserves health will fail the degradation assertion. This MVP tests transport/status health, not a latency SLO or business-response schema.

## Cleanup and deadlines

Every API operation is bounded. Health probes respect the remaining phase deadline, and a healthy response completed after the recovery deadline cannot pass. Elapsed time uses a monotonic clock. Fault observations use a full request budget so phase-end truncation cannot falsely count as degradation.

On SIGINT or SIGTERM, the runner requests cancellation and restores its fault. An in-flight request may finish or time out first. Cleanup is armed before mutation, including response-loss cases, retries at most three times, and has a best-effort destructor fallback.

A local per-proxy lock prevents overlapping cooperative runs on one host. Concurrent mutation by other tools or hosts is unsupported. A lock remains if cleanup cannot be confirmed. SIGKILL, power loss or an unreachable Toxiproxy cannot guarantee restoration. In that case, inspect the selected proxy, remove only the run's `recovery_lab_...` toxic or restore its enabled state after confirming ownership, then remove its stale `recovery-lab-*.lock` from the OS temporary directory. Never reset unrelated proxies.

Reports contain scenario names, phase messages, timings and outcomes. They omit response bodies and target URLs. Keep secrets out of names and output paths.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Healthy baseline, observed degradation, successful restoration and timely recovery |
| 1 | Baseline unhealthy, no observed degradation, or recovery deadline missed |
| 2 | Invalid input, operational failure or report-write error |
| 3 | Cleanup failed; manual inspection required |
| 130 | Graceful signal cancellation after cleanup attempt |

Cleanup failure takes priority over cancellation. Output paths are reserved before mutation. Invalid CLI syntax cannot produce reports when destinations are unknown. A later report-write failure returns 2; an earlier successfully written report may still describe the completed test outcome.

## Development and CI

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
TOXIPROXY_BIN=/absolute/path/to/toxiproxy-server cargo test --locked --test real_toxiproxy -- --ignored --test-threads=1
```

CI downloads a pinned official Toxiproxy release, verifies its publisher-provided checksum, and runs both ordinary and real integration suites. Mock tests validate protocol, safety and reporting; only the real suite validates Toxiproxy TCP behavior. Linux is the currently validated platform; other operating systems are not claimed as tested.

## Scope

Toxiproxy supplies the fault mechanism. Recovery Lab adds bounded health/recovery assertions, scoped cleanup and test-runner output. Compared with broader experiment systems such as [Chaos Toolkit](https://chaostoolkit.org/), this MVP deliberately limits itself to one dependency fault in a local or CI environment.

Not supported: distributed orchestration, production/remote faults, deterministic concurrency replay, arbitrary plugins, durable crash recovery, sustained recovery windows, response-body assertions or recovery percentile statistics. A first timely healthy response is sufficient for success.

## License

Licensed under [Apache License 2.0](LICENSE). Dependencies retain their respective licenses and notice requirements. Cargo publication remains disabled; a package release is a separate decision.

## Beyond Horizons

Developed by [Beyond Horizons](https://behoin.tech/).
