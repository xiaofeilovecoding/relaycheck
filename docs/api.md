# HTTP API (local, schema 1)

All endpoints are on `http://127.0.0.1:<port>`. The Host header must be
exactly `127.0.0.1:<port>`; cross-origin writes are rejected; there is no
CORS and no WebSocket.

## Session

| Method | Path | Auth | Notes |
| --- | --- | --- | --- |
| POST | `/v1/session/exchange` | one-time `#rcb=` token | body `{"token": "..."}`; sets the HttpOnly/SameSite=Strict session cookie; single use, short TTL |
| POST | `/v1/session/bootstrap` | session + CSRF | mints an extra one-time token (rate-limited) for a second tab |
| GET | `/v1/me` | session | returns the CSRF token |

Write requests (`POST` with side effects) require:

- `X-RelayCheck-CSRF: <csrf>` (from `/v1/me`),
- `Origin: http://127.0.0.1:<port>`,
- `Idempotency-Key: <key>` where noted (same key + same body replays the
  original response; same key + different body → `409`),
- `If-Match: <task revision>` where noted (stale revision → `409`).

## Tasks

| Method | Path | Notes |
| --- | --- | --- |
| POST | `/v1/tasks` | create task; idempotent |
| GET | `/v1/tasks/{id}` | full detail envelope (task, snapshots, verifications, handoffs, imports, acceptances, computed `primary`, stale banner, A→B diff) |
| POST | `/v1/tasks/{id}` | edit goal/constraints/criteria; `If-Match` required; bumps the revision |
| POST | `/v1/tasks/{id}/capture/plan` | scope preview (included/excluded/untracked) |
| POST | `/v1/tasks/{id}/capture` | capture a candidate; body `attest_stopped` must be true; idempotent; async operation |
| POST | `/v1/tasks/{id}/profile` | save the check profile (bumps revision) |
| POST | `/v1/tasks/{id}/verify` | run checks; body `{"trust_confirmed": true}` is the explicit approval; async operation |
| POST | `/v1/tasks/{id}/handoffs` | export the handoff package (immutable) |
| GET | `/v1/handoffs/{id}/package` | download the package bytes (preview and download are the same bytes) |
| POST | `/v1/tasks/{id}/imports` | upload a return package ZIP (raw body, ≤100 MiB); quarantine import |
| POST | `/v1/tasks/{id}/candidate` | confirm an imported snapshot as the new candidate; `If-Match` |
| GET | `/v1/tasks/{id}/diff?from=<snap>&to=<snap>` | change list |
| POST | `/v1/tasks/{id}/acceptances` | accept; `If-Match`; body `{"criteria_ack": [...]}` |
| POST | `/v1/operations/{id}/cancel` | cooperative cancel; stop state may be `E_STOP_UNCONFIRMED` |
| GET | `/v1/operations/{id}` | operation state |
| GET | `/v1/events?after=<seq>` | SSE stream with replay; gap → `E_EVENT_GAP` |
| POST | `/v1/claims` | claim a CLI-registered project with its one-time code |
| POST | `/v1/demo` | create the local demo task |

## CLI channel (Bearer)

`/v1/cli/attach`, `/v1/cli/capture`, `/v1/cli/verify`, `/v1/cli/tasks` —
`Authorization: Bearer <token>` from `<data-dir>/cli-token`. The browser
session cannot call these (and the bearer is not a session).

## Envelope

Success: `{"operation": {...} | null, "revision": n, "result": {...}}`.
Error: `{"error": {"code", "message", "details": []}}`. Details carry only
relative paths and safe identifiers — never raw exceptions or secret
material.

## Error codes

| Code | HTTP | Meaning |
| --- | --- | --- |
| `E_REVISION_CONFLICT` | 409 | If-Match/revision CAS failed |
| `E_EVIDENCE_STALE` | 409 | the passing result does not apply to the current (revision, snapshot, profile, epoch) |
| `E_PROFILE_CHANGED` | 409 | profile hash changed after approval |
| `E_UNVERIFIED_IMPORT` | 422 | package failed validation or a claim tried to pose as a local verification |
| `E_CAPTURE_UNSTABLE` | 409 | files changed during capture; snapshot aborted, nothing activated |
| `E_PATH_BOUNDARY` | 400 | UNC/device/reparse/ADS/traversal/case-conflict/reserved-name rejection |
| `E_SECRET_DETECTED` | 422 | high-confidence secret blocked the capture/handoff |
| `E_LIMIT` | 429 | size/count limits, single write slot busy, bootstrap rate limit |
| `E_STOP_UNCONFIRMED` | 409 | a cancelled process tree did not confirm termination; the write lock is kept |
| `E_EVENT_GAP` | 409 | SSE `after` points outside the retained range; re-read the snapshot |
| `E_CORRUPT` | 500 | blob/DB integrity failure; checks and acceptance are barred |
| `E_PROFILE_UNTRUSTED` | 403 | checks require the explicit user approval |
| `E_MISSING_DEPENDENCY` | 424 | check/prep executable not found |
| `E_VALIDATION` | 400 | malformed input |
| `E_UNAUTHORIZED` / `E_FORBIDDEN` | 401 / 403 | session, CSRF, Origin, Host, bearer |
| `E_NOT_FOUND` | 404 | unknown resource |
| `E_STATE` | 409 | operation not in the required state |
| `E_REQUEST_CONFLICT` | 409 | idempotency key reused with a different body |
| `E_INTERNAL` | 500 | unexpected; the message never includes raw exceptions |
