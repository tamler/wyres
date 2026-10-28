//! Human-readable rendering for CLI output (`--json` bypasses all of this).

use std::fmt::Write;

use serde::Serialize;
use wyres_core::{
    ActivityFeed, Agent, AuditEntry, Event, HeartbeatReport, Hold, HoldConflict, HoldReport, Inbox,
    LeaseOutcome, Message, NodeInfo, Project, Request, Resource, Thread, ThreadSummary, Token,
};

#[derive(Debug, Serialize)]
pub struct Status {
    pub node: NodeInfo,
    pub db_path: String,
    pub projects: Vec<Project>,
    pub agents: Vec<Agent>,
}

pub fn status(s: &Status) -> String {
    let mut out = format!(
        "node      {} ({}, {})\ndatabase  {}\nprojects  {}\nagents    {}\n",
        s.node.node_id,
        s.node.display_name,
        s.node.role,
        s.db_path,
        s.projects.len(),
        s.agents.len()
    );
    if !s.agents.is_empty() {
        out.push('\n');
        out.push_str(&agents(&s.agents));
    }
    out
}

pub fn agents(agents: &[Agent]) -> String {
    agents
        .iter()
        .map(|a| {
            format!(
                "{:<24} {:<9} {:<8} last seen {}\n",
                a.address,
                a.program,
                a.status,
                a.last_seen_at.as_deref().unwrap_or("never")
            )
        })
        .collect()
}

pub fn sent(m: &Message) -> String {
    let mut out = format!("sent {} to {}", m.message_id, m.to.join(", "));
    if !m.cc.is_empty() {
        let _ = write!(out, " (cc {})", m.cc.join(", "));
    }
    if let Some(r) = &m.request_id {
        let _ = write!(out, "\nopened request {r}");
    }
    out
}

fn flag(m: &Message) -> &'static str {
    match m.importance {
        wyres_core::Importance::Urgent => "!!",
        wyres_core::Importance::High => "! ",
        _ => "  ",
    }
}

pub fn inbox(i: &Inbox) -> String {
    let mut out = format!(
        "{}: {} unread ({} urgent), {} awaiting ack, {} request(s)\n",
        i.agent,
        i.unread,
        i.urgent_unread,
        i.awaiting_ack,
        i.requests.len()
    );
    for item in &i.messages {
        let m = &item.message;
        let state = if item.read_at.is_none() {
            "new"
        } else if m.ack_required && item.acked_at.is_none() {
            "ack?"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "{} {:<4} {}  {:<16} {}  [{}]",
            flag(m),
            state,
            m.created_at,
            m.from,
            m.subject,
            m.message_id
        );
    }
    if !i.requests.is_empty() {
        out.push_str("\nrequests:\n");
        out.push_str(&requests(&i.requests));
    }
    out
}

pub fn threads(ts: &[ThreadSummary]) -> String {
    ts.iter()
        .map(|t| {
            format!(
                "{}  {:>3} msg {:>3} unread  {}  ({})  [{}]\n",
                t.last_at,
                t.messages,
                t.unread,
                t.subject,
                t.participants.join(", "),
                t.thread_id
            )
        })
        .collect()
}

pub fn thread(t: &Thread) -> String {
    let mut out = String::new();
    for m in &t.messages {
        let _ = writeln!(
            out,
            "{}{} {} -> {}  [{}]\n  {}\n",
            flag(m).trim(),
            m.created_at,
            m.from,
            m.to.join(", "),
            m.message_id,
            m.subject
        );
        for line in m.body.lines() {
            let _ = writeln!(out, "  {line}");
        }
        out.push('\n');
    }
    out
}

pub fn requests(rs: &[Request]) -> String {
    rs.iter()
        .map(|r| {
            let owner = r.owner.as_deref().unwrap_or("-");
            let due = r
                .due_at
                .as_deref()
                .map(|d| format!(" due {d}"))
                .unwrap_or_default();
            format!(
                "{:<9} {:<16} {}  (from {}{due})  [{}]\n",
                r.status.as_str(),
                owner,
                r.title,
                r.created_by,
                r.request_id
            )
        })
        .collect()
}

pub fn holds(hs: &[Hold]) -> String {
    hs.iter()
        .map(|h| {
            format!(
                "{:<9} {:<16} {}:{}  until {}{}  [{}]\n",
                if h.exclusive { "exclusive" } else { "shared" },
                h.agent,
                h.project,
                h.pattern,
                h.expires_at,
                h.reason
                    .as_deref()
                    .map(|r| format!("  ({r})"))
                    .unwrap_or_default(),
                h.hold_id
            )
        })
        .collect()
}

pub fn conflicts(cs: &[HoldConflict]) -> String {
    cs.iter()
        .map(|c| {
            format!(
                "conflict: {} is held by {} ({}, {} until {})\n",
                c.path,
                c.held.agent,
                c.held.pattern,
                if c.held.exclusive {
                    "exclusive"
                } else {
                    "shared"
                },
                c.held.expires_at
            )
        })
        .collect()
}

pub fn hold_report(r: &HoldReport) -> String {
    if r.conflicts.is_empty() {
        holds(&r.granted)
    } else {
        format!("nothing acquired\n{}", conflicts(&r.conflicts))
    }
}

pub fn heartbeat(h: &HeartbeatReport) -> String {
    let holds = if h.status == "offline" {
        format!("released {} hold(s)", h.released_holds.len())
    } else {
        format!("renewed {} hold(s)", h.renewed_holds.len())
    };
    format!(
        "{} {}: {holds}; {} unread ({} urgent), {} open request(s)",
        h.agent, h.status, h.unread, h.urgent_unread, h.open_requests
    )
}

