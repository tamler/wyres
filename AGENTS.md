# AGENTS.md — build brief (Milestone 2: mail checking + activity feed)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults in [docs/DECISIONS.md](docs/DECISIONS.md). This file is the scope for the current coding milestone.

## Status

Milestone 2: **complete** (definition of done met). Milestone 1 (local core) shipped in tamler/wyres#1. Keep the [README.md](README.md) status current in every PR.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** only (`docs/SPEC.md`, `docs/DECISIONS.md`, this file).
- **Do not** copy code, schemas, SQL, proprietary phrasing, or distinctive APIs from `mcp_agent_mail`, `mcp_agent_mail_rust`, forks, or any project with a non-permissive license rider.
- Public docs for MCP, `rmcp`, SQLite, Rust crates, and each client's hook documentation are fine.

## Locked defaults

See [docs/DECISIONS.md](docs/DECISIONS.md): Apache-2.0; agent names unique per project (`name@project`); DB at `~/.wyres` (or `$WYRES_HOME`); Rust + `rmcp` + SQLite; hooks-first wake; free and open source always; loopback-only binds; MSRV Rust 1.88.

## Milestone 2 — in scope

The v1 delivery guarantee: agents learn about mail because a lifecycle hook injects a tiny digest, not because they poll.

1. **Activity feed** — a readable projection of the event log, by category `mail | request | hold | resource | session | system | federation`.
   - MCP `activity_feed(since?, limit?, kinds?)` (oldest first, `next_since` cursor; `since` = duration, RFC 3339 time, or event id).
   - MCP `activity_summarize(since?)` (counts, active agents, request/hold totals, highlights, text digest).
   - CLI `wyres activity [--since] [--limit] [--kinds] [--summary] [--json]`.
   - Items carry one-line summaries and ids only — **never message bodies**.
2. **Session events** — `session.started` / `session.ended` recorded in the event log (`heartbeat` with `start` / `end`; hooks call them).
3. **Hook packs** (POSIX `sh`) for Claude Code, Codex, Grok CLI, and Cursor (best effort), all calling one wrapper that runs `wyres hook run <event> --client <c>`:
   - session start → register agent if needed, `session.started`, capped inbox + activity digest;
   - each prompt → heartbeat (renew holds) and a one-line notice only for mail not yet announced;
   - session end → `session.ended`, release holds.
4. **`wyres hook install|uninstall --client <claude|codex|grok|cursor|all> [--dry-run]`** and `wyres hook status`:
   - idempotent; backs up any file it changes; never clobbers existing hooks or unrelated settings;
   - refuses to edit invalid JSON or files it does not own; all-or-nothing across clients.
5. **Docs** — [docs/HOOKS.md](docs/HOOKS.md) install guide per client (including trust steps).
6. **Tests** — feed/summary/session events in core; install/uninstall/run against a temporary `HOME`; MCP tools.

### Definition of done (Milestone 2)

- [x] `activity_feed` + `activity_summarize` MCP tools and `wyres activity` (with `--summary`)
- [x] `session.started` / `session.ended` events; feed never exposes message bodies
- [x] Hook packs for Claude Code, Codex, Grok CLI, Cursor (best effort) with a capped digest (≤ 1500 chars)
- [x] `wyres hook install|uninstall|status` — idempotent, backed up, non-clobbering, `--dry-run`
- [x] docs/HOOKS.md install guide
- [x] Tests run against a temp HOME
- [x] CI green: test + clippy (`-D warnings`) + fmt + MSRV 1.88
- [x] README status updated

## Explicitly OUT of scope (later milestones)

- Resource registry, orphan scanner, reap (Milestone 3)
- Authenticated remote peers, tokens, `GET /activity` over HTTP (Milestone 4)
- Federation / pairing / sync (Milestone 5)
- Herdr plugin, tmux/cmux wake adapters, release builds (Milestone 6)

## Implementation notes

- Hooks must **fail open**: `wyres hook run` always exits 0 and prints nothing on error; the wrapper exits 0 even if `wyres` is missing.
- Default hook identity is `<client>@<project>` (project = nearest git root of the client's cwd), registered on first use; `$WYRES_AGENT` overrides it. No client gets special treatment.
- Injected text is factual and short; keep it under the cap and include ids so agents can act through MCP or the CLI.
- Clients differ: see the decisions recorded in docs/DECISIONS.md (Grok de-duplication, Cursor prompt hooks).

## Keep the README status current

Whenever you complete a definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same PR.
