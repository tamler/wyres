# AGENTS.md — build brief (Milestone 6: polish)

**Read this before writing code.** Product vision lives in [docs/SPEC.md](docs/SPEC.md); locked defaults and every milestone decision in [docs/DECISIONS.md](docs/DECISIONS.md); contributor rules in [CONTRIBUTING.md](CONTRIBUTING.md). This file is the scope for the current coding milestone.

## Status

Milestone 6: **complete** (definition of done met). Earlier: M1 local core (tamler/wyres#1), M2 mail checking + activity feed (tamler/wyres#2), M3 resource registry (tamler/wyres#3), M4 authenticated remote peer (tamler/wyres#4), M5 federation (tamler/wyres#5). Keep the [README.md](README.md) status current in every PR.

## Clean-room rules (mandatory)

- Implement from **this repository's docs** and public docs of integrated tools (tmux, cmux, Herdr, GitHub Actions) only.
- **Do not** copy code, schemas, SQL, phrasing, or distinctive APIs from `mcp_agent_mail*`, forks, or any project with a non-permissive license rider.
- cmux is GPL: call its CLI as a separate process only; never link or vendor it.

## Locked defaults

Apache-2.0; agent names unique per project; hooks-first (wake adapters are optional extras); free and open source always; loopback-only without auth; never reap without human approval; never remote reap; MSRV 1.88.

## Milestone 6 — in scope

1. **`wyres wake`** — nudge idle agents with unread mail through optional adapters: `tmux` (send-keys), `cmux` (`send --surface` or `notify`), `herdr` (`agent prompt`). Stored per-agent targets (`wyres wake set|clear|list`), auto-captured by session hooks from `$TMUX_PANE` / `$CMUX_SURFACE_ID` (never replacing manual targets); `--all`, `--dry-run`, `--force`, `--message`. Argv only, no shell; skip when nothing is waiting.
2. **Herdr plugin (optional)** — [integrations/herdr](integrations/herdr): `herdr-plugin.toml` with wake + summary actions and a live activity pane; POSIX `sh` helper.
3. **Release builds** — `.github/workflows/release.yml`: macOS arm64 + x86_64, Linux static musl x86_64 + arm64; smoke tests; SHA-256 sums; GitHub Release on `v*` tags; build-only on PRs touching the workflow.
4. **Docs** — clean README quickstart, [CONTRIBUTING.md](CONTRIBUTING.md), [docs/WAKE.md](docs/WAKE.md).

### Definition of done (Milestone 6)

- [x] `wyres wake` with tmux / cmux / Herdr adapters, stored + auto-captured targets, tests with fake adapter binaries
- [x] Optional Herdr plugin (manifest + helper), validated in tests
- [x] Release workflow for macOS (arm64, x86_64) and Linux, verified by a PR build
- [x] README quickstart + CONTRIBUTING.md + docs/WAKE.md
- [x] CI green: test + clippy (`-D warnings`) + fmt + MSRV 1.88
- [x] README status updated

## Explicitly OUT of scope

- Web UI / TUI, auto-reap policies, pre-commit hold guard, mesh federation, end-to-end mail encryption (future milestones; see SPEC "Later")

## Keep the README status current

Whenever you complete a definition-of-done item or open a PR that changes milestone progress, update the **Status** section in [README.md](README.md) in the same PR.
