# plugbay

mise for MCP servers. Small committable `plugbay.toml` files declare which MCP servers, and which account for each, a directory tree needs. Every AI agent started there (Claude Code, Codex, T3 Code) gets exactly those servers through one global entry, with credentials resolved securely on the machine.

Status: v0.1 in progress on `feat/v0.1`. This repo is the Rust implementation; `../plugbay-ts` is a TypeScript + Effect v4 spike of the same slice for comparison.

## Read first

- `docs/plan.md`: vision, positioning, every settled decision, v0.1 scope, spike slice. Source of truth; update it when a decision changes
- `docs/config-schema.md`: the config file shape is **open**. Goal is mise-level elegance. Settle it with Mihai before building the parser beyond the spike's minimal schema
- `docs/research.md`: competitors (Toolport, 1MCP, Docker MCP Gateway, ToolHive…), client behaviour facts, OAuth realities

## Core ideas (details in the plan)

- **Hierarchy like mise:** walk from `/` to the project dir, deeper wins, merged field by field. `name = false` disables an inherited server, `root = true` stops inheritance. `plugbay.local.toml` holds personal overrides
- **Accounts are first-class:** the same server with different accounts per project (`posthog:client-a` vs `posthog:client-b`). Binding one server to two accounts is an error unless explicitly opted in. Wrong-account mix-ups are the #1 pain this exists to kill
- **Router, not a config generator:** each agent runs the `plugbay` stdio shim → unix socket → auto-started daemon that resolves config, shares upstream servers per (server identity, account), and passes tools through as `server__tool`
- **Project detection:** `CLAUDE_PROJECT_DIR` / explicit flag > MCP roots (hint only) > shim cwd (ignore `/`, `$HOME`) > global
- **Config never holds secrets.** OAuth logins and static tokens (`plugbay login <server> --token`) live in the OS keychain; stdio env secrets are refs (`op://`, `keychain:`, `env:`, `fnox:`)
- **Credentials bind to server identity** (URL / OAuth issuer / stdio command hash) plus account, so a repo can't redirect a token by redefining a server name
- **Trust only for executable config** (stdio commands, secret refs) in project files: one in-agent prompt, `plugbay trust` as fallback, hashing only the executable parts so an edit to them asks again. Never trusted on sight: an agent can write files too. The global config is always trusted

## Stack

- Rust, tokio, `rmcp` 3.x (official SDK; the one Codex uses; full OAuth client incl. CIMD/DCR/refresh), `keyring` for the keychain, possibly `fnox-core` for secret refs
- Single binary: `plugbay connect` (shim), `plugbay daemon`, plus user commands (`ls`, `accounts`, `use`, `add`, `login`, `trust`, `migrate`, `doctor`, `setup`; all take `-C <dir>`)
- Verify loop: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (end-to-end tests in `tests/` drive the real binary against `examples/echo_server.rs`)
- Debug: `plugbay daemon --log-level debug` runs the daemon in the foreground; otherwise it logs to `~/.local/state/plugbay/daemon.log`. `PLUGBAY_HOME=/tmp/x` isolates config + state (tests use it). Agents can call the `plugbay__status` tool

## Working rules

- Readability first, then performance. Reuse crates before writing abstractions
- Agents must be able to debug this: structured logs (`tracing`) from daemon and shim, a `--log-level` flag, and a way to run the daemon in the foreground
- Never write real tokens or API keys into the repo, test fixtures or logs. Redact secret-shaped values in debug output
- Treat all upstream server metadata (OAuth endpoints, URLs) as hostile input; validate before use (lesson from mcp-remote CVE-2025-6514)
- Test against real clients early: Claude Code and Codex are the v1 targets, T3 Code comes for free through them
- Keep `docs/plan.md` current; record new decisions there rather than in chat

## Code map

- `src/main.rs` CLI · `shim.rs` (`connect`) → `daemon.rs` (socket, control, idle exit) → `session.rs` (one agent: refresh, upstream connects, routing, config watch) + `session/{enable,login,prompt,render,tools}.rs` (session pins, login stubs, trust/account prompts, instructions and status, built-in tools) → `upstream.rs` (shared upstream pool) + `upstream/stdio.rs` (process groups, message cap)
- `config.rs` + `config/{file,merge,build}.rs` + `registry.rs` + `project.rs`: resolution (catalog = global entries with `enabled = false`) · `index.rs`: every config directory seen, for commands that work from anywhere
- `secrets.rs`, `credentials.rs`, `oauth.rs`, `trust.rs`, `identity.rs`: security and verified accounts
- `commands/`: `access.rs` (login/logout/trust), `inspect.rs` (ls/accounts/why), `pins.rs` (use/unuse/add/remove) · `clients.rs` + `clients/{claude,codex,import,migrate}.rs`: native client configs, `setup`, `migrate` · `doctor.rs`

## Skills

Project skills live in `.agents/skills/`; `.claude/skills` is a symlink to it, so both Claude and other agents see the same set. Load `plugbay-rust` (house style, wins on conflicts) and `rust-best-practices` (Apollo's handbook) before writing Rust here.
