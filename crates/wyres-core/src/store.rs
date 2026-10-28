//! SQLite store: connection handling, schema migrations, local node identity and the
//! append-only event log that every mutation writes to first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::time::{fmt_ts, now_ms};

/// File name of the node database inside the Wyres home directory.
pub const DB_FILE: &str = "wyres.db";

/// Source of "now" for the store. Tests use a manual clock to exercise TTLs.
#[derive(Clone)]
pub enum Clock {
    System,
    Manual(Arc<AtomicI64>),
}

impl Clock {
    /// A manual clock starting at `start_ms`; returns the clock and a handle to move it.
    pub fn manual(start_ms: i64) -> (Clock, Arc<AtomicI64>) {
        let handle = Arc::new(AtomicI64::new(start_ms));
        (Clock::Manual(handle.clone()), handle)
    }

    pub fn now(&self) -> i64 {
        match self {
            Clock::System => now_ms(),
            Clock::Manual(t) => t.load(Ordering::SeqCst),
        }
    }
}

/// Resolve the Wyres home directory: `$WYRES_HOME`, else `~/.wyres`.
pub fn default_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("WYRES_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::Invalid("cannot locate home directory; set WYRES_HOME".into()))?;
    Ok(PathBuf::from(home).join(".wyres"))
}

/// Handle to a node database. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    node_id: String,
    clock: Clock,
    path: Option<PathBuf>,
}

/// Public view of the local node.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NodeInfo {
    pub node_id: String,
    pub display_name: String,
    pub role: String,
    pub created_at: String,
}

/// One record of the append-only event log.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Event {
    pub seq: i64,
    pub event_id: String,
    pub node_id: String,
    pub ts: String,
    pub actor: String,
    pub kind: String,
    pub payload: Value,
    pub prev_id: Option<String>,
}

impl Store {
    /// Open (creating if needed) the node database inside `home`.
    pub fn open(home: &Path) -> Result<Store> {
        Self::open_with_clock(home, Clock::System)
    }

    pub fn open_with_clock(home: &Path, clock: Clock) -> Result<Store> {
        std::fs::create_dir_all(home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = home.join(DB_FILE);
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn, clock, Some(path))
    }

    /// An in-memory store (tests and experiments).
    pub fn open_in_memory(clock: Clock) -> Result<Store> {
        Self::init(Connection::open_in_memory()?, clock, None)
    }

    fn init(mut conn: Connection, clock: Clock, path: Option<PathBuf>) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        let node_id = ensure_local_node(&mut conn, clock.now())?;
        Ok(Store {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                node_id,
                clock,
                path,
            }),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.inner.node_id
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub fn now(&self) -> i64 {
        self.inner.clock.now()
    }

    pub fn node(&self) -> Result<NodeInfo> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT node_id, display_name, role, created_at FROM nodes WHERE node_id = ?1",
                [self.node_id()],
                |r| {
                    Ok(NodeInfo {
                        node_id: r.get(0)?,
                        display_name: r.get(1)?,
                        role: r.get(2)?,
                        created_at: fmt_ts(r.get(3)?),
                    })
                },
            )?)
        })
    }

    /// Most recent events, newest last.
    pub fn events(&self, limit: u32) -> Result<Vec<Event>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id FROM (
                   SELECT * FROM events ORDER BY seq DESC LIMIT ?1
                 ) ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map([limit], |r| {
                let payload: String = r.get(6)?;
                Ok(Event {
                    seq: r.get(0)?,
                    event_id: r.get(1)?,
                    node_id: r.get(2)?,
                    ts: fmt_ts(r.get(3)?),
                    actor: r.get(4)?,
                    kind: r.get(5)?,
                    payload: serde_json::from_str(&payload).unwrap_or(Value::String(payload)),
                    prev_id: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Run a read-only closure against the connection.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.lock();
        f(&conn)
    }

    /// Run `f` inside an immediate write transaction; commits on `Ok`.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ctx = Tx {
            tx,
            node_id: &self.inner.node_id,
            now: self.inner.clock.now(),
        };
        let out = f(&ctx)?;
        ctx.tx.commit()?;
        Ok(out)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-call; the
        // transaction it held was rolled back, so the connection is still usable.
        self.inner
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A write transaction plus the context every mutation needs.
pub(crate) struct Tx<'a> {
    pub tx: rusqlite::Transaction<'a>,
    pub node_id: &'a str,
    pub now: i64,
}

impl std::ops::Deref for Tx<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.tx
    }
}

