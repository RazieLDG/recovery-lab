# Reference backend and application

These fixtures use only Python 3.10+ standard-library modules. They bind to
`127.0.0.1` by default and bypass environment HTTP proxies. They are deliberately
small teaching/test services, not production applications.

The backend serves JSON at `/work` and `/healthz`. The app polls the backend,
serves HTTP 200 at `/healthz` when its latest dependency check succeeded, and
serves HTTP 503 otherwise. `/state` exposes attempt/success/failure counters.

- `--mode retry`: keeps checking and becomes healthy after recovery
- `--mode latch`: deliberately latches its first dependency failure and remains
  unhealthy even after the backend recovers; restart it to reset this state

The health endpoint reports the dependency state instead of merely reporting
that the app process is alive. That is essential for a useful recovery test.

## Manual local quickstart

From the repository root, use an **already installed** `toxiproxy-server`.
Nothing in these scripts downloads or installs software. Start these processes
in separate terminals:

```sh
python3 fixtures/reference_app.py backend --port 18081

toxiproxy-server -host 127.0.0.1 -port 8474
```

Create a dedicated, empty, enabled proxy:

```sh
curl --fail --silent --show-error \
  -X POST http://127.0.0.1:8474/proxies \
  -H 'Content-Type: application/json' \
  -d '{"name":"reference","listen":"127.0.0.1:18082","upstream":"127.0.0.1:18081","enabled":true}'
```

Start the application in another terminal:

```sh
python3 fixtures/reference_app.py app \
  --backend http://127.0.0.1:18082/work --port 18080 --mode retry
curl --fail http://127.0.0.1:18080/healthz
```

Use `http://127.0.0.1:18080/healthz` as the scenario's `health_url`,
`http://127.0.0.1:8474` as `toxiproxy_url`, and `reference` as `proxy`.
For example, save this as a new scenario file:

```json
{
  "name": "reference-outage",
  "toxiproxy_url": "http://127.0.0.1:8474",
  "proxy": "reference",
  "health_url": "http://127.0.0.1:18080/healthz",
  "fault": {"kind": "outage", "duration_ms": 500},
  "request_timeout_ms": 150,
  "recovery_timeout_ms": 1500,
  "poll_interval_ms": 25,
  "preflight_timeout_ms": 1500
}
```

Then run the built CLI with fresh output filenames:

```sh
cargo build
./target/debug/recovery-lab run scenario.json \
  --json report.json --junit report.xml
```

To observe an intentional recovery failure, stop the app and restart the same
command with `--mode latch`, then run the CLI with new report paths. Stop all
three background services when finished. The CLI restores its injected fault,
but does not own these manually started services or delete their proxy.

## Repeatable tests

The harness starts isolated ephemeral-port services and shuts them down after
each test. It does not touch an existing Toxiproxy instance. Build the CLI first:

```sh
cargo build
python3 scripts/test_integration.py --suite all
```

Real Toxiproxy tests explicitly skip if `toxiproxy-server` is not installed.
For a required real-proxy run, use an existing binary:

```sh
TOXIPROXY_BIN=/absolute/path/to/toxiproxy-server \
  python3 scripts/test_integration.py --suite all --require-real
```

`--binary /path/to/recovery-lab` or `RECOVERY_LAB_BIN` selects a prebuilt CLI.
`--toxiproxy /path/to/toxiproxy-server` is equivalent to `TOXIPROXY_BIN`.
Run individual layers with `--suite fixture`, `--suite mock`, or `--suite real`.

Coverage is separated honestly:

- Fixture tests check retry/latch behavior without a Rust binary
- Mock-contract tests check CLI validation, JSON/JUnit reporting, fault cleanup,
  response-loss cleanup, cancellation, and refusing an already-dirty proxy;
  their HTTP-only double does **not** prove Toxiproxy TCP behavior
- Real-proxy tests exercise outage, latency, reset-peer, unrecoverable failure,
  and SIGINT cleanup using the actual Toxiproxy data plane

SIGINT cancellation tests run on POSIX. `--require-real` turns a missing real
Toxiproxy binary into an error, preventing a silent green CI run without real
proxy coverage. The runner also errors when its requested CLI binary is missing.

To retain a reviewable six-case real-proxy demonstration (including expected
failed recovery and signal cancellation), choose a new output directory:

```sh
TOXIPROXY_BIN=/absolute/path/to/toxiproxy-server \
  python3 scripts/record_demo.py evidence/my-run
```

This saves per-case scenarios, JSON/JUnit reports, process logs, and before/after
proxy state. `summary.json` records expected vs actual exits and restoration.
The saved scenarios contain ephemeral ports from that run; regenerate evidence
instead of expecting those ports to remain live.
