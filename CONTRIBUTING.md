# Contributing to Wyres

Thanks for helping. Wyres is free and open source, always: every feature ships in this repository under [Apache-2.0](LICENSE). There is no paid tier, no telemetry and no open-core split.

## Ground rules

1. **Clean room.** Implement from this repository's docs ([docs/SPEC.md](docs/SPEC.md), [docs/DECISIONS.md](docs/DECISIONS.md), [AGENTS.md](AGENTS.md)) and public documentation of the tools we integrate with.
   - Do **not** copy code, schemas, SQL, distinctive APIs or phrasing from `mcp_agent_mail`, its forks, or any project with a non-permissive license or license rider.
   - Prior art listed in SPEC is for ideas only.
2. **Vendor-neutral.** Claude Code, Codex, Grok CLI, Grok Bot and Cursor are treated equally. Don't tune behaviour for one vendor; when clients differ, handle the difference explicitly and document it.
3. **Safe defaults.**
   - Bind to loopback unless authentication is on.
   - Never terminate a process without a human confirmation flag.
   - Never allow remote reaping.
   - Agents act only on their own node.
4. **No secrets or personal data in the tree.** That means tokens, emails, phone numbers and machine-specific absolute paths. Tests use temporary directories and placeholder paths such as `/work/app`.
5. **Record design calls.** If a change makes a decision the docs don't cover, add a row to the decisions table in [docs/DECISIONS.md](docs/DECISIONS.md) in the same PR.

## Setup

You need Rust **1.88 or newer**; that is the MSRV and CI checks it.

```bash
git clone https://github.com/tamler/wyres
```

```bash
cd wyres && cargo test
```

The workspace has two crates:

| Crate | What lives there |
|---|---|
| `crates/wyres-core` | SQLite store, event log and signatures, agents, mail, requests, holds, activity, resources, tokens, federation logic. No async, no networking beyond reachability checks. |
| `crates/wyres` | The `wyres` binary and library: CLI, MCP server (`rmcp`), HTTP serving and auth, hook packs, orphan scanner and reaping, federation transport, wake adapters. |

Schema changes are new entries at the end of `MIGRATIONS` in `crates/wyres-core/src/store.rs`; never edit an existing migration. Every mutation appends an event first, then updates projections in the same transaction.

## Before you open a PR

Run what CI runs:

```bash
cargo fmt --all -- --check
```

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

```bash
cargo test
```

```bash
cargo +1.88 check --workspace --all-targets --locked
```

- Add tests for new behaviour. Core logic gets tests in `crates/wyres-core/tests`; CLI, MCP, hooks and HTTP get tests in `crates/wyres/tests`, which run the real binary, with a temporary `HOME` / `WYRES_HOME` where relevant.
- Keep the **Status** section of [README.md](README.md) and the current [AGENTS.md](AGENTS.md) brief accurate when milestone progress changes.
- Dependency updates must keep building on Rust 1.88. `.cargo/config.toml` makes `cargo update` prefer compatible versions.

## Releases

Pushing a tag `vX.Y.Z` runs `.github/workflows/release.yml`. It builds macOS (arm64, x86_64) and static Linux (x86_64, arm64) archives and publishes them with SHA-256 checksums as a GitHub Release.

## License

By contributing you agree that your contributions are licensed under the Apache License 2.0, the same terms as the project.
