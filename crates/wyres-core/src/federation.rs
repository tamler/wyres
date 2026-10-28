//! Federation (v2): nodes exchange their append-only, signed event logs through a hub.
//!
//! - Each node is authoritative for its own agents, holds and resources; foreign events are
//!   projected into local read models but never rewrite another node's ownership.
//! - Spokes dial out to a self-hostable hub: they **push** events they authored and **pull**
//!   a filtered stream of everyone else's. The hub relays; spokes never talk to each other.
//! - Every event is signed by its origin node, so relayed events can be checked end to end.
//! - What a spoke may do is scoped by the permissions the hub granted at pairing:
//!   `read_feed`, `send_mail`, `borrow_resource`. There is no remote reap permission.

use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::crypto;
use crate::error::{Error, Result};
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};

/// What a paired node may do through the hub.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Perm {
    /// Receive the activity feed of other nodes.
    ReadFeed,
    /// Mail agents on other nodes.
    SendMail,
    /// Borrow resources owned by other nodes.
    BorrowResource,
}

impl Perm {
    pub const ALL: [Perm; 3] = [Perm::ReadFeed, Perm::SendMail, Perm::BorrowResource];

    pub fn as_str(self) -> &'static str {
        match self {
            Perm::ReadFeed => "read_feed",
            Perm::SendMail => "send_mail",
            Perm::BorrowResource => "borrow_resource",
        }
    }
}

impl std::str::FromStr for Perm {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Perm::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown permission {s:?}; expected read_feed, send_mail or borrow_resource \
                     (there is no remote reap permission)"
                ))
            })
    }
}

/// One event as it travels between nodes. `payload` is the exact signed text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireEvent {
    /// The sender's local sequence number (informational).
    pub seq: i64,
    pub event_id: String,
    pub node_id: String,
    pub ts: i64,
    pub actor: String,
    pub kind: String,
    pub payload: String,
    pub prev_id: Option<String>,
    pub sig: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NodeEntry {
    pub node_id: String,
    pub display_name: String,
    pub public_key: String,
}

/// Contents of an invite code (`wyres pair invite`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    pub v: u8,
    pub hub_url: String,
    pub hub: NodeEntry,
    pub invite_id: String,
    pub secret: String,
    pub perms: Vec<Perm>,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub invite_id: String,
    pub secret: String,
    pub node: NodeEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub hub: NodeEntry,
    pub perms: Vec<Perm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushRequest {
    pub events: Vec<WireEvent>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Rejected {
    pub event_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct IngestReport {
    pub accepted: usize,
    pub duplicates: usize,
    pub rejected: Vec<Rejected>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullResponse {
    pub events: Vec<WireEvent>,
    /// Hub sequence to pass as `after` next time.
    pub cursor: i64,
    pub more: bool,
    /// Every node the hub knows (including itself), for addressing and signature checks.
    pub nodes: Vec<NodeEntry>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Peer {
    pub node_id: String,
    pub display_name: String,
    /// `hub` (this node dials it) or `spoke` (it dials this node).
    pub role: String,
    pub url: Option<String>,
    /// For a spoke: what it may do here. For the hub: what it granted this node.
    pub perms: Vec<Perm>,
    pub fingerprint: String,
    pub paired_at: String,
    pub last_sync_at: Option<String>,
    pub revoked_at: Option<String>,
    pub push_cursor: i64,
    pub pull_cursor: i64,
}

/// How an event may travel.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Share {
    /// Stays on its node (security records, pairing, local-only mail).
    Never,
    /// Identity: node, project and agent registrations — sent to every peer.
    Directory,
    /// Activity: sent to peers with `read_feed`, plus these nodes regardless.
    Feed(Vec<String>),
    /// Mail: only to the nodes that host a recipient (or the message's origin).
    Mail(Vec<String>),
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn classify(kind: &str, payload: &Value, origin: &str) -> Share {
    let not_origin =
        |nodes: Vec<String>| -> Vec<String> { nodes.into_iter().filter(|n| n != origin).collect() };
    match kind.split('.').next().unwrap_or_default() {
        "token" | "peer" => return Share::Never,
        _ => {}
    }
    match kind {
        "node.created" | "node.renamed" | "project.ensured" | "agent.registered" => {
            Share::Directory
        }
        "message.sent" => {
            let nodes = not_origin(strings(payload, "recipient_nodes"));
            if nodes.is_empty() {
                Share::Never
            } else {
                Share::Mail(nodes)
            }
        }
        "delivery.read" | "delivery.acked" => {
            match payload.get("message_node").and_then(Value::as_str) {
                Some(n) if n != origin => Share::Mail(vec![n.to_string()]),
                _ => Share::Never,
            }
        }
        _ => {
            let mut also = strings(payload, "resource_nodes");
            if let Some(n) = payload.get("resource_node").and_then(Value::as_str) {
                also.push(n.to_string());
            }
            Share::Feed(not_origin(also))
        }
    }
}

fn perms_of(text: &str) -> Vec<Perm> {
    serde_json::from_str(text).unwrap_or_default()
}

fn validate_node_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 40
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid node name {name:?}: use 1-40 ASCII letters, digits, '-', '_' or '.'"
        )))
    }
}

