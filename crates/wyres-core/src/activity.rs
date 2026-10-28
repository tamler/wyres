//! Activity feed: a readable projection of the event log so every peer can see who is
//! doing what. Items carry one-line summaries and ids, never message bodies.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::identity::address;
use crate::store::Store;
use crate::time::fmt_ts;

/// Coarse activity categories, derived from event kinds.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Mail,
    Request,
    Hold,
    Resource,
    Session,
    System,
    Federation,
}

impl ActivityKind {
    pub const ALL: [ActivityKind; 7] = [
        ActivityKind::Mail,
        ActivityKind::Request,
        ActivityKind::Hold,
        ActivityKind::Resource,
        ActivityKind::Session,
        ActivityKind::System,
        ActivityKind::Federation,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ActivityKind::Mail => "mail",
            ActivityKind::Request => "request",
            ActivityKind::Hold => "hold",
            ActivityKind::Resource => "resource",
            ActivityKind::Session => "session",
            ActivityKind::System => "system",
            ActivityKind::Federation => "federation",
        }
    }

    /// Category of an event kind such as `message.sent` or `hold.expired`.
    pub fn of_event(kind: &str) -> ActivityKind {
        match kind.split('.').next().unwrap_or_default() {
            "message" | "delivery" => ActivityKind::Mail,
            "request" => ActivityKind::Request,
            "hold" => ActivityKind::Hold,
            "resource" | "orphan" => ActivityKind::Resource,
            "session" | "agent" => ActivityKind::Session,
            "peer" | "sync" | "pair" | "federation" => ActivityKind::Federation,
            _ => ActivityKind::System,
        }
    }
}

impl std::str::FromStr for ActivityKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        ActivityKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown activity kind {s:?}; expected mail, request, hold, resource, session, system or federation"
                ))
            })
    }
}

/// Where a feed read starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Since {
    /// Events at or after this time (ms since epoch).
    Time(i64),
    /// Events strictly after this event id (a cursor from a previous read).
    After(String),
}

impl Since {
    /// Parse `15m` / `2h` / `1d` / `90s` (relative to `now`), an RFC 3339 time, or an event id.
    pub fn parse(raw: &str, now: i64) -> Result<Since> {
        let raw = raw.trim();
        if let Some(ms) = parse_duration_ms(raw) {
            return Ok(Since::Time(now - ms));
        }
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(raw) {
            return Ok(Since::Time(t.timestamp_millis()));
        }
        if raw.len() == 26 && ulid::Ulid::from_string(raw).is_ok() {
            return Ok(Since::After(raw.to_ascii_uppercase()));
        }
        Err(Error::Invalid(format!(
            "invalid since {raw:?}: use a duration like 30m or 2h, an RFC 3339 time, or an event id"
        )))
    }
}

