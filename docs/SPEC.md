# Wyres: cross-agent inbox + resource registry — research + v1 spec

Clean-room research and v1 specification for **Wyres** — a free, local, open sibling under the [WyreUp](https://wyreup.com/) brand. Cross-agent inbox, activity feed, path holds, resource registry, and a designed-for-v2 federation model so a laptop and a remote agent box stay aware of each other. Hooks-first; Herdr/cmux/tmux optional.

Living design Page (optional): <https://cursor.com/canvas/store-f97c4d9c-6914-4d0e-b170-0d5f5d7e2a98/f9b88a9b-d8f5-4fb6-b818-fe86475343c7>

Locked product defaults live in [DECISIONS.md](DECISIONS.md). Milestone build brief: [../AGENTS.md](../AGENTS.md).

## Decisions locked

- Skip MCP Agent Mail (MIT + OpenAI/Anthropic license rider).
- Build Wyres clean-room under Apache-2.0 as a WyreUp sibling: free, local, open.
- Hooks first (Claude Code, Grok, Codex; Cursor best-effort). Herdr = optional plugin.
- Stack = Rust + official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk).
- Grok Bot is a first-class peer via activity feed + authenticated remote access.
- Federation (laptop ↔ remote box) is designed now and shipped as **v2**; v1 schema carries `node_id` + event log.
- **Free and open source, always** — every feature, including federation, ships in the open repo. No paid tier, no paid hosted relay, no open-core upsell.

## Why build

Adjacent coding agents still coordinate through stale markdown files. Off-the-shelf agent mail exists, but the leading option carries a license rider we reject, and every prior system still leaves the hard parts soft: getting agents to check mail, expiring file holds, request ownership, and discovering who owns which port / GPU / model server.

**Pros**

- Permissive license we own; Claude / Grok / Codex / Grok Bot treated equally
- Inbox + activity timeline + resource registry others under-serve
- Local-first SQLite; single static Rust binary; free and open always

**Cons**

- Build + maintain a daemon, hook packs, orphan scanner, and remote-auth path
- Clients differ on hooks; MCP resource subscribe is unreliable
- Clean-room discipline — ideas from prior art, never their code

## Brand + positioning

| | |
|---|---|
| **Name** | Wyres — cross-agent inbox + resource registry |
| **Brand** | Sibling agent-tools product under WyreUp (wyreup.com) |
| **WyreUp today** | Free browser image/PDF/file tools + tool chains; MIT; local-first |
| **Wyres fit** | Free and open source always (every feature in the open repo) |
| **CLI** | `wyres` — `serve`, `inbox`, `resources`, `activity`, … |

## Design principles

1. **Vendor-neutral** — Plain MCP (stdio + optional HTTP). Equal first-class: Claude Code, Grok CLI, Codex CLI, Grok Bot. Cursor optional. No Herdr required for core.
2. **Local-first + durable** — One machine daemon; SQLite source of truth; optional JSONL/Git export. No required cloud.
3. **Mail, not chat** — Discrete messages with subject, recipients, thread id, importance. No broadcast-by-default.
4. **Requests have owners** — `open → claimed → done | stale | cancelled`, with owner agent and due/TTL.
5. **Holds and resources expire** — Path holds and resource leases use TTL + heartbeat; orphan detection + human-approved reap.
6. **Shared awareness** — Activity feed so every peer (including remote Grok Bot) can see who is doing what.
7. **Check is a product feature** — Hook packs + AGENTS.md first; Herdr/cmux/tmux adapters optional.
8. **Free and open source, always** — Every feature — including federation — ships in the open repo. No paid tier, no paid hosted relay, no open-core upsell. Community-run or self-hosted hubs only.
9. **Permissive license** — Apache-2.0 (patent grant; company-friendly; matches Herdr/A2A/rmcp).

## Prior art (ideas only — clean room)

Study features and protocols; do **not** copy code, schemas, or distinctive phrasing.

| Project | License | Takeaways for us | Avoid |
|---|---|---|---|
| MCP Agent Mail (Rust) | MIT + rider | Identities, threads, acks, file leases, human web/TUI | Their code, schema, rider, Beads-specific coupling |
| agent-loom | MIT | Durable spool, path claims, obligations across projects | Copying implementation |
| agent-bus-mcp | MIT | Simple SQLite topic bus + peer cursors | Topic-only model if we need ownership |
| Beads / beads_rust | check LICENSE | Dependency-aware task board pairs with mail as channel | Replacing our request table in v1 |
| A2A protocol | Apache-2.0 | Task + message + artifact vocabulary; federation later | Overbuilding federation in v1 |
| agentmail.to | Hosted SaaS | Real email inboxes for agents — different problem | Confusing with local agent↔agent mail |