pub fn events(es: &[Event]) -> String {
    es.iter()
        .map(|e| {
            format!(
                "{:>6} {}  {:<18} {:<16} {}\n",
                e.seq, e.ts, e.kind, e.actor, e.event_id
            )
        })
        .collect()
}

pub fn activity(f: &ActivityFeed) -> String {
    f.items
        .iter()
        .map(|i| format!("{}  {:<8} {}\n", i.ts, i.category.as_str(), i.summary))
        .collect()
}

#[derive(Debug, Serialize)]
pub struct HookStatus {
    pub client: String,
    pub installed: bool,
    pub config: String,
}

pub fn hook_status(rows: &[HookStatus]) -> String {
    rows.iter()
        .map(|r| {
            format!(
                "{:<7} {:<13} {}\n",
                r.client,
                if r.installed {
                    "installed"
                } else {
                    "not installed"
                },
                r.config
            )
        })
        .collect()
}

pub fn hook_report(r: &crate::hooks::HookReport) -> String {
    let mut out = String::new();
    if r.dry_run {
        out.push_str("dry run: nothing written\n");
    }
    for c in &r.changes {
        let verb = match c.change {
            crate::hooks::ChangeKind::Create => "create",
            crate::hooks::ChangeKind::Update => "update",
            crate::hooks::ChangeKind::Delete => "delete",
            crate::hooks::ChangeKind::Unchanged => "unchanged",
        };
        let _ = write!(out, "{verb:<9} {}", c.path);
        if let Some(b) = &c.backup {
            let _ = write!(out, "  (backup: {b})");
        }
        out.push('\n');
    }
    for n in &r.notes {
        let _ = writeln!(out, "note: {n}");
    }
    out
}

pub fn resources(rs: &[Resource]) -> String {
    if rs.is_empty() {
        return "no resources registered".to_string();
    }
    rs.iter()
        .map(|r| {
            let who = match (&r.owner, r.refcount) {
                (Some(o), n) if n > 1 => format!("{o} +{} borrower(s)", n - 1),
                (Some(o), _) => o.clone(),
                (None, _) => "-".to_string(),
            };
            format!(
                "{:<8} {:<12} {:<26} {:<9} {:<28} {}{}  [{}]\n",
                r.status,
                r.kind.as_str(),
                r.name,
                r.mode.as_str(),
                who,
                r.endpoint.as_deref().unwrap_or(""),
                r.purpose
                    .as_deref()
                    .map(|p| format!("  ({p})"))
                    .unwrap_or_default(),
                r.resource_id
            )
        })
        .collect()
}

pub fn lease_outcome(o: &LeaseOutcome) -> String {
    let lease = o
        .lease
        .as_ref()
        .map(|l| format!(" as {} until {}", l.role, l.expires_at))
        .unwrap_or_default();
    format!(
        "{} {} {}{lease}\n{}",
        o.outcome,
        o.resource.kind.as_str(),
        o.resource.name,
        resources(std::slice::from_ref(&o.resource))
    )
}

pub fn orphans(r: &crate::reap::OrphanReport) -> String {
    let mut out = String::new();
    if r.orphans.is_empty() {
        out.push_str("no orphans found\n");
    }
    for o in &r.orphans {
        let _ = writeln!(
            out,
            "{:<22} {:<28} pid {:<7} port {:<6} age {}s  {}",
            o.source,
            o.target,
            o.pid.map_or("-".into(), |p| p.to_string()),
            o.port.map_or("-".into(), |p| p.to_string()),
            o.age_seconds.map_or("?".into(), |a| a.to_string()),
            o.command.as_deref().unwrap_or("")
        );
    }
    if let Some(scan) = &r.scan {
        if let Some(e) = &scan.error {
            let _ = writeln!(out, "scan unavailable: {e}");
        }
    }
    out
}

pub fn reap_plan(p: &crate::reap::ReapPlan) -> String {
    let mut out = format!(
        "target {} (pid {}): {}\n",
        p.target,
        p.pid.map_or("-".into(), |x| x.to_string()),
        p.command.as_deref().unwrap_or("unknown command")
    );
    if let Some(o) = &p.outcome {
        let _ = writeln!(out, "{o}");
    } else if p.eligible {
        let _ = writeln!(
            out,
            "eligible; nothing was done. Re-run with --confirm to send SIGTERM."
        );
    } else {
        for b in &p.blockers {
            let _ = writeln!(out, "blocked: {b}");
        }
    }
    out
}

pub fn tokens(ts: &[Token]) -> String {
    if ts.is_empty() {
        return "no tokens".to_string();
    }
    ts.iter()
        .map(|t| {
            let state = if t.revoked_at.is_some() {
                "revoked"
            } else {
                "active"
            };
            format!(
                "{:<8} {}  {:<20} projects [{}]  last used {}{}\n",
                state,
                t.token_id,
                t.agent,
                t.project_globs.join(", "),
                t.last_used_at.as_deref().unwrap_or("never"),
                t.label
                    .as_deref()
                    .map(|l| format!("  ({l})"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

pub fn audit(es: &[AuditEntry]) -> String {
    es.iter()
        .map(|e| {
            format!(
                "{}  {:<7} {:<16} {:<32} {}{}\n",
                e.ts,
                e.outcome,
                e.agent.as_deref().unwrap_or("-"),
                e.operation,
                e.detail.as_deref().unwrap_or(""),
                e.remote
                    .as_deref()
                    .map(|r| format!("  [{r}]"))
                    .unwrap_or_default()
            )
        })
        .collect()
}