fn parse_duration_ms(raw: &str) -> Option<i64> {
    let (num, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    n.checked_mul(secs * 1000)
}

#[derive(Debug, Clone, Default)]
pub struct ActivityQuery {
    pub since: Option<Since>,
    /// Maximum items (default 50, at most 500).
    pub limit: u32,
    /// Only these categories (empty = all).
    pub kinds: Vec<ActivityKind>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ActivityItem {
    pub event_id: String,
    pub ts: String,
    pub node_id: String,
    pub actor: String,
    pub category: ActivityKind,
    /// Raw event kind, e.g. `message.sent`.
    pub kind: String,
    /// One-line, human-readable description (no message bodies).
    pub summary: String,
    /// Ids of the objects involved (message_id, request_id, hold_id, ...).
    pub refs: Value,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ActivityFeed {
    /// Oldest first.
    pub items: Vec<ActivityItem>,
    /// Pass back as `since` to continue after the last item.
    pub next_since: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AgentPresence {
    pub agent: String,
    pub program: String,
    pub status: String,
    pub last_seen_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RequestCounts {
    pub open: u32,
    pub claimed: u32,
    pub stale: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ActivitySummary {
    pub since: String,
    pub until: String,
    /// Events in the window per category.
    pub counts: BTreeMap<String, u32>,
    /// Agents seen in the window or currently active.
    pub agents: Vec<AgentPresence>,
    pub requests: RequestCounts,
    pub live_holds: u32,
    /// Most recent notable events (deliveries and renewals left out), newest last.
    pub highlights: Vec<String>,
    /// Compact plain-text digest of the above.
    pub text: String,
}

const MAX_LIMIT: u32 = 500;
const HIGHLIGHTS: usize = 8;

/// Render one event as a feed item, looking up names the payload only references by id.
fn item(conn: &Connection, row: EventRow) -> Result<ActivityItem> {
    let p: Value = serde_json::from_str(&row.payload).unwrap_or(Value::Null);
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let list = |k: &str| {
        p.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    let actor = &row.actor;
    let (summary, refs) = match row.kind.as_str() {
        "node.created" => (
            format!("node {} created", s("display_name")),
            json!({ "node_id": s("node_id") }),
        ),
        "project.ensured" => (
            format!("project {} registered at {}", s("slug"), s("abs_path")),
            json!({ "project_id": s("project_id") }),
        ),
        "agent.registered" => (
            format!("{actor} registered ({})", s("program")),
            json!({ "agent_id": s("agent_id") }),
        ),
        "session.started" | "session.ended" => {
            let verb = if row.kind == "session.started" {
                "started"
            } else {
                "ended"
            };
            let client = p
                .get("client")
                .and_then(Value::as_str)
                .map(|c| format!(" ({c})"))
                .unwrap_or_default();
            (
                format!("{actor} {verb} a session{client}"),
                json!({ "agent_id": s("agent_id"), "session_id": p.get("session_id") }),
            )
        }
        "message.sent" => {
            let kind = if p.get("request_id").is_some_and(|r| !r.is_null()) {
                "request"
            } else {
                "message"
            };
            let urgency = match s("importance").as_str() {
                "urgent" => " [urgent]",
                "high" => " [high]",
                _ => "",
            };
            (
                format!(
                    "{actor} sent {kind} to {}: {}{urgency}",
                    list("to"),
                    s("subject")
                ),
                json!({ "message_id": s("message_id"), "thread_id": s("thread_id"), "request_id": p.get("request_id") }),
            )
        }
        "delivery.read" | "delivery.acked" => {
            let verb = if row.kind == "delivery.acked" {
                "acknowledged"
            } else {
                "read"
            };
            let mid = s("message_id");
            (
                format!("{actor} {verb} \"{}\"", message_subject(conn, &mid)?),
                json!({ "message_id": mid }),
            )
        }
        "request.created" => (
            format!("{actor} opened request: {}", s("title")),
            json!({ "request_id": s("request_id"), "thread_id": s("thread_id") }),
        ),
        "request.claimed" | "request.released" | "request.completed" | "request.cancelled"
        | "request.stale" => {
            let rid = s("request_id");
            let title = request_title(conn, &rid)?;
            let text = match row.kind.as_str() {
                "request.claimed" => format!("{actor} claimed request: {title}"),
                "request.released" => format!("{actor} released request: {title}"),
                "request.completed" => format!("{actor} completed request: {title}"),
                "request.cancelled" => format!("{actor} cancelled request: {title}"),
                _ => format!("request went stale (past due): {title}"),
            };
            (text, json!({ "request_id": rid }))
        }
        "hold.acquired" | "hold.updated" => {
            let mode = if p.get("exclusive").and_then(Value::as_bool) == Some(true) {
                "exclusive"
            } else {
                "shared"
            };
            let project = project_slug(conn, &s("project_id"))?;
            (
                format!("{actor} holds {project}:{} ({mode})", s("pattern")),
                json!({ "hold_id": s("hold_id"), "project_id": s("project_id") }),
            )
        }
        "hold.renewed" | "hold.released" => {
            let n = p
                .get("hold_ids")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let verb = if row.kind == "hold.renewed" {
                "renewed"
            } else {
                "released"
            };
            let why = match s("reason").as_str() {
                "session_end" => " at session end",
                _ => "",
            };
            (
                format!("{actor} {verb} {n} hold(s){why}"),
                json!({ "hold_ids": p.get("hold_ids") }),
            )
        }
        "hold.expired" => {
            let who = agent_address(conn, &s("agent_id"))?;
            (
                format!("hold on {} by {who} expired", s("pattern")),
                json!({ "hold_id": s("hold_id") }),
            )
        }
        other => (format!("{actor}: {other}"), p.clone()),
    };
    Ok(ActivityItem {
        category: ActivityKind::of_event(&row.kind),
        event_id: row.event_id,
        ts: fmt_ts(row.ts),
        node_id: row.node_id,
        actor: row.actor,
        kind: row.kind,
        summary,
        refs,
    })
}

fn message_subject(conn: &Connection, id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT subject FROM messages WHERE message_id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| id.to_string()))
}

fn request_title(conn: &Connection, id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT title FROM requests WHERE request_id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| id.to_string()))
}

fn project_slug(conn: &Connection, id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT slug FROM projects WHERE project_id = ?1",
            [id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| id.to_string()))
}

fn agent_address(conn: &Connection, id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT a.name, p.slug FROM agents a
             LEFT JOIN projects p ON p.project_id = a.project_id WHERE a.agent_id = ?1",
            [id],
            |r| {
                Ok(address(
                    &r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.as_deref(),
                ))
            },
        )
        .optional()?
        .unwrap_or_else(|| id.to_string()))
}

