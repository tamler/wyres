# Wyres

Free and open source, always: a local-first, vendor-neutral inbox (and later, resource registry) for coding agents. Claude Code, Grok, Codex, and others are treated equally via plain MCP + a shared CLI. Sibling agent-tools project under the [WyreUp](https://wyreup.com/) brand.

**License:** [Apache-2.0](LICENSE) · **Status:** Milestone 1 in progress (no shipped binary yet)

## Docs

| Doc | Purpose |
|---|---|
| [AGENTS.md](AGENTS.md) | **Build brief for Milestone 1** (start here if coding) |
| [CLAUDE.md](CLAUDE.md) | Points Claude Code at AGENTS.md |
| [docs/SPEC.md](docs/SPEC.md) | Full research + product spec |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Locked v1 defaults |

## Direction

- Rust daemon + `wyres` CLI, official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) MCP SDK, SQLite under `~/.wyres`
- Hooks-first agent check (Claude Code, Grok, Codex; Cursor best effort); Herdr / cmux / tmux as optional adapters later
- Requests with status (`open`, `claimed`, `done`, `stale`) and an owner; file holds with TTL + heartbeat
- Schema ready for federation (`node_id` + append-only event log); federation itself is **v2**
- Resource registry, activity feed, and remote peers: see SPEC (after Milestone 1)

## Milestone 1 (current)

Rust workspace, SQLite + event log, agents, mail/threads, requests, file holds, ~8 MCP tools (stdio + local streamable HTTP), tests, and CI. **Out of scope:** resource registry, federation, web UI, Herdr plugin.

## Clean-room

Written from our own spec. Do **not** copy code from MCP Agent Mail or other non-permissive / rider-licensed projects.

## Development (once the crate exists)

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```
