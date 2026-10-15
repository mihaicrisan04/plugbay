# Config schema

The semantics are settled ([plan.md](plan.md#config-model)). The bar for the file shape: as elegant and obvious as `mise.toml`. Someone should read a `plugbay.toml` once and understand it.

**Status:** v0.2 adds the catalog (below) and `plugbay use` / `unuse` / `add` / `remove` to edit files without opening them. v0.1 ships the shape below (Direction B with a small bundled registry). It is provisional: Mihai has not signed it off yet, and the parser lives in one module (`src/config.rs` plus its stages in `src/config/`: `file.rs` parses, `merge.rs` layers, `build.rs` resolves servers) so changing it stays cheap. The original drafts and open questions are kept further down.

## v0.1 shape

```toml
# ~/work/plugbay.toml — every repo under ~/work inherits this
[servers]
linear = "initech"        # registry server, account "initech"
sentry = "initech"

# ~/work/client-a/plugbay.toml — every repo for this client
account = "client-a"       # default account for servers in this tree
[servers]
sentry = true              # → sentry:client-a
posthog = true             # → posthog:client-a
linear = false             # not for this client

# ~/work/client-a/app/plugbay.toml
[servers]
posthog = { exclude_tools = ["delete_*"] }
```

### Top-level keys

| key | meaning |
|---|---|
| `root = true` | stop inheriting from parent directories (the global config still applies) |
| `account = "x"` | default account for servers in this tree that don't name one |
| `[servers]` | server name → entry |
| `[accounts.<server>.<account>]` | account metadata, any file |
| `trusted_paths = ["~/work/**"]` | global config only: trust executable config under these globs |

### Server entries

Values are polymorphic, like mise tool entries:

| value | meaning |
|---|---|
| `"acct"` | enable with this account |
| `true` | enable with the tree's default account (`account = ...`), else `default` |
| `false` | disable (an inherited server) |
| `["a", "b"]` | explicit multi-account opt-in: both exposed, tools prefixed `name-a__`, `name-b__` |
| `{ ... }` | inline table, fields below; enables the server unless `enabled = false` |

Table fields:

| field | meaning |
|---|---|
| `enabled` | bool, default `true` |
| `account` / `accounts` | one account, or the multi-account list |
| `url` | streamable HTTP endpoint (custom remote server, or override a registry URL) |
| `headers` | `{ Name = "value" }`; values may hold secret refs, though a token for a remote server is better stored with `plugbay login <server> --token` (bound to its url, no trust needed). Names are case-insensitive (stored lowercase; two names differing only in case in one table are an error). `Host`, `Content-Length`, `Transfer-Encoding` and `Connection` are rejected: only the `url` decides where requests go |
| `command`, `args`, `env`, `cwd` | stdio server; needs trust when it comes from a project file |
| `tools`, `exclude_tools` | glob filters on the upstream tool names |

A server needs either `url` or `command`, unless its name is in the registry (`src/registry.rs`). Registry entries are remote-only, so they never run code.

### Catalog

Definitions you want everywhere but pinned only in some trees live in the global config, switched off:

```toml
# ~/.config/plugbay/config.toml   (plugbay add wa --url http://127.0.0.1:7373/mcp)
[servers.wa]
url = "http://127.0.0.1:7373/mcp"
enabled = false

[servers.teams]                   # plugbay add teams -- npx -y msteams-mcp@0.28.0
command = "npx"
args = ["-y", "msteams-mcp@0.28.0"]
enabled = false
```

A tree then pins it like a registry server (`wa = true`, `teams = "work"`; `plugbay use wa` writes that). The global config is always trusted, so catalog stdio servers never prompt for trust, and project files stay pure pins. `ls` lists catalog entries as available, not as disabled; agents can switch one on for their session with `plugbay__enable`.

### Merge rules

- Files load in order: global `~/.config/plugbay/config.toml`, then for each directory from `/` down to the project dir `plugbay.toml` then `plugbay.local.toml`. A file with `root = true` drops everything before it except the global config
- Deeper wins field by field: `posthog = { exclude_tools = [...] }` keeps the inherited account and URL
- `"acct"`, `true` and `false` only touch `enabled` and the account; the rest of the definition is inherited
- Setting `url` clears an inherited `command` and vice versa (a server is one transport)
- Table-valued fields (`headers`, `env`) merge per key (header names case-insensitively, so `Authorization` overrides an inherited `authorization`); lists (`args`, `tools`) replace
- Account metadata merges per key the same way

### Accounts

```toml
[accounts.posthog.client-a]
region = "eu"
url = "https://mcp-eu.posthog.com/mcp"          # optional: the account picks the endpoint
headers = { Authorization = "Bearer ${op://Clients/posthog-a/key}" }
```

`url` and `headers` override the server's for that account; `env = { NAME = "..." }` merges over a stdio server's `env` per key (per-account API keys: `env = { API_KEY = "${op://...}" }`). Like a server's `env`, account `env` always counts as executable config for trust. String fields other than `url`, `headers`, `env` and `expect` are free-form metadata shown to the agent in the server instructions ("PostHog → client-a, region eu").

```toml
[accounts.linear.personal]
expect = "mihaicrisan"     # the login must really be this workspace
```

Verified accounts: when a registry server that defines a whoami tool (Linear `get_workspace`, GitHub `get_me`, …) connects, the daemon asks it who the login is (at most daily) and records the answer in `~/.local/state/plugbay/accounts.toml`. `ls`, `accounts`, the agent's instructions and `plugbay__status` show it next to the label; two labels of one server resolving to the same login are flagged by `accounts` and `doctor`. `expect` is a rule, not metadata: when the verified login doesn't contain its text (case-insensitive), the server is off and the agent is told to have the human `plugbay logout` / `plugbay login` it. An unverified login passes. Only the registry chooses the whoami tool, and only for the registry's own url, so committed config can't make plugbay call a tool.

### Identity and accounts conflicts

- A server's identity is its normalised URL (remote) or a hash of command + args (stdio). OAuth tokens are stored per identity + account, so renaming a server or redefining a name to another URL never reuses a token
- Two enabled entries with the same identity but different accounts (`sentry = "a"` plus `sentry-b = { url = <sentry url>, account = "b" }`) is an error: both are left out and `ls` explains why. The list form is the only way to get two accounts of one server
- Two entries whose tools would share a prefix (`alpha = ["client-a", "b"]` exposes `alpha-client-a__*`, as would a server named `alpha-client-a`) is an error too: all of them are left out

### Secret refs

`${op://vault/item/field}`, `${keychain:service/account}`, `${env:NAME}`, `${fnox:NAME}` inside `headers` and `env` values (server or account level), with literal text around them (`"Bearer ${op://...}"`). A `${` anywhere else (`url`, `command`, `args`, `cwd`, tool globs, account metadata) is an error and the server is left out. Config files never hold secret values.

### Trust

Only executable parts need trust: stdio `command`/`args`/`env`/`cwd` (server or account `env`), header values holding a secret ref, and the `url` (server or account level) of a server whose headers hold a secret ref. The last one stops a committed file from pointing a global server's `Authorization = "Bearer ${op://...}"` at a host it chose. A file's fingerprint hashes those parts (every `url` included), so other edits never re-prompt; it is only reported for files some server actually needs trust from, so remote servers without secret refs never prompt. Global config is always trusted.

### Naming rules

Server names and accounts: lowercase letters, digits and `-` (no `_`: tools are exposed as `server__tool`). The server name `plugbay` is reserved for plugbay's own tools (`plugbay__status`).

## Open questions (still open after v0.1)

1. Registry: bundled list only for now. Seed from the official MCP Registry later? Who decides `linear` means `mcp.linear.app`?
2. Profiles: dropped for v0.1; the directory tree is the composition. Revisit if repos for one client live in unrelated trees (`include`?)
3. Is `account` the right word? Alternatives: `as`, `identity`, `org`, `workspace`
4. Tree-wide tool filters (`exclude_tools` at top level)?
5. Should stdio servers be visually distinct (they need trust) so a reviewer spots them in a diff?

## History: Draft A (felt too heavy)

```toml
[servers.linear]
url = "https://mcp.linear.app/mcp"
account = "initech"

[profiles.client-a]
servers = { posthog = "client-a", sentry = "client-a" }
```

Problems: URLs repeated in every tree, three concepts (servers, accounts, profiles) visible at once, `linear = false` at top level next to `use` while servers live under `[servers]`. Direction B (registry + polymorphic values + tree-wide `account`) replaced it and is what v0.1 implements.
