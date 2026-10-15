---
name: plugbay-rust
description: How to write, test and debug Rust in the plugbay repo — house style (readability over cleverness), error handling, tokio and rmcp patterns, logging, and the verify loop. Use before writing or reviewing any Rust code here, together with rust-best-practices.
---

# Rust in plugbay

House rules distilled from Zed's `.rules`, mise's agent guide and Mihai's preferences. Where they disagree with `rust-best-practices` (Apollo), this file wins.

## Readability first

- Correctness and clarity before speed. Take a small performance cost (a clone, a second pass, an extra allocation) whenever it makes the code easier to read. Optimise only with a measurement
- No clever one-liners: long iterator chains with nested closures, `Option` combinator soup, or turbofish puzzles become a named function, a `match`, or a plain `for` loop
- Full words for names (`server`, `account`, `upstream`, never `s`, `acct`, `up`). Functions say what they do: `resolve_project_dir`, not `rpd` or `handle`
- A function that needs a paragraph of comment is the wrong function: split it and name the parts. Comments explain *why* only, in a line or two. No comments that narrate the code, no section banners
- Prefer adding to an existing file over creating many small files; a new file is a new logical component. Never `mod.rs`: use `src/thing.rs` + `src/thing/part.rs`
- Reuse a crate or an existing helper before writing an abstraction. No traits with one implementation, no generics where a concrete type reads better

## Errors

- The binary uses `anyhow`; add `.context("what we were doing")` / `.with_context(|| format!(..))` at each I/O or parse boundary so messages read like a story
- Use `thiserror` only when callers branch on the error kind
- No `unwrap()` / `expect()` / indexing that can panic outside tests. Use `?`, `let ... else`, `.get()`
- Never `let _ = fallible();` to drop an error. Propagate it, or log it: `if let Err(error) = x { tracing::warn!(%error, "...") }`
- Errors that reach an agent (tool results, server instructions) must say what to do next: "run `plugbay login linear:initech`"

## Async (tokio)

- Never block the runtime: wrap keychain calls, `std::fs` on hot paths, and child-process waits in `tokio::task::spawn_blocking` or use `tokio::fs` / `tokio::process`
- Scope clones for spawned tasks with shadowing so ownership is obvious:
  ```rust
  tokio::spawn({
      let sessions = sessions.clone();
      async move { sessions.notify_all().await }
  });
  ```
- Don't hold a `Mutex` guard across `.await`. Use `tokio::sync::Mutex` only when you must; otherwise copy what you need out of a `std::sync::Mutex` and drop the guard
- Every spawned task either has an owner that awaits/aborts it or logs its own failure. Put timeouts (`tokio::time::timeout`) on anything talking to an upstream server

## rmcp

- Source is in `~/.cargo/registry/src/*/rmcp-3.5.*`; read it (and its `examples/` upstream) instead of guessing APIs. Model types are `#[non_exhaustive]`: build them with their constructors/`Default` + field assignment, not struct literals
- Server side: implement `ServerHandler`; client side: `().serve(transport)` or a `ClientHandler`. Talk to the client through `context.peer` (elicitation, roots, notifications)

## Security rules that touch code

- Never log or print secret values. Use `secrets::redact` style helpers; `Debug` impls on types holding secrets must redact
- Treat upstream metadata (OAuth endpoints, URLs, tool descriptions) as hostile input: validate schemes and hosts before use
- Tests and fixtures never contain real tokens; use obviously fake values like `test-token`

## Logging

- `tracing` with structured fields: `tracing::info!(server = %name, account, "connected")`. `debug!` for per-request detail, `info!` for lifecycle, `warn!` for recoverable problems
- The shim logs to stderr (clients capture it); the daemon logs to `~/.local/state/plugbay/daemon.log` and to the terminal in the foreground (`plugbay daemon --log-level debug`)

## Tests

- Unit tests in the same file under `#[cfg(test)] mod tests`. Name them as behaviour: `deeper_file_overrides_account`
- Use `tempfile::TempDir` + `Paths::for_test` / `PLUGBAY_HOME` so tests never touch the real home, keychain or 1Password. Keychain/`op` paths are tested through their parsing, not their side effects
- `insta` snapshots for CLI output where the exact text matters

## Verify loop

Run before reporting work as done, and fix everything it reports:

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```