## Data model

Single SQLite database (WAL) per node. **v1 ships federation-ready columns** even though multi-node sync is v2: every durable row carries `node_id`, and mutable state is projected from an append-only event log with node-scoped IDs (**ULIDs** preferred; Lamport/hybrid clocks acceptable for ordering).

> **v1 schema must be federation-ready.** Add `node_id` (local default), `event_id` (ULID), and event-log-shaped records now so v2 does not force a breaking migration. Avoid CRDTs where a single owning node is authoritative.

| Entity | Fields (summary) |
|---|---|
| **Node** | `node_id` (ULID), `display_name`, `public_key?`, `paired_at`, role `home\|spoke\|relay\|peer` |
| **Project** | `id`, `node_id`, `abs_path` (unique per node), `slug`, `created_at` |
| **Agent** | `id`, `node_id`, `project_id?`, `name` → address `agent@project@node`, `program`, `last_seen_at`, `status` |
| **EventLog** (append-only) | `event_id` (ULID), `node_id`, `ts`, `actor`, `kind`, `payload` JSON, `prev_id?` — source of truth; tables are projections |
| **Request** | status `open \| claimed \| done \| stale \| cancelled`; owner; `due_at` |
| **Hold** (paths) | `node_id` + repo-scoped glob; never cross-node locks — `exclusive\|shared`, TTL, heartbeat |
| **Resource** | `node_id`, kind, endpoint/bind, owner, purpose, `exclusive\|shared`, refcount, ttl, heartbeat, reachability, metadata |
| **ActivityEvent** | projection of EventLog — mail\|request\|hold\|resource\|session\|system\|federation |

**Agent naming (locked):** global agent names on a node; use `name@project` for disambiguation. Federation later: `agent@project@node`.

## Resource registry (used / owned)

> Major component (after milestone 1 core mail). Before starting a model server, dev server, or GPU job, an agent asks Wyres whether one is already running and **borrows** it (shared-use leases + reference counts) instead of spawning a duplicate.

Register running model servers, dev servers, listening ports, GPU/VRAM allocations, containers, and long jobs with an owner agent/project, a purpose, a lease/TTL, and a heartbeat. Common on Apple Silicon Macs: Ollama, llama.cpp, and MLX servers (unified memory — VRAM is not a separate pool; track process RSS / Metal footprint in metadata).

### Kinds

| Kind | Example identity | Notes |
|---|---|---|
| `model_server` | `ollama@11434`, llama.cpp, mlx | Share by default when compatible |
| `dev_server` | `vite@5173`, `next@3000` | Often exclusive per project |
| `port` | `tcp:0.0.0.0:5432` | Normalized bind key |
| `gpu` | `metal:0` / `cuda:0` + mb reserved | Apple Silicon: unified memory fields |
| `container` | docker/podman id | Owner + purpose |
| `long_job` | train/eval pid + cmdline | Heartbeat required |

### Lifecycle

1. **Find before start** — `resource_find` → prefer borrow over spawn.
2. **Register or borrow** — `resource_register` / `resource_borrow` with TTL + heartbeat.
3. **Heartbeat** — renews owned/borrowed leases; SessionEnd releases unless `--keep`.
4. **Orphan detect** — lease lapsed or owner session ended; process scan (`lsof`/`ps`; `nvidia-smi` or macOS `powermetrics`/Activity/Metal) finds unregistered listeners.
5. **Notify then reap** — `orphans_list` → notify human; `resource_reap` kills only after grace + **human approval by default**.

### macOS specifics

- Prefer `lsof -iTCP -sTCP:LISTEN`, `ps`, and `sample`/`powermetrics` for GPU pressure.
- Unified memory: store `estimated_bytes` + backend (`metal|mlx|cpu`).
- Common scanner targets: `ollama`, `llama-server` / llama.cpp, `mlx_lm.server`, vite/next.

## Activity feed + remote peers (Grok Bot)

All systems must understand activity underway. The activity feed is a machine-readable timeline of who is working on what: open requests, holds, resources, mail, and session start/end. Readable over MCP, CLI, and HTTP.

