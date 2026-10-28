//! Resource registry: model servers, dev servers, ports, GPUs, containers and long jobs,
//! each with an owner, a purpose, and leases (owner or borrower) that expire unless
//! renewed. "Find before start": agents borrow a running resource instead of spawning a
//! duplicate. A resource whose leases have all lapsed stays listed as an orphan candidate
//! until it is re-adopted, released as stopped, or reaped by a human.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::identity::{address, find_project, resolve_agent, AgentRef};
use crate::mail::touch;
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};

pub const DEFAULT_LEASE_TTL_SECONDS: i64 = 30 * 60;
pub const MIN_LEASE_TTL_SECONDS: i64 = 60;
pub const MAX_LEASE_TTL_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    ModelServer,
    DevServer,
    Port,
    Gpu,
    Container,
    LongJob,
}

impl ResourceKind {
    pub const ALL: [ResourceKind; 6] = [
        ResourceKind::ModelServer,
        ResourceKind::DevServer,
        ResourceKind::Port,
        ResourceKind::Gpu,
        ResourceKind::Container,
        ResourceKind::LongJob,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::ModelServer => "model_server",
            ResourceKind::DevServer => "dev_server",
            ResourceKind::Port => "port",
            ResourceKind::Gpu => "gpu",
            ResourceKind::Container => "container",
            ResourceKind::LongJob => "long_job",
        }
    }
}

impl std::str::FromStr for ResourceKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        ResourceKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown resource kind {s:?}; expected model_server, dev_server, port, gpu, container or long_job"
                ))
            })
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    /// Only the owner uses it; borrowing is refused.
    Exclusive,
    /// Others may borrow it (reference counted).
    #[default]
    Shared,
}

impl LeaseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseMode::Exclusive => "exclusive",
            LeaseMode::Shared => "shared",
        }
    }
}

