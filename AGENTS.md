# AGENTS.md — build brief (Milestone 4: authenticated remote peer)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults and milestone decisions in [docs/DECISIONS.md](docs/DECISIONS.md). This file is the scope for the current coding milestone.

## Status

Milestone 4: **complete** (definition of done met). Earlier: M1 local core (tamler/wyres#1), M2 mail checking + activity feed (tamler/wyres#2), M3 resource registry (tamler/wyres#3). Keep the [README.md](README.md) status current in every PR.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** only.
- **Do not** copy code, schemas, SQL, phrasing, or distinctive APIs from `mcp_agent_mail*`, forks, or any project with a non-permissive license rider.
- Public docs for MCP, `rmcp`, axum, Tailscale, Cloudflare and OpenSSH are fine.

## Locked defaults

Apache-2.0; agent names unique per project; DB at `~/.wyres`; Rust + `rmcp` + SQLite; hooks-first; free and open source always; MSRV 1.88. **Never bind beyond loopback without auth. Never reap remotely.**

## Milestone 4 — in scope

Grok Bot (and any remote agent) as a first-class peer over authenticated streamable HTTP MCP.

1. **Tokens** — `wyres token create --agent NAME [--project GLOB]… [--label] [--expires]`, `wyres token list`, `wyres token revoke ID`. A token acts as exactly one agent and only within its project globs; only a SHA-256 hash is stored and the secret is shown once.
2. **Authenticated HTTP** — `wyres serve --auth`: `/mcp` and `GET /activity` require `Authorization: Bearer`; every request is verified (tunnels arrive from loopback). `--allowed-host` accepts tunnel hostnames.
3. **Scoping** — per-tool rules: the acting agent must be the token's agent (filled in when omitted); project arguments must match the globs; threads only if the agent is a participant; feed/summary/resource_find are node-wide read-only awareness; `orphans_list` never scans processes remotely.
4. **Never `remote_reap`** — `resource_reap` is refused for every token.
5. **Audit log** — every remote request (allowed, failed or denied) with token, agent, operation, outcome, redacted arguments and remote address; `wyres audit`.
6. **Binding** — a non-loopback bind needs both `--auth` and `--allow-remote-bind`; otherwise refused.
7. **`GET /activity`** — `since`, `limit`, `kinds`; same JSON as `activity_feed`; no message bodies.
8. **Docs** — [docs/REMOTE.md](docs/REMOTE.md): Tailscale Serve, Cloudflare Tunnel, `ssh -R`, `program=grok_bot` registration.

### Definition of done (Milestone 4)

- [x] `wyres token create|list|revoke`; hashed secrets; expiry; revocation effective on the next request
- [x] `wyres serve --auth` with bearer verification on `/mcp` and `GET /activity`; `--allowed-host`
- [x] Agent + project-glob scoping for every tool; `resource_reap` never allowed remotely
- [x] Audit log of every remote call (redacted); `wyres audit`
- [x] Non-loopback bind only with `--auth` and `--allow-remote-bind`
- [x] Tests: 401s, scoping, denials, audit entries and redaction, GET /activity, revocation, expiry, CLI and bind rules
- [x] docs/REMOTE.md (Tailscale Serve, Cloudflare Tunnel, ssh -R, grok_bot registration)
- [x] CI green: test + clippy (`-D warnings`) + fmt + MSRV 1.88
- [x] README status updated

## Explicitly OUT of scope (later milestones)

- Node pairing, per-node keys, event-log sync, cross-node mail (Milestone 5)
- Built-in TLS termination (use a tunnel or a reverse proxy), OAuth
- Wake adapters, Herdr plugin, release builds (Milestone 6)

## Keep the README status current

Whenever you complete a definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same PR.
