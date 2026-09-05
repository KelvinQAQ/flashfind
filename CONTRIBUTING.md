# Contributing to FlashFind

## Development setup

Install the Rust toolchain specified by `rust-toolchain.toml`, then run:

```bash
cargo build
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

## Test levels

Fast integration checks:

```bash
./scripts/integration_test.py --quick
```

Full daemon integration suite, including a 5,000-file directory rename stress test:

```bash
./scripts/integration_test.py
```

Filesystem event latency benchmark:

```bash
./scripts/benchmark_event_updates.py --iterations 5
```

## Daemon debugging

```bash
flashfind daemon start
flashfind daemon start --wait
flashfind daemon status
flashfind daemon logs --follow
flashfind daemon stop
```

Use an isolated data directory for manual tests:

```bash
XDG_DATA_HOME=/tmp/flashfind-dev ./target/release/flashfind daemon --root /tmp/root start --wait
```

## Pull requests

- Keep one logical change per PR.
- Add a regression test for bugs and state the affected platform/filesystem.
- Run format, unit tests, clippy, and the appropriate integration suite before requesting review.
- Do not commit SQLite indexes, daemon tokens, endpoint files, logs, or release binaries.
- Use concise Conventional Commit-style subjects, for example `fix: recover stale daemon endpoint`.

## Performance changes

Include a reproducible command, root size, iteration count, and before/after median and p95. Do not merge an optimization that makes correctness, consistency, or tail latency worse.
