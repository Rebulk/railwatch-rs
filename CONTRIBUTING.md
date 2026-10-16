# Contributing

Issues and pull requests are welcome. For anything bigger than a bug fix,
open an issue first so we can agree on the shape before you write it.
Security problems go through [SECURITY.md](SECURITY.md), not public issues.

## Build

You need Rust 1.95 (CI's pinned toolchain; `rust-version` is the floor),
Node 22 + pnpm 10 (versions pinned in `bin/build`; `mise` picks them up if
installed), Ruby (bin/build unpacks the railwatch gem's frontend from its
pinned, checksummed `.gem`) and `brotli`.

```sh
bin/build                  # frontend into frontend/dist, then target/release/railwatch with it embedded
SKIP_CARGO=1 bin/build     # frontend only (the server embeds frontend/dist, so tests need it)
```

## Test

What CI runs (`.github/workflows/ci.yml`); a PR needs all of it green:

```sh
SKIP_CARGO=1 bin/build
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo audit --deny warnings
cargo deny --locked check  # licenses (deny.toml), bans, sources, advisories
```

`bin/release vX.Y.Z` builds the release artifacts reproducibly in a
digest-pinned container (docs/SELF-HOST.md, "Releases"); it needs Docker.
Install proofs (`bench/install/*.sh`) exercise the installer, TLS, upgrades
and backups in throwaway containers.

## Differential harnesses

This server is a port of the Rails implementation (railwatch-cloud @
464f0fd, gem 0.9.x). Where Rails computes something (ingest rows, rollups,
t-digests, grouping, dashboard props, issues, MCP, `/api/v1`, detectors),
the harnesses in `bench/diff/` feed both servers the same input and diff
the output; `docs/DIFFERENCES.md` lists every known difference and why.

- Pure Rust checks run in `cargo test`: `mapper_vs_ruby`, `tdigest_vs_ruby`
  and `schema` compare against Ruby outputs captured in
  `crates/server/tests/fixtures/`.
- The live harnesses (`bench/diff/run.sh`, `props/`, `issues/`, `mcp/`,
  `api/`, `detectors/`, `google/`) need the Rails baseline running in
  Docker (`bench/rails/up.sh`, see `bench/rails/README.md`). Building it
  needs a railwatch-cloud checkout, which is not public today; their
  `last-diff*.txt` files are the most recent results.

## Conventions

- **Same telemetry schema.** Tenant files stay byte-compatible with the
  gem's (`bench/export/schema_export.json`). Server-only tables go in the
  control database, never in a tenant file.
- **Name Rust modules after the Ruby classes they port**, so a reader can
  diff them side by side.
- **Never differ silently.** A deliberate improvement over Rails goes in
  `docs/DIFFERENCES.md` with the reason and a measurement.
- **Measure performance work** against a realistic tenant (`rw-load` builds
  one) and report the numbers in the PR.
- Small, logical commits. Dependencies must pass `cargo deny` (permissive
  licenses only; see deny.toml) and GitHub Actions are pinned to full
  commit SHAs.
- Shell gotcha: `pkill -f 'railwatch serve'` matches your own shell when
  the command line contains that string; use `pkill -x railwatch`.

By contributing you agree that your contribution is licensed under the
project's license, AGPL-3.0-only.