| Surface | API |
|---|---|
| MCP | `activity_feed(since?, limit?, kinds?)` + `activity_summarize` |
| CLI | `wyres activity [--since] [--json]` |
| HTTP | `GET /activity` (same auth as remote MCP) |

### How Grok Bot connects

1. **Preferred (post-M1 remote path):** authenticated MCP over HTTPS — Streamable HTTP on loopback by default; for remote peers, terminate TLS at a tunnel (Cloudflare Tunnel, Tailscale Serve, or `ssh -R`) with a bearer token or mTLS. Register as `program=grok_bot`.
2. **Alternative:** periodic pull of `wyres activity --json` and inbox via short-lived token (outbound-only).
3. **Security:** Never bind `0.0.0.0` without auth. Token scoped to agent name + project globs. Audit log every remote tool call. No automatic `resource_reap` from remote by default. Prefer Tailscale/private network over public internet.

## Federation (designed now, ship as v2)

Each machine runs a Wyres **node** that is authoritative for its own agents, holds, and resources. Nodes exchange an append-only event log so a home laptop and a remote runner stay aware of each other’s activity. Design in v1 schema; implement sync after v1.

### Authority + event log

- Owning node decides: agents, path holds, and resource leases are authored only on their home `node_id`.
- Sync = append-only EventLog with node-scoped IDs (ULIDs preferred).
- Receivers project remote events into local read models — they do not rewrite foreign ownership.
- Avoid CRDTs where a single owner suffices; never merge competing exclusive leases — last authoritative event from the owner node wins; conflicts are surfaced.

### Addressing + mail

Address agents as `agent@project@node`. Cross-node mail is **store-and-forward**: enqueue on the sender, deliver when the path is up, catch up from the last acked `event_id` when a spoke reconnects (offline-tolerant, like Matrix PDU delivery responsibility).

### Topology

| | |
|---|---|
| **v2 start** | Hub-and-spoke — free self-hostable home/hub node; spokes dial out (NATS leaf-node pattern) |
| **Later** | Optional mesh once hub-and-spoke is solid (Tailscale-style peer paths) |
| **Grok Bot** | Joins as a node or lightweight peer; same event log + scoped permissions |

### Transport + auth

- Prefer Tailscale / WireGuard private network, or mTLS over a relay.
- Pairing exchanges per-node keys; permissions scoped: `read_feed | send_mail | borrow_resource` (and never `remote_reap` by default).
- Kill/reap of processes stays on the **owning node** only — remote peers may request, not execute.

### Remote resources + holds

- Resources are visible across paired nodes; borrowable only if network-reachable (annotate latency/egress in metadata).
- File holds stay **repo/node-scoped** — never distributed locks across machines.
- Cross-node git-level conflicts are **surfaced** in activity/mail, not locked.

### Federation prior art (ideas only)

