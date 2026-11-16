# Changelog

Each release's GitHub page also lists its pull requests (generated from the
tag). This file is the human summary. Versions follow semver; a `-rc.N`
tag is a release candidate (marked pre-release, not "latest", so
`install.sh` and `railwatch upgrade` do not pick it up).

## v0.3.0 (2026-11-16)

This is the first release since railwatch.rebulk.com moved off Rails, and
since the decision that this is the one Railwatch server. The Rails app
(railwatch-cloud) is now a frozen reference.

**Memory and scale** (2 threads / 4 GiB: 250 apps -> 500; docs/SCALE.md)
- Memory per tenant is about 3.5x smaller: steady private memory per app
  fell from 8.8 to 2.5 MiB (10 -> 100 apps). At 500 apps on 2 threads /
  4 GiB the server holds steady at about 1.5 GiB of private memory, where
  v0.2.x was killed at the cap (docs/SCALE.md).
- Server memory is bounded: idle connections close, and readers and rollup
  accumulators are capped across tenants (#90).
- Each tenant's memory is fixed at the root. There is no per-batch
  savepoint, so private memory stays flat at 500 apps instead of growing
  until the cap (#92).
- WAL checkpoints move off the ingest writer to one background
  checkpointer thread. Each WAL is capped: past 8000 pages (32 MB) the
  writer checkpoints inline, as SQLite's autocheckpoint would (#92,
  docs/PERF.md "Background checkpointer").
- No transparent huge pages for the server process: `PR_SET_THP_DISABLE`,
  the same as `MIMALLOC_ALLOW_THP=0`. On a THP-`always` host this about
  halves memory under ingest (#89).
- Faster ingest: parsing takes 9-12% less CPU per event (#89), and the
  writer thread's CPU per 1k events fell from 34.5 to 20.8 ms at one batch
  per transaction. At 50 apps the server took 17,211 events/s, against
  13,889 before (#92).
- Dashboard reads by day open only the day files inside the window, not
  every file the tenant has (#87).
- New tenant files make seven indexes partial, at 0.82 -> 0.79 GB per
  million events. Existing files are not altered (#86, DIFFERENCES #16).

**Features**
- Uptime monitors with incidents, alerts and a public status page (#75; also
  in v0.2.19).

**Security**
- rusqlite 0.41 and rustix 1.1.6 for two RustSec advisories (#84).

**Docs and benchmarks**
- Rails vs Rust: 3 runs each, the full server on both sides
  (docs/RAILS-VS-RUST.md, #88). App-count scale test on 2 threads / 4 GiB
  (docs/SCALE.md, #91).
- The marketing site moved to railwatch.dev as a static build (#79).

**Upgrading** from v0.2.x: `sudo railwatch upgrade`. Behind kamal-proxy,
copy the new binary to `bin/railwatch.new` first, then run `railwatch-swap`.
Without `railwatch.new` it only restarts the current binary
(deploy/kamal-proxy/README.md). The control DB gains the uptime tables (migration 4,
applied on start). Telemetry files need no migration.

## v0.2.19 (2026-11-01)
- Uptime monitors with incidents, alerts and a public status page (#75).

## v0.2.18 (2026-10-29)
- Filters fail visibly: `a|b` alternatives, errors for unknown keys and
  values, `get_execution` by id alone (#71).

## v0.2.17 (2026-10-28)
- Session journeys across tabs (#70).

Earlier releases: see the
[GitHub releases](https://github.com/Rebulk/railwatch-rs/releases).
