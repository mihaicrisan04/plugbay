# Security

## Reporting a vulnerability

Please report vulnerabilities privately through [GitHub security advisories](https://github.com/mihaicrisan04/plugbay/security/advisories/new), not in public issues. Include the plugbay version (`plugbay --version`), your OS, and steps to reproduce. You should get a reply within a week.

Only the latest release is supported with fixes.

## Threat model in short

plugbay sits between AI agents and the MCP servers and accounts they use, so its job is to keep credentials on your machine and make sure the right account reaches the right project.

- **Config never holds secrets.** OAuth logins and static tokens live in the OS keychain. Environment secrets for stdio servers are references (`op://`, `keychain:`, `env:`, `fnox:`) resolved at runtime.
- **Credentials are bound to server identity plus account.** The identity is the server URL, OAuth issuer or stdio command hash, so a repository cannot redirect a stored token by redefining a server name.
- **Executable config from elsewhere needs trust.** Stdio commands and secret references in a config file that arrived through git (clone, pull, checkout) or a download must be approved once, in the agent or with `plugbay trust`. Files written on this machine are trusted on sight. Trust covers only the executable parts of a file, and a revoked file stays revoked.
- **Upstream metadata is hostile input.** OAuth endpoints and URLs advertised by servers are validated before use.

Reports about any of these guarantees failing are especially welcome.
