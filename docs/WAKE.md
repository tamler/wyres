# Waking idle agents (optional adapters)

Hooks are the primary way agents learn about mail: a digest at session start and a notice on the next prompt (see [HOOKS.md](HOOKS.md)). An agent sitting idle at its prompt only sees new mail when something types into its terminal. `wyres wake` does that through optional adapters. Nothing in Wyres requires them.

| Adapter | What it runs |
|---|---|
| `tmux` | `tmux send-keys -t <pane> -l "<message>"`, then `tmux send-keys -t <pane> Enter` |
| `cmux` | `cmux send --surface <surface> "<message>\n"`; with target `notify`, `cmux notify --title Wyres --body "<message>"` |
| `herdr` | `herdr agent prompt <agent> "<message>"` (uses `$HERDR_BIN_PATH` when set) |

Wyres only calls these external CLIs as separate processes, with no shell and no linking.

## Targets

Wyres stores one wake target per agent:

```bash
wyres wake set --agent codex@shop --via tmux --target %3
```

```bash
wyres wake list
```

```bash
wyres wake clear --agent codex@shop
```

When a Wyres session hook runs inside tmux (`$TMUX_PANE`) or cmux (`$CMUX_SURFACE_ID`), it records that pane automatically. Automatic captures never replace a target you set by hand. Herdr agents default to the Wyres agent's bare name when no target is stored.

## Waking

```bash
wyres wake --agent codex@shop
```

```bash
wyres wake --all
```

```bash
wyres wake --agent codex@shop --dry-run
```

- The default message is a one-line inbox summary: unread count, urgent count, open requests and up to three subjects. Message bodies are never included.
- An agent with nothing unread and no open requests is skipped unless you pass `--force`. `--message` replaces the text.
- `--all` wakes every agent that has a stored target and something waiting, which makes it suitable for a cron job or a Herdr action.
- `--dry-run` prints the commands without running them.

## Herdr plugin

[integrations/herdr](../integrations/herdr) is an optional Herdr plugin. It adds a "wake agents with unread mail" action, an activity summary action, and a live activity pane:

```bash
herdr plugin install tamler/wyres/integrations/herdr
```
