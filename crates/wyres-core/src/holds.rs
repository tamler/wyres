//! Path holds: advisory, project-scoped leases on files or globs with a TTL.
//!
//! Holds signal intent; they do not lock files. An exclusive hold conflicts with any
//! overlapping hold of another agent; shared holds only conflict with exclusive ones.
//! Holds lapse when their TTL passes without a heartbeat and are then expired by the sweeper.

use std::path::Path;

use globset::{Glob, GlobBuilder, GlobMatcher};
use rusqlite::{params, Connection, Row};
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{address, find_project, resolve_agent, Project};
use crate::mail::touch;
use crate::requests::stale_overdue_tx;
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;

pub const DEFAULT_HOLD_TTL_SECONDS: i64 = 30 * 60;
pub const MIN_HOLD_TTL_SECONDS: i64 = 60;
pub const MAX_HOLD_TTL_SECONDS: i64 = 4 * 60 * 60;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Hold {
    pub hold_id: String,
    pub project: String,
    pub agent: String,
    /// Path or glob relative to the project root.
    pub pattern: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    pub ttl_seconds: i64,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldConflict {
    /// The requested path or glob.
    pub path: String,
    /// The existing hold it collides with.
    pub held: Hold,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldReport {
    /// Holds now owned by the caller (empty if any conflict was found).
    pub granted: Vec<Hold>,
    pub conflicts: Vec<HoldConflict>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SweepReport {
    pub expired_holds: usize,
    pub stale_requests: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HeartbeatReport {
    pub agent: String,
    pub status: String,
    pub last_seen_at: String,
    pub renewed_holds: Vec<Hold>,
    pub released_holds: Vec<String>,
    pub unread: u32,
    pub urgent_unread: u32,
    /// Requests addressed to or owned by the agent that still need attention.
    pub open_requests: u32,
}

/// Which step of a session a heartbeat represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionPhase {
    Start,
    #[default]
    Beat,
    End,
}

/// Optional context recorded on `session.started` / `session.ended` events.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// Client program, e.g. `claude` or `codex`.
    pub client: Option<String>,
    /// The client's own session identifier, if it has one.
    pub session_id: Option<String>,
}

/// Input for [`Store::acquire_holds`].
#[derive(Debug, Clone)]
pub struct AcquireHolds {
    pub agent: String,
    /// Absolute project path or project slug.
    pub project: String,
    pub paths: Vec<String>,
    pub ttl_seconds: Option<i64>,
    pub exclusive: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct HoldFilter {
    pub project: Option<String>,
    pub agent: Option<String>,
}

const HOLD_SELECT: &str =
    "SELECT h.hold_id, p.slug, a.name, ap.slug, h.pattern, h.exclusive, h.reason,
       h.ttl_seconds, h.created_at, h.expires_at
     FROM holds h
     JOIN projects p ON p.project_id = h.project_id
     JOIN agents a ON a.agent_id = h.agent_id
     LEFT JOIN projects ap ON ap.project_id = a.project_id";

fn hold_from_row(r: &Row<'_>) -> rusqlite::Result<Hold> {
    let name: String = r.get(2)?;
    let agent_slug: Option<String> = r.get(3)?;
    Ok(Hold {
        hold_id: r.get(0)?,
        project: r.get(1)?,
        agent: address(&name, agent_slug.as_deref()),
        pattern: r.get(4)?,
        exclusive: r.get(5)?,
        reason: r.get(6)?,
        ttl_seconds: r.get(7)?,
        created_at: fmt_ts(r.get(8)?),
        expires_at: fmt_ts(r.get(9)?),
    })
}

fn live_holds(
    conn: &Connection,
    where_sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Hold>> {
    let mut stmt = conn.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND {where_sql} ORDER BY h.created_at, h.hold_id"
    ))?;
    let rows = stmt.query_map(args, hold_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn clamp_ttl(ttl: Option<i64>) -> Result<i64> {
    let ttl = ttl.unwrap_or(DEFAULT_HOLD_TTL_SECONDS);
    if ttl <= 0 {
        return Err(Error::Invalid("ttl_seconds must be positive".into()));
    }
    Ok(ttl.clamp(MIN_HOLD_TTL_SECONDS, MAX_HOLD_TTL_SECONDS))
}

/// Normalize a requested path or glob to a project-relative pattern.
pub(crate) fn normalize_pattern(project: &Project, raw: &str) -> Result<String> {
    let raw = raw.trim();
    let root = project.abs_path.trim_end_matches('/');
    let rel = if Path::new(raw).is_absolute() {
        match raw.strip_prefix(root) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
            _ => {
                return Err(Error::Invalid(format!(
                    "path {raw:?} is outside project {}",
                    project.abs_path
                )))
            }
        }
    } else {
        raw
    };
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(Error::Invalid(format!(
                    "path {raw:?} must not contain '..'"
                )))
            }
            p => parts.push(p),
        }
    }
    // An empty pattern means the whole project.
    let pattern = if parts.is_empty() {
        "**".to_string()
    } else {
        parts.join("/")
    };
    compile(&pattern)?;
    Ok(pattern)
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::Invalid(format!("invalid glob {pattern:?}: {e}")))?;
    Ok(glob.compile_matcher())
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

