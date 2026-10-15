# Contributing

Issues and pull requests are welcome. For anything larger than a fix, open an issue first so we can agree on the approach.

## Verify loop

Every change should pass what CI runs:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The end-to-end tests in `tests/` drive the real binary against `examples/echo_server.rs`, over real unix sockets.

## Debugging

- `plugbay daemon --log-level debug` runs the daemon in the foreground. Otherwise it logs to `~/.local/state/plugbay/daemon.log`.
- `PLUGBAY_HOME=/tmp/somewhere` isolates config and state from your real setup. The tests use it.
- Inside an agent, the `plugbay__status` tool shows which servers and accounts were resolved and why any are missing.

Never paste real tokens or API keys into issues, test fixtures or logs.

## Style

Readability first, then performance. Reuse crates before writing abstractions. The house style lives in [`.agents/skills/plugbay-rust`](.agents/skills/plugbay-rust/SKILL.md); read it before writing Rust here. [`docs/plan.md`](docs/plan.md) records the design decisions.
