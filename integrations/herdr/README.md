# Wyres Herdr plugin (optional)

[Herdr](https://herdr.dev/) is an optional adapter for Wyres. Wyres works without it: hooks are the primary way agents learn about mail (see [docs/HOOKS.md](../../docs/HOOKS.md)).

The plugin adds:

| Item | What it does |
|---|---|
| Action **Wyres: wake agents with unread mail** | Runs `wyres wake --all`. For each agent with a stored wake target and unread mail or open requests, it runs that target's adapter; for Herdr targets that is `herdr agent prompt <agent> "<inbox summary>"` |
| Action **Wyres: activity summary** | Runs `wyres activity --summary --since 1h` |
| Pane **Wyres activity** | Shows a live, refreshing view of the last hour of activity |

## Install

```bash
herdr plugin install tamler/wyres/integrations/herdr
```

From a local checkout:

```bash
herdr plugin link ./integrations/herdr
```

## Pointing Wyres at Herdr agents

Herdr addresses agents by name. Tell Wyres which Herdr agent a Wyres agent runs in:

```bash
wyres wake set --agent codex@shop --via herdr --target reviewer
```

Without a stored target, `wyres wake --agent NAME --via herdr` uses the Wyres agent's bare name.

The plugin only calls the `wyres` CLI and the `herdr` binary. It links nothing, and neither tool depends on the other.