impl Tx<'_> {
    /// Append an event to the log, chained to this node's previous event. Returns its id.
    pub fn emit(&self, actor: &str, kind: &str, payload: Value) -> Result<String> {
        let prev_id: Option<String> = self
            .query_row(
                "SELECT event_id FROM events WHERE node_id = ?1 ORDER BY seq DESC LIMIT 1",
                [self.node_id],
                |r| r.get(0),
            )
            .optional()?;
        let event_id = new_id();
        self.execute(
            "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                event_id,
                self.node_id,
                self.now,
                actor,
                kind,
                payload.to_string(),
                prev_id
            ],
        )?;
        Ok(event_id)
    }
}

/// A fresh ULID string.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE nodes (
    node_id      TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    role         TEXT NOT NULL DEFAULT 'home',
    is_local     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE TABLE events (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    node_id  TEXT NOT NULL REFERENCES nodes(node_id),
    ts       INTEGER NOT NULL,
    actor    TEXT NOT NULL,
    kind     TEXT NOT NULL,
    payload  TEXT NOT NULL,
    prev_id  TEXT
);
CREATE INDEX events_by_node ON events(node_id, seq);
CREATE INDEX events_by_kind ON events(kind, seq);

CREATE TABLE projects (
    project_id TEXT PRIMARY KEY,
    node_id    TEXT NOT NULL REFERENCES nodes(node_id),
    abs_path   TEXT NOT NULL,
    slug       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    event_id   TEXT NOT NULL,
    UNIQUE (node_id, abs_path),
    UNIQUE (node_id, slug)
);

CREATE TABLE agents (
    agent_id     TEXT PRIMARY KEY,
    node_id      TEXT NOT NULL REFERENCES nodes(node_id),
    project_id   TEXT REFERENCES projects(project_id),
    name         TEXT NOT NULL,
    program      TEXT NOT NULL,
    model        TEXT,
    description  TEXT,
    status       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_seen_at INTEGER,
    event_id     TEXT NOT NULL
);
CREATE UNIQUE INDEX agents_identity ON agents(node_id, name, ifnull(project_id, ''));

CREATE TABLE messages (
    message_id   TEXT PRIMARY KEY,
    node_id      TEXT NOT NULL REFERENCES nodes(node_id),
    thread_id    TEXT NOT NULL,
    reply_to     TEXT REFERENCES messages(message_id),
    sender_id    TEXT NOT NULL REFERENCES agents(agent_id),
    subject      TEXT NOT NULL,
    body         TEXT NOT NULL,
    importance   TEXT NOT NULL,
    ack_required INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    event_id     TEXT NOT NULL
);
CREATE INDEX messages_by_thread ON messages(thread_id, created_at);

CREATE TABLE deliveries (
    message_id TEXT NOT NULL REFERENCES messages(message_id),
    agent_id   TEXT NOT NULL REFERENCES agents(agent_id),
    node_id    TEXT NOT NULL REFERENCES nodes(node_id),
    role       TEXT NOT NULL,
    read_at    INTEGER,
    acked_at   INTEGER,
    PRIMARY KEY (message_id, agent_id)
);
CREATE INDEX deliveries_by_agent ON deliveries(agent_id, read_at);

