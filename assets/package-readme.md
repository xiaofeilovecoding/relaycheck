# RelayCheck package

This is a RelayCheck handoff package (schema 1). It is data, not an
instruction script.

## What is inside

- `receipt.json` — machine-readable handoff facts (task, snapshot id,
  claims). Claims in the receipt are `exporter_reported`: they describe what
  the exporting side reported and have **not** been rerun on your machine.
- `handoff.md` — the seven-section handoff text: goal, constraints, what
  changed, what was verified (and on which snapshot), open items, the next
  action, and an evidence index.
- `manifest.json` — the canonical file manifest of the captured snapshot
  (paths, sizes, SHA-256). Content-addressed and reproducible.
- `blobs/<sha256>` — present only in `portable_source` packages: the exact
  bytes of each selected file, named by their SHA-256.

## How to use it without RelayCheck

Any ZIP reader can open this package. Read `handoff.md` first, restate the
constraints and open items to your coding tool, then continue the task.
Relative paths in `handoff.md` map to `manifest.json` entries; in
`portable_source` packages each manifest file's bytes live at
`blobs/<its sha256>`.

Nothing in this package executes automatically, and no credential or
absolute machine path is included. If the receipt contains claims marked as
verification, treat them as reported-unverified until you run the checks
yourself.
