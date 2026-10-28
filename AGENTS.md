# AGENTS.md — build brief (Milestone 1 only)

**Read this before writing code.** Product vision and later milestones live in [docs/SPEC.md](docs/SPEC.md) and [docs/DECISIONS.md](docs/DECISIONS.md). This file is the **only** scope for the current coding milestone.

## Status

Milestone 1: **complete** (definition of done met; CI green). Keep the [README.md](README.md) status line accurate as you finish work (and when you open PRs).

## Clean-room rules (mandatory)

- Implement from **this repository’s docs** (`docs/SPEC.md`, `docs/DECISIONS.md`, this file) only.
- **Do not** copy code, schemas, SQL, proprietary phrasing, or distinctive APIs from:
  - `mcp_agent_mail`, `mcp_agent_mail_rust`, or forks
  - any project with a non-permissive license **rider** (e.g. OpenAI/Anthropic restricted MIT variants)
- Reading public docs for MCP, `rmcp`, SQLite, and Rust crates is fine. Prior art in SPEC.md is for **ideas only**.
- Prefer boring, original names and schemas that fit Wyres.

## Locked defaults

See [docs/DECISIONS.md](docs/DECISIONS.md):

- License: **Apache-2.0**
- Agent names unique per project, addressed as `name@project` (bare `name` only when unambiguous)
- DB: **`~/.wyres`** (or `$WYRES_HOME`)
- Beads: deferred
- Stack: **Rust + official `rmcp` + SQLite**
- Wake: **hooks-first** (hook packs themselves can land after M1 core; design for them)
- Free and open source, always

## Milestone 1 — in scope

Deliver a working local Wyres core:

1. **Rust workspace** with a daemon and a `wyres` CLI binary (single repo / Cargo workspace is fine).
2. **SQLite** store under `~/.wyres` (WAL), with:
   - `node_id` on durable rows (local node created on first boot)
   - **append-only event-log-shaped records** (`event_id` ULID, `node_id`, `ts`, `actor`, `kind`, `payload`, optional `prev_id`) as the source of truth; projection tables for queries are OK
3. **Agent register / identity** — register agents with program (`claude|grok|codex|cursor|grok_bot|other`), touch `last_seen`
4. **Mail** — send, inbox, reply, threads (immutable messages; per-recipient delivery/read/ack state)
5. **Requests** — status `open | claimed | done | stale | cancelled`, with owner; claim / complete / release
6. **File holds** — path/glob leases with TTL + heartbeat; conflict check; auto-expire via sweeper; release on explicit end/heartbeat-end
7. **MCP tools (~8)** over **rmcp** with **stdio** and **streamable HTTP**, bound **local-only** (loopback) by default
8. **Tests + CI** — `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` via GitHub Actions

### Suggested ~8 MCP tools (M1)

Keep the surface small; combine related mutations if needed, but expose these capabilities:

| # | Tool | Purpose |
|---|---|---|
| 1 | `ensure_project` | Register absolute project path |
| 2 | `register_agent` | Register/update agent identity; heartbeat last_seen |
| 3 | `send_message` | Send to recipients (optional create_request) |
| 4 | `fetch_inbox` | Unread / urgent / open requests for an agent |
| 5 | `acknowledge` | Mark read and/or ack a delivery |
| 6 | `claim_request` | Claim / complete / release (or split if cleaner) |
| 7 | `hold_paths` | Acquire / renew / release path holds; conflict report |
| 8 | `heartbeat` | Session alive; renew owned holds; optional `--end` release |

CLI should mirror the same capabilities for humans/scripts (`wyres serve`, `wyres inbox`, `wyres send`, …).

### Definition of done (Milestone 1)

- [x] `cargo build --release` produces `wyres`
- [x] `wyres serve` starts local MCP (stdio and/or loopback streamable HTTP); refuses non-local binds without an explicit future flag (M1: local only)
- [x] Fresh install creates `~/.wyres` (or `$WYRES_HOME`) DB with `node_id` + event log
- [x] Integration or unit tests cover: register agent, send→inbox→ack, request claim→done, hold acquire→conflict→expire/release
- [x] CI green: test + clippy + fmt
- [x] README status updated to reflect M1 completion (or “in progress” with accurate checklist)
- [x] No secrets, tokens, private emails/phones, or machine-specific absolute paths in the tree

## Explicitly OUT of scope (do not build in M1)

- Resource registry (ports/GPU/model servers, orphans, reap)
- Federation / pairing / sync / mesh / hub relay
- Web UI / TUI
- Herdr plugin (and cmux/tmux wake adapters)
- Hook pack installers (document the contract; implement later if time — **do not block M1**)
- Beads integration
- Public/authenticated remote MCP for Grok Bot (design only; M1 is local-only)

If you finish early, improve tests, docs, and CLI UX — do **not** start OUT-of-scope features without a new milestone brief.

## Implementation notes

- Prefer `rusqlite` or `sqlx` with migrations.
- ULIDs for `event_id` / `node_id` (crate of your choice that is maintained and Apache/MIT).
- Event log first: mutating operations append an event, then update projections in the same transaction.
- Errors: clear, structured JSON from CLI `--json` where applicable.
- Logging: tracing; no PII beyond agent names and paths the user already passed in.

## Keep the README status current

Whenever you complete a Definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same commit/PR.
