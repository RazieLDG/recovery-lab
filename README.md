# Recovery Lab

> **Under heavy, active development.** This is an experimental pre-1.0 project. APIs and behavior are still evolving; do not assume production readiness. Breaking API changes are planned for new minor versions (for example, 0.1 to 0.2); patch versions are intended for compatible fixes. Review changes and validate them against your application before upgrading.

A Rust library and CLI for observing dependency health and testing application recovery.

## Two ways to use it

**Embed passive monitoring in your application.** The default library exposes an async probe/event API. Your application receives an initial status, confirmed health loss and stable recovery, and decides what actions to take. Monitoring sends ordinary health probes; it never configures Toxiproxy or injects faults.

```sh
cargo run --locked --example monitor
```

This self-contained example uses application-provided observations and host-owned state changes. It does not alter network connectivity.

**Run opt-in recovery experiments.** Enable `fault-injection` to build the CLI, blocking test runner and reference fixture. Existing CLI arguments are unchanged.

```sh
cargo build --locked --features fault-injection --bins
cargo run --locked --features fault-injection --bin recovery-lab -- run examples/outage.json --json outage.json --junit outage.xml
```

A plain `cargo build` builds the passive library, not the feature-gated CLIs. Toxiproxy remains a separately installed fault engine, needed only for experiments and real fault tests. The library, CLI, fixtures and tests are Rust.

## Embed the library

For a local checkout, use a path dependency:

```toml
[dependencies]
recovery-lab = { path = "../recovery-lab", default-features = false }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

For an available crates.io release, use the registry dependency below. Verify that the requested version exists before installing; these commands do not assert registry availability:

```toml
recovery-lab = { version = "0.1.0", default-features = false }
```

The corresponding CLI installation command is:

```sh
cargo install recovery-lab --version 0.1.0 --locked --features fault-injection --bin recovery-lab
```

To install from a local checkout instead, use `cargo install --path . --locked --features fault-injection --bin recovery-lab`. The optional reference tool can be selected with `--bin recovery-fixture`. [API documentation on docs.rs](https://docs.rs/recovery-lab) is available for published versions after their documentation build succeeds.

See [examples/monitor.rs](examples/monitor.rs) for a complete, compile-tested consumer of the public API. Replace its application probe with an `HttpProbe` to observe a dependency endpoint, or implement `Probe` for your application's own checks. Your application owns retries, reconnect calls, work scheduling and other responses to events.

HTTP health is an observation of that endpoint. A timeout, transport error or non-2xx response does not prove that the host's internet connection is down. Recovery means the configured probe is healthy again; it does not prove a service client has successfully reconnected.

### Event and stability contract

- `InitialStatus` reports the first observation. Initial success is not a recovery event; initial failure can later produce a confirmed recovery
- `HealthLost` follows confirmed unhealthy observations over `loss_debounce`
- `Recovered` follows healthy observations over `recovery_stability`
- Opposite observations reset an unconfirmed transition; unchanged confirmed health emits no repeated transition
- `Stopped` terminates graceful shutdown

Probes are serial, with a configured interval and per-probe timeout. The monitor owns its background task. Explicit `shutdown().await` cancels the active probe future and joins the task; dropping the owner aborts its task. Custom probe futures must cooperate with async polling, avoid blocking the executor, and clean up work they create. Rust cannot forcibly interrupt blocking code inside a custom future or automatically cancel its independently spawned work.

Events use a bounded broadcast channel. `RecvError::Lagged` explicitly reports missed events; consumers can use `Monitor::current_status()` to resynchronize to the latest confirmed state. That snapshot excludes samples still inside a debounce window, and cannot reconstruct lost event history. Queued events may be older than the snapshot, so actions should reconcile current desired state. Monitoring never waits on a slow consumer or invokes application callbacks. The initial receiver is created before the task starts so its first status cannot be lost in a subscribe/start race. Additional subscriptions see future events only.

The built-in HTTP probe uses normal TLS verification for HTTPS and bounded async probing. See the public Rust documentation for configuration validation, failure types and lifecycle details.

## Requirements and checks

- Rust 1.98.1 or newer; 1.98.1 is the declared and tested minimum
- An official [Toxiproxy](https://github.com/Shopify/toxiproxy) 2.12.0 binary only for real fault tests
- An isolated local environment for fault injection

```sh
cargo test --locked --no-default-features
cargo test --locked --no-default-features --example monitor
cargo test --locked --features fault-injection
TOXIPROXY_BIN=/absolute/path/to/toxiproxy-server cargo test --locked --features fault-injection --test real_toxiproxy -- --ignored --test-threads=1
```

Real tests start isolated loopback Toxiproxy and Rust fixtures and stop them afterward. They cover outage, latency, reset-peer, a deliberately broken client, signal cancellation and safe proxy setup. A missing binary fails the explicitly requested real suite. It cannot silently pass without exercising the data plane.

## Opt-in fault-test library

The `fault-injection` feature also exposes the shared `runner` API used by the CLI: scenario parsing/validation, a blocking runner, cancellation and JSON/JUnit reports. Calling `runner::run_blocking` explicitly performs a local recovery experiment. It is separate from passive monitoring; do not run it in an application's normal health-observation path or directly on an async executor thread. An `Ok(report)` means the experiment completed, not necessarily that its assertions passed: inspect `report.exit_code` and `report.outcome`. The optional live-event callback is synchronous and must return quickly; blocking it can delay cancellation and restoration.

## Run a scenario against your application

Create a dedicated local Toxiproxy proxy and route your application's dependency through it. Then:

```sh
cargo run --locked --features fault-injection --bin recovery-lab -- run examples/outage.json --json outage.json --junit outage.xml
```

Output files must not exist. The example expects proxy `reference` on the API at `127.0.0.1:8474`, and application health at `127.0.0.1:18080/healthz`. Adapt those values to your local app.

The health endpoint must reflect the affected dependency, not just process liveness. Recovery Lab requires at least one non-2xx response or request failure during the fault. An unrelated, permanently healthy endpoint produces an inconclusive failure rather than a misleading pass.

### Try the Rust reference application

Start an official Toxiproxy server with its API bound to `127.0.0.1:8474`. Run the following backend and application commands in separate terminals; run the proxy-creation command once after the backend starts:

```sh
cargo run --locked --features fault-injection --bin recovery-fixture -- backend --port 18081
cargo run --locked --features fault-injection --bin recovery-fixture -- proxy --api http://127.0.0.1:8474 --name reference --listen 127.0.0.1:18082 --upstream 127.0.0.1:18081
cargo run --locked --features fault-injection --bin recovery-fixture -- app --backend http://127.0.0.1:18082/work --port 18080 --mode retry
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

