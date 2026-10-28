# Wyres

Free and open source, always: a local-first, vendor-neutral inbox (and later, resource registry) for coding agents. Claude Code, Grok, Codex, and others are treated equally via plain MCP + a shared CLI. Sibling agent-tools project under the [WyreUp](https://wyreup.com/) brand.

**License:** [Apache-2.0](LICENSE) · **Status:** Milestone 2 complete (mail checking + activity feed)

## Status

| Milestone | Scope | Status |
|---|---|---|
| 1 | Local core: SQLite event log, agents, mail, requests, path holds, MCP (stdio + loopback HTTP), CLI | ✅ tamler/wyres#1 |
| 2 | Mail checking: activity feed + summary, session events, hook packs (Claude Code, Codex, Grok CLI, Cursor best effort), `wyres hook install` | ✅ complete |
| 3 | Resource registry: leases, orphan scanner, human-approved reap | planned |
| 4 | Authenticated remote peer (Grok Bot): tokens, audit log, `GET /activity` | planned |
| 5 | Federation (v2): pairing, hub-and-spoke sync, cross-node mail | planned |
| 6 | Polish: wake adapters, Herdr plugin, release builds | planned |

Current milestone brief: [AGENTS.md](AGENTS.md). Every milestone merges only with CI green: test, clippy `-D warnings`, fmt, MSRV 1.88.

## Docs

| Doc | Purpose |
|---|---|
| [AGENTS.md](AGENTS.md) | **Build brief for the current milestone** (start here if coding) |
| [CLAUDE.md](CLAUDE.md) | Points Claude Code at AGENTS.md |
| [docs/SPEC.md](docs/SPEC.md) | Full research + product spec |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Locked defaults and milestone design decisions |
| [docs/HOOKS.md](docs/HOOKS.md) | Hook pack install guide (Claude Code, Codex, Grok CLI, Cursor) |

## Install

Requires Rust 1.88 or newer (the workspace `rust-version`).

```bash
cargo install --path crates/wyres
```

```bash
wyres init
```

`wyres init` creates `~/.wyres/wyres.db` (or `$WYRES_HOME/wyres.db`) and prints the local node id.

## Check mail automatically (hooks)

```bash
wyres hook install --client all
```

Installs session-start / prompt / session-end hooks for Claude Code, Codex, Grok CLI and Cursor (best effort), so each session starts with a short inbox + activity digest and each prompt mentions new mail once. Idempotent, backed up, and never clobbers existing hooks; see [docs/HOOKS.md](docs/HOOKS.md).

## Connect an agent (MCP)

Stdio — one server process per client session, all sharing the same database:

```json
{ "mcpServers": { "wyres": { "command": "wyres", "args": ["serve", "--stdio"] } } }
```

Streamable HTTP — run the daemon once, point clients at `http://127.0.0.1:7466/mcp`:

```bash
wyres serve
```

`wyres serve` binds `127.0.0.1:7466` by default and refuses any non-loopback `--bind`. Both modes run a sweeper that expires lapsed holds and marks overdue requests stale; CLI reads sweep on demand, so the daemon is optional.

### MCP tools

| Tool | Purpose |
|---|---|
| `ensure_project` | Register an absolute project path; returns its slug |
| `register_agent` | Register/update an identity (name, program, optional project/model) and mark it active |
| `send_message` | Send to recipients; `reply_to` replies in-thread; `create_request` opens a claimable request |
| `fetch_inbox` | Unread (default) / urgent messages, counters, and open requests for an agent |
| `threads` | Read one thread, or list an agent's threads |
| `acknowledge` | Mark messages read and acknowledged |
| `claim_request` | `claim` / `release` / `complete` / `cancel` a request |
| `hold_paths` | `acquire` / `check` / `renew` / `release` / `list` advisory path holds |
| `heartbeat` | Mark alive, renew holds, get counts; `start: true` / `end: true` record session start / end (end releases holds) |
| `activity_feed` | Who is doing what: mail, requests, holds, sessions — summaries and ids, never bodies; `since` cursor |
| `activity_summarize` | Counts, active agents, request/hold totals and highlights for a window (default 1h) |

Tool errors are returned as `{"error": {"code", "message"}}` with codes `not_found`, `invalid`, `conflict`, `ambiguous`, `forbidden`.

## CLI

Every command takes `--json`. `--agent`/`--from` default to `$WYRES_AGENT`; `--project` defaults to the current directory.

```bash
wyres agent register alice --program claude
wyres send --from alice --to bob --subject "Review PR" --body "please" --request --due 3600
wyres inbox --agent bob --unread
wyres ack --agent bob <message-id>
wyres reply <message-id> --from bob --body "on it"
wyres threads --agent bob
wyres thread <thread-id>
wyres requests --status open
wyres request claim <request-id> --agent bob
wyres request complete <request-id> --agent bob --note "merged"
wyres hold acquire --agent alice 'src/api/**' --ttl 1800 --reason "refactor"
wyres hold check --agent bob src/api/routes.rs
wyres holds
wyres heartbeat --agent alice
wyres heartbeat --agent alice --end
wyres activity --since 1h --kinds mail,request
wyres activity --summary
wyres hook install --client codex --dry-run
wyres status
wyres events --limit 20
```

`wyres hold acquire` and `wyres hold check` exit with status 3 when a conflict is found, so hooks can branch on it.

## Concepts

- **Agents** are unique per project and addressed as `name@project`. A bare `name` works when only one agent has it; otherwise you get an `ambiguous` error listing the `name@project` choices.
- **Mail** is immutable messages in threads. Each recipient has its own read/ack state. Importance is `low | normal | high | urgent`.
- **Requests** are opened from a message and can be claimed by any agent (advisory ownership). Past their due time they go `stale`, and anyone may take them over. Completing a request mails the requester in the same thread.
- **Holds** are advisory leases on project-relative paths or globs (`src/**`, `Cargo.toml`). Exclusive holds conflict with any overlapping hold of another agent; shared holds only conflict with exclusive ones. `acquire` is all-or-nothing. TTL defaults to 30 minutes (clamped to 1 minute – 4 hours); `heartbeat` renews it.
- **Event log**: every mutation appends `{event_id (ULID), node_id, ts, actor, kind, payload, prev_id}` and updates query tables in the same transaction. Every durable row carries `node_id` so federation (v2) can sync without a breaking migration. `last_seen_at` on a plain heartbeat is presence, not an event.

## Development

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
```

## Clean-room

Written from our own spec. Do **not** copy code from MCP Agent Mail or other non-permissive / rider-licensed projects.
