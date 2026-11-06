# Contributing

GitHub Issues and pull requests are welcome. Describe a concrete workflow or
include a minimal reproduction. A maintainer reviews changes after CI.

## Layout

```text
src/            Rust service + CLI (single binary; the web UI is embedded)
web/            React/TypeScript frontend (built into the binary)
tests/          Integration gate suites (real git repos, real check processes)
schemas/        JSON Schema for receipt.json / manifest.json
docs/           API contract and public screenshots made with synthetic data
scripts/        Browser gates and local verification utilities
```

## Gate map (spec RELAYCHECK-SPEC.md §9)

| Gate | Where |
| --- | --- |
| R01 closed loop | `tests/r01_closed_loop.rs` |
| R02 byte fidelity / limits / secrets paths | `tests/r02_fidelity.rs` |
| R03 version binding (A/B/A, revision/profile/epoch CAS) | `tests/r03_version_binding.rs` |
| R04 forged packages / external claims | `tests/r04_forged_import.rs` |
| R05 check semantics (PASS text, deps, invalidation, truncation, env) | `tests/r05_check_semantics.rs` |
| R06 concurrency + idempotency + write slot | `tests/r10_http.rs` |
| R07 cancel / tree kill / crash recovery | `tests/r07_cancel_restart.rs` |
| R08 zip + path hardening | `tests/r08_zip_hardening.rs` |
| R09 malicious git config, corrupt blobs, clean-filter hazard | `tests/r09_malicious_git.rs` |
| R10 HTTP surface (session/CSRF/Origin/Host/SSE/bearer) | `tests/r10_http.rs` |
| R11 secrets in files/output/handoff | `src/secrets.rs` tests, `r02`, `r05` |
| R12 XSS/CSP/no external requests | production screenshots probe (`scripts/capture_screenshots.py` prints `EXTERNAL_REQUESTS`), CSP test in `r10_http.rs` |
| R13 return association rules | `tests/r04_forged_import.rs` |
| R14 export immutability | `tests/r10_http.rs` |
| R15/R16 production run + visuals | `scripts/capture_screenshots.py`, `scripts/verify_runtime.py` |
| R17 resources | `scripts/verify_runtime.py` (report JSON) |
| R18 no-install read | `scripts/verify_runtime.py` (zipfile section) |
| R19 handoff card rules | `src/handoff.rs` tests + r01 |
| R20 return baseline rules | `tests/r01_closed_loop.rs` (+ diff baseline in `src/http.rs` build_task_detail) |
| R21 matched-fixture comparison | `scripts/relay_experiment.py` → `r21-experiment.json` |
| V01–V06 visual gates | production screenshots in `docs/img/` + artifacts |

## Commands

```text
npm --prefix web ci
npm --prefix web run build
cargo test --locked
cargo build --release --locked
python -m pip install playwright==1.64.1
python -m playwright install chromium
python scripts/run_ui_gates.py target/release/relaycheck target/ui-gates.json
```

On Windows use `target/release/relaycheck.exe` for the browser gate. Git and
Python 3 must be on PATH. Set `RELAYCHECK_TEST_ROOT` to redirect test scratch
files from the system temporary directory.

## Rules this project holds itself to

- Real assertions only: no skipped assertions, no `or True`, no sleeps
  standing in for conditions.
- A fix lands together with the regression that failed first.
- Screenshots are real production captures — never mock-ups.
- Product data and internal reports stay local. Public screenshots use synthetic data.
- Honesty over gloss: unverified things are recorded as unverified.