On SIGINT or SIGTERM, the CLI requests cancellation and the runner restores its fault. Library callers own cancellation; the library installs no process-wide signal handler. An in-flight request may finish or time out first. Cleanup is armed before mutation, including response-loss cases, retries at most three times, and has a best-effort destructor fallback.

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

## Compatibility expectations

- **Rust:** the minimum supported Rust version is 1.98.1. CI validates that exact toolchain
- **Runtime:** passive monitoring requires a Tokio runtime with its time driver enabled; custom probes must be cooperative, cancellation-safe futures
- **Features:** default features are empty; passive monitoring is the default API. `fault-injection` explicitly enables the blocking runner, CLI and fixture. Keep it disabled in passive-only applications
- **Reports:** fault-test JSON carries `schema_version: 1`. This identifies the current layout; it is not a promise that the experimental format will never change. Consumers should check the version and tolerate additional fields
- **Platforms:** Linux x86_64 is currently tested. macOS, Windows, other architectures and an actual HTTPS handshake are not claimed as verified
- **Versioning:** this is an early 0.1 API. Future 0.x minor releases may be incompatible; use a lockfile or an appropriate version constraint and read release changes

## Development and CI

```sh
cargo fmt --check
cargo clippy --locked --features fault-injection --all-targets -- -D warnings
cargo test --locked --no-default-features
cargo test --locked --no-default-features --example monitor
cargo test --locked --features fault-injection
TOXIPROXY_BIN=/absolute/path/to/toxiproxy-server cargo test --locked --features fault-injection --test real_toxiproxy -- --ignored --test-threads=1
```

CI downloads a pinned official Toxiproxy release, verifies its publisher-provided checksum, and runs both ordinary and real integration suites. Mock tests validate protocol, safety and reporting; only the real suite validates Toxiproxy TCP behavior. Linux is the currently validated platform; other operating systems are not claimed as tested.

## Scope

Toxiproxy supplies the fault mechanism. Recovery Lab adds bounded health/recovery assertions, scoped cleanup and test-runner output. For fault experiments, compared with broader systems such as [Chaos Toolkit](https://chaostoolkit.org/), this MVP deliberately limits itself to one dependency fault in a local or CI environment.

The current monitoring API observes health; it does not implement a service reconnect strategy.

Not supported: distributed orchestration, production/remote fault injection, deterministic concurrency replay, arbitrary plugins, durable crash recovery, response-body assertions or recovery percentile statistics. Fault-runner assertions currently accept the first timely healthy response; the passive monitor separately supports configured recovery stability.

## License

Licensed under [Apache License 2.0](LICENSE). Dependencies retain their respective licenses and notice requirements. The package is configured for crates.io; package preparation and dry runs do not publish a release.

## Beyond Horizons

Developed by [Beyond Horizons](https://behoin.tech/).
