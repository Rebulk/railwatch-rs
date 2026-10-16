# Plan (2026-10-13)

Decisions: plain axum from scratch + inertia-omega (rev 06754a7) for Inertia;
telemetry SQLite schema byte-identical to the railwatch gem's; phased with gates; built as a spike, then hardened for release.

| # | Deliverable | Verified by |
|---|---|---|
| 0 | Bench harness + Rails baseline (prod Docker image of railwatch-cloud @ main) | RSS idle/load, events/s at p99<250ms, CPU/event, 1/10/50 envs; saved to bench/results |
| 1 | Rust ingest: /ingest (direct + receipted), /ingest/ping, /deploys, /kamal, /sourcemaps; token auth, pause, quota | Differential test vs Ruby Mapper/Writer (identical rows), all wire fixtures, real gem app. GATE 1: head-to-head |
| 2 | Rollups (t-digest bytes identical to Ruby), receipt followups, exception grouping, prune | Rollup rows identical to RollupJob on the same raw data |
| 3 | Dashboard on the gem's React pages via inertia-omega | Props diffed against Rails on the same tenant file; page timings |
| 4 | Single binary (embedded assets) + install script | Timed fresh-VPS install to first event |
| 5 | Report | docs/RESULTS.md |

Out of spike: MCP, /api/v1, Linear, Google OAuth, Slack, billing, digest.
Stop rule: if Gate 1 does not clearly beat Rails on RSS and events/s, recommend stopping.