/// The part of a pattern before its first glob metacharacter.
fn literal_prefix(pattern: &str) -> &str {
    match pattern.find(['*', '?', '[', '{']) {
        Some(i) => &pattern[..i],
        None => pattern,
    }
}

/// `dir` names `path` itself or a directory containing it.
fn covers(dir: &str, path: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// Conservative overlap test between two project-relative patterns. A plain path also
/// covers everything below it, so holding `src` overlaps `src/main.rs`. False positives
/// are acceptable for advisory holds; false negatives are not.
pub fn patterns_overlap(a: &str, b: &str) -> bool {
    if a == b || a == "**" || b == "**" {
        return true;
    }
    match (is_glob(a), is_glob(b)) {
        (false, false) => covers(a, b) || covers(b, a),
        (true, false) => glob_touches_literal(a, b),
        (false, true) => glob_touches_literal(b, a),
        (true, true) => {
            let (pa, pb) = (literal_prefix(a), literal_prefix(b));
            pa.starts_with(pb) || pb.starts_with(pa)
        }
    }
}

fn matches(glob: &str, path: &str) -> bool {
    // Patterns are validated on the way in; treat an unparsable one as overlapping.
    compile(glob).map(|m| m.is_match(path)).unwrap_or(true)
}

fn glob_touches_literal(glob: &str, literal: &str) -> bool {
    if matches(glob, literal) {
        return true;
    }
    // The literal may be a directory with matches of the glob below it: compare the
    // glob's leading components at the literal's depth.
    let comps: Vec<&str> = glob.split('/').collect();
    let lit: Vec<&str> = literal.split('/').collect();
    if let Some(pos) = comps.iter().position(|c| c.contains("**")) {
        if pos < lit.len() {
            return pos == 0 || matches(&comps[..pos].join("/"), &lit[..pos].join("/"));
        }
    }
    comps.len() > lit.len() && matches(&comps[..lit.len()].join("/"), literal)
}

fn conflicts_for(
    tx: &Tx<'_>,
    project_id: &str,
    agent_id: &str,
    patterns: &[(String, String)],
    exclusive: bool,
) -> Result<Vec<HoldConflict>> {
    let mut stmt = tx.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND h.expires_at > ?3
           AND h.project_id = ?1 AND h.agent_id != ?2
         ORDER BY h.created_at"
    ))?;
    let others = stmt
        .query_map(params![project_id, agent_id, tx.now], hold_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut conflicts = Vec::new();
    for (requested, pattern) in patterns {
        for held in &others {
            if (exclusive || held.exclusive) && patterns_overlap(pattern, &held.pattern) {
                conflicts.push(HoldConflict {
                    path: requested.clone(),
                    held: held.clone(),
                });
            }
        }
    }
    Ok(conflicts)
}

