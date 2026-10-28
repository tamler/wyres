//! `wyres wake`: nudge an agent's terminal when it has mail, through optional adapters
//! (tmux, cmux, Herdr). Hooks remain the primary delivery path; this is for agents that sit
//! idle between prompts. Commands run as argv (no shell); nothing runs without a reason
//! unless `--force` is given.

use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use wyres_core::{InboxQuery, Store, WakeVia};

#[derive(Debug, Clone, Serialize)]
pub struct WakePlan {
    pub agent: String,
    pub via: Option<WakeVia>,
    pub target: Option<String>,
    pub message: Option<String>,
    /// The commands that run (or would run, with --dry-run).
    pub commands: Vec<Vec<String>>,
    pub sent: bool,
    /// Why nothing was sent.
    pub skipped: Option<String>,
}

/// Overrides for a single wake.
#[derive(Debug, Clone, Default)]
pub struct WakeOptions {
    pub via: Option<WakeVia>,
    pub target: Option<String>,
    pub message: Option<String>,
    /// Wake even with nothing unread.
    pub force: bool,
    pub dry_run: bool,
}

/// The adapter commands for a wake.
pub fn commands(via: WakeVia, target: &str, message: &str) -> Vec<Vec<String>> {
    let s = |x: &str| x.to_string();
    match via {
        WakeVia::Tmux => vec![
            vec![
                s("tmux"),
                s("send-keys"),
                s("-t"),
                s(target),
                s("-l"),
                s(message),
            ],
            vec![s("tmux"), s("send-keys"), s("-t"), s(target), s("Enter")],
        ],
        WakeVia::Cmux if target == "notify" => vec![vec![
            s("cmux"),
            s("notify"),
            s("--title"),
            s("Wyres"),
            s("--body"),
            s(message),
        ]],
        // `cmux send` unescapes `\n`, which submits the line.
        WakeVia::Cmux => vec![vec![
            s("cmux"),
            s("send"),
            s("--surface"),
            s(target),
            format!("{message}\\n"),
        ]],
        WakeVia::Herdr => {
            let herdr = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| s("herdr"));
            vec![vec![herdr, s("agent"), s("prompt"), s(target), s(message)]]
        }
    }
}

fn default_message(store: &Store, agent: &str) -> Result<(Option<String>, String)> {
    let inbox = store.inbox(
        agent,
        InboxQuery {
            unread_only: true,
            urgent_only: false,
            limit: 3,
        },
    )?;
    let open = inbox.requests.len();
    if inbox.unread == 0 && open == 0 {
        return Ok((None, inbox.agent));
    }
    let subjects: Vec<String> = inbox
        .messages
        .iter()
        .map(|m| format!("\"{}\"", m.message.subject))
        .collect();
    let mut msg = format!(
        "Wyres: {} unread message(s) ({} urgent) and {} open request(s) for {}",
        inbox.unread, inbox.urgent_unread, open, inbox.agent
    );
    if !subjects.is_empty() {
        msg.push_str(&format!(": {}", subjects.join(", ")));
    }
    msg.push_str(". Check them with fetch_inbox.");
    Ok((Some(msg), inbox.agent))
}

/// Plan (and unless `dry_run`, send) a wake for one agent.
pub fn wake(store: &Store, agent: &str, opts: &WakeOptions) -> Result<WakePlan> {
    let stored = store.wake_target(agent)?;
    let (auto_message, address) = default_message(store, agent)?;
    let mut plan = WakePlan {
        agent: address.clone(),
        via: opts.via.or(stored.as_ref().map(|t| t.via)),
        target: opts
            .target
            .clone()
            .or_else(|| stored.as_ref().map(|t| t.target.clone())),
        message: opts.message.clone().or(auto_message),
        commands: Vec::new(),
        sent: false,
        skipped: None,
    };
    if plan.message.is_none() {
        if !opts.force {
            plan.skipped = Some("nothing unread and no open requests".into());
            return Ok(plan);
        }
        plan.message = Some(format!("Wyres: check in with fetch_inbox ({address})."));
    }
    let Some(via) = plan.via else {
        plan.skipped = Some(format!(
            "no wake target for {address}; set one with `wyres wake set --agent {address} --via tmux --target <pane>`"
        ));
        return Ok(plan);
    };
    if plan.target.is_none() && via == WakeVia::Herdr {
        // Herdr addresses agents by name; default to the Wyres agent's name.
        plan.target = Some(address.split('@').next().unwrap_or(&address).to_string());
    }
    let Some(target) = plan.target.clone() else {
        plan.skipped = Some(format!("no {} target for {address}", via.as_str()));
        return Ok(plan);
    };
    plan.commands = commands(via, &target, plan.message.as_deref().unwrap_or_default());
    if opts.dry_run {
        plan.skipped = Some("dry run".into());
        return Ok(plan);
    }
    for argv in &plan.commands {
        let status = Command::new(&argv[0])
            .args(&argv[1..])
            .status()
            .with_context(|| format!("running {} (is it installed?)", argv[0]))?;
        if !status.success() {
            bail!("{} exited with {status}", argv.join(" "));
        }
    }
    plan.sent = true;
    Ok(plan)
}

/// Wake every agent with a stored target that has something waiting.
pub fn wake_all(store: &Store, opts: &WakeOptions) -> Result<Vec<WakePlan>> {
    store
        .wake_targets()?
        .into_iter()
        .map(|t| wake(store, &t.agent, opts))
        .collect()
}

/// Record the terminal a hook runs in, so `wyres wake` can find the agent later.
pub fn capture_target(store: &Store, agent: &str) {
    let found = std::env::var("TMUX_PANE")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| (WakeVia::Tmux, p))
        .or_else(|| {
            std::env::var("CMUX_SURFACE_ID")
                .ok()
                .filter(|p| !p.is_empty())
                .map(|p| (WakeVia::Cmux, p))
        });
    if let Some((via, target)) = found {
        let _ = store.set_wake_target(agent, via, &target, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_commands() {
        let t = commands(WakeVia::Tmux, "%3", "hi");
        assert_eq!(t[0], ["tmux", "send-keys", "-t", "%3", "-l", "hi"]);
        assert_eq!(t[1], ["tmux", "send-keys", "-t", "%3", "Enter"]);
        let c = commands(WakeVia::Cmux, "surface-1", "hi");
        assert_eq!(c[0], ["cmux", "send", "--surface", "surface-1", "hi\\n"]);
        let n = commands(WakeVia::Cmux, "notify", "hi");
        assert_eq!(n[0][1], "notify");
        let h = commands(WakeVia::Herdr, "reviewer", "hi");
        assert_eq!(&h[0][1..], ["agent", "prompt", "reviewer", "hi"]);
    }
}
