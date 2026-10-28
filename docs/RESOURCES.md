# Resource registry

Before an agent starts a model server, dev server, database, container or long job, it asks Wyres whether one is already running and **borrows** it instead of spawning a duplicate. Resources carry an owner, a purpose and leases that expire unless renewed. Anything left without a lease shows up as an orphan, and only a human can reap it.

## Kinds

| Kind | Example name | Notes |
|---|---|---|
| `model_server` | `ollama@11434`, `llama-server@8080`, `mlx@8081` | Usually shared |
| `dev_server` | `vite@5173`, `next@3000` | Often exclusive per project |
| `port` | `tcp:127.0.0.1:5432` | Port derived from the name |
| `gpu` | `metal:0`, `cuda:0` | Put reserved memory in metadata; on Apple Silicon memory is unified, so use `estimated_bytes` + `backend` |
| `container` | `docker:3f2a…` | No pid needed; stop it yourself, then `release --stopped` |
| `long_job` | `eval-run-42` | Register the pid so it can be matched and reaped |

A resource is addressed by its id or by `kind:name`, e.g. `model_server:ollama@11434`.

## Lifecycle

```bash
wyres resources -q ollama
```

```bash
wyres borrow model_server:ollama@11434 --agent codex@shop
```

```bash
wyres resource register model_server ollama@11434 --agent claude@shop --endpoint http://127.0.0.1:11434 --pid 4242 --purpose "local llm" --meta backend=metal
```

- **Leases**: the registering agent holds the **owner** lease; others take **borrower** leases (shared resources only; `--exclusive` refuses borrowers). `refcount` is the number of live leases.
- **TTL**: default 30 minutes, clamped to 1 minute – 24 hours. `wyres heartbeat` (and the session hooks) renew every lease the agent holds; `wyres resource heartbeat` renews just resources.
- **Release**: `wyres release <target> --agent X`. If the owner leaves while others borrow, the oldest borrower becomes owner. After shutting a resource down, the owner runs `wyres release <target> --agent X --stopped`.
- **Session end**: `wyres heartbeat --end` (and the session-end hook) releases the agent's holds and resource leases. Add `--keep` to keep resource leases for a server meant to outlive the session.
- **Lapse**: when the last lease expires or is released, the resource becomes `lapsed`. It stays listed, and registering or borrowing it again **adopts** it.

## Orphans

```bash
wyres orphans
```

Two sources:

1. **Lapsed leases**: registered resources with no live lease.
2. **Unregistered listeners**: listening TCP sockets found by `lsof -nP -iTCP -sTCP:LISTEN` (macOS and Linux), or by `ss -ltnp` when `lsof` is missing, that no registered resource matches by pid or port. `ps` supplies each process's age and command line. System services (launchd, sshd, mDNSResponder, systemd, …) and Wyres itself are never listed.

Pass `--no-scan` to skip the process scan.

## Reaping (human only)

```bash
wyres reap model_server:ollama@11434
```

```bash
wyres reap model_server:ollama@11434 --confirm
```

- Without `--confirm`, `wyres reap` only prints the plan and its blockers; nothing is signalled.
- A target is eligible only if:
  - it has **no live lease** (pid targets must be unregistered listeners);
  - it has been orphaned for at least the **grace period** (`--grace`, default 600 seconds); for a pid target, the process must be at least that old;
  - its process is owned by the **current user**;
  - it is **not** a protected system process.
- `--confirm` sends SIGTERM and waits up to 5 seconds. SIGKILL is sent only with `--force`.
- Agents cannot reap. The MCP `resource_reap` tool returns the same plan, records `resource.reap_requested` in the activity feed, and hands back the exact command for a human to run.
- Wyres never reaps automatically.

## MCP tools

`resource_register`, `resource_find`, `resource_borrow`, `resource_release`, `resource_heartbeat`, `orphans_list`, `resource_reap` (plan and request only). See the snippet in [AGENTS.md](../AGENTS.md) for wording to give your agents.
