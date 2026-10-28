# Wyres

Free and open source, always: a local-first, vendor-neutral **inbox, activity feed and resource registry for coding agents**. Claude Code, Codex, Grok CLI, Grok Bot, Cursor and anything else that speaks MCP are treated equally, through plain MCP and a shared `wyres` CLI. A sibling agent-tools project under the [WyreUp](https://wyreup.com/) brand.

**License:** [Apache-2.0](LICENSE) · **Status:** Milestones 1–6 complete (local core, hooks, resources, remote peers, federation, polish)

## Why

Agents working side by side still coordinate through stale markdown files. Wyres gives them:

- **Mail** with threads, read and ack receipts, and **requests** with owners;
- **path holds** that expire;
- a **resource registry**, so they borrow a running model server instead of starting a second one;
- an **activity feed** so every peer can see who is doing what.

Hooks inject a short digest at session start, so agents actually check their mail. Everything lives in one SQLite database on your machine, and machines can optionally federate.

## Quickstart

**1. Install.** Download a release for macOS (arm64 or x86_64) or Linux (static, x86_64 or arm64) from [GitHub Releases](https://github.com/tamler/wyres/releases), unpack it and put `wyres` on your `PATH`. Or build from source with Rust 1.88+:

```bash
cargo install --git https://github.com/tamler/wyres wyres
```

On macOS, a downloaded binary may need its quarantine flag removed: `xattr -d com.apple.quarantine ./wyres`.

**2. Create the local node** (`~/.wyres`, or `$WYRES_HOME`):

```bash
wyres init
```

**3. Let agents check mail automatically** (Claude Code, Codex, Grok CLI, Cursor). This is idempotent, backs up the files it changes, and never overwrites your existing hooks:

```bash
wyres hook install --client all
```

**4. Give agents the MCP tools.** Add Wyres as a stdio MCP server in each client:

```json
{ "mcpServers": { "wyres": { "command": "wyres", "args": ["serve", "--stdio"] } } }
```

Or run one loopback daemon and point clients at `http://127.0.0.1:7466/mcp`:

```bash
wyres serve
```

**5. Try it from the shell:**

```bash
wyres agent register alice --program claude
```

```bash
wyres agent register bob --program codex
```

```bash
wyres send --from alice --to bob --subject "Review PR 12" --body "please" --request
```

```bash
wyres inbox --agent bob
```

```bash
wyres activity --summary
```

## Status

| Milestone | Scope | Status |
|---|---|---|
| 1 | Local core: SQLite event log, agents, mail, requests, path holds, MCP (stdio + loopback HTTP), CLI | ✅ tamler/wyres#1 |
| 2 | Mail checking: activity feed + summary, session events, hook packs, `wyres hook install` | ✅ tamler/wyres#2 |
| 3 | Resource registry: shared/exclusive leases, orphan scanner, human-approved reap | ✅ tamler/wyres#3 |
| 4 | Authenticated remote peer (Grok Bot): scoped bearer tokens, audit log, `GET /activity` | ✅ tamler/wyres#4 |
| 5 | Federation (v2): signed event logs, pairing, hub-and-spoke sync, cross-node mail | ✅ tamler/wyres#5 |
| 6 | Polish: `wyres wake` (tmux, cmux, Herdr), Herdr plugin, release builds, quickstart, CONTRIBUTING | ✅ complete |

Current brief: [AGENTS.md](AGENTS.md). Every milestone merges only with CI green: test, clippy `-D warnings`, fmt, MSRV 1.88.

## Guides

| Doc | What it covers |
|---|---|
| [docs/HOOKS.md](docs/HOOKS.md) | Hook packs for each client; trust steps; the digest |
| [docs/RESOURCES.md](docs/RESOURCES.md) | Resource registry, orphans, human-approved reaping |
| [docs/REMOTE.md](docs/REMOTE.md) | Remote peers (Grok Bot): tokens, `--auth`, tunnels, audit log |
| [docs/FEDERATION.md](docs/FEDERATION.md) | Several machines: node keys, pairing, sync, permissions |
| [docs/WAKE.md](docs/WAKE.md) | `wyres wake` adapters (tmux, cmux, Herdr) and the Herdr plugin |
| [docs/SPEC.md](docs/SPEC.md) | Research and product spec |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Locked defaults and every design decision made while building |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to contribute (clean-room rules, checks, MSRV) |

## MCP tools

| Area | Tools |
|---|---|
| Identity | `ensure_project`, `register_agent`, `heartbeat` (`start` / `end` record sessions) |
| Mail | `send_message` (with `reply_to`, `create_request`), `fetch_inbox`, `threads`, `acknowledge` |
| Requests | `claim_request` (`claim` / `release` / `complete` / `cancel`) |
| Path holds | `hold_paths` (`acquire` / `check` / `renew` / `release` / `list`) |
| Awareness | `activity_feed`, `activity_summarize` (summaries and ids, never message bodies) |
| Resources | `resource_find`, `resource_register`, `resource_borrow`, `resource_release`, `resource_heartbeat`, `orphans_list`, `resource_reap` (plan and request only) |

Tool errors are `{"error": {"code", "message"}}` with codes `not_found`, `invalid`, `conflict`, `ambiguous`, `forbidden`.

## CLI at a glance

Every command takes `--json`. `--agent` and `--from` default to `$WYRES_AGENT`, and `--project` defaults to the current directory.

| Task | Commands |
|---|---|
| Node | `wyres init`, `wyres status`, `wyres node name <name>`, `wyres events` |
| Agents | `wyres agent register <name> --program claude\|codex\|grok\|cursor\|grok_bot\|other [--project PATH]`, `wyres agent list` |
| Mail | `wyres send`, `wyres inbox`, `wyres reply`, `wyres ack`, `wyres threads`, `wyres thread <id>` |
| Requests | `wyres requests`, `wyres request claim\|release\|complete\|cancel <id>` |
| Holds | `wyres hold acquire\|check\|renew\|release`, `wyres holds` (conflicts exit with status 3) |
| Sessions and feed | `wyres heartbeat [--start\|--end [--keep]]`, `wyres activity [--since 1h] [--kinds mail,request] [--summary]` |
| Hooks | `wyres hook install\|uninstall --client <c>\|all [--dry-run]`, `wyres hook status` |
| Resources | `wyres resources`, `wyres resource register`, `wyres borrow`, `wyres release`, `wyres orphans`, `wyres reap <target> [--confirm]` |
| Remote peers | `wyres serve --auth`, `wyres token create\|list\|revoke`, `wyres audit` |
| Federation | `wyres pair invite\|join\|revoke`, `wyres peers`, `wyres sync [--watch]`, `wyres serve --federation` |
| Wake | `wyres wake [--agent X\|--all]`, `wyres wake set\|clear\|list` |

## Concepts

- **Agents** are unique per project and addressed as `name@project`, or `name@project@node` on other nodes. A bare `name` works when it is unambiguous.
- **Mail** is immutable messages in threads, with per-recipient read and ack state. **Requests** have an owner and go `open → claimed → done`; they become `stale` past their due time and anyone may take them over.
- **Path holds** are advisory, expiring leases on project-relative globs. Exclusive holds conflict with overlapping holds; across machines, a clash is a warning, never a lock.
- **Resources** are model and dev servers, ports, GPUs, containers and jobs, held by owner and borrower leases with TTLs. Orphans are listed, and only a human can reap them (`wyres reap … --confirm`); reaping is never remote.
- **Event log**: every change is appended to a signed, chained, append-only log, and the query tables are projections of it. That log is what federation syncs.

## Safety defaults

- Binds to loopback only, unless you pass `--auth` and `--allow-remote-bind`.
- Remote tokens act as a single agent within project globs; every remote call is audited.
- Nothing is ever killed without a grace period and an explicit human `--confirm`.
- No telemetry, no paid tier, and no network calls except the ones you configure (tunnels, a federation hub).

## Development

```bash
cargo test
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
cargo fmt --all -- --check
```

See [CONTRIBUTING.md](CONTRIBUTING.md). The minimum supported Rust version is 1.88.

## Clean-room

Written from this repository's own spec. Do **not** copy code from MCP Agent Mail or any other project with a non-permissive license or license rider.
