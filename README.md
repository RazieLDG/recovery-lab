# Recovery Lab

A small Rust CLI that turns a local network fault into an application recovery assertion.

**MVP / pre-release.** Supply a bounded JSON scenario and an existing dedicated local Toxiproxy proxy. Recovery Lab checks baseline HTTP health, applies one fault, actively probes during the fault, restores connectivity, and checks recovery before a monotonic deadline. It produces a timestamped timeline, JSON evidence, JUnit XML, and CI-friendly exit codes.

This is application-level testing: point `health_url` at an application endpoint that actually depends on the proxied service. A static `/health` endpoint can conceal dependency failures. Recovery Lab requires at least one non-2xx response or request failure during the fault; otherwise it reports an inconclusive assertion failure rather than a misleading pass.

## Prerequisites

- Rust 1.98+ (2021 edition; CI pins tested toolchain 1.98.1), Python 3.10+ for fixtures/tests
- [Toxiproxy](https://github.com/Shopify/toxiproxy) server 2.12.0 or later, installed from its official releases
- A dedicated test environment, never a production proxy

No application SDK, account, daemon managed by Recovery Lab, or remote credentials are required.

## Quickstart

```sh
cargo build
# In another terminal, with an official toxiproxy-server binary on PATH:
toxiproxy-server -host 127.0.0.1
```

The Python fixture and its demonstration/test commands are documented in `fixtures/README.md`. Create an isolated proxy whose listen and upstream addresses both use literal loopback IPs, route the fixture application's dependency through it, and run:

```sh
mkdir -p results
cargo run -- run examples/outage.json --json results/outage.json --junit results/outage.xml
cargo run -- run examples/latency.json --json results/latency.json --junit results/latency.xml
cargo run -- run examples/reset.json --json results/reset.json --junit results/reset.xml
```

Output files must not already exist. This deliberately avoids overwriting previous evidence. Each scenario is an independent run; restart a deliberately broken/latching fixture before another baseline test.

## Scenario format

See the three complete examples in `examples/`. JSON only, UTF-8, at most 64 KiB. Unknown fields are rejected, including unknown fault attributes. One fault per run, no shell commands or arbitrary code execution.

| Field | Meaning / bounds |
| --- | --- |
| `name` | 1–120 bytes, no control characters |
| `toxiproxy_url` | HTTP origin, literal loopback IP, e.g. `http://127.0.0.1:8474` |
| `proxy` | Existing proxy, 1–80 ASCII letters/digits/underscores/hyphens |
| `health_url` | Application HTTP endpoint, literal loopback IP |
| `fault.kind` | `outage`, `latency`, or `reset_peer` |
| `fault.duration_ms` | 10–60,000; at least `request_timeout_ms + poll_interval_ms` |
| `fault.latency_ms` | Only for latency; 1–60,000; zero jitter |
| `request_timeout_ms` | Each network operation: 10–10,000 |
| `preflight_timeout_ms` | Baseline total: 10–60,000 |
| `recovery_timeout_ms` | Recovery total after restoration: 10–300,000 |
| `poll_interval_ms` | Between observations: 10–10,000 |

URLs cannot contain credentials, queries or fragments. Hostnames (even `localhost`), HTTPS, redirects, environment HTTP proxies, remote addresses, and wildcard proxy listeners are intentionally excluded. IPv6 loopback is supported. There is no remote override in this MVP. The configured proxy must be enabled and free of existing toxics. Recovery Lab also checks its upstream address, so a local control API cannot silently target a remote dependency.

### Fault semantics

- **Outage:** disables the selected Toxiproxy proxy; restoration re-enables it
- **Latency:** adds a uniquely named downstream latency toxic, `toxicity=1`, `jitter=0`; restoration removes only this toxic
- **Reset:** adds a uniquely named downstream `reset_peer` toxic with `timeout=0`; restoration removes only this toxic

These use [Toxiproxy's documented API](https://github.com/Shopify/toxiproxy#http-api). Recovery Lab never calls global `/reset`, deletes a proxy, or changes its listen/upstream addresses. Latency examples deliberately exceed the fixture dependency timeout to cause observable degradation; a small latency that preserves successful health will fail the degradation assertion. This first version asserts status/transport health, not a latency SLO or business-response schema.

## Safety and cleanup

Each API request has a total timeout. Health probes are capped by the remaining phase deadline; a healthy response completing after the recovery deadline does not pass. All elapsed times use a monotonic clock. SIGINT and SIGTERM request graceful cancellation, followed by restoration; cancellation may wait for the in-flight bounded request.

Cleanup is armed **before** sending a mutation, including when the API response is lost. Cleanup retries at most three times. A best-effort destructor is a final fallback. A per-proxy local temporary-file lock prevents overlapping cooperative runs from the same host. The lock remains if cleanup cannot be confirmed. Concurrent edits by other tools or hosts are unsupported: reserve a dedicated proxy for each test. Proxy names and API endpoints are not a distributed ownership mechanism.

No program can guarantee restoration after SIGKILL, host/power loss, or an unreachable Toxiproxy. If cleanup fails, inspect the selected proxy and remove only its `recovery_lab_...` toxic or re-enable it after verifying ownership. Once connectivity and the absence of another active run are confirmed, remove its stale `recovery-lab-*.lock` file from the OS temporary directory. Do not indiscriminately reset all proxies. Cleanup retries are bounded, so an unreachable control plane can extend shutdown by several request timeouts.

Reports contain scenario name, phase messages, timings and outcomes. Response bodies, target URLs, and credentials are not included. Keep secrets out of scenario names and file paths.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Healthy baseline, observed degradation, successful restore, timely recovery |
| 1 | Baseline unhealthy, no observed degradation, or missed recovery deadline |
| 2 | Invalid scenario, operational failure, or report-write error |
| 3 | Cleanup failed; manual inspection required |
| 130 | Graceful cancellation (SIGINT or SIGTERM), after cleanup attempt |

Cleanup failure takes priority over cancellation. Invalid CLI syntax cannot produce reports because output destinations may be unknown. Output paths are reserved before the experiment so an existing/unwritable path prevents fault injection. A later report-write error is emitted on stderr and returns 2; already-written evidence records the test outcome, which may differ from the final process exit if another output fails.

## Testing

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build
# Mock control-plane tests (not real network fault validation):
python3 -m unittest discover -s tests -v
# Set the official binary to enable real TCP fault integration tests:
TOXIPROXY_BIN=/path/to/toxiproxy-server python3 -m unittest discover -s tests -v
```

The test suite must explicitly distinguish mock protocol/cleanup tests from real Toxiproxy end-to-end tests. CI downloads an official pinned Toxiproxy release, checks its release checksum, builds Rust, and exercises the real tests.

## Why this exists

[Toxiproxy](https://github.com/Shopify/toxiproxy) provides the network fault mechanism. Recovery Lab adds a narrowly scoped scenario format, bounded health/recovery assertion, owned cleanup, timeline and test-runner output around it. It complements Toxiproxy; it does not implement another proxy.

[Chaos Toolkit](https://chaostoolkit.org/) supports broader experiments and extensible actions. Recovery Lab chooses a much smaller local/CI-first surface: a single dependency fault, application health, and an explicit recovery deadline. Broader orchestration, production experiments, arbitrary plugins and distributed workflows are outside this MVP.

## Limits and next steps

- No deterministic concurrency replay, distributed scheduling, seed-based execution replay or causal tracing claims
- No durable crash-recovery journal, automatic stale-lock deletion, or cross-host lock
- A first healthy HTTP response is sufficient; sustained recovery windows and body assertions are future work
- Health failures must be observed during a bounded polling window; very brief failures can be missed
- No recovery percentile/SLO statistics, dashboards, hosted service, or multi-fault schedules
- HTTP only; future TLS and remote support require an explicit security design
- Choose and add an open-source license before public distribution; no license or copyright owner is presumed

The next useful milestone is validating against a real application's reconnect/backoff behavior, then adding a sustained-recovery window and durable cleanup evidence.

## Beyond Horizons

Company website: [Beyond Horizons](https://behoin.tech/).
