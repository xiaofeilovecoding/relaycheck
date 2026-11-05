# Contributing

RelayCheck 0.1 is a review candidate. This document maps the acceptance
gates to code and tests so a reviewer can reproduce everything locally.

## Layout

```text
src/            Rust service + CLI (single binary; the web UI is embedded)
web/            React/TypeScript frontend (built into the binary)
tests/          Integration gate suites (real git repos, real check processes)
schemas/        JSON Schema for receipt.json / manifest.json
docs/           api.md, screenshots (generated locally, not committed)
scripts/        capture_screenshots.py, verify_runtime.py, relay_experiment.py
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
cargo test                                   # all gates (needs python.exe + git on PATH)
cd web && npm install && npm run build       # rebuild embedded UI, then cargo build
cargo build --release                        # single-file production candidate
```

Set `RELAYCHECK_TEST_ROOT` to redirect the integration test scratch area
(default `D:/dev/relaycheck-test-data` on the authoring machine; any temp
dir works).

## Rules this project holds itself to

- Real assertions only: no skipped assertions, no `or True`, no sleeps
  standing in for conditions.
- A fix lands together with the regression that failed first.
- Screenshots are real production captures — never mock-ups.
- Product data, reports and screenshots stay out of the product git history.
- Honesty over gloss: unverified things are recorded as unverified.