/// Input for [`Store::register_resource`].
#[derive(Debug, Clone)]
pub struct RegisterResource {
    pub agent: String,
    pub kind: ResourceKind,
    /// Identity on this node, e.g. `ollama@11434`, `vite@5173`, `tcp:127.0.0.1:5432`, `metal:0`.
    pub name: String,
    /// How to reach it, e.g. `http://127.0.0.1:11434`.
    pub endpoint: Option<String>,
    /// Listening port, if any (derived from name/endpoint when omitted).
    pub port: Option<u16>,
    /// Process id, if known (needed for orphan matching and reaping).
    pub pid: Option<u32>,
    /// Absolute project path or slug.
    pub project: Option<String>,
    pub purpose: Option<String>,
    pub mode: LeaseMode,
    pub ttl_seconds: Option<i64>,
    /// Free-form details: model, backend (`metal|mlx|cpu|cuda`), estimated_bytes, cmdline, ...
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Lease {
    pub lease_id: String,
    pub agent: String,
    /// `owner` or `borrower`.
    pub role: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Resource {
    pub resource_id: String,
    pub node_id: String,
    pub kind: ResourceKind,
    pub name: String,
    pub endpoint: Option<String>,
    pub port: Option<u16>,
    pub pid: Option<u32>,
    pub project: Option<String>,
    pub purpose: Option<String>,
    pub mode: LeaseMode,
    pub owner: Option<String>,
    /// Live leases (owner and borrowers).
    pub leases: Vec<Lease>,
    pub refcount: u32,
    /// `live` (has a live lease), `lapsed` (no live lease: orphan candidate) or `released`.
    pub status: String,
    /// When the last lease ended, for lapsed resources.
    pub lapsed_since: Option<String>,
    pub metadata: Value,
    pub created_at: String,
}

#[derive(Debug, Clone, Default)]
pub struct ResourceFilter {
    pub kind: Option<ResourceKind>,
    /// Case-insensitive substring of name, endpoint or purpose.
    pub query: Option<String>,
    pub project: Option<String>,
    /// Include released resources too.
    pub include_released: bool,
}

/// Which resource an operation targets: an id, or `kind` + `name`.
#[derive(Debug, Clone)]
pub enum ResourceRef {
    Id(String),
    Name(ResourceKind, String),
}

impl ResourceRef {
    /// Parse a resource id, or `kind:name` such as `model_server:ollama@11434`.
    pub fn parse(raw: &str) -> Result<ResourceRef> {
        match raw.split_once(':') {
            Some((kind, name)) if kind.parse::<ResourceKind>().is_ok() && !name.is_empty() => {
                Ok(ResourceRef::Name(kind.parse()?, name.to_string()))
            }
            _ => Ok(ResourceRef::Id(raw.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LeaseOutcome {
    pub resource: Resource,
    /// The caller's lease after the operation (none after a release).
    pub lease: Option<Lease>,
    /// What happened: registered, adopted, borrowed, renewed, released, transferred, stopped.
    pub outcome: String,
}

fn clamp_ttl(ttl: Option<i64>) -> Result<i64> {
    let ttl = ttl.unwrap_or(DEFAULT_LEASE_TTL_SECONDS);
    if ttl <= 0 {
        return Err(Error::Invalid("ttl_seconds must be positive".into()));
    }
    Ok(ttl.clamp(MIN_LEASE_TTL_SECONDS, MAX_LEASE_TTL_SECONDS))
}

/// The trailing port in `ollama@11434`, `tcp:0.0.0.0:5432` or `http://127.0.0.1:5173/x`.
pub fn derive_port(text: &str) -> Option<u16> {
    let host = text
        .split_once("://")
        .map_or(text, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    host.rsplit([':', '@'])
        .next()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .and_then(|p| p.parse().ok())
        .filter(|p| *p > 0)
}

fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "resource name must be 1-200 printable characters".into(),
        ));
    }
    Ok(())
}

const RESOURCE_COLUMNS: &str =
    "r.resource_id, r.node_id, r.kind, r.name, r.endpoint, r.port, r.pid,
    p.slug, r.purpose, r.mode, r.metadata, r.created_at, r.released_at, r.lapsed_at";

struct Row0 {
    resource: Resource,
    released: bool,
    lapsed_at: Option<i64>,
}

fn resource_from_row(r: &Row<'_>) -> rusqlite::Result<Row0> {
    let kind: String = r.get(2)?;
    let mode: String = r.get(9)?;
    let metadata: String = r.get(10)?;
    let released_at: Option<i64> = r.get(12)?;
    let lapsed_at: Option<i64> = r.get(13)?;
    Ok(Row0 {
        resource: Resource {
            resource_id: r.get(0)?,
            node_id: r.get(1)?,
            kind: kind.parse().unwrap_or(ResourceKind::LongJob),
            name: r.get(3)?,
            endpoint: r.get(4)?,
            port: r.get(5)?,
            pid: r.get(6)?,
            project: r.get(7)?,
            purpose: r.get(8)?,
            mode: if mode == "exclusive" {
                LeaseMode::Exclusive
            } else {
                LeaseMode::Shared
            },
            owner: None,
            leases: Vec::new(),
            refcount: 0,
            status: String::new(),
            lapsed_since: None,
            metadata: serde_json::from_str(&metadata).unwrap_or(Value::Null),
            created_at: fmt_ts(r.get(11)?),
        },
        released: released_at.is_some(),
        lapsed_at,
    })
}

fn live_leases(
    conn: &Connection,
    resource_id: &str,
) -> Result<Vec<(String, String, String, Lease)>> {
    let mut stmt = conn.prepare(
        "SELECT l.lease_id, l.agent_id, a.name, p.slug, l.role, l.expires_at
         FROM resource_leases l
         JOIN agents a ON a.agent_id = l.agent_id
         LEFT JOIN projects p ON p.project_id = a.project_id
         WHERE l.resource_id = ?1 AND l.released_at IS NULL
         ORDER BY CASE l.role WHEN 'owner' THEN 0 ELSE 1 END, l.created_at, l.rowid",
    )?;
    let rows = stmt.query_map([resource_id], |r| {
        let lease_id: String = r.get(0)?;
        let agent_id: String = r.get(1)?;
        let addr = address(
            &r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?.as_deref(),
        );
        let role: String = r.get(4)?;
        Ok((
            lease_id.clone(),
            agent_id,
            role.clone(),
            Lease {
                lease_id,
                agent: addr,
                role,
                expires_at: fmt_ts(r.get(5)?),
            },
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn hydrate(conn: &Connection, row: Row0) -> Result<Resource> {
    let mut res = row.resource;
    let leases = live_leases(conn, &res.resource_id)?;
    res.owner = leases
        .iter()
        .find(|(_, _, role, _)| role == "owner")
        .map(|(_, _, _, l)| l.agent.clone());
    res.refcount = leases.len() as u32;
    res.leases = leases.into_iter().map(|(_, _, _, l)| l).collect();
    res.status = if row.released {
        "released"
    } else if res.refcount > 0 {
        "live"
    } else {
        "lapsed"
    }
    .to_string();
    res.lapsed_since = if res.status == "lapsed" {
        fmt_opt(row.lapsed_at)
    } else {
        None
    };
    Ok(res)
}

pub(crate) fn get_resource(conn: &Connection, resource_id: &str) -> Result<Resource> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {RESOURCE_COLUMNS} FROM resources r
                 LEFT JOIN projects p ON p.project_id = r.project_id WHERE r.resource_id = ?1"
            ),
            [resource_id],
            resource_from_row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("unknown resource {resource_id:?}")))?;
    hydrate(conn, row)
}

/// Resolve a reference to a resource id that is not released.
fn resolve(conn: &Connection, node_id: &str, target: &ResourceRef) -> Result<String> {
    let found: Option<String> = match target {
        ResourceRef::Id(id) => conn
            .query_row(
                "SELECT resource_id FROM resources WHERE resource_id = ?1 AND released_at IS NULL",
                [id],
                |r| r.get(0),
            )
            .optional()?,
        ResourceRef::Name(kind, name) => conn
            .query_row(
                "SELECT resource_id FROM resources
                 WHERE node_id = ?1 AND kind = ?2 AND name = ?3 AND released_at IS NULL",
                params![node_id, kind.as_str(), name],
                |r| r.get(0),
            )
            .optional()?,
    };
    found.ok_or_else(|| {
        let what = match target {
            ResourceRef::Id(id) => id.clone(),
            ResourceRef::Name(k, n) => format!("{}:{n}", k.as_str()),
        };
        Error::NotFound(format!("no registered resource {what:?}"))
    })
}

fn add_lease(
    tx: &Tx<'_>,
    resource_id: &str,
    agent: &AgentRef,
    role: &str,
    ttl: i64,
) -> Result<String> {
    let lease_id = new_id();
    tx.execute(
        "INSERT INTO resource_leases (lease_id, resource_id, node_id, agent_id, role, ttl_seconds,
           created_at, renewed_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
        params![
            lease_id,
            resource_id,
            tx.node_id,
            agent.id,
            role,
            ttl,
            tx.now,
            tx.now + ttl * 1000
        ],
    )?;
    tx.execute(
        "UPDATE resources SET lapsed_at = NULL WHERE resource_id = ?1",
        [resource_id],
    )?;
    Ok(lease_id)
}

/// After leases end: promote the oldest borrower if the owner is gone, and stamp
/// `lapsed_at` when nothing is left.
fn settle(tx: &Tx<'_>, resource_id: &str, actor: &str, when: i64) -> Result<Option<String>> {
    let leases = live_leases(tx, resource_id)?;
    if leases.is_empty() {
        tx.execute(
            "UPDATE resources SET lapsed_at = coalesce(lapsed_at, ?2) WHERE resource_id = ?1",
            params![resource_id, when],
        )?;
        return Ok(None);
    }
    if leases.iter().any(|(_, _, role, _)| role == "owner") {
        return Ok(None);
    }
    let (lease_id, agent_id, _, lease) = &leases[0];
    tx.emit(
        actor,
        "resource.transferred",
        json!({ "resource_id": resource_id, "lease_id": lease_id, "owner_id": agent_id }),
    )?;
    tx.execute(
        "UPDATE resource_leases SET role = 'owner' WHERE lease_id = ?1",
        [lease_id],
    )?;
    Ok(Some(lease.agent.clone()))
}

fn my_lease(tx: &Tx<'_>, resource_id: &str, agent_id: &str) -> Result<Option<(String, String)>> {
    Ok(tx
        .query_row(
            "SELECT lease_id, role FROM resource_leases
             WHERE resource_id = ?1 AND agent_id = ?2 AND released_at IS NULL",
            params![resource_id, agent_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

fn outcome(tx: &Tx<'_>, resource_id: &str, agent_id: &str, what: &str) -> Result<LeaseOutcome> {
    let resource = get_resource(tx, resource_id)?;
    let lease = match my_lease(tx, resource_id, agent_id)? {
        Some((lease_id, _)) => resource
            .leases
            .iter()
            .find(|l| l.lease_id == lease_id)
            .cloned(),
        None => None,
    };
    Ok(LeaseOutcome {
        resource,
        lease,
        outcome: what.to_string(),
    })
}

/// Expire lapsed leases. Returns how many expired.
pub(crate) fn expire_leases_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(
        "SELECT lease_id, resource_id, agent_id, expires_at FROM resource_leases
         WHERE node_id = ?1 AND released_at IS NULL AND expires_at <= ?2",
    )?;
    let expired = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (lease_id, resource_id, agent_id, expires_at) in &expired {
        tx.emit(
            "system",
            "resource.lease_expired",
            json!({ "lease_id": lease_id, "resource_id": resource_id, "agent_id": agent_id }),
        )?;
        tx.execute(
            "UPDATE resource_leases SET released_at = expires_at, release_reason = 'expired'
             WHERE lease_id = ?1",
            [lease_id],
        )?;
        settle(tx, resource_id, "system", *expires_at)?;
    }
    Ok(expired.len())
}

/// Renew (or release, at session end) every live lease an agent holds.
pub(crate) fn session_leases_tx(tx: &Tx<'_>, agent: &AgentRef, release: bool) -> Result<u32> {
    let mut stmt = tx.prepare(
        "SELECT lease_id, resource_id, ttl_seconds FROM resource_leases
         WHERE agent_id = ?1 AND released_at IS NULL",
    )?;
    let leases = stmt
        .query_map([&agent.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if leases.is_empty() {
        return Ok(0);
    }
    let ids: Vec<&String> = leases.iter().map(|(id, _, _)| id).collect();
    if release {
        tx.emit(
            &agent.address,
            "resource.released",
            json!({ "lease_ids": ids, "reason": "session_end" }),
        )?;
        for (lease_id, resource_id, _) in &leases {
            tx.execute(
                "UPDATE resource_leases SET released_at = ?2, release_reason = 'session_end'
                 WHERE lease_id = ?1",
                params![lease_id, tx.now],
            )?;
            settle(tx, resource_id, &agent.address, tx.now)?;
        }
    } else {
        tx.emit(
            &agent.address,
            "resource.renewed",
            json!({ "lease_ids": ids }),
        )?;
        for (lease_id, _, ttl) in &leases {
            tx.execute(
                "UPDATE resource_leases SET renewed_at = ?2, expires_at = ?2 + ?3 * 1000
                 WHERE lease_id = ?1",
                params![lease_id, tx.now, ttl],
            )?;
        }
    }
    Ok(leases.len() as u32)
}

impl Store {
    /// Register a running resource and take the owner lease. If a resource with the same
    /// kind and name is already registered, this fails with `conflict` while it has live
    /// leases (borrow it instead), and adopts it when it has lapsed.
    pub fn register_resource(&self, input: RegisterResource) -> Result<LeaseOutcome> {
        validate_name(&input.name)?;
        let ttl = clamp_ttl(input.ttl_seconds)?;
        let metadata = input
            .metadata
            .clone()
            .unwrap_or(Value::Object(Default::default()));
        if !metadata.is_object() {
            return Err(Error::Invalid("metadata must be a JSON object".into()));
        }
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let project_id = match &input.project {
                Some(p) => Some(find_project(tx, tx.node_id, p)?.project_id),
                None => None,
            };
            let port = input
                .port
                .or_else(|| input.endpoint.as_deref().and_then(derive_port))
                .or_else(|| derive_port(&input.name));
            let target = ResourceRef::Name(input.kind, input.name.clone());
            if let Ok(existing) = resolve(tx, tx.node_id, &target) {
                let current = get_resource(tx, &existing)?;
                if current.refcount > 0 {
                    return Err(Error::Conflict(format!(
                        "{}:{} is already registered (resource {}, owner {}); borrow it instead",
                        input.kind.as_str(),
                        input.name,
                        existing,
                        current.owner.as_deref().unwrap_or("none")
                    )));
                }
                tx.emit(
                    &me.address,
                    "resource.adopted",
                    json!({ "resource_id": existing, "kind": input.kind.as_str(), "name": input.name }),
                )?;
                tx.execute(
                    "UPDATE resources SET endpoint = coalesce(?2, endpoint), port = coalesce(?3, port),
                       pid = coalesce(?4, pid), purpose = coalesce(?5, purpose), mode = ?6,
                       project_id = coalesce(?7, project_id)
                     WHERE resource_id = ?1",
                    params![
                        existing,
                        input.endpoint,
                        port,
                        input.pid,
                        input.purpose,
                        input.mode.as_str(),
                        project_id
                    ],
                )?;
                add_lease(tx, &existing, &me, "owner", ttl)?;
                touch(tx, &me.id)?;
                return outcome(tx, &existing, &me.id, "adopted");
            }
            let resource_id = new_id();
            let event_id = tx.emit(
                &me.address,
                "resource.registered",
                json!({
                    "resource_id": resource_id,
                    "kind": input.kind.as_str(),
                    "name": input.name,
                    "endpoint": input.endpoint,
                    "port": port,
                    "pid": input.pid,
                    "project_id": project_id,
                    "purpose": input.purpose,
                    "mode": input.mode.as_str(),
                    "ttl_seconds": ttl,
                    "metadata": metadata,
                }),
            )?;
            tx.execute(
                "INSERT INTO resources (resource_id, node_id, kind, name, endpoint, port, pid,
                   project_id, purpose, mode, metadata, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    resource_id,
                    tx.node_id,
                    input.kind.as_str(),
                    input.name,
                    input.endpoint,
                    port,
                    input.pid,
                    project_id,
                    input.purpose,
                    input.mode.as_str(),
                    metadata.to_string(),
                    tx.now,
                    event_id
                ],
            )?;
            add_lease(tx, &resource_id, &me, "owner", ttl)?;
            touch(tx, &me.id)?;
            outcome(tx, &resource_id, &me.id, "registered")
        })
    }

    /// Find registered resources (live and lapsed by default), newest first.
    pub fn find_resources(&self, filter: ResourceFilter) -> Result<Vec<Resource>> {
        self.sweep()?;
        self.read(|conn| {
            let project_id = match &filter.project {
                Some(p) => Some(find_project(conn, self.node_id(), p)?.project_id),
                None => None,
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT {RESOURCE_COLUMNS} FROM resources r
                 LEFT JOIN projects p ON p.project_id = r.project_id
                 WHERE r.node_id = ?1 AND (?2 IS NULL OR r.kind = ?2)
                   AND (?3 IS NULL OR r.project_id = ?3)
                   AND (?4 OR r.released_at IS NULL)
                   AND (?5 IS NULL OR instr(lower(r.name || ' ' || ifnull(r.endpoint, '') || ' ' ||
                        ifnull(r.purpose, '')), lower(?5)) > 0)
                 ORDER BY r.created_at DESC, r.resource_id DESC"
            ))?;
            let rows = stmt
                .query_map(
                    params![
                        self.node_id(),
                        filter.kind.map(|k| k.as_str()),
                        project_id,
                        filter.include_released,
                        filter.query
                    ],
                    resource_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|r| hydrate(conn, r)).collect()
        })
    }

    pub fn resource(&self, target: &ResourceRef) -> Result<Resource> {
        self.read(|conn| {
            let id = match target {
                ResourceRef::Id(id) => id.clone(),
                other => resolve(conn, self.node_id(), other)?,
            };
            get_resource(conn, &id)
        })
    }

    /// Take a shared-use lease. Exclusive resources refuse other agents. Borrowing a lapsed
    /// resource adopts it (the borrower becomes the owner). Borrowing again renews.
    pub fn borrow_resource(
        &self,
        agent: &str,
        target: &ResourceRef,
        ttl_seconds: Option<i64>,
    ) -> Result<LeaseOutcome> {
        let ttl = clamp_ttl(ttl_seconds)?;
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let rid = resolve(tx, tx.node_id, target)?;
            if let Some((lease_id, _)) = my_lease(tx, &rid, &me.id)? {
                tx.emit(
                    &me.address,
                    "resource.renewed",
                    json!({ "lease_ids": [lease_id] }),
                )?;
                tx.execute(
                    "UPDATE resource_leases SET ttl_seconds = ?2, renewed_at = ?3,
                       expires_at = ?3 + ?2 * 1000 WHERE lease_id = ?1",
                    params![lease_id, ttl, tx.now],
                )?;
                touch(tx, &me.id)?;
                return outcome(tx, &rid, &me.id, "renewed");
            }
            let current = get_resource(tx, &rid)?;
            if current.refcount == 0 {
                tx.emit(
                    &me.address,
                    "resource.adopted",
                    json!({ "resource_id": rid }),
                )?;
                add_lease(tx, &rid, &me, "owner", ttl)?;
                touch(tx, &me.id)?;
                return outcome(tx, &rid, &me.id, "adopted");
            }
            if current.mode == LeaseMode::Exclusive {
                return Err(Error::Conflict(format!(
                    "{} {} is exclusive to {}",
                    current.kind.as_str(),
                    current.name,
                    current.owner.as_deref().unwrap_or("its owner")
                )));
            }
            tx.emit(
                &me.address,
                "resource.borrowed",
                json!({ "resource_id": rid, "ttl_seconds": ttl }),
            )?;
            add_lease(tx, &rid, &me, "borrower", ttl)?;
            touch(tx, &me.id)?;
            outcome(tx, &rid, &me.id, "borrowed")
        })
    }

    /// Drop the caller's lease. If the owner leaves while others borrow, the oldest borrower
    /// becomes owner. With `stopped`, the owner declares the resource gone: it is marked
    /// released and every lease ends.
    pub fn release_resource(
        &self,
        agent: &str,
        target: &ResourceRef,
        stopped: bool,
    ) -> Result<LeaseOutcome> {
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let rid = resolve(tx, tx.node_id, target)?;
            let mine = my_lease(tx, &rid, &me.id)?;
            let current = get_resource(tx, &rid)?;
            if stopped {
                let owner_ok =
                    mine.as_ref().is_some_and(|(_, role)| role == "owner") || current.refcount == 0;
                if !owner_ok {
                    return Err(Error::Forbidden(format!(
                        "only the owner ({}) can mark {} stopped",
                        current.owner.as_deref().unwrap_or("none"),
                        current.name
                    )));
                }
                tx.emit(
                    &me.address,
                    "resource.stopped",
                    json!({ "resource_id": rid }),
                )?;
                tx.execute(
                    "UPDATE resource_leases SET released_at = ?2, release_reason = 'stopped'
                     WHERE resource_id = ?1 AND released_at IS NULL",
                    params![rid, tx.now],
                )?;
                tx.execute(
                    "UPDATE resources SET released_at = ?2, release_reason = 'stopped'
                     WHERE resource_id = ?1",
                    params![rid, tx.now],
                )?;
                touch(tx, &me.id)?;
                return outcome(tx, &rid, &me.id, "stopped");
            }
            let Some((lease_id, _)) = mine else {
                return Err(Error::NotFound(format!(
                    "{} has no lease on {}",
                    me.address, current.name
                )));
            };
            tx.emit(
                &me.address,
                "resource.released",
                json!({ "resource_id": rid, "lease_ids": [lease_id], "reason": "released" }),
            )?;
            tx.execute(
                "UPDATE resource_leases SET released_at = ?2, release_reason = 'released'
                 WHERE lease_id = ?1",
                params![lease_id, tx.now],
            )?;
            let promoted = settle(tx, &rid, &me.address, tx.now)?;
            touch(tx, &me.id)?;
            let what = if promoted.is_some() {
                "transferred"
            } else {
                "released"
            };
            outcome(tx, &rid, &me.id, what)
        })
    }

    /// Renew the caller's leases (all, or only the listed resources).
    pub fn heartbeat_resources(
        &self,
        agent: &str,
        targets: &[ResourceRef],
    ) -> Result<Vec<Resource>> {
        self.write(|tx| {
            crate::holds::sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let mut stmt = tx.prepare(
                "SELECT lease_id, resource_id, ttl_seconds FROM resource_leases
                 WHERE agent_id = ?1 AND released_at IS NULL",
            )?;
            let mine = stmt
                .query_map([&me.id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let wanted: Vec<String> = targets
                .iter()
                .map(|t| resolve(tx, tx.node_id, t))
                .collect::<Result<_>>()?;
            let selected: Vec<_> = mine
                .into_iter()
                .filter(|(_, rid, _)| wanted.is_empty() || wanted.contains(rid))
                .collect();
            if let Some(missing) = wanted
                .iter()
                .find(|w| !selected.iter().any(|(_, rid, _)| rid == *w))
            {
                return Err(Error::NotFound(format!(
                    "{} has no live lease on resource {missing}",
                    me.address
                )));
            }
            if !selected.is_empty() {
                let ids: Vec<&String> = selected.iter().map(|(l, _, _)| l).collect();
                tx.emit(&me.address, "resource.renewed", json!({ "lease_ids": ids }))?;
            }
            for (lease_id, _, ttl) in &selected {
                tx.execute(
                    "UPDATE resource_leases SET renewed_at = ?2, expires_at = ?2 + ?3 * 1000
                     WHERE lease_id = ?1",
                    params![lease_id, tx.now, ttl],
                )?;
            }
            touch(tx, &me.id)?;
            selected
                .iter()
                .map(|(_, rid, _)| get_resource(tx, rid))
                .collect()
        })
    }

    /// Registered resources with no live lease (orphan candidates), oldest lapse first.
    pub fn lapsed_resources(&self) -> Result<Vec<Resource>> {
        let mut all = self.find_resources(ResourceFilter::default())?;
        all.retain(|r| r.status == "lapsed");
        all.sort_by(|a, b| a.lapsed_since.cmp(&b.lapsed_since));
        Ok(all)
    }

    /// Milliseconds since the resource lapsed, or `None` if it is live or released.
    pub fn lapsed_for_ms(&self, resource_id: &str) -> Result<Option<i64>> {
        let now = self.now();
        self.read(|conn| {
            let row: Option<(Option<i64>, Option<i64>)> = conn
                .query_row(
                    "SELECT lapsed_at, released_at FROM resources WHERE resource_id = ?1",
                    [resource_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match row {
                None => Err(Error::NotFound(format!("unknown resource {resource_id:?}"))),
                Some((_, Some(_))) | Some((None, None)) => Ok(None),
                Some((Some(at), None)) => Ok(Some(now - at)),
            }
        })
    }

    /// Log that an agent asked for a reap (it is never executed from MCP).
    pub fn note_reap_request(&self, agent: &str, target: &str, reason: &str) -> Result<()> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            tx.emit(
                &me.address,
                "resource.reap_requested",
                json!({ "target": target, "reason": reason }),
            )?;
            Ok(())
        })
    }

    /// Record a reap a human confirmed. Marks a registered resource released.
    pub fn record_reap(
        &self,
        actor: &str,
        resource_id: Option<&str>,
        pid: Option<u32>,
        detail: &str,
    ) -> Result<()> {
        self.write(|tx| {
            tx.emit(
                actor,
                "resource.reaped",
                json!({ "resource_id": resource_id, "pid": pid, "detail": detail }),
            )?;
            if let Some(rid) = resource_id {
                tx.execute(
                    "UPDATE resource_leases SET released_at = ?2, release_reason = 'reaped'
                     WHERE resource_id = ?1 AND released_at IS NULL",
                    params![rid, tx.now],
                )?;
                tx.execute(
                    "UPDATE resources SET released_at = ?2, release_reason = 'reaped'
                     WHERE resource_id = ?1",
                    params![rid, tx.now],
                )?;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_ports() {
        assert_eq!(derive_port("ollama@11434"), Some(11434));
        assert_eq!(derive_port("tcp:0.0.0.0:5432"), Some(5432));
        assert_eq!(derive_port("http://127.0.0.1:5173/app"), Some(5173));
        assert_eq!(derive_port("http://[::1]:3000"), Some(3000));
        assert_eq!(derive_port("metal:0"), None);
        assert_eq!(derive_port("train-job"), None);
    }

    #[test]
    fn parses_refs() {
        assert!(matches!(
            ResourceRef::parse("model_server:ollama@11434").unwrap(),
            ResourceRef::Name(ResourceKind::ModelServer, n) if n == "ollama@11434"
        ));
        assert!(matches!(
            ResourceRef::parse("01J0000000000000000000000A").unwrap(),
            ResourceRef::Id(_)
        ));
    }
}
