# Wyres decisions

Locked defaults for v1 (2026-10-28). See [SPEC.md](SPEC.md) for the full research and design.

| Decision | Choice | Notes |
|---|---|---|
| License | **Apache-2.0** | Patent grant; company-friendly; matches `rmcp` / A2A / Herdr. |
| Agent names | **Unique per project** | Addressed as `name@project` (an agent registered without a project is just `name`). A bare `name` resolves when exactly one agent on the node has it; otherwise it is rejected as ambiguous and callers must use `name@project`. Federation later uses `agent@project@node`. |
| Database location | **`~/.wyres`** (override with `$WYRES_HOME`) | One local node database; not per-repo. |
| Beads | **Deferred** | Keep requests inside Wyres for now; revisit linkage later. |
| Stack | **Rust + official `rmcp` + SQLite** | Single static binary; hooks stay tiny shell scripts. |
| Wake strategy | **Hooks-first** | Claude Code, Grok, Codex (Cursor best-effort). Herdr / cmux / tmux are optional adapters, not required for core. |
| Business model | **Free and open source, always** | Every feature (including federation) ships in the open repo. No paid tier, no paid hosted relay, no open-core upsell. Self-hostable hub; community-run relays optional. |
| Binding (milestone 1) | **Local-only** | Loopback / stdio by default. No public bind in M1. |
| MCP surface (v1) | **9 tools** | `ensure_project`, `register_agent`, `send_message`, `fetch_inbox`, `threads`, `acknowledge`, `claim_request`, `hold_paths`, `heartbeat`. `threads` (read a thread or list an agent's threads) was added to the ~8 suggested in AGENTS.md. |

Clean-room: implement from this repo's docs only. Do not copy code or schemas from MCP Agent Mail or any project with a non-permissive license rider.
