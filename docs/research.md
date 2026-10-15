# "mise for MCP" — research notes (2026-10-13)

## Idea
Local daemon routes MCP servers to any agent. Agents connect via one global stdio shim from any cwd.
Daemon resolves hierarchical committable `mcp.toml` (walk up, child overrides/disables), spawns/connects
upstream servers, injects creds from a secure store. Accounts per server (posthog:client-a / client-b)
composed into reusable profiles referenced from project config. Later: hosted per-user vault for roaming.

## Local competitors (ranked by closeness)
1. **Toolport** (Rust/Tauri, MIT, 227★, very active, v1.24 2026-10-07, paid Teams)
   - stdio adapter → one shared daemon; sends cwd + client-id + profile headers; root = TOOLPORT_ROOT > roots > cwd
   - central GUI `registry.json`; `folder_profiles` = longest path prefix → ONE profile; no inheritance, not committable
   - keychain (keyring crate), OAuth DCR+refresh, lazy search + code mode, list_changed passthrough
   - multi-account = duplicate server entries
2. **1MCP** (TS, Apache-2.0, 516★, v0.39.1 2026-10-12)
   - HTTP runtime + `1mcp proxy` stdio bridge; proxy sends signed project context from cwd
   - global `mcp.json` with tags/presets; `.1mcprc` walk-up nearest-file only selects preset/tags (can't define servers)
   - template servers: Handlebars `{{project.path}}` → one instance per context
   - keychain via docker-credential-helpers, SDK OAuthClientProvider
3. **Docker MCP Gateway** (Go, 1.6k★) — profiles exportable/committable/OCI push, keychain OAuth on CE; needs Docker, no shared daemon, no hierarchy
4. **mcpproxy-go** (Go, 390★) — polished daemon, BM25 search, quarantine, keyring, OAuth; zero project awareness
5. **ToolHive** (Go, 2.3k★, Stacklok) — best OAuth (RFC 9728/8414/DCR/PKCE) + secrets (keyring-encrypted, 1Password); container/k8s-oriented
6. **mcp-hub** (JS, 520★, dormant) — ordered global+project config merge, `${cmd: op read …}` resolver
7. **mcpd** (Go, Mozilla, 184★) — committable `.mcpd.toml` + separate local secrets file; REST not MCP to agents
- Also: MCPM (1k★, v2 dropped daemon, plain-JSON secrets), MetaMCP (2.7k★, Postgres server, namespaces→endpoints, slowing),
  MCP Router (Electron, non-OSS license, projects), mcpmini (1★, Go, nice small OAuth code), house-mcp-manager (dormant)

## Gap nobody fills
- hierarchical merged committable config (best is nearest-file or longest-prefix → one profile)
- first-class accounts per server (everyone duplicates servers)
- repo config referencing local reusable profiles while creds stay off-repo

## Hosted / credential brokers
- Composio Tool Router: best multi-account (aliases, 2–10 per toolkit, pin per session) → possible backend
- Smithery (acquired by Arcade 2026-08): namespaces ≈ profiles, one URL, tools prefixed; agent.pw OSS cred layer
- Arcade, Klavis Strata, Glama, MintMCP, Cloudflare MCP Portals, Pipedream (→ Workday)
- claude.ai connectors auto-sync into Claude Code: roam, but Claude-only + one account per service
- No hosted player does per-directory routing
- Incidents: Smithery path traversal 2025, mcp-remote CVE-2025-6514, rmcp CVE-2026-63127, Vercel/Context.ai OAuth token breach 2026

## OAuth reality (spec 2026-07-28)
- registration priority: pre-registered > CIMD (host JSON on own domain) > DCR (deprecated fallback, application_type native)
- creds keyed by issuer; PKCE, RFC 8707 resource, RFC 9207 iss validation; treat metadata URLs as hostile
- Linear/Sentry/Notion: DCR ok. PostHog: OAuth or project-scoped API key (great for accounts). GitHub: no DCR → own OAuth app or PAT.
  **Figma: allowlists client_name → unknown clients 403** (business step to get in catalog)
- refresh tokens rotate → need writable store (keychain/1Password), not git-encrypted files

## Client facts that shape the design
- Project detection order: explicit arg / `CLAUDE_PROJECT_DIR` > roots (if declared) > shim cwd (ignore `/`, `$HOME`) > global
- Spec 2026-07-28 deprecates roots + goes stateless → shim-reported cwd is the reliable signal; speak 2025-11-25 to clients for now
- Codex: cwd = session cwd, env allowlisted, **never re-fetches tools on list_changed**, no roots → tool set fixed at spawn
- Gemini strips *TOKEN*/*KEY* env; Codex allowlists env → reach daemon via fixed unix socket, not env
- Tool search native: Claude Code, Codex, Cursor, VS Code (virtual tools). None: Gemini, opencode, Zed. Caps: VS Code 128, Devin 100
  → optional meta-tool mode (search_tools/call_tool) keyed on clientInfo
- Process model: shim per session (Claude/Codex/Gemini), per dir (opencode), per window (VS Code/Cursor) → daemon refcounts upstreams per (root, server+account)
- Worktrees: resolve config by git common dir, run upstream with worktree cwd
- Trust: daemon reading repo config bypasses each client's trust gate → own `trust` (mise-style, hash-pinned)
- Name: "mcpx" taken (mcp.run)

## mise trust/merge model to mirror
- trust by path in state dir; declarations-only files don't need trust, anything executing does
- merge /etc → global → root…cwd, deeper wins, `*.local.toml` > shared, env overlays, `ceiling_paths`
- per-key replace, not deep merge
