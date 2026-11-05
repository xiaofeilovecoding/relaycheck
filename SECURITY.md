# Security policy

## Reporting

RelayCheck 0.1 is a local review candidate without a public issue tracker
yet. Report suspected vulnerabilities privately to the maintainer (see the
repository host once published); do not open public issues for
vulnerability reports. There is no paid bounty program.

## Scope

In scope for 0.1:

- the local HTTP surface (session bootstrap, CSRF/Origin/Host, SSE, CLI
  bearer channel),
- the capture/import validation pipeline (path boundaries, ZIP hardening,
  secret scanning),
- process execution of user-approved check profiles (environment
  allowlist, Job Object containment on Windows),
- the embedded web UI (XSS surface, CSP).

Out of scope for 0.1 (known limitations, documented honestly):

- RelayCheck is **not a sandbox**. An approved check runs YOUR code with
  YOUR user's permissions; the scratch copy and Job Object give lifecycle
  control, not privilege isolation.
- Package hashes bind content, not senders. Nothing authenticates who
  produced a package.
- Environment changes outside the recorded `environment_epoch` (external
  tool upgrades, OS changes) are not detected.
- Remote access is out of scope by design: the service binds to
  127.0.0.1 only and has no remote mode.

## Design notes

- Secrets: capture blocks high-confidence hits with the offending paths;
  check output is redacted before storage; handoff narrative fields are
  scanned before export. A hit never silently deletes a file.
- Git is invoked read-only with a fixed argv whitelist, isolated config
  (`GIT_CONFIG_NOSYSTEM`, null global/system config) and hooks pinned to
  an empty directory. `git status` is deliberately never run — it can
  execute `.gitattributes` clean filters on modified files.
- Imports unpack into a quarantine directory with per-entry validation
  (traversal, duplicates, size limits, symlink rejection) and never write
  into a registered repository.
- The accepted snapshot is immutable content; accepting requires all
  required checks to have passed on the current (revision, snapshot,
  profile hash, environment epoch) combination.
