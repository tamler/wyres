# Wyres decisions

Locked defaults for v1 (2026-10-28). See [SPEC.md](SPEC.md) for the full research and design.

| Decision | Choice | Notes |
|---|---|---|
| License | **Apache-2.0** | Patent grant; company-friendly; matches `rmcp` / A2A / Herdr. |
| Agent names | **Unique per project** | Addressed as `name@project` (an agent registered without a project is just `name`). A bare `name` resolves when exactly one agent on the node has it; otherwise it is rejected as ambiguous and callers must use `name@project`. Across nodes (M5): `name@project@node`, or `name@@node` for agents without a project. |
| Database location | **`~/.wyres`** (override with `$WYRES_HOME`) | One local node database; not per-repo. |
| Beads | **Deferred** | Keep requests inside Wyres for now; revisit linkage later. |
| Stack | **Rust + official `rmcp` + SQLite** | Single static binary; hooks stay tiny shell scripts. |
| Wake strategy | **Hooks-first** | Claude Code, Grok, Codex (Cursor best-effort). Herdr / cmux / tmux are optional adapters, not required for core. |
| Business model | **Free and open source, always** | Every feature (including federation) ships in the open repo. No paid tier, no paid hosted relay, no open-core upsell. Self-hostable hub; community-run relays optional. |
| Binding | **Loopback by default** | Loopback / stdio by default. From M4, a non-loopback bind needs both `--auth` and `--allow-remote-bind`; tunnels to a loopback `--auth` server are preferred. |
| MCP surface | **Small, additive** | M1: `ensure_project`, `register_agent`, `send_message`, `fetch_inbox`, `threads`, `acknowledge`, `claim_request`, `hold_paths`, `heartbeat` (9; `threads` added to the ~8 suggested). M2 adds `activity_feed`, `activity_summarize` (11). M3 adds `resource_register`, `resource_find`, `resource_borrow`, `resource_release`, `resource_heartbeat`, `orphans_list`, `resource_reap` (18). |

Clean-room: implement from this repo's docs only. Do not copy code or schemas from MCP Agent Mail or any project with a non-permissive license rider.

## Milestone decisions

Design calls not covered by SPEC, recorded as they were made.

