# plugbay — plan

Decided in a grill session on 2026-10-13. Research behind it: [research.md](research.md). The config file shape is still open: [config-schema.md](config-schema.md).

## Vision

mise for MCP servers. You declare which MCP servers (and which account for each) a directory tree needs in small committable files. Every AI agent you run there gets exactly those servers, with credentials resolved securely on your machine. One setup, every agent, the right client account in the right repo.

Pain it solves, in order:

1. Wrong servers or accounts for a project (client mix-ups: client A's Sentry in client B's repo)
2. Configuring the same servers again in every agent
3. Tool bloat from servers a project doesn't need

Gap in the market (see research): nobody combines hierarchical committable config, first-class accounts per server, and one shared router for every agent. Toolport is closest but GUI-first, one profile per folder, accounts as duplicated servers.

## Positioning

- Open source, Apache-2.0. Product ambition only if it proves value; a future hosted tier (credential vault, team sync) stays closed (open core)
- CLI-first, no GUI. Built fresh, borrowing ideas: Toolport (shim → daemon), 1MCP (per-project template instances, signed context), ToolHive (OAuth), mcpd (manifest vs secrets split)
- Validation: dogfood 2–4 weeks → 3–5 agency colleagues with multiple clients → public launch once it has saved a real client mix-up (that story is the launch post)

## Clients