pub(crate) fn sweep_tx(tx: &Tx<'_>) -> Result<SweepReport> {
    let mut stmt = tx.prepare(
        "SELECT hold_id, pattern, agent_id FROM holds
         WHERE node_id = ?1 AND released_at IS NULL AND expires_at <= ?2",
    )?;
    let expired = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hold_id, pattern, agent_id) in &expired {
        tx.emit(
            "system",
            "hold.expired",
            json!({ "hold_id": hold_id, "pattern": pattern, "agent_id": agent_id }),
        )?;
        tx.execute(
            "UPDATE holds SET released_at = expires_at, release_reason = 'expired' WHERE hold_id = ?1",
            [hold_id],
        )?;
    }
    let stale_requests = stale_overdue_tx(tx)?;
    Ok(SweepReport {
        expired_holds: expired.len(),
        stale_requests,
    })
}

fn release_tx(tx: &Tx<'_>, actor: &str, hold_ids: &[String], reason: &str) -> Result<()> {
    if hold_ids.is_empty() {
        return Ok(());
    }
    tx.emit(
        actor,
        "hold.released",
        json!({ "hold_ids": hold_ids, "reason": reason }),
    )?;
    for id in hold_ids {
        tx.execute(
            "UPDATE holds SET released_at = ?2, release_reason = ?3 WHERE hold_id = ?1",
            params![id, tx.now, reason],
        )?;
    }
    Ok(())
}

fn renew_tx(tx: &Tx<'_>, actor: &str, holds: &[(String, i64)]) -> Result<()> {
    if holds.is_empty() {
        return Ok(());
    }
    let ids: Vec<&String> = holds.iter().map(|(id, _)| id).collect();
    tx.emit(actor, "hold.renewed", json!({ "hold_ids": ids }))?;
    for (id, ttl) in holds {
        tx.execute(
            "UPDATE holds SET renewed_at = ?2, expires_at = ?2 + ?3 * 1000 WHERE hold_id = ?1",
            params![id, tx.now, ttl],
        )?;
    }
    Ok(())
}

/// Live holds owned by `agent_id`, optionally limited to `only` ids (error on unknown ids).
fn owned_holds(tx: &Tx<'_>, agent_id: &str, only: &[String]) -> Result<Vec<(String, i64)>> {
    let mut stmt = tx.prepare(
        "SELECT hold_id, ttl_seconds FROM holds
         WHERE agent_id = ?1 AND released_at IS NULL ORDER BY created_at",
    )?;
    let owned = stmt
        .query_map([agent_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if only.is_empty() {
        return Ok(owned);
    }
    only.iter()
        .map(|id| {
            owned
                .iter()
                .find(|(h, _)| h == id)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("no live hold {id:?} owned by this agent")))
        })
        .collect()
}

fn holds_by_id(conn: &Connection, ids: &[String]) -> Result<Vec<Hold>> {
    ids.iter()
        .map(|id| {
            Ok(conn.query_row(
                &format!("{HOLD_SELECT} WHERE h.hold_id = ?1"),
                [id],
                hold_from_row,
            )?)
        })
        .collect()
}

impl Store {
    /// Expire lapsed holds and mark overdue requests stale.
    pub fn sweep(&self) -> Result<SweepReport> {
        self.write(sweep_tx)
    }