| # | Milestone | Decision |
|---|---|---|
| D1 | M2 | **Activity feed never carries message bodies** — items are one-line summaries plus ids, so any peer (including a remote Grok Bot later) can read the feed without seeing mail content. |
| D2 | M2 | **Plain heartbeats are presence, not events.** `session.started` / `session.ended`, hold renewals and every other mutation are logged; a bare heartbeat only updates `last_seen_at`, so per-prompt hooks do not flood the log. |
| D3 | M2 | **Default hook identity is `<client>@<project>`** (project = nearest git root of the client's cwd), auto-registered on first use; `$WYRES_AGENT` overrides. Same rule for every client. |
| D4 | M2 | **Grok de-duplication.** Grok also runs Claude/Cursor hook files. A Wyres hook invoked under Grok (`GROK_HOOK_EVENT` set) acts as `grok` and stands down when the Grok pack is installed, so each event runs once. |
| D5 | M2 | **Cursor is best effort.** Its prompt hook cannot inject context, so only session start carries the digest there; the prompt hook renews holds and returns `{"continue": true}`. |
| D6 | M2 | **Prompt notices are once per message.** The last announced message id per agent is kept in `$WYRES_HOME/hooks/state/`, so repeated prompts do not repeat the same notice. |
| D7 | M2 | **Hook installer edits user-level configs only**, refuses invalid JSON or foreign files, backs up before writing, and is all-or-nothing across the clients requested. Project-level hook files are left to the user. |
| D8 | M3 | **Reaping is human-only.** MCP `resource_reap` returns a plan and logs `resource.reap_requested`; it never signals a process. Only `wyres reap --confirm` (CLI) terminates, after a grace period (default 10 min), for the current user's non-system processes; SIGTERM first, SIGKILL only with `--force`. There is no auto-reap. |
| D9 | M3 | **Lapsed resources stay listed.** A resource with no live lease is an orphan candidate, not deleted; registering or borrowing it again adopts it. `release --stopped` (owner) marks it gone. |
| D10 | M3 | **Ownership passes to the oldest borrower** when the owner releases a shared resource others still use. |
| D11 | M3 | **Session end releases holds and resource leases.** `heartbeat --end --keep` keeps resource leases (servers meant to outlive a session); holds are always released. |
| D12 | M3 | **Orphan scan** uses `lsof` (macOS first, also Linux) with `ss` as a fallback and `ps` for age/command; system daemons and Wyres are allowlisted out. Reaping by pid is limited to unregistered listeners the scan finds; registered resources are reaped by id. |
| D13 | M3 | **Resource leases** default to 30 minutes, clamped to 1 minute – 24 hours (holds stay 1 minute – 4 hours). |
| D14 | M4 | **Tokens act as one agent within project globs.** Secrets are `wyr_<id>_<random>`; only a SHA-256 hash is stored and the secret is printed once. No globs means no project access (mailbox, heartbeat and feed still work). |
| D15 | M4 | **`--auth` verifies every request, including loopback ones**, because tunnels (Tailscale Serve, cloudflared, `ssh -R`) deliver from loopback. Run local agents on a separate unauthenticated loopback server or stdio. |
| D16 | M4 | **Feed, summary and `resource_find` are node-wide read-only awareness for any token** (the feed carries no bodies); mailbox, holds, requests and resource leases are scoped. `orphans_list` never runs a process scan for remote callers. |
| D17 | M4 | **`resource_reap` is refused for every remote caller**, regardless of token scope. |
| D18 | M4 | **Audit log is separate from the event log**: node-local security records (not synced), arguments redacted (bodies and notes replaced by their size). Denied and unauthenticated requests are logged too. |
| D19 | M4 | **No built-in TLS.** Remote access goes through a tunnel or reverse proxy that terminates TLS; `--allowed-host` names tunnel hostnames for DNS-rebinding protection. |
| D20 | M5 | **Ed25519 node keys; every event signed by its origin.** Older databases get a key on upgrade and their events are signed then. Hubs pin a spoke's key at pairing; spokes trust the hub's node directory (hub-and-spoke trust). |
| D21 | M5 | **Agent addresses are stored** (`agents.address`) so foreign agents read as `name@project@node` everywhere (from/to, owners, leases) and replies route unambiguously. Acting identities must be local; recipients and filters may be remote. |
| D22 | M5 | **Routing classes**: directory events to every peer; mail only to recipient nodes (local-only mail never leaves its node); receipts to the message's node; activity to `read_feed` peers plus the owner of a borrowed resource; tokens, audit and pairing records never. |
| D23 | M5 | **Hub enforces permissions on push** (a spoke may only push its own events; `send_mail` for cross-node mail; `borrow_resource` for borrowing other nodes' resources). Rejected events are reported and skipped so the spoke's cursor still advances. |
| D24 | M5 | **Requests are node-local.** Opening a request with a recipient on another node is refused; plain mail works. |
| D25 | M5 | **Remote resources**: reachability is a TCP connect from this machine to the endpoint; foreign leases count until their TTL even if the borrower goes offline; stop/reap only on the owning node; a lapsed remote resource can only be adopted on its node. |
| D26 | M5 | **Cross-node hold clashes are warnings**, matched by project slug (repository name) since paths differ between machines. |
| D27 | M5 | **Node names must be unique** in a federation and set before pairing (`wyres node name`); the default `local` cannot pair. |
| D28 | M5 | **Hub reads relayed mail bodies** (self-hosted hub; end-to-end encryption deferred). Node-to-node requests are signed and time-bounded (±5 min); replays are harmless because push is idempotent and pull is read-only. |