struct EventRow {
    event_id: String,
    node_id: String,
    ts: i64,
    actor: String,
    kind: String,
    payload: String,
}

fn kind_matches(kinds: &[ActivityKind], kind: &str) -> bool {
    kinds.is_empty() || kinds.contains(&ActivityKind::of_event(kind))
}

/// Events after `since`, oldest first; without `since`, the newest `limit` events.
fn read_events(
    conn: &Connection,
    since: Option<&Since>,
    kinds: &[ActivityKind],
    limit: u32,
) -> Result<Vec<EventRow>> {
    let after_seq: Option<i64> = match since {
        Some(Since::After(id)) => Some(
            conn.query_row("SELECT seq FROM events WHERE event_id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("unknown event id {id:?}")))?,
        ),
        _ => None,
    };
    let since_ts = match since {
        Some(Since::Time(t)) => Some(*t),
        _ => None,
    };
    let newest_first = since.is_none();
    let sql = format!(
        "SELECT event_id, node_id, ts, actor, kind, payload FROM events
         WHERE (?1 IS NULL OR seq > ?1) AND (?2 IS NULL OR ts >= ?2)
         ORDER BY seq {}",
        if newest_first { "DESC" } else { "ASC" }
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(params![after_seq, since_ts])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let kind: String = r.get(4)?;
        if !kind_matches(kinds, &kind) {
            continue;
        }
        out.push(EventRow {
            event_id: r.get(0)?,
            node_id: r.get(1)?,
            ts: r.get(2)?,
            actor: r.get(3)?,
            kind,
            payload: r.get(5)?,
        });
        if out.len() as u32 >= limit {
            break;
        }
    }
    if newest_first {
        out.reverse();
    }
    Ok(out)
}

impl Store {
    /// Read the activity feed.
    pub fn activity(&self, query: ActivityQuery) -> Result<ActivityFeed> {
        let limit = match query.limit {
            0 => 50,
            n => n.min(MAX_LIMIT),
        };
        self.sweep()?;
        self.read(|conn| {
            let rows = read_events(conn, query.since.as_ref(), &query.kinds, limit)?;
            let items = rows
                .into_iter()
                .map(|r| item(conn, r))
                .collect::<Result<Vec<_>>>()?;
            let next_since = items
                .last()
                .map(|i| i.event_id.clone())
                .or(match &query.since {
                    Some(Since::After(id)) => Some(id.clone()),
                    _ => None,
                });
            Ok(ActivityFeed { items, next_since })
        })
    }

    /// Summarize activity since a point in time (default: the last hour).
    pub fn activity_summary(&self, since: Option<Since>) -> Result<ActivitySummary> {
        let now = self.now();
        let since = since.unwrap_or(Since::Time(now - 3_600_000));
        self.sweep()?;
        self.read(|conn| {
            let since_ms = match &since {
                Since::Time(t) => *t,
                Since::After(id) => conn
                    .query_row("SELECT ts FROM events WHERE event_id = ?1", [id], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .ok_or_else(|| Error::NotFound(format!("unknown event id {id:?}")))?,
            };
            let rows = read_events(conn, Some(&since), &[], u32::MAX)?;
            let mut counts: BTreeMap<String, u32> = BTreeMap::new();
            for r in &rows {
                *counts
                    .entry(ActivityKind::of_event(&r.kind).as_str().to_string())
                    .or_default() += 1;
            }
            let notable: Vec<EventRow> = rows
                .into_iter()
                .filter(|r| {
                    !matches!(
                        r.kind.as_str(),
                        "delivery.read" | "delivery.acked" | "hold.renewed"
                    )
                })
                .collect();
            let skip = notable.len().saturating_sub(HIGHLIGHTS);
            let highlights = notable
                .into_iter()
                .skip(skip)
                .map(|r| item(conn, r).map(|i| i.summary))
                .collect::<Result<Vec<_>>>()?;

            let mut stmt = conn.prepare(
                "SELECT a.name, p.slug, a.program, a.status, a.last_seen_at FROM agents a
                 LEFT JOIN projects p ON p.project_id = a.project_id
                 WHERE a.node_id = ?1 AND (a.status = 'active' OR a.last_seen_at >= ?2)
                 ORDER BY a.last_seen_at DESC",
            )?;
            let agents = stmt
                .query_map(params![self.node_id(), since_ms], |r| {
                    Ok(AgentPresence {
                        agent: address(
                            &r.get::<_, String>(0)?,
                            r.get::<_, Option<String>>(1)?.as_deref(),
                        ),
                        program: r.get(2)?,
                        status: r.get(3)?,
                        last_seen_at: r.get::<_, Option<i64>>(4)?.map(fmt_ts),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let requests = conn.query_row(
                "SELECT count(*) FILTER (WHERE status = 'open'),
                        count(*) FILTER (WHERE status = 'claimed'),
                        count(*) FILTER (WHERE status = 'stale')
                 FROM requests WHERE node_id = ?1",
                [self.node_id()],
                |r| {
                    Ok(RequestCounts {
                        open: r.get(0)?,
                        claimed: r.get(1)?,
                        stale: r.get(2)?,
                    })
                },
            )?;
            let live_holds: u32 = conn.query_row(
                "SELECT count(*) FROM holds WHERE node_id = ?1 AND released_at IS NULL",
                [self.node_id()],
                |r| r.get(0),
            )?;

            let active: Vec<&str> = agents
                .iter()
                .filter(|a| a.status == "active")
                .map(|a| a.agent.as_str())
                .collect();
            let mut text = format!(
                "{} event(s) since {}; active agents: {}; requests: {} open, {} claimed, {} stale; {} live hold(s).",
                counts.values().sum::<u32>(),
                fmt_ts(since_ms),
                if active.is_empty() { "none".to_string() } else { active.join(", ") },
                requests.open,
                requests.claimed,
                requests.stale,
                live_holds
            );
            for h in &highlights {
                text.push_str("\n- ");
                text.push_str(h);
            }
            Ok(ActivitySummary {
                since: fmt_ts(since_ms),
                until: fmt_ts(now),
                counts,
                agents,
                requests,
                live_holds,
                highlights,
                text,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_since() {
        let now = 10_000_000;
        assert_eq!(
            Since::parse("15m", now).unwrap(),
            Since::Time(now - 900_000)
        );
        assert_eq!(
            Since::parse("2h", now).unwrap(),
            Since::Time(now - 7_200_000)
        );
        assert_eq!(
            Since::parse("1970-01-01T00:00:01Z", now).unwrap(),
            Since::Time(1000)
        );
        let id = ulid::Ulid::new().to_string();
        assert_eq!(Since::parse(&id, now).unwrap(), Since::After(id));
        assert!(Since::parse("yesterday", now).is_err());
        assert!(Since::parse("5y", now).is_err());
    }

    #[test]
    fn categories() {
        assert_eq!(ActivityKind::of_event("message.sent"), ActivityKind::Mail);
        assert_eq!(ActivityKind::of_event("delivery.acked"), ActivityKind::Mail);
        assert_eq!(ActivityKind::of_event("hold.expired"), ActivityKind::Hold);
        assert_eq!(
            ActivityKind::of_event("session.started"),
            ActivityKind::Session
        );
        assert_eq!(ActivityKind::of_event("node.created"), ActivityKind::System);
    }
}