- First-class in v1: Claude Code, Codex, T3 Code (T3 wraps the other two; user-scope `~/.claude.json` servers already reach T3's Claude sessions)
- Everything else best-effort through the same generic shim, no client-specific hacks until asked

## Config model

Exact syntax is open (see [config-schema.md](config-schema.md)); these semantics are settled:

- Project file `plugbay.toml`, personal overrides `plugbay.local.toml` (gitignored), global `~/.config/plugbay/config.toml`. Config files never hold secrets (AWS CLI style split)
- Committed files may define servers, accounts (metadata only, e.g. region) and reusable bundles. A teammate cd-ing in is asked once to log in to any account they lack
- Resolution walks from `/` down to the project dir like mise: deeper wins, merged field by field, so "same server, other account" is a one-liner
- Disable an inherited server (`name = false`); `root = true` stops inheritance entirely (hard wall for client repos)
- Binding one server to two accounts is an error unless explicitly opted in, which then exposes both with account-prefixed tools
- Optional per-server tool filters (`tools` / `exclude_tools` globs). Servers are the main unit
- `[mcp]` inside `mise.toml` is a later experiment

## Security

- Credentials are keyed by server identity (remote URL / OAuth issuer / stdio command hash) plus account. A repo that redefines `linear` to another URL gets no token
- Trust, VS Code style: only config that executes code (stdio `command`, `${cmd:}`) needs it. First session in such a directory gets one in-agent prompt (elicitation, answered by the human, never the agent): `allow_once` (session) / `allow_always` (this folder). Untrusted stdio servers stay off; remote-only configs never prompt
  - Hash only the executable parts, so routine edits never re-prompt
  - "Trust parent" via CLI; global `trusted_paths = ["~/work/**"]`
  - Single-enum prompt so T3's Codex approval UI maps it cleanly
  - Fallback where elicitation is declined (T3 Claude sessions today, Codex with `approval_policy = never`): server instructions tell the agent to ask you to run `plugbay trust`
- OAuth client per spec 2026-07-28: discovery (RFC 9728/8414), PKCE, resource indicators, `iss` validation, registration via CIMD → pre-registered → DCR → manual `client_id`. Treat all server metadata URLs as hostile
- Figma deferred (it allowlists OAuth clients). GitHub via PAT or our own OAuth app
- Storage: OAuth tokens in the OS keychain, all in one `plugbay-vault` item (they rotate, so the store must be writable; one item = one prompt per unsigned upgrade). Static tokens for remote servers via `plugbay login --token`, bound like OAuth (v0.2). Secret refs (`op://`, `keychain:`, `env:`, `fnox:`) remain for stdio `env` and unusual headers. Roaming comes free through 1Password until a hosted tier exists

## Runtime

- Each agent gets one global MCP entry: the `plugbay` stdio shim. It connects over a unix socket (never env vars: Codex allowlists env, Gemini strips secret-looking vars) to a daemon
- The shim auto-starts the daemon, which exits when idle. Optional `plugbay daemon install` (launchd/systemd); in-process fallback if the daemon can't start
- Project detection: `--project` / `CLAUDE_PROJECT_DIR` > MCP roots (deprecated in spec 2026-07-28, hint only) > shim cwd (ignore `/` and `$HOME`) > global
- Speak protocol 2025-11-25 to clients for now
- Worktrees: resolve config by git common dir, run upstream servers with the worktree as cwd
- Upstream servers shared across sessions per (server identity, account), ref-counted
- Tools passed through as `server__tool` (short server keys, no underscores, names ≤ 64 chars). Rely on native tool search (Claude Code, Codex); meta-tool search mode later for clients without it
- Server instructions state the active account ("PostHog → client-a, EU") so agent and human see which client they touch
- Missing credential: the server appears as a single `<server>__login` stub tool (URL elicitation) plus `plugbay login <server>:<account>`
- Daemon watches config files and sends `tools/list_changed` (live in Claude Code; Codex never re-fetches, so it picks changes up next session)

## v0.1 scope

- In: shim + daemon, config hierarchy, accounts, remote HTTP (OAuth or static headers via secret refs), stdio servers behind trust, keychain + `op://`, `setup` for Claude Code / Codex / T3
- CLI: `ls` (resolved servers here), `why <server>` (which file set what, like `mise config ls`), `login`, `trust`, `import` (migrate native configs, with backup), `doctor` (warn on duplicates with native config)
- Out: meta-tool search, other clients, mise integration, hosted tier, Figma, GUI, approval policies

## Stack

- Leaning Rust: tokio + `rmcp` 3.5 (official SDK, the one Codex uses, full OAuth client incl. CIMD), `keyring` crate, ~10 MB daemon, easy cross-compile, `fnox-core` embeddable
- A TypeScript spike runs in the sibling repo `../plugbay-ts` (Effect v4 + `@modelcontextprotocol/client`, `bun build --compile`). Known costs there: Effect only ships an MCP server (no client, no OAuth); compiled daemon ~72 MB binary / ~47 MB idle RSS; cross-compiling drops the N-API keyring addon
- Both spikes build the same thin slice, then pick on dev experience and how well agents can debug it, not just performance

## Spike slice (both repos)

1. `plugbay` binary with `connect` (stdio shim) and `daemon` subcommands, talking over a unix socket under `$TMPDIR` or `~/.local/state/plugbay/`
2. Shim reports project dir; daemon walks up and merges `plugbay.toml` files (minimal schema)
3. One remote OAuth server (Linear) with tokens in the keychain, one stdio server
4. Aggregated tools served back to Claude Code and Codex; `cd` into two dirs with different config and see different tool sets

## v0.1 implementation decisions (2026-10-13)

Made while building v0.1 on `feat/v0.1`; revisit any of them freely.

- Config shape: Direction B from [config-schema.md](config-schema.md) with a small bundled registry, provisional until Mihai signs it off. Account tables may also set `url`, `headers` and `env` (stdio servers need per-account keys)
- Secret refs in a project file count as executable config and need trust, like stdio commands: otherwise a committed file could send `${env:AWS_SECRET_ACCESS_KEY}` or a keychain item to a URL it chose
- Wire protocol: the shim writes one JSON hello line (`session` with project hints, or `control`), then the socket carries plain MCP. The shim is a byte pipe; the daemon runs the rmcp server per connection
- Daemon single-instance via a lock file in the state dir; socket `~/.local/state/plugbay/daemon.sock` (0600, dir 0700); idle exit after 10 min; upstreams kept 60 s after their last session. `PLUGBAY_HOME` relocates config + state (tests)
- If the daemon can't start, the shim serves the session in-process
- Stdio upstreams are shared per (identity, account, working dir), so each worktree gets its own process; remote ones per (identity, account)
- A remote server without a static `Authorization` header uses stored OAuth tokens if any, else connects anonymously and turns a 401 challenge into the `<server>__login` stub
- Trust prompt: shown at most once per version of the untrusted config per daemon run (since 2026-10-17; was once per folder); `allow_once` lasts for the daemon's lifetime. Codex doesn't re-list tools, so tools unlocked mid-session appear in its next session
- Built-in `plugbay__status` tool: what resolved, from which files, and why a server is missing. Server name `plugbay` is reserved
- Config changes are detected by polling file stamps every 2 s per session (simpler than fs watchers across the whole `/`→project chain)
- Secret refs are only allowed in `headers` and `env` values. A project file that changes the `url` of a server carrying secret headers needs trust (otherwise it could redirect an inherited credential)
- Upstreams are shared only when their whole unresolved transport config matches (url incl. query, header/env templates, args), not just identity + account
- OAuth metadata is validated on every discovery (https only; plain http loopback only for loopback servers) before any token is sent; the server's 401 challenge seeds login discovery
- `ls` exits non-zero on config problems; `doctor` on errors
- `Host`, `Content-Length`, `Transfer-Encoding`, `Connection` headers are rejected; header names are case-insensitive (lowercased before merging)
- Stdio upstreams run in their own process group, killed with the daemon, so launcher children (`npx` → `node`) don't linger

## v0.2 direction (2026-10-15)

Dogfooding v0.1 on Mihai's real tree (work / personal / master, eight projects) surfaced the friction: trust prompts on every secret header, commands that only work from the right directory, and a migration that took an afternoon by hand. The fix is mise's split of three jobs that v0.1 mixed in one file:

| layer | holds | lives in | trust |
|---|---|---|---|
| Definitions | how to reach a server (url or command) | bundled registry, user catalog in the global config | never: we or you wrote them |
| Pins | which servers and accounts a tree uses | any `plugbay.toml` | never |
| Credentials | OAuth logins and static tokens | keychain, bound to server identity + account | never: you created them |

Roadmap, in order:

1. **Done:** tokens bound at login (decisions below); trust on sight was tried and dropped
2. **Done:** location-independent CLI: an index of every config directory plugbay has seen; `login` from anywhere and `login` with no args (every missing credential in one go); global `-C <dir>`; `ls --all`; `accounts`; `use` / `unuse` (`-g`, `--local`); `add` / `remove` for the catalog; `plugbay__enable` / `plugbay__disable` for one agent session
3. **Done:** `migrate` replacing `import` (kept, hidden): native configs plus Claude Code's per-directory disable lists → catalog definitions + inferred pin tree; bearer tokens → bound credentials, other secrets → keychain refs; `setup`; native entries removed; one journal; `--undo`
4. **Done:** verified accounts: who a login really is (a registry-defined whoami tool, run by the daemon once per new connection, cached a day in `state/accounts.toml`) shown next to the label in `ls`, `accounts`, `doctor` and the agent's instructions; two labels on the same account flagged; `[accounts.<server>.<label>] expect = "..."` turns a mismatched server off with a message saying how to fix it
5. Then: `search` (registry), curated stdio entries in the registry (convex, xcodebuild), `tools`, `call`, `logs`, `restart`, `status`, `export` (plain `.mcp.json` for teammates/CI), `prune`, `completion`, `self-update`
6. Spike, later: meta-tool search mode so agents load tools on demand

Decisions made for step 1 (Mihai said go ahead on the proposal; both defaults are revisitable):

- `plugbay login <server> --token` (hidden prompt or stdin), `--from-env VAR`, `--header NAME` store a static token in keychain service `plugbay-token` under `<identity>#<account>`, the same key OAuth uses in `plugbay-oauth`. Config files no longer need `${keychain:...}` header refs, so they need no trust. A token is never taken as a CLI argument (shell history, `ps`)
- Order on connect: an `Authorization` header from config, else a stored token, else a stored OAuth login. A rejected stored token is reported as such, not as the OAuth login stub
- ~~Files written on this machine are trusted on sight and recorded in `trust.toml`: outside git, untracked, ignored, or with uncommitted edits, and not carrying macOS's download quarantine mark. A file exactly as committed (clone, pull, checkout) still asks.~~ Dropped on 2026-10-17, see below
- The catalog of definitions stays in the global `config.toml` (it is already the always-trusted file); no separate `servers.toml`
- ~~Known limit: a new token only applied on reconnect.~~ Fixed in step 4: `login`/`logout` send `credentials_changed`, the daemon closes that account's pooled connections and every session reconnects with the new credentials

Decisions made for steps 2 and 3 (Mihai approved the session-enable idea; the rest are defaults he can revisit):

- **Catalog = global `[servers]` entries with `enabled = false`.** No new table: the merge rules already let a pin (`wa = true`, `wa = "home"`) inherit the definition. `ls` lists them as "available from the catalog"; they never show as "disabled". `plugbay add <name> --url ...` / `plugbay add <name> -- <command...>` writes one (checked by resolving it before keeping the edit), `remove` deletes it and lists directories that still pin it
- **Index** (`state/index.toml`): directories holding a `plugbay.toml` / `plugbay.local.toml`, recorded whenever config resolves (agent sessions and CLI commands); entries whose files are gone are skipped. Powers `ls --all`, `accounts`, `login` without a target and `login <server>` from anywhere (this directory first, then the one match in the index, then catalog/registry; several matches → asks for `server:account`)
- `login` falls back to asking for an API token when the server has no OAuth (TTY only), so "is it OAuth or a token?" is no longer the user's question
- `use` / `unuse` edit with `toml_edit` (comments kept). `use` reverts its edit and explains when the server can't resolve (unknown → "add it first"). `unuse` removes a pin, keeps a definition by setting `enabled = false`, and writes `name = false` when the server is inherited. Refuses `/` and `$HOME` (use `-g`)
- **Session pins** (`plugbay__enable { server, account? }`, `plugbay__disable { server }`): an in-memory layer merged after every file, per agent session, never saved. Only names plugbay can already reach (catalog, registry, inherited definitions) — the agent can't pass a url or command, so prompt injection can't make it run code or send a token somewhere new. The client's own tool approval is the human gate. A pin that doesn't connect (unknown, failed, untrusted) is rolled back; "needs login" stays on with its login tool. Verified live in Claude Code: the new tools are usable in the same turn (through ToolSearch). Codex ignores `list_changed`, so there it only applies next session
- **migrate:** definitions all go to the global catalog (trusted, so no trust prompts ever). A user-wide server switched off in some directories becomes either "on everywhere + `false` at the topmost uniformly-off directories" or "catalog + `true` at the topmost uniformly-on directories", whichever needs fewer pins (ties keep it on everywhere); pins never climb to `$HOME`. Local-scope servers are pinned in their directory. Pins inside a git checkout go to `plugbay.local.toml`. `.mcp.json` entries are pinned but never removed (team file). Accounts are `default`; rename with `use`
- **migrate --undo** restores every plugbay file and the Codex config from the journal, deletes keychain entries and tokens the run created (pre-existing ones are never recorded), removes the `plugbay` entry from Claude Code if the run added it, and re-adds removed Claude Code entries from the saved copy of `~/.claude.json` unless they hold a literal secret (never put on a command line; the user copies those back). `~/.claude.json` itself is never restored wholesale: Claude Code rewrites it constantly
- Verified accounts (step 4): only registry entries carry a whoami (`tool`, `arguments`, JSON pointers to show), matched by server identity, so committed config can never choose which tool plugbay calls; a whoami tool annotated as not read-only is refused. Answers are hostile input: over 64 KiB refused, control/bidi characters stripped, 80 chars max. Not in the keychain (not secret, and reads would prompt). Configured for linear, github, sentry, atlassian, vercel, stripe, supabase; posthog and notion wait for a confirmed tool. Servers whose whoami lists every org a user can see (sentry, atlassian, vercel) describe the user's reach rather than one workspace, so the "same account" check is worded as "fine if intended"
- **One keychain item (`plugbay-vault`)** holds every secret plugbay stores itself (OAuth logins, tokens, `${keychain:plugbay/...}` refs) as a JSON map. Dogfooding showed why: macOS asks before an unrecognised app reads each item, an unsigned binary is unrecognised after every rebuild/upgrade, and status commands read every login, so N logins meant N+ prompts per upgrade. Now it is one. Reads are cached per process (the daemon drops its cache on `reload` / `credentials_changed`); writes re-read the item under `state/vault.lock`. Old per-login items are copied in on first read and left in place (deleting them could prompt again). User-owned `${keychain:<other service>/...}` refs are untouched. Tests use `PLUGBAY_INSECURE_TEST_VAULT=<file>` so they never touch the keychain. Developer ID signing (zero prompts) is a launch item in docs/releasing.md
- `setup` writes the `PATH` link to the binary (`/opt/homebrew/bin/plugbay`), not the versioned file behind it, so `brew upgrade` doesn't break agents. With no keychain at all (Linux without a Secret Service), reads find nothing and writes explain what to start

## Hardening decisions (2026-10-17)

From an audit of the session runtime; Mihai approved each.

- **No trust on sight.** A project file's executable parts (stdio commands, secret refs) run only with a hash grant: `plugbay trust`, the in-agent `allow_once` / `allow_always` prompt, or a `trusted_paths` glob. The global config stays always trusted. Why: an agent (possibly prompt-injected) can write files, so a fresh `plugbay.local.toml` with a `command` or a `${env:AWS_SECRET_ACCESS_KEY}` header would have run or been sent by the daemon outside the agent's sandbox; `curl | tar` output also lacks the quarantine mark. Nothing plugbay writes itself needs a grant: definitions live in the global catalog and `use` / `migrate` only write pins to project files. Existing grants keep working; the `revoked` list in `trust.toml` is no longer written or read
- **Edits ask again, live.** The prompt is keyed by the untrusted files and their fingerprints, so editing a trusted command mid-session drops its tools (`tools/list_changed`) and asks the human again; a denied version is not asked about again until the next edit or `plugbay trust --revoke`. Clients without elicitation see the server as untrusted in the instructions and `plugbay__status`, which point to `plugbay trust`
- **Refreshes are serialised.** One lock covers resolving config and applying it, so a slower, older resolution can't overwrite a newer one (it used to wipe a fresh `plugbay__enable` pin), and the first refresh is single-flight
- **Upstreams connect in the background, concurrently.** The session's state lock is never held across an `.await`; each new server sits in "connecting" until its own task finishes. A refresh (and so `initialize` and the first `tools/list`) waits at most 8 s (`READY_DEADLINE`, under Codex's 10 s startup timeout) and returns what is ready; stragglers send `tools/list_changed` when they land. Claude Code picks them up; Codex ignores the notification, so `plugbay__status` says those servers appear next session there. A slow upstream `tools/list` is cut off at the same deadline with the same follow-up. `plugbay__status` and calls to connected servers never wait on a slow one
- **`initialize` waits for the first refresh** (bounded as above), so the server instructions list only servers whose tools are exposed. Failed, untrusted, still connecting, wrong-account and needs-login servers go on one "Not available right now" line pointing to `plugbay__status`. Stdio servers with only the implicit `default` account show no account label
- **`plugbay__enable` can't switch a pinned account on its own.** If config files give the server an account here (a server pin or the tree's `account = ...`) and the agent asks for another, the human is asked through elicitation ("switch <server> from <a> to <b> for this session?"). Declined, or no elicitation in the client: refused, telling the agent to ask the human to run `plugbay use <server>:<account>`. No prompt when no file names an account or the account is the same
- An in-agent login (`<server>__login`) forgets the cached verified account, like the CLI login, so the next connection is verified afresh
- **Identities are never shown raw.** `ServerIdentity` has no `Display`; every message, status line and CLI output uses `masked()` (conflict problems, `why`, `login --token`, `migrate`), so a key in a url path or userinfo can't reach the model. The raw form is only a credential key