CREATE TABLE requests (
    request_id TEXT PRIMARY KEY,
    node_id    TEXT NOT NULL REFERENCES nodes(node_id),
    message_id TEXT NOT NULL REFERENCES messages(message_id),
    thread_id  TEXT NOT NULL,
    title      TEXT NOT NULL,
    created_by TEXT NOT NULL REFERENCES agents(agent_id),
    owner_id   TEXT REFERENCES agents(agent_id),
    status     TEXT NOT NULL,
    due_at     INTEGER,
    note       TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    claimed_at INTEGER,
    closed_at  INTEGER
);
CREATE INDEX requests_by_status ON requests(status, due_at);

CREATE TABLE holds (
    hold_id        TEXT PRIMARY KEY,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    project_id     TEXT NOT NULL REFERENCES projects(project_id),
    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),
    pattern        TEXT NOT NULL,
    exclusive      INTEGER NOT NULL,
    reason         TEXT,
    ttl_seconds    INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    renewed_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    released_at    INTEGER,
    release_reason TEXT
);
CREATE INDEX holds_live ON holds(project_id, released_at, expires_at);
"#,
    r#"
CREATE TABLE resources (
    resource_id    TEXT PRIMARY KEY,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    kind           TEXT NOT NULL,
    name           TEXT NOT NULL,
    endpoint       TEXT,
    port           INTEGER,
    pid            INTEGER,
    project_id     TEXT REFERENCES projects(project_id),
    purpose        TEXT,
    mode           TEXT NOT NULL,
    metadata       TEXT NOT NULL DEFAULT '{}',
    created_at     INTEGER NOT NULL,
    lapsed_at      INTEGER,
    released_at    INTEGER,
    release_reason TEXT,
    event_id       TEXT NOT NULL
);
CREATE UNIQUE INDEX resources_live_identity ON resources(node_id, kind, name)
    WHERE released_at IS NULL;

CREATE TABLE resource_leases (
    lease_id       TEXT PRIMARY KEY,
    resource_id    TEXT NOT NULL REFERENCES resources(resource_id),
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),
    role           TEXT NOT NULL,
    ttl_seconds    INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    renewed_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    released_at    INTEGER,
    release_reason TEXT
);
CREATE INDEX resource_leases_by_resource ON resource_leases(resource_id, released_at);
CREATE INDEX resource_leases_by_agent ON resource_leases(agent_id, released_at);
"#,
    r#"
CREATE TABLE tokens (
    token_id      TEXT PRIMARY KEY,
    node_id       TEXT NOT NULL REFERENCES nodes(node_id),
    agent_id      TEXT NOT NULL REFERENCES agents(agent_id),
    label         TEXT,
    project_globs TEXT NOT NULL,
    secret_hash   TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    revoked_at    INTEGER,
    last_used_at  INTEGER
);

CREATE TABLE audit_log (
    audit_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id   TEXT NOT NULL REFERENCES nodes(node_id),
    ts        INTEGER NOT NULL,
    token_id  TEXT,
    agent     TEXT,
    operation TEXT NOT NULL,
    outcome   TEXT NOT NULL,
    detail    TEXT,
    remote    TEXT
);
"#,
];

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let current = usize::try_from(current).unwrap_or(0);
    if current > MIGRATIONS.len() {
        return Err(Error::Invalid(format!(
            "database schema v{current} is newer than this wyres build supports (v{}); upgrade wyres",
            MIGRATIONS.len()
        )));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

fn ensure_local_node(conn: &mut Connection, now: i64) -> Result<String> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<String> = tx
        .query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let node_id = new_id();
    let display_name = std::env::var("WYRES_NODE_NAME").unwrap_or_else(|_| "local".to_string());
    tx.execute(
        "INSERT INTO nodes (node_id, display_name, role, is_local, created_at)
         VALUES (?1, ?2, 'home', 1, ?3)",
        params![node_id, display_name, now],
    )?;
    {
        let ctx = Tx {
            tx,
            node_id: &node_id,
            now,
        };
        ctx.emit(
            "system",
            "node.created",
            serde_json::json!({ "node_id": node_id, "display_name": display_name, "role": "home" }),
        )?;
        ctx.tx.commit()?;
    }
    Ok(node_id)
}