fn local_entry(conn: &Connection) -> Result<NodeEntry> {
    Ok(conn.query_row(
        "SELECT node_id, display_name, public_key FROM nodes WHERE is_local = 1",
        [],
        |r| {
            Ok(NodeEntry {
                node_id: r.get(0)?,
                display_name: r.get(1)?,
                public_key: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        },
    )?)
}

fn require_named(entry: &NodeEntry) -> Result<()> {
    if entry.display_name == "local" {
        return Err(Error::Invalid(
            "give this node a unique name before pairing: `wyres node name <name>`".into(),
        ));
    }
    Ok(())
}

/// Record (or confirm) another node. Keys are pinned on first sight.
fn upsert_node(tx: &Tx<'_>, node: &NodeEntry) -> Result<()> {
    if node.node_id == tx.node_id {
        return Ok(());
    }
    crypto::decode_public(&node.public_key)?;
    let known: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT display_name, public_key FROM nodes WHERE node_id = ?1",
            [&node.node_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match known {
        Some((_, Some(key))) if key != node.public_key => Err(Error::Conflict(format!(
            "node {} presented a different key than before; refusing",
            node.node_id
        ))),
        Some((name, _)) => {
            if name != node.display_name {
                tx.execute(
                    "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                    params![node.node_id, node.display_name],
                )?;
            }
            tx.execute(
                "UPDATE nodes SET public_key = ?2 WHERE node_id = ?1 AND public_key IS NULL",
                params![node.node_id, node.public_key],
            )?;
            Ok(())
        }
        None => {
            tx.execute(
                "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
                 VALUES (?1, ?2, 'peer', 0, ?3, ?4)",
                params![node.node_id, node.display_name, tx.now, node.public_key],
            )?;
            Ok(())
        }
    }
}

fn wire_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WireEvent> {
    Ok(WireEvent {
        seq: r.get(0)?,
        event_id: r.get(1)?,
        node_id: r.get(2)?,
        ts: r.get(3)?,
        actor: r.get(4)?,
        kind: r.get(5)?,
        payload: r.get(6)?,
        prev_id: r.get(7)?,
        sig: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
    })
}

const WIRE_SELECT: &str =
    "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig FROM events";

/// (role, perms, revoked_at)
type PeerRow = (String, Vec<Perm>, Option<i64>);

fn peer_row(conn: &Connection, node_id: &str) -> Result<Option<PeerRow>> {
    Ok(conn
        .query_row(
            "SELECT role, perms, revoked_at FROM peers WHERE node_id = ?1",
            [node_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?)),
        )
        .optional()?
        .map(|(role, perms, revoked)| (role, perms_of(&perms), revoked)))
}

/// The hub this node syncs with, if it is a spoke: (node_id, url, granted perms).
pub(crate) fn hub_of(conn: &Connection) -> Result<Option<(String, String, Vec<Perm>)>> {
    Ok(conn
        .query_row(
            "SELECT node_id, url, perms FROM peers WHERE role = 'hub' AND revoked_at IS NULL LIMIT 1",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()?
        .map(|(n, u, p)| (n, u, perms_of(&p))))
}

/// Whether this node may act on `perm` towards `node` (another node).
pub(crate) fn allowed_towards(conn: &Connection, node: &str, perm: Perm) -> Result<()> {
    if let Some((_, _, granted)) = hub_of(conn)? {
        if granted.contains(&perm) {
            return Ok(());
        }
        return Err(Error::Forbidden(format!(
            "the hub did not grant this node {}",
            perm.as_str()
        )));
    }
    // A hub may act towards any node paired with it.
    match peer_row(conn, node)? {
        Some((role, _, None)) if role == "spoke" => Ok(()),
        _ => Err(Error::Forbidden(format!(
            "node {node} is not paired with this node"
        ))),
    }
}

/// TCP reachability of a foreign resource from this machine.
pub fn reachable(endpoint: Option<&str>, port: Option<u16>) -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    let host = endpoint
        .map(|e| e.split_once("://").map_or(e, |(_, rest)| rest))
        .map(|e| e.split('/').next().unwrap_or_default().to_string());
    let (host, port) = match host {
        Some(h) if h.starts_with('[') => {
            let (addr, rest) = h.split_once(']').unwrap_or((&h, ""));
            let p = rest.trim_start_matches(':').parse().ok().or(port);
            (addr.trim_start_matches('[').to_string(), p)
        }
        Some(h) => match h.rsplit_once(':') {
            Some((host, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (host.to_string(), p.parse().ok().or(port))
            }
            _ => (h, port),
        },
        None => return false,
    };
    let Some(port) = port else { return false };
    let Ok(addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, std::time::Duration::from_millis(500)).is_ok())
}

fn verify_wire(conn: &Connection, ev: &WireEvent) -> Result<()> {
    let key: Option<String> = conn
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            [&ev.node_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(key) = key else {
        return Err(Error::Forbidden(format!(
            "unknown origin node {}",
            ev.node_id
        )));
    };
    let msg = crypto::event_message(
        &ev.event_id,
        &ev.node_id,
        ev.ts,
        &ev.actor,
        &ev.kind,
        &ev.payload,
    );
    if crypto::verify(&key, &msg, &ev.sig) {
        Ok(())
    } else {
        Err(Error::Forbidden("bad event signature".into()))
    }
}

/// Permission checks the hub applies to events a spoke pushes.
fn spoke_may_publish(perms: &[Perm], from: &str, ev: &WireEvent, payload: &Value) -> Result<()> {
    if ev.node_id != from {
        return Err(Error::Forbidden(
            "spokes may only push events they authored".into(),
        ));
    }
    match classify(&ev.kind, payload, &ev.node_id) {
        Share::Never => Err(Error::Forbidden(format!(
            "{} events are not shared",
            ev.kind
        ))),
        Share::Mail(_) if ev.kind == "message.sent" && !perms.contains(&Perm::SendMail) => Err(
            Error::Forbidden("this node may not send mail to other nodes".into()),
        ),
        Share::Feed(others)
            if ev.kind == "resource.borrowed"
                && !others.is_empty()
                && !perms.contains(&Perm::BorrowResource) =>
        {
            Err(Error::Forbidden(
                "this node may not borrow resources of other nodes".into(),
            ))
        }
        _ => Ok(()),
    }
}

fn agent_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM agents WHERE agent_id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some())
}

fn project_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM projects WHERE project_id = ?1", [id], |_| {
            Ok(())
        })
        .optional()?
        .is_some())
}