## Known gaps after v0.1 (follow-ups)

- No byte cap on remote (HTTP) upstream responses: rmcp's HTTP adapter buffers whole JSON bodies. Stdio messages are capped at 16 MiB
- Upstream stderr is logged with resolved secret env values redacted (lines capped at 4 KiB); other secrets a server prints itself are not caught
- Server urls are masked for display (userinfo, query values, fragment, token-looking path segments), but the masking of path tokens is a heuristic, and `ServerIdentity` itself (the credential key) still holds the full path and userinfo; it is rendered only through `ServerIdentity::masked()`
- Version skew: after an upgrade the running daemon keeps the old version until `plugbay stop` (doctor warns). Retiring it when a newer shim connects was rejected for now: a retired daemon still serving old sessions would no longer receive `logout` / `trust --revoke` control messages
- Remote MCP requests don't follow redirects and have a 10s connect timeout; a read only times out after 10 minutes of silence (the longest tool call), so idle event streams reconnect at most that often
- OAuth registration is DCR-only until we host a CIMD document; no manual `client_id` flow yet
- Untested against live services: a real OAuth login (Linear), `setup`/`import --apply` against real Claude Code and Codex configs, keychain prompts on macOS
- The status tool, `ls` and `doctor` each render "what resolved here" separately; worth one shared report model

## Next steps

1. Sign off (or change) the provisional config schema ([config-schema.md](config-schema.md))
2. v0.1 is built here in Rust (`feat/v0.1`); compare with the TS spike in `../plugbay-ts` if still useful
3. Real-service checks from the gaps list above
4. Dogfood → colleagues → launch
5. Side quest: PR to T3 Code wiring `onElicitation` into its Claude adapter so trust prompts work there

## Naming

- `plugbay` (may change). Short alias TBD: avoid `pg` (reads as Postgres); `pb` or `plug` are candidates
- Taken / avoid: patchbay, switchboard, conduit, tether, mcpmux, mcpx. Free alternatives: jackfield, patchd, mcpenv
