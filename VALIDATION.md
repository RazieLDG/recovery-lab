# Validation record — 2026-09-30

Development snapshot: Recovery Lab 0.1.0. At the initial validation, license
selection was pending. The owner subsequently approved Apache-2.0 on the same
date; see the licensing update below. No package release has been published.

## Environment and provenance

- Isolated Linux x86_64 cloud workspace; no changes to a user computer
- rustc 1.98.1, cargo 1.98.1, official Rust toolchain
- Official Shopify Toxiproxy 2.12.0 Linux amd64 release; release archive SHA-256
  verified against the publisher's `checksums.txt`; binary version verified
- Rust dependency resolution captured in `Cargo.lock`

## Rust checks — passed

Executed from the project root:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
```

4 Rust unit tests passed; no failures, ignored tests or warnings. Full output is
in `evidence/rust-checks.log`. Unit coverage includes local URL/socket validation,
strict fault parsing and safe XML escaping.

## Integration checks

25 Python tests passed: 2 fixture, 17 mock-contract, and 6 real Toxiproxy tests.
No failures or skips. Full output is in `evidence/integration-final.log`.
The suite uses real local TCP/HTTP services, and explicitly separates the mock
control-plane tests from tests using the actual Toxiproxy binary.

```sh
TOXIPROXY_BIN=/workspace/shared/recovery-toolchain/toxiproxy-server \
  python3 scripts/test_integration.py --suite all --require-real
```

The script refuses to silently skip real-proxy coverage when `--require-real` is
set. The fixtures cover both a retrying application and an application that
intentionally latches a dependency failure and never recovers.

## Retained real-proxy demonstrations — all expected results verified

`evidence/real-demo/summary.json` indexes six independently exercised cases:

| Case | Expected and actual exit | Selected proxy restored |
| --- | --- | --- |
| Outage, retrying app | 0 | Yes |
| Latency, retrying app | 0 | Yes |
| Reset-peer, retrying app | 0 | Yes |
| Outage, deliberately latching app | 1 | Yes |
| SIGINT during outage | 130 | Yes |
| SIGTERM during latency | 130 | Yes |

Each case retains the exact scenario, JSON report, parseable JUnit XML, console
output, Toxiproxy log, and before/after proxy descriptions. The deliberate
failure is a successful negative test, not an unresolved product defect.

## Independent review

Independent source and runtime review found no remaining blocking issues after
fixes. In addition to repeating Rust and real/mock integration checks, it verified:

- A healthy response delayed 200 ms cannot pass a 60 ms recovery deadline
  (observed failure verdict at 61 ms)
- SIGTERM removes the run-owned latency toxic and exits 130
- XML 1.0-invalid Unicode characters in a scenario name produce valid JUnit
- An existing output file stays untouched and prevents any fault mutation
- A separately created real proxy and its user-owned toxic remain unchanged

Reviewed `src/main.rs` SHA-256:
`9caed63861719e3aa92b1f98057886517f4e74f5c07d8588edc9068ccb948705`.

## Hosted CI — initial commit passed

- Hosted CI passed for initial commit `d09922ca930e173b375dab2f56042211fc4371ee`
  on 2026-09-30: [PR run 36709123329](https://github.com/RazieLDG/recovery-lab/actions/runs/36709123329).
  This evidence applies to that exact commit; the licensing-update commit has
  its own CI run, whose live status is available on the PR.

## Not verified / remaining limits

- macOS, Windows, other architectures and alternate Rust/Toxiproxy versions
  have not been exercised
- No production or remote faults were run; remote targets are deliberately rejected
- SIGKILL, power loss and unreachable control planes cannot guarantee cleanup
- No distributed/concurrent deterministic replay, sustained recovery window,
  latency-SLO assertion, or body/business-response assertion is claimed
- A package release still needs a separate publication decision; license selection
  was resolved by the owner-approved Apache-2.0 update

Local validation was completed before repository synchronization. Subsequent source
publication is tracked through the repository draft pull request; no software
release, merge, or deployment is included in this validation claim.

## Licensing update — 2026-09-30

The owner approved Apache-2.0 after the initial local validation and draft PR.
The unmodified [official Apache license text](https://www.apache.org/licenses/LICENSE-2.0.txt)
is now in `LICENSE` (SHA-256
`cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30`), and Cargo
metadata declares `license = "Apache-2.0"`. `publish = false` remains in force.
No legal copyright-holder name has been invented. The earlier license-pending
file was removed. `DEPENDENCIES.md` records the metadata-level dependency review
and its limits; it is not a legal compliance certification.

Checks were rerun after the licensing changes; logs are retained at
`evidence/license-update-rust-checks.log` and
`evidence/license-update-integration.log`. These do not replace the original
dated evidence. The Rust implementation was unchanged.