| System | What we borrow | Link |
|---|---|---|
| Matrix federation | Signed server-server events, origin delivers, offline catch-up | [spec.matrix.org](https://spec.matrix.org/latest/server-server-api/) |
| ActivityPub | Inbox/outbox + server-to-server delivery vocabulary | [W3C TR/activitypub](https://www.w3.org/TR/activitypub/) |
| NATS leaf nodes | Spoke dials hub; firewall-friendly hub-and-spoke | [docs.nats.io leaf-nodes](https://docs.nats.io/learn/topologies/leaf-nodes) |
| Tailscale / WireGuard | Private mesh underlay; pairing without public bind | [tailscale.com](https://tailscale.com/learn/understanding-mesh-vpns) |

### Self-hostable hub / community relay

The hub in hub-and-spoke is just another free Wyres node (always-on box or the laptop). Anyone can self-host a hub; community-run relays are optional and likewise free/open. No paid hosted relay and no open-core upsell — federation ships in the same open repo as local mail and the resource registry.

## MCP tools + CLI (full vision)

### Mail / requests / holds

| Tool | Purpose |
|---|---|
| `ensure_project` | Register abs path → project |
| `register_agent` | Name + program/model; touch last_seen |
| `send_message` | Thread + recipients + body; optional create_request |
| `fetch_inbox` | Unread / urgent / ack-overdue / open requests |
| `mark_read` / `acknowledge` | Delivery state |
| `claim_request` / `complete_request` / `release_request` | Ownership |
| `list_requests` | Filter by status/owner/stale |
| `hold_paths` / `renew_hold` / `release_hold` | Path TTL leases |
| `check_hold_conflicts` | Before edit |
| `heartbeat` | Session alive; renews holds + owned resources |
| `search_messages` | FTS over subject/body |
| `activity_feed` / `activity_summarize` | Timeline for all peers |

### Resource registry tools (after M1)

| Tool | Purpose |
|---|---|
| `resource_register` | Announce a live resource |
| `resource_find` | Discover before spawning |
| `resource_borrow` | Shared-use lease |
| `resource_release` | Drop ownership or borrow |
| `resource_heartbeat` | Renew leases |
| `orphans_list` | Lapsed leases + unregistered listeners |
| `resource_reap` | Kill/cleanup after grace; human approval by default |

### CLI (`wyres`)

- `wyres serve` — daemon (stdio MCP and/or authenticated HTTP)
- `wyres inbox | send | ack | requests | holds`
- `wyres resources | borrow | release | orphans | reap`
- `wyres activity`
- `wyres doctor`
- `wyres wake --agent NAME --via herdr|tmux|cmux` (optional adapters)
- `wyres hook install --client claude|grok|codex|cursor`
- `wyres pair` / `wyres sync` — v2 federation (stub/docs in v1)

## Mail-checking strategy per client

> **Hooks first — not tied to Herdr.** v1 delivery guarantee is lifecycle hooks that inject a tiny inbox + activity digest. Do not rely on MCP resources/subscribe (Codex does not act on those notifications — [openai/codex#16159](https://github.com/openai/codex/issues/16159)).

| Client | Mechanism | Verified? | Recipe |
|---|---|---|---|
| Claude Code | SessionStart + UserPromptSubmit (+ Stop/SessionEnd) | Verified | Shell hook → digest; SessionEnd → `heartbeat --end` |
| Codex CLI | hooks.json + AGENTS.md | Verified | Same digest; AGENTS.md: check wyres before new work |
| Grok CLI | `~/.grok/hooks` + MCP config | Verified | Mirror Claude hook pack |
| Cursor | sessionStart / beforeSubmitPrompt | Verified (IDE) | Best-effort; cloud sessionStart unreliable |
| Grok Bot | Remote MCP / periodic pull | Design | Authenticated tunnel; no local hooks |
| Herdr (optional) | Plugin: agent prompt when idle | Verified API | Adoption add-on |
| tmux / cmux (optional) | send-keys / notify rings | Verified pattern | Optional adapters only |
| MCP resources/subscribe | Server push | Unreliable | Later; not a v1 delivery guarantee |

### Hook flow

1. **Session start** — `wyres inbox --json --urgent` + short `wyres activity --since 1h`.
2. **Each user prompt** — optional light check (~500 tokens).
3. **Before spawn** — AGENTS.md: `resource_find` before starting servers (when registry ships).
4. **Session end** — `wyres heartbeat --end` → release holds/exclusive resources unless `--keep`.

## Stale-hold design

Path holds are advisory leases on globs. They signal intent; they do not replace git.

1. Acquire: `hold_paths(paths, ttl_seconds, exclusive?, reason?)` — fail soft with conflict list.
2. Heartbeat renews `expires_at` (cap max TTL).
3. Auto-release: SessionEnd; daemon sweeper; optional git-commit PostToolUse (opt-in).
4. Conflict escalation: after N conflicts or overdue request, mark stale and notify human — never silent-steal exclusive holds without force.
5. Federation: holds remain node/repo-scoped; cross-node path clashes are activity warnings, not distributed locks.

## Integrations

| Tool | License | What it adds | Effort | Notes |
|---|---|---|---|---|
| Hooks (Claude/Grok/Codex/Cursor) | n/a | Primary check path | Low | v1 must |
| Herdr plugin | Apache-2.0 | Adoption add-on: wake/spawn/toasts | Med | Optional |
| cmux (manaflow) | GPL (site) | Optional pane UI + notify rings | Med | CLI/socket only; avoid embedding |
| tmux | ISC | Optional send-keys wake | Low | Fallback adapter |
| zellij | MIT | Optional pane scripting | Med | Less agent-aware than Herdr |
| git worktrees | Git | Isolate concurrent edits | Low | Complement path holds |
| OS notify (`osascript`) | OS | Human ping on orphans/reaps | Low | Default before kill |

## Scope

### v1 (product vision; milestone 1 is a subset — see AGENTS.md)

- Rust daemon + rmcp (stdio + authenticated HTTP) + SQLite
- Mail, requests, path holds+TTL; resource registry; activity feed
- Schema: `node_id` on all durable rows + append-only EventLog (ULIDs)
- Hook packs (shell): Claude, Grok, Codex (+ Cursor best-effort)
- Remote peer path for Grok Bot (token + tunnel; no public bind by default)
- macOS orphan scanner; human approval default on `resource_reap`
- Herdr / cmux / tmux adapters optional

### v2 federation

- Node pairing + hub-and-spoke event sync (store-and-forward, offline catch-up)
- Addressing `agent@project@node`; cross-node mail + merged activity feed
- Remote resource visibility + borrow when reachable; owning-node reap only
- Scoped permissions; no remote kill by default
- Optional mesh after hub-and-spoke is solid
- Self-hostable hub node + optional community-run free relays

### Later

- Herdr plugin packaged; cmux notify + inbox pane
- Web UI / TUI; pre-commit hold guard; auto-reap policies
- Cross-repo contact directory; A2A bridge; Beads deep link
- MCP resources/subscribe where clients catch up

## Stack

**Recommend Rust + official `rmcp` + SQLite.**

[`rmcp`](https://crates.io/crates/rmcp) is the official Tier-1 Rust MCP SDK ([modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk), Apache-2.0). Use `rusqlite` or `sqlx` for SQLite WAL. Hooks stay tiny shell scripts calling `wyres … --json`.

TS/Bun prototype tradeoff: faster to sketch tool shapes, weaker as a long-lived process/port/GPU monitor. Prefer Rust for ship; do not dual-maintain.

| | |
|---|---|
| License | Apache-2.0 |
| Language | Rust (edition 2021+); MSRV per rmcp |
| MCP | `rmcp` + `rmcp-macros` (`cargo add rmcp --features server`) |
| Store | SQLite WAL under `~/.wyres/` or `$WYRES_HOME` |
| Hooks | POSIX/macOS shell only |
| Remote | Streamable HTTP MCP behind Tailscale/CF Tunnel + bearer/mTLS (after M1) |

## Risks

- Clean-room leakage: do not clone `mcp_agent_mail*`; write from this spec only.
- Hook trust UX: Codex trusts hooks explicitly; Grok needs `/hooks-trust` — document install.
- Remote exposure: default loopback-only; require explicit enable for remote.
- False orphan kills: never auto-kill without approval in early versions; allowlist system daemons.
- Apple Silicon accounting: unified memory makes GPU leases approximate.
- cmux GPL: optional external CLI only, never link into wyres.
- Agents ignore digests — keep them tiny.
- Hold/resource false confidence: leases are social until commit/process guards exist.
- Federation split-brain: owning `node_id` is law; never CRDT-merge ownership.
- Clock skew / offline queues: document catch-up windows.
- Community/self-hosted hub trust: prefer end-to-end encryption so a relay operator cannot read mail bodies.
- Cross-node git confusion: AGENTS.md must say holds are node-local.

## Sources

1. [WyreUp](https://wyreup.com/)
2. [Official MCP SDKs (Rust Tier 1)](https://modelcontextprotocol.io/docs/2026-07-28/sdk)
3. [rmcp — official Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
4. [rmcp on crates.io](https://crates.io/crates/rmcp)
5. [Herdr](https://herdr.dev/) / [agent automation](https://herdr.dev/docs/agent-automation/)
6. [cmux](https://cmux.com/)
7. [Claude Code hooks](https://code.claude.com/docs/en/hooks)
8. [Codex hooks](https://learn.chatgpt.com/docs/hooks)
9. [Codex MCP subscribe gap](https://github.com/openai/codex/issues/16159)
10. [Grok Build hooks + MCP](https://docs.x.ai/build/features/hooks)
11. [Cursor hooks](https://cursor.com/docs/hooks)
12. [A2A protocol](https://a2a-protocol.org/latest/)
13. [Matrix Server-Server API](https://spec.matrix.org/latest/server-server-api/)
14. [ActivityPub (W3C)](https://www.w3.org/TR/activitypub/)
15. [NATS leaf nodes](https://docs.nats.io/learn/topologies/leaf-nodes)
16. [Tailscale mesh VPN overview](https://tailscale.com/learn/understanding-mesh-vpns)
