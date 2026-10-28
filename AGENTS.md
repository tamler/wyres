# AGENTS.md — build brief (Milestone 3: resource registry)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults and milestone decisions in [docs/DECISIONS.md](docs/DECISIONS.md). This file is the scope for the current coding milestone.

## Status

Milestone 3: **complete** (definition of done met). Earlier: M1 local core (tamler/wyres#1), M2 mail checking + activity feed (tamler/wyres#2). Keep the [README.md](README.md) status current in every PR.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** only.
- **Do not** copy code, schemas, SQL, phrasing, or distinctive APIs from `mcp_agent_mail*`, forks, or any project with a non-permissive license rider.
- Public docs for MCP, `rmcp`, SQLite, Rust crates, `lsof`, `ss` and `ps` are fine.

## Locked defaults

Apache-2.0; agent names unique per project; DB at `~/.wyres`; Rust + `rmcp` + SQLite; hooks-first; free and open source always; loopback-only binds; MSRV 1.88. **Never reap without human approval.**

## Milestone 3 — in scope

"Find before start": before starting a model server, dev server or GPU job, an agent asks Wyres whether one is already running and **borrows** it instead of spawning a duplicate.

1. **Registry** — kinds `model_server | dev_server | port | gpu | container | long_job`; identity `kind:name` (e.g. `model_server:ollama@11434`), endpoint, port, pid, project, purpose, metadata (backend, estimated bytes for Apple Silicon unified memory, ...).
2. **Leases** — owner + borrowers, `shared` (refcounted) or `exclusive`, TTL (default 30 min, 1 min–24 h) + heartbeat; sweeper expiry; owner release passes ownership to the oldest borrower; `--stopped` ends it.
3. **Orphans** — resources with no live lease stay listed as orphan candidates (re-adoptable); a macOS-first scanner (`lsof`, `ss` fallback, `ps` for age/command) finds listening processes no registered resource accounts for; system daemons are allowlisted out.
4. **Reap** — needs a grace period (default 10 min) **and** an explicit human `--confirm` on the CLI. MCP `resource_reap` only returns the plan and records a request. Only the current user's non-system processes; SIGTERM, SIGKILL only with `--force`.
5. **Session end** — releases holds and resource leases (`heartbeat --end --keep` keeps resource leases).
6. **Surfaces** — MCP `resource_register`, `resource_find`, `resource_borrow`, `resource_release`, `resource_heartbeat`, `orphans_list`, `resource_reap`; CLI `wyres resources | resource register|heartbeat | borrow | release | orphans | reap`.
7. **Docs** — [docs/RESOURCES.md](docs/RESOURCES.md) and the AGENTS.md snippet below.

### Definition of done (Milestone 3)

- [x] Registry with shared/exclusive leases, refcounts, TTL + heartbeat, transfer, adoption
- [x] Orphan list: lapsed leases + unregistered listeners (lsof / ss / ps), system allowlist
- [x] Reap gated by grace period + explicit human `--confirm`; MCP never terminates anything
- [x] Holds and resource leases released on session end (`--keep` for resources)
- [x] MCP tools + CLI commands; activity feed summaries for resource events
- [x] Tests: core lease lifecycle; CLI; reap of a real process; MCP reap never signals; listener scan
- [x] CI green: test + clippy (`-D warnings`) + fmt + MSRV 1.88
- [x] README status updated

## Explicitly OUT of scope (later milestones)

- Authenticated remote peers, tokens, audit log, `GET /activity` (Milestone 4)
- Federation, remote resource visibility/borrow (Milestone 5)
- Auto-reap policies (never by default), GPU pressure sampling (`powermetrics`), wake adapters, release builds (Milestone 6 / later)

## Snippet for your project's AGENTS.md

Paste this into the AGENTS.md (or CLAUDE.md) of any repository where agents start servers:

```markdown
## Shared resources (Wyres)

- Before starting a model server, dev server, database, container or long job, call
  `resource_find` (or run `wyres resources -q <name-or-port>`). If it is already running,
  `resource_borrow` it instead of starting a duplicate.
- If you start one, `resource_register` it right away with its port, pid and purpose, and keep
  it alive with `heartbeat` / `resource_heartbeat`. Release it (`resource_release`) when done;
  pass `stopped: true` if you shut it down.
- Never kill processes you did not start. To get rid of an orphan, use `resource_reap`, which
  asks a human; only a human runs `wyres reap --confirm`.
```

## Keep the README status current

Whenever you complete a definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same PR.
