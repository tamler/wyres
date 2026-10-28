# Hook contract

Wyres is hooks-first (see [DECISIONS.md](DECISIONS.md)): agents learn about new mail because a lifecycle hook injects a short digest, not because they poll or rely on MCP resource subscriptions. Milestone 1 ships the CLI surface that hook packs call; the per-client hook packs and `wyres hook install` land later.

## Environment

| Variable | Meaning |
|---|---|
| `WYRES_AGENT` | The agent identity the hook acts as (`name` or `name@project`). |
| `WYRES_HOME` | Optional data directory (default `~/.wyres`). |

The agent must be registered once (`wyres agent register NAME --program PROGRAM [--project PATH]`, or the `register_agent` MCP tool).

## Events

| Client moment | Command | Notes |
|---|---|---|
| Session start | `wyres --json inbox --unread --limit 10` | Inject a short summary: counts, urgent subjects, open requests. |
| Each user prompt (optional) | `wyres --json heartbeat` | Renews holds; returns `unread`, `urgent_unread`, `open_requests`. Mention mail only when counts are non-zero. |
| Before editing shared files (optional) | `wyres hold check PATH…` | Exit status 3 means another agent holds an overlapping path. |
| Session end | `wyres heartbeat --end` | Releases every hold and marks the agent offline. |

## Output and exit codes

- With `--json`, stdout is a single JSON document. Errors are `{"error": {"code": "...", "message": "..."}}` with exit status 1.
- `hold acquire` / `hold check` exit 3 on conflict (nothing is acquired).
- Hooks should fail open: if `wyres` is missing or errors, let the session continue.

## Digest guidance

Keep injected text tiny (agents ignore long digests): one line of counts plus at most a few urgent subjects and request titles, each with its id so the agent can act on it through MCP or the CLI.