/// Fold a foreign event into local read models. Idempotent; unknown kinds are only stored.
fn project_foreign(tx: &Tx<'_>, ev: &WireEvent, p: &Value) -> Result<()> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(str::to_string);
    let ts = ev.ts;
    let origin = ev.node_id.as_str();
    match ev.kind.as_str() {
        "node.created" | "node.renamed" => {
            if let Some(name) = s("display_name") {
                tx.execute(
                    "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                    params![origin, name],
                )?;
            }
        }
        "project.ensured" => {
            tx.execute(
                "INSERT OR IGNORE INTO projects (project_id, node_id, abs_path, slug, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![s("project_id"), origin, s("abs_path"), s("slug"), ts, ev.event_id],
            )?;
        }
        "agent.registered" => {
            let Some(agent_id) = s("agent_id") else {
                return Ok(());
            };
            let name = s("name").unwrap_or_default();
            let project_id = s("project_id").filter(|id| project_exists(tx, id).unwrap_or(false));
            let slug: Option<String> = match &project_id {
                Some(id) => tx
                    .query_row(
                        "SELECT slug FROM projects WHERE project_id = ?1",
                        [id],
                        |r| r.get(0),
                    )
                    .optional()?,
                None => None,
            };
            let node_name: String = tx.query_row(
                "SELECT display_name FROM nodes WHERE node_id = ?1",
                [origin],
                |r| r.get(0),
            )?;
            let address = format!("{name}@{}@{node_name}", slug.unwrap_or_default());
            tx.execute(
                "INSERT INTO agents (agent_id, node_id, project_id, name, program, model, description,
                   status, created_at, updated_at, last_seen_at, event_id, address)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?8, ?8, ?9, ?10)
                 ON CONFLICT(agent_id) DO UPDATE SET program = excluded.program,
                   model = excluded.model, description = excluded.description,
                   updated_at = excluded.updated_at, last_seen_at = excluded.last_seen_at,
                   status = 'active', address = excluded.address",
                params![
                    agent_id,
                    origin,
                    project_id,
                    name,
                    s("program").unwrap_or_else(|| "other".into()),
                    s("model"),
                    s("description"),
                    ts,
                    ev.event_id,
                    address
                ],
            )?;
        }
        "session.started" | "session.ended" => {
            let status = if ev.kind == "session.started" {
                "active"
            } else {
                "offline"
            };
            tx.execute(
                "UPDATE agents SET status = ?2, last_seen_at = ?3 WHERE agent_id = ?1",
                params![s("agent_id"), status, ts],
            )?;
        }
        "message.sent" => {
            let (Some(message_id), Some(sender)) = (s("message_id"), s("sender_id")) else {
                return Ok(());
            };
            if !agent_exists(tx, &sender)? {
                return Err(Error::NotFound(format!("unknown sender agent {sender}")));
            }
            let reply_to = s("reply_to").filter(|id| {
                tx.query_row("SELECT 1 FROM messages WHERE message_id = ?1", [id], |_| {
                    Ok(())
                })
                .optional()
                .ok()
                .flatten()
                .is_some()
            });
            tx.execute(
                "INSERT OR IGNORE INTO messages (message_id, node_id, thread_id, reply_to, sender_id,
                   subject, body, importance, ack_required, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    message_id,
                    origin,
                    s("thread_id").unwrap_or_else(|| message_id.clone()),
                    reply_to,
                    sender,
                    s("subject").unwrap_or_default(),
                    s("body").unwrap_or_default(),
                    s("importance").unwrap_or_else(|| "normal".into()),
                    p.get("ack_required").and_then(Value::as_bool).unwrap_or(false),
                    ts,
                    ev.event_id
                ],
            )?;
            for r in p
                .get("recipients")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let agent = r
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !agent_exists(tx, agent)? {
                    continue;
                }
                tx.execute(
                    "INSERT OR IGNORE INTO deliveries (message_id, agent_id, node_id, role)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        message_id,
                        agent,
                        r.get("node_id").and_then(Value::as_str).unwrap_or(origin),
                        r.get("role").and_then(Value::as_str).unwrap_or("to")
                    ],
                )?;
            }
        }
        "delivery.read" | "delivery.acked" => {
            let acked = ev.kind == "delivery.acked";
            tx.execute(
                "UPDATE deliveries SET read_at = coalesce(read_at, ?3),
                   acked_at = CASE WHEN ?4 THEN coalesce(acked_at, ?3) ELSE acked_at END
                 WHERE message_id = ?1 AND agent_id = ?2",
                params![s("message_id"), s("agent_id"), ts, acked],
            )?;
        }
        "hold.acquired" | "hold.updated" => {
            let (Some(hold_id), Some(project), Some(agent)) =
                (s("hold_id"), s("project_id"), s("agent_id"))
            else {
                return Ok(());
            };
            if !project_exists(tx, &project)? || !agent_exists(tx, &agent)? {
                return Ok(());
            }
            let ttl = p.get("ttl_seconds").and_then(Value::as_i64).unwrap_or(1800);
            let expires = p
                .get("expires_at")
                .and_then(Value::as_i64)
                .unwrap_or(ts + ttl * 1000);
            let exclusive = p.get("exclusive").and_then(Value::as_bool).unwrap_or(true);
            tx.execute(
                "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern, exclusive, reason,
                   ttl_seconds, created_at, renewed_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)
                 ON CONFLICT(hold_id) DO UPDATE SET exclusive = excluded.exclusive,
                   reason = excluded.reason, ttl_seconds = excluded.ttl_seconds,
                   renewed_at = excluded.renewed_at, expires_at = excluded.expires_at",
                params![hold_id, origin, project, agent, s("pattern"), exclusive, s("reason"), ttl, ts, expires],
            )?;
        }
        "hold.renewed" => {
            for id in strings(p, "hold_ids") {
                tx.execute(
                    "UPDATE holds SET renewed_at = ?2, expires_at = ?2 + ttl_seconds * 1000
                     WHERE hold_id = ?1 AND node_id = ?3 AND released_at IS NULL",
                    params![id, ts, origin],
                )?;
            }
        }
        "hold.released" => {
            for id in strings(p, "hold_ids") {
                tx.execute(
                    "UPDATE holds SET released_at = ?2, release_reason = ?3
                     WHERE hold_id = ?1 AND node_id = ?4 AND released_at IS NULL",
                    params![id, ts, s("reason"), origin],
                )?;
            }
        }
        "hold.expired" => {
            tx.execute(
                "UPDATE holds SET released_at = expires_at, release_reason = 'expired'
                 WHERE hold_id = ?1 AND node_id = ?2 AND released_at IS NULL",
                params![s("hold_id"), origin],
            )?;
        }
        "resource.registered" => {
            let Some(rid) = s("resource_id") else {
                return Ok(());
            };
            let project = s("project_id").filter(|id| project_exists(tx, id).unwrap_or(false));
            tx.execute(
                "INSERT OR IGNORE INTO resources (resource_id, node_id, kind, name, endpoint, port, pid,
                   project_id, purpose, mode, metadata, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    rid,
                    origin,
                    s("kind"),
                    s("name"),
                    s("endpoint"),
                    p.get("port").and_then(Value::as_i64),
                    p.get("pid").and_then(Value::as_i64),
                    project,
                    s("purpose"),
                    s("mode").unwrap_or_else(|| "shared".into()),
                    p.get("metadata").cloned().unwrap_or(json!({})).to_string(),
                    ts,
                    ev.event_id
                ],
            )?;
            foreign_lease(
                tx,
                &rid,
                s("lease_id"),
                s("owner_id"),
                "owner",
                p,
                ts,
                origin,
            )?;
        }
        "resource.adopted" => {
            let Some(rid) = s("resource_id") else {
                return Ok(());
            };
            foreign_lease(
                tx,
                &rid,
                s("lease_id"),
                s("agent_id"),
                "owner",
                p,
                ts,
                origin,
            )?;
        }
        "resource.borrowed" => {
            let Some(rid) = s("resource_id") else {
                return Ok(());
            };
            // The owning node decides: no borrowers on exclusive or released resources.
            let ok: Option<(String, Option<i64>)> = tx
                .query_row(
                    "SELECT mode, released_at FROM resources WHERE resource_id = ?1",
                    [&rid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if matches!(ok, Some((ref mode, None)) if mode == "shared") {
                foreign_lease(
                    tx,
                    &rid,
                    s("lease_id"),
                    s("agent_id"),
                    "borrower",
                    p,
                    ts,
                    origin,
                )?;
            }
        }
        "resource.renewed" => {
            for id in strings(p, "lease_ids") {
                tx.execute(
                    "UPDATE resource_leases SET renewed_at = ?2,
                       ttl_seconds = coalesce(?4, ttl_seconds),
                       expires_at = ?2 + coalesce(?4, ttl_seconds) * 1000
                     WHERE lease_id = ?1 AND node_id = ?3 AND released_at IS NULL",
                    params![id, ts, origin, p.get("ttl_seconds").and_then(Value::as_i64)],
                )?;
            }
        }
        "resource.released" => {
            for id in strings(p, "lease_ids") {
                tx.execute(
                    "UPDATE resource_leases SET released_at = ?2, release_reason = ?3
                     WHERE lease_id = ?1 AND node_id = ?4 AND released_at IS NULL",
                    params![
                        id,
                        ts,
                        s("reason").unwrap_or_else(|| "released".into()),
                        origin
                    ],
                )?;
            }
        }
        "resource.lease_expired" => {
            tx.execute(
                "UPDATE resource_leases SET released_at = ?2, release_reason = 'expired'
                 WHERE lease_id = ?1 AND node_id = ?3 AND released_at IS NULL",
                params![s("lease_id"), ts, origin],
            )?;
        }
        "resource.transferred" => {
            tx.execute(
                "UPDATE resource_leases SET role = 'owner' WHERE lease_id = ?1",
                params![s("lease_id")],
            )?;
        }
        "resource.stopped" | "resource.reaped" => {
            // Only the owning node's word ends a resource.
            if let Some(rid) = s("resource_id") {
                tx.execute(
                    "UPDATE resources SET released_at = ?2, release_reason = ?3
                     WHERE resource_id = ?1 AND node_id = ?4 AND released_at IS NULL",
                    params![rid, ts, ev.kind.trim_start_matches("resource."), origin],
                )?;
                tx.execute(
                    "UPDATE resource_leases SET released_at = ?2, release_reason = 'stopped'
                     WHERE resource_id = ?1 AND released_at IS NULL",
                    params![rid, ts],
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn foreign_lease(
    tx: &Tx<'_>,
    resource_id: &str,
    lease_id: Option<String>,
    agent_id: Option<String>,
    role: &str,
    p: &Value,
    ts: i64,
    origin: &str,
) -> Result<()> {
    let (Some(lease_id), Some(agent_id)) = (lease_id, agent_id) else {
        return Ok(());
    };
    if !agent_exists(tx, &agent_id)? {
        return Ok(());
    }
    let exists = tx
        .query_row(
            "SELECT 1 FROM resources WHERE resource_id = ?1",
            [resource_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(());
    }
    let ttl = p.get("ttl_seconds").and_then(Value::as_i64).unwrap_or(1800);
    tx.execute(
        "INSERT OR IGNORE INTO resource_leases (lease_id, resource_id, node_id, agent_id, role,
           ttl_seconds, created_at, renewed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![
            lease_id,
            resource_id,
            origin,
            agent_id,
            role,
            ttl,
            ts,
            ts + ttl * 1000
        ],
    )?;
    tx.execute(
        "UPDATE resources SET lapsed_at = NULL WHERE resource_id = ?1",
        [resource_id],
    )?;
    Ok(())
}

fn peer_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Peer> {
    let perms: String = r.get(4)?;
    let key: Option<String> = r.get(5)?;
    Ok(Peer {
        node_id: r.get(0)?,
        display_name: r.get(1)?,
        role: r.get(2)?,
        url: r.get(3)?,
        perms: perms_of(&perms),
        fingerprint: crypto::fingerprint(&key.unwrap_or_default()),
        paired_at: fmt_ts(r.get(6)?),
        last_sync_at: fmt_opt(r.get(7)?),
        revoked_at: fmt_opt(r.get(8)?),
        push_cursor: r.get(9)?,
        pull_cursor: r.get(10)?,
    })
}

const PEER_SELECT: &str = "SELECT p.node_id, n.display_name, p.role, p.url, p.perms, n.public_key,
       p.paired_at, p.last_sync_at, p.revoked_at, p.push_cursor, p.pull_cursor
     FROM peers p JOIN nodes n ON n.node_id = p.node_id";

impl Store {
    /// Rename this node (do it before pairing: other nodes address agents by node name).
    pub fn set_node_name(&self, name: &str) -> Result<NodeEntry> {
        validate_node_name(name)?;
        self.write(|tx| {
            let taken = tx
                .query_row(
                    "SELECT 1 FROM nodes WHERE display_name = ?1 AND is_local = 0",
                    [name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if taken {
                return Err(Error::Conflict(format!(
                    "another known node is already named {name:?}"
                )));
            }
            tx.execute(
                "UPDATE nodes SET display_name = ?2 WHERE node_id = ?1",
                params![tx.node_id, name],
            )?;
            tx.emit(
                "system",
                "node.renamed",
                json!({ "node_id": tx.node_id, "display_name": name }),
            )?;
            local_entry(tx)
        })
    }

    pub fn node_entry(&self) -> Result<NodeEntry> {
        self.read(local_entry)
    }

    /// Sign a node-to-node HTTP request.
    pub fn sign_request(
        &self,
        method: &str,
        path_and_query: &str,
        time_ms: i64,
        body: &[u8],
    ) -> String {
        crypto::sign(
            self.signing_key(),
            &crypto::request_message(method, path_and_query, time_ms, body),
        )
    }

    /// Hub side: check a signed request from a paired node. Returns its granted perms.
    pub fn verify_peer_request(
        &self,
        node_id: &str,
        method: &str,
        path_and_query: &str,
        time_ms: i64,
        body: &[u8],
        signature: &str,
    ) -> Result<Vec<Perm>> {
        let denied = || Error::Forbidden("request not signed by a paired node".into());
        if (self.now() - time_ms).abs() > 5 * 60 * 1000 {
            return Err(Error::Forbidden(
                "request time is more than 5 minutes off; check the clocks".into(),
            ));
        }
        self.read(|conn| {
            let row: Option<(String, Option<i64>, Option<String>)> = conn
                .query_row(
                    "SELECT p.perms, p.revoked_at, n.public_key FROM peers p
                     JOIN nodes n ON n.node_id = p.node_id
                     WHERE p.node_id = ?1 AND p.role = 'spoke'",
                    [node_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((perms, None, Some(key))) = row else {
                return Err(denied());
            };
            let msg = crypto::request_message(method, path_and_query, time_ms, body);
            if crypto::verify(&key, &msg, signature) {
                Ok(perms_of(&perms))
            } else {
                Err(denied())
            }
        })
    }

    /// Hub side: create a single-use invite code for a spoke.
    pub fn create_invite(
        &self,
        hub_url: &str,
        perms: &[Perm],
        expires_in_seconds: i64,
    ) -> Result<String> {
        if !(hub_url.starts_with("http://") || hub_url.starts_with("https://")) {
            return Err(Error::Invalid(
                "hub url must start with http:// or https://".into(),
            ));
        }
        if expires_in_seconds <= 0 {
            return Err(Error::Invalid("invite expiry must be positive".into()));
        }
        let mut secret = [0u8; 24];
        getrandom::fill(&mut secret)
            .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
        let secret = crypto::b64(&secret);
        let perms: Vec<Perm> = perms
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.write(|tx| {
            let hub = local_entry(tx)?;
            require_named(&hub)?;
            let invite_id = new_id();
            let expires_at = tx.now + expires_in_seconds * 1000;
            tx.execute(
                "INSERT INTO invites (invite_id, secret_hash, perms, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    invite_id,
                    crypto::sha256_hex(secret.as_bytes()),
                    serde_json::to_string(&perms)?,
                    tx.now,
                    expires_at
                ],
            )?;
            let invite = Invite {
                v: 1,
                hub_url: hub_url.trim_end_matches('/').to_string(),
                hub,
                invite_id,
                secret,
                perms,
                expires_at,
            };
            Ok(format!(
                "wyres-invite:{}",
                crypto::b64(serde_json::to_string(&invite)?.as_bytes())
            ))
        })
    }

    /// Spoke side: decode an invite and build the pairing request for it.
    pub fn prepare_join(&self, code: &str) -> Result<(Invite, PairRequest)> {
        let raw = code.trim().strip_prefix("wyres-invite:").ok_or_else(|| {
            Error::Invalid("not a wyres invite code (expected wyres-invite:...)".into())
        })?;
        let invite: Invite = serde_json::from_slice(&crypto::unb64(raw)?)
            .map_err(|e| Error::Invalid(format!("invalid invite code: {e}")))?;
        if invite.expires_at <= self.now() {
            return Err(Error::Invalid(
                "this invite has expired; ask for a new one".into(),
            ));
        }
        let me = self.node_entry()?;
        require_named(&me)?;
        if me.display_name == invite.hub.display_name {
            return Err(Error::Conflict(format!(
                "the hub is also named {:?}; rename this node first",
                me.display_name
            )));
        }
        let req = PairRequest {
            invite_id: invite.invite_id.clone(),
            secret: invite.secret.clone(),
            node: me,
        };
        Ok((invite, req))
    }

    /// Hub side: accept a pairing request carrying a valid, unused invite.
    pub fn accept_pairing(&self, req: &PairRequest) -> Result<PairResponse> {
        validate_node_name(&req.node.display_name)?;
        crypto::decode_public(&req.node.public_key)?;
        self.write(|tx| {
            let denied = || Error::Forbidden("invalid, used or expired invite".into());
            let row: Option<(String, String, i64, Option<i64>)> = tx
                .query_row(
                    "SELECT secret_hash, perms, expires_at, used_at FROM invites WHERE invite_id = ?1",
                    [&req.invite_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((hash, perms, expires_at, None)) = row else {
                return Err(denied());
            };
            if hash != crypto::sha256_hex(req.secret.as_bytes()) || expires_at <= tx.now {
                return Err(denied());
            }
            let hub = local_entry(tx)?;
            let clash = tx
                .query_row(
                    "SELECT 1 FROM nodes WHERE display_name = ?1 AND node_id != ?2",
                    params![req.node.display_name, req.node.node_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if clash || req.node.node_id == hub.node_id {
                return Err(Error::Conflict(format!(
                    "a node named {:?} is already known; rename the joining node",
                    req.node.display_name
                )));
            }
            upsert_node(tx, &req.node)?;
            tx.execute(
                "INSERT INTO peers (node_id, role, perms, paired_at) VALUES (?1, 'spoke', ?2, ?3)
                 ON CONFLICT(node_id) DO UPDATE SET perms = excluded.perms,
                   paired_at = excluded.paired_at, revoked_at = NULL",
                params![req.node.node_id, perms, tx.now],
            )?;
            tx.execute(
                "UPDATE invites SET used_at = ?2, used_by = ?3 WHERE invite_id = ?1",
                params![req.invite_id, tx.now, req.node.node_id],
            )?;
            tx.emit(
                "human (wyres pair)",
                "peer.paired",
                json!({ "node_id": req.node.node_id, "display_name": req.node.display_name, "role": "spoke", "perms": perms_of(&perms) }),
            )?;
            Ok(PairResponse {
                hub,
                perms: perms_of(&perms),
            })
        })
    }

    /// Spoke side: record the hub after it accepted the pairing.
    pub fn complete_join(&self, invite: &Invite, resp: &PairResponse) -> Result<Peer> {
        if resp.hub.node_id != invite.hub.node_id || resp.hub.public_key != invite.hub.public_key {
            return Err(Error::Forbidden(
                "the hub answered with a different identity than the invite".into(),
            ));
        }
        self.write(|tx| {
            if let Some((existing, _, _)) = hub_of(tx)? {
                if existing != resp.hub.node_id {
                    return Err(Error::Conflict(
                        "this node already syncs with another hub; unpair it first".into(),
                    ));
                }
            }
            upsert_node(tx, &resp.hub)?;
            tx.execute(
                "INSERT INTO peers (node_id, role, url, perms, paired_at) VALUES (?1, 'hub', ?2, ?3, ?4)
                 ON CONFLICT(node_id) DO UPDATE SET url = excluded.url, perms = excluded.perms,
                   revoked_at = NULL",
                params![resp.hub.node_id, invite.hub_url, serde_json::to_string(&resp.perms)?, tx.now],
            )?;
            tx.emit(
                "human (wyres pair)",
                "peer.paired",
                json!({ "node_id": resp.hub.node_id, "display_name": resp.hub.display_name, "role": "hub", "perms": resp.perms }),
            )?;
            Ok(tx.query_row(
                &format!("{PEER_SELECT} WHERE p.node_id = ?1"),
                [&resp.hub.node_id],
                peer_from_row,
            )?)
        })
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!("{PEER_SELECT} ORDER BY p.paired_at"))?;
            let rows = stmt.query_map([], peer_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// The hub this node syncs with: (node_id, url).
    pub fn hub(&self) -> Result<Option<(String, String)>> {
        self.read(|conn| Ok(hub_of(conn)?.map(|(n, u, _)| (n, u))))
    }

    /// Stop accepting requests from (or syncing with) a peer.
    pub fn revoke_peer(&self, node: &str) -> Result<Peer> {
        self.write(|tx| {
            let id: String = tx
                .query_row(
                    "SELECT p.node_id FROM peers p JOIN nodes n ON n.node_id = p.node_id
                     WHERE p.node_id = ?1 OR n.display_name = ?1",
                    [node],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("no paired node {node:?}")))?;
            tx.execute(
                "UPDATE peers SET revoked_at = ?2 WHERE node_id = ?1",
                params![id, tx.now],
            )?;
            tx.emit(
                "human (wyres pair revoke)",
                "peer.revoked",
                json!({ "node_id": id }),
            )?;
            Ok(tx.query_row(
                &format!("{PEER_SELECT} WHERE p.node_id = ?1"),
                [&id],
                peer_from_row,
            )?)
        })
    }

    /// Spoke side: events this node authored that may leave it, after `after_seq`.
    /// Returns them and the highest sequence scanned (to advance the cursor past skips).
    pub fn outbound_events(&self, after_seq: i64, limit: u32) -> Result<(Vec<WireEvent>, i64)> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{WIRE_SELECT} WHERE node_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(params![self.node_id(), after_seq, limit], wire_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let high = rows.last().map_or(after_seq, |e| e.seq);
            let out = rows
                .into_iter()
                .filter(|e| {
                    let p: Value = serde_json::from_str(&e.payload).unwrap_or(Value::Null);
                    classify(&e.kind, &p, &e.node_id) != Share::Never
                })
                .collect();
            Ok((out, high))
        })
    }

    pub fn set_push_cursor(&self, peer: &str, seq: i64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET push_cursor = max(push_cursor, ?2), last_sync_at = ?3 WHERE node_id = ?1",
                params![peer, seq, tx.now],
            )?;
            Ok(())
        })
    }

    pub fn set_pull_cursor(&self, peer: &str, seq: i64) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "UPDATE peers SET pull_cursor = max(pull_cursor, ?2), last_sync_at = ?3 WHERE node_id = ?1",
                params![peer, seq, tx.now],
            )?;
            Ok(())
        })
    }

    pub fn push_cursor(&self, peer: &str) -> Result<(i64, i64)> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT push_cursor, pull_cursor FROM peers WHERE node_id = ?1",
                [peer],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }

    /// Hub side: what a spoke receives after `after_seq` (its own events excluded).
    pub fn pull_for(&self, peer: &str, after_seq: i64, limit: u32) -> Result<PullResponse> {
        self.read(|conn| {
            let Some((role, perms, None)) = peer_row(conn, peer)? else {
                return Err(Error::Forbidden("not a paired node".into()));
            };
            if role != "spoke" {
                return Err(Error::Forbidden("only spokes pull from a hub".into()));
            }
            let scan = limit.saturating_mul(4).max(limit);
            let mut stmt = conn.prepare(&format!(
                "{WIRE_SELECT} WHERE seq > ?1 AND node_id != ?2 ORDER BY seq LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(params![after_seq, peer, scan], wire_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let more = rows.len() as u32 == scan;
            let mut cursor = after_seq;
            let mut events = Vec::new();
            for e in rows {
                cursor = e.seq;
                let p: Value = serde_json::from_str(&e.payload).unwrap_or(Value::Null);
                let send = match classify(&e.kind, &p, &e.node_id) {
                    Share::Never => false,
                    Share::Directory => true,
                    Share::Feed(also) => {
                        perms.contains(&Perm::ReadFeed) || also.iter().any(|n| n == peer)
                    }
                    Share::Mail(nodes) => nodes.iter().any(|n| n == peer),
                };
                if send {
                    events.push(e);
                    if events.len() as u32 >= limit {
                        break;
                    }
                }
            }
            let more = more || events.len() as u32 >= limit;
            let mut stmt = conn.prepare(
                "SELECT node_id, display_name, public_key FROM nodes WHERE public_key IS NOT NULL",
            )?;
            let nodes = stmt
                .query_map([], |r| {
                    Ok(NodeEntry {
                        node_id: r.get(0)?,
                        display_name: r.get(1)?,
                        public_key: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(PullResponse {
                events,
                cursor,
                more,
                nodes,
            })
        })
    }

    /// Store and project events received from `from` (a spoke pushing, or the hub relaying).
    /// Each event is verified against its origin node's key; duplicates are skipped.
    pub fn ingest(
        &self,
        from: &str,
        events: &[WireEvent],
        nodes: &[NodeEntry],
    ) -> Result<IngestReport> {
        self.write(|tx| {
            let Some((role, perms, None)) = peer_row(tx, from)? else {
                return Err(Error::Forbidden("not a paired node".into()));
            };
            for n in nodes {
                upsert_node(tx, n)?;
            }
            let mut report = IngestReport::default();
            for ev in events {
                let reject = |reason: String| Rejected {
                    event_id: ev.event_id.clone(),
                    reason,
                };
                if ev.node_id == tx.node_id {
                    report.rejected.push(reject("events about this node come only from this node".into()));
                    continue;
                }
                let payload: Value = match serde_json::from_str(&ev.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        report.rejected.push(reject(format!("invalid payload: {e}")));
                        continue;
                    }
                };
                if role == "spoke" {
                    if let Err(e) = spoke_may_publish(&perms, from, ev, &payload) {
                        report.rejected.push(reject(e.to_string()));
                        continue;
                    }
                }
                if let Err(e) = verify_wire(tx, ev) {
                    report.rejected.push(reject(e.to_string()));
                    continue;
                }
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![ev.event_id, ev.node_id, ev.ts, ev.actor, ev.kind, ev.payload, ev.prev_id, ev.sig],
                )?;
                if inserted == 0 {
                    report.duplicates += 1;
                    continue;
                }
                if let Err(e) = project_foreign(tx, ev, &payload) {
                    tracing::warn!(event = %ev.event_id, kind = %ev.kind, error = %e, "could not project foreign event");
                }
                report.accepted += 1;
            }
            Ok(report)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        let o = "A";
        assert_eq!(classify("token.created", &json!({}), o), Share::Never);
        assert_eq!(
            classify("agent.registered", &json!({}), o),
            Share::Directory
        );
        assert_eq!(
            classify("message.sent", &json!({ "recipient_nodes": ["A"] }), o),
            Share::Never,
            "local-only mail stays local"
        );
        assert_eq!(
            classify("message.sent", &json!({ "recipient_nodes": ["A", "B"] }), o),
            Share::Mail(vec!["B".into()])
        );
        assert_eq!(
            classify("delivery.acked", &json!({ "message_node": "C" }), o),
            Share::Mail(vec!["C".into()])
        );
        assert_eq!(
            classify("hold.acquired", &json!({}), o),
            Share::Feed(vec![])
        );
        assert_eq!(
            classify("resource.borrowed", &json!({ "resource_node": "B" }), o),
            Share::Feed(vec!["B".into()])
        );
    }

    #[test]
    fn reachability() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert!(reachable(Some(&format!("http://127.0.0.1:{port}/x")), None));
        assert!(reachable(Some("127.0.0.1"), Some(port)));
        drop(l);
        assert!(!reachable(None, Some(port)));
        assert!(!reachable(Some("http://127.0.0.1:1"), None));
    }
}
