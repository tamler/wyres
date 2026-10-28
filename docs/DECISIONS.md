# Wyres decisions

Locked defaults for v1 (2026-10-28). See [SPEC.md](SPEC.md) for the full research and design.

| Decision | Choice | Notes |
|---|---|---|
| License | **Apache-2.0** | Patent grant; company-friendly; matches `rmcp` / A2A / Herdr. |
| Agent names | **Global** | Unique agent names on a node; use `name@project` when the same agent identity needs project disambiguation. Federation later uses `agent@project@node`. |
| Database location | **`~/.wyres`** (override with `$WYRES_HOME`) | One local node database; not per-repo. |
| Beads | **Deferred** | Keep requests inside Wyres for now; revisit linkage later. |
| Stack | **Rust + official `rmcp` + SQLite** | Single static binary; hooks stay tiny shell scripts. |
| Wake strategy | **Hooks-first** | Claude Code, Grok, Codex (Cursor best-effort). Herdr / cmux / tmux are optional adapters, not required for core. |
| Business model | **Free and open source, always** | Every feature (including federation) ships in the open repo. No paid tier, no paid hosted relay, no open-core upsell. Self-hostable hub; community-run relays optional. |
| Binding (milestone 1) | **Local-only** | Loopback / stdio by default. No public bind in M1. |

Clean-room: implement from this repo's docs only. Do not copy code or schemas from MCP Agent Mail or any project with a non-permissive license rider.