    /// Acquire holds on paths/globs. All-or-nothing: if any requested path conflicts with
    /// another agent's live hold, nothing is granted and the conflicts are reported.
    pub fn acquire_holds(&self, input: AcquireHolds) -> Result<HoldReport> {
        if input.paths.is_empty() {
            return Err(Error::Invalid("no paths given".into()));
        }
        let ttl = clamp_ttl(input.ttl_seconds)?;
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let project = find_project(tx, tx.node_id, &input.project)?;
            let mut patterns: Vec<(String, String)> = Vec::new();
            for raw in &input.paths {
                let p = normalize_pattern(&project, raw)?;
                if !patterns.iter().any(|(_, q)| *q == p) {
                    patterns.push((raw.clone(), p));
                }
            }
            let conflicts =
                conflicts_for(tx, &project.project_id, &me.id, &patterns, input.exclusive)?;
            if !conflicts.is_empty() {
                touch(tx, &me.id)?;
                return Ok(HoldReport {
                    granted: Vec::new(),
                    conflicts,
                });
            }
            let mut granted_ids = Vec::new();
            for (_, pattern) in &patterns {
                // Re-acquiring a pattern you already hold updates and renews that hold.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT hold_id FROM holds WHERE agent_id = ?1 AND project_id = ?2
                           AND pattern = ?3 AND released_at IS NULL",
                        params![me.id, project.project_id, pattern],
                        |r| r.get(0),
                    )
                    .ok();
                let expires_at = tx.now + ttl * 1000;
                let payload = json!({
                    "project_id": project.project_id,
                    "pattern": pattern,
                    "exclusive": input.exclusive,
                    "reason": input.reason,
                    "ttl_seconds": ttl,
                    "expires_at": expires_at,
                });
                let hold_id = match existing {
                    Some(id) => {
                        let mut payload = payload;
                        payload["hold_id"] = json!(id);
                        tx.emit(&me.address, "hold.updated", payload)?;
                        tx.execute(
                            "UPDATE holds SET exclusive = ?2, reason = ?3, ttl_seconds = ?4,
                               renewed_at = ?5, expires_at = ?6 WHERE hold_id = ?1",
                            params![id, input.exclusive, input.reason, ttl, tx.now, expires_at],
                        )?;
                        id
                    }
                    None => {
                        let id = new_id();
                        let mut payload = payload;
                        payload["hold_id"] = json!(id);
                        tx.emit(&me.address, "hold.acquired", payload)?;
                        tx.execute(
                            "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern,
                               exclusive, reason, ttl_seconds, created_at, renewed_at, expires_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                            params![
                                id,
                                tx.node_id,
                                project.project_id,
                                me.id,
                                pattern,
                                input.exclusive,
                                input.reason,
                                ttl,
                                tx.now,
                                expires_at
                            ],
                        )?;
                        id
                    }
                };
                granted_ids.push(hold_id);
            }
            touch(tx, &me.id)?;
            Ok(HoldReport {
                granted: holds_by_id(tx, &granted_ids)?,
                conflicts: Vec::new(),
            })
        })
    }

    /// Report which of `paths` would conflict for `agent` (no holds are taken).
    pub fn check_holds(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
        exclusive: bool,
    ) -> Result<Vec<HoldConflict>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let patterns = paths
                .iter()
                .map(|raw| Ok((raw.clone(), normalize_pattern(&project, raw)?)))
                .collect::<Result<Vec<_>>>()?;
            conflicts_for(tx, &project.project_id, &me.id, &patterns, exclusive)
        })
    }

    /// Extend the agent's live holds (all, or just `hold_ids`) by their TTL from now.
    pub fn renew_holds(&self, agent: &str, hold_ids: &[String]) -> Result<Vec<Hold>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, hold_ids)?;
            renew_tx(tx, &me.address, &owned)?;
            touch(tx, &me.id)?;
            let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
            holds_by_id(tx, &ids)
        })
    }

    /// Release the agent's live holds (all, or just `hold_ids`). Returns the released ids.
    pub fn release_holds(&self, agent: &str, hold_ids: &[String]) -> Result<Vec<String>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let ids: Vec<String> = owned_holds(tx, &me.id, hold_ids)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            release_tx(tx, &me.address, &ids, "released")?;
            touch(tx, &me.id)?;
            Ok(ids)
        })
    }

    /// Live holds, optionally filtered by project and/or agent.
    pub fn holds(&self, filter: HoldFilter) -> Result<Vec<Hold>> {
        self.sweep()?;
        self.read(|conn| {
            let project_id = match &filter.project {
                Some(p) => Some(find_project(conn, self.node_id(), p)?.project_id),
                None => None,
            };
            let agent_id = match &filter.agent {
                Some(a) => Some(resolve_agent(conn, self.node_id(), a)?.id),
                None => None,
            };
            live_holds(
                conn,
                "h.node_id = ?1 AND (?2 IS NULL OR h.project_id = ?2) AND (?3 IS NULL OR h.agent_id = ?3)",
                &[&self.node_id(), &project_id, &agent_id],
            )
        })
    }

    /// Session heartbeat: touch `last_seen_at` and renew the agent's holds. With `end`,
    /// release every hold instead and mark the agent offline.
    pub fn heartbeat(&self, agent: &str, end: bool) -> Result<HeartbeatReport> {
        let phase = if end {
            SessionPhase::End
        } else {
            SessionPhase::Beat
        };
        self.session(agent, phase, SessionInfo::default())
    }

    /// Record a session lifecycle step. `Start` and `End` append `session.started` /
    /// `session.ended` events; a plain `Beat` only updates presence and renews holds.
    pub fn session(
        &self,
        agent: &str,
        phase: SessionPhase,
        info: SessionInfo,
    ) -> Result<HeartbeatReport> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, &[])?;
            let session_payload = || {
                json!({
                    "agent_id": me.id,
                    "client": info.client,
                    "session_id": info.session_id,
                })
            };
            let (renewed, released, status) = if phase == SessionPhase::End {
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                release_tx(tx, &me.address, &ids, "session_end")?;
                tx.emit(&me.address, "session.ended", session_payload())?;
                tx.execute(
                    "UPDATE agents SET status = 'offline', last_seen_at = ?2 WHERE agent_id = ?1",
                    params![me.id, tx.now],
                )?;
                (Vec::new(), ids, "offline")
            } else {
                if phase == SessionPhase::Start {
                    tx.emit(&me.address, "session.started", session_payload())?;
                }
                renew_tx(tx, &me.address, &owned)?;
                touch(tx, &me.id)?;
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                (holds_by_id(tx, &ids)?, Vec::new(), "active")
            };
            let (unread, urgent_unread): (u32, u32) = tx.query_row(
                "SELECT count(*), count(*) FILTER (WHERE m.importance IN ('high', 'urgent'))
                 FROM deliveries d JOIN messages m ON m.message_id = d.message_id
                 WHERE d.agent_id = ?1 AND d.read_at IS NULL",
                [&me.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let open_requests = crate::requests::list_requests_for_inbox(tx, &me.id)?.len() as u32;
            Ok(HeartbeatReport {
                agent: me.address,
                status: status.to_string(),
                last_seen_at: fmt_ts(tx.now),
                renewed_holds: renewed,
                released_holds: released,
                unread,
                urgent_unread,
                open_requests,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_rules() {
        assert!(patterns_overlap("src/main.rs", "src/main.rs"));
        assert!(patterns_overlap("src", "src/main.rs"));
        assert!(!patterns_overlap("src/main.rs", "src/lib.rs"));
        assert!(!patterns_overlap("src", "srcx/a.rs"));
        assert!(patterns_overlap("src/*.rs", "src/main.rs"));
        assert!(!patterns_overlap("src/*.rs", "src/a/b.rs"));
        assert!(patterns_overlap("src/**", "src/a/b.rs"));
        assert!(patterns_overlap("src/**/*.rs", "src/a"));
        assert!(patterns_overlap("src/**/*.rs", "src"));
        assert!(patterns_overlap("src/**", "src/*.toml"));
        assert!(!patterns_overlap("docs/**", "src/**"));
        assert!(patterns_overlap("**", "anything"));
    }

    #[test]
    fn normalizes_patterns() {
        let project = Project {
            project_id: "p".into(),
            node_id: "n".into(),
            abs_path: "/work/repo".into(),
            slug: "repo".into(),
            created_at: String::new(),
        };
        assert_eq!(normalize_pattern(&project, "./src/").unwrap(), "src");
        assert_eq!(
            normalize_pattern(&project, "/work/repo/src/a.rs").unwrap(),
            "src/a.rs"
        );
        assert_eq!(normalize_pattern(&project, "/work/repo").unwrap(), "**");
        assert!(normalize_pattern(&project, "/work/repository/a").is_err());
        assert!(normalize_pattern(&project, "../x").is_err());
        assert!(normalize_pattern(&project, "src/[").is_err());
    }
}
