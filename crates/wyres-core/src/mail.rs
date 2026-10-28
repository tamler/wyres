//! Mail: immutable messages grouped into threads, with per-recipient delivery state.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{address, resolve_agent, AgentRef};
use crate::requests::{create_request_tx, list_requests_for_inbox, Request};
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};

const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_SUBJECT_CHARS: usize = 300;

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

impl Importance {
    pub fn as_str(self) -> &'static str {
        match self {
            Importance::Low => "low",
            Importance::Normal => "normal",
            Importance::High => "high",
            Importance::Urgent => "urgent",
        }
    }
}

impl std::str::FromStr for Importance {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "low" => Ok(Importance::Low),
            "normal" => Ok(Importance::Normal),
            "high" => Ok(Importance::High),
            "urgent" => Ok(Importance::Urgent),
            _ => Err(Error::Invalid(format!(
                "unknown importance {s:?}; expected low, normal, high or urgent"
            ))),
        }
    }
}

/// Input for [`Store::send_message`].
#[derive(Debug, Clone, Default)]
pub struct SendMessage {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub importance: Importance,
    pub ack_required: bool,
    /// Reply to this message: joins its thread.
    pub reply_to: Option<String>,
    /// Post into an existing thread without replying to a specific message.
    pub thread_id: Option<String>,
    /// Also open a request (owned work item) addressed to the recipients.
    pub request: Option<NewRequest>,
}

#[derive(Debug, Clone, Default)]
pub struct NewRequest {
    /// Request becomes `stale` if not done this many seconds after creation.
    pub due_in_seconds: Option<i64>,
}

/// Input for [`Store::reply`].
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub from: String,
    pub message_id: String,
    pub body: String,
    /// Reply to every original recipient, not just the sender.
    pub all: bool,
    pub importance: Option<Importance>,
    pub ack_required: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Message {
    pub message_id: String,
    pub thread_id: String,
    pub reply_to: Option<String>,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub importance: Importance,
    pub ack_required: bool,
    pub created_at: String,
    /// Request opened by this message, if any.
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct InboxItem {
    #[serde(flatten)]
    pub message: Message,
    /// `to` or `cc`.
    pub role: String,
    pub read_at: Option<String>,
    pub acked_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct InboxQuery {
    pub unread_only: bool,
    /// Only `high` and `urgent` messages.
    pub urgent_only: bool,
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Inbox {
    pub agent: String,
    pub unread: u32,
    pub urgent_unread: u32,
    pub awaiting_ack: u32,
    pub messages: Vec<InboxItem>,
    /// Requests addressed to this agent that are open or stale, plus ones it owns.
    pub requests: Vec<Request>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub subject: String,
    pub messages: u32,
    pub unread: u32,
    pub participants: Vec<String>,
    pub last_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Thread {
    pub thread_id: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AckOutcome {
    pub agent: String,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
}

fn check_text(subject: &str, body: &str) -> Result<()> {
    if subject.trim().is_empty() {
        return Err(Error::Invalid("subject must not be empty".into()));
    }
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        return Err(Error::Invalid(format!(
            "subject is longer than {MAX_SUBJECT_CHARS} characters"
        )));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid(format!(
            "body is larger than {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(())
}

pub(crate) fn load_message(conn: &Connection, message_id: &str) -> Result<Message> {
    let row = conn
        .query_row(
            "SELECT m.message_id, m.thread_id, m.reply_to, a.address, NULL, m.subject, m.body,
                    m.importance, m.ack_required, m.created_at,
                    (SELECT request_id FROM requests r WHERE r.message_id = m.message_id)
             FROM messages m
             JOIN agents a ON a.agent_id = m.sender_id
             LEFT JOIN projects p ON p.project_id = a.project_id
             WHERE m.message_id = ?1",
            [message_id],
            |r| {
                let name: String = r.get(3)?;
                let slug: Option<String> = r.get(4)?;
                let importance: String = r.get(7)?;
                Ok(Message {
                    message_id: r.get(0)?,
                    thread_id: r.get(1)?,
                    reply_to: r.get(2)?,
                    from: address(&name, slug.as_deref()),
                    to: Vec::new(),
                    cc: Vec::new(),
                    subject: r.get(5)?,
                    body: r.get(6)?,
                    importance: importance.parse().unwrap_or_default(),
                    ack_required: r.get(8)?,
                    created_at: fmt_ts(r.get(9)?),
                    request_id: r.get(10)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("unknown message {message_id:?}")))?;
    let mut msg = row;
    let mut stmt = conn.prepare(
        "SELECT d.role, a.address, NULL FROM deliveries d
         JOIN agents a ON a.agent_id = d.agent_id
         LEFT JOIN projects p ON p.project_id = a.project_id
         WHERE d.message_id = ?1 ORDER BY d.rowid",
    )?;
    let rows = stmt.query_map([message_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            address(
                &r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.as_deref(),
            ),
        ))
    })?;
    for row in rows {
        let (role, addr) = row?;
        if role == "cc" {
            msg.cc.push(addr);
        } else {
            msg.to.push(addr);
        }
    }
    Ok(msg)
}

fn resolve_all(tx: &Tx<'_>, names: &[String]) -> Result<Vec<AgentRef>> {
    names
        .iter()
        .map(|n| crate::identity::resolve_any(tx, tx.node_id, n))
        .collect()
}

pub(crate) fn send_tx(tx: &Tx<'_>, input: &SendMessage) -> Result<Message> {
    check_text(&input.subject, &input.body)?;
    if input.to.is_empty() {
        return Err(Error::Invalid("at least one recipient is required".into()));
    }
    let sender = resolve_agent(tx, tx.node_id, &input.from)?;
    let to = resolve_all(tx, &input.to)?;
    let cc = resolve_all(tx, &input.cc)?;

    let message_id = new_id();
    let thread_id = match (&input.reply_to, &input.thread_id) {
        (Some(parent), given) => {
            let parent_thread: String = tx
                .query_row(
                    "SELECT thread_id FROM messages WHERE message_id = ?1",
                    [parent],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("unknown message {parent:?}")))?;
            if given.as_ref().is_some_and(|t| *t != parent_thread) {
                return Err(Error::Invalid(
                    "thread_id does not match the thread of reply_to".into(),
                ));
            }
            parent_thread
        }
        (None, Some(thread)) => {
            let exists = tx
                .query_row(
                    "SELECT 1 FROM messages WHERE thread_id = ?1 LIMIT 1",
                    [thread],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(Error::NotFound(format!("unknown thread {thread:?}")));
            }
            thread.clone()
        }
        (None, None) => message_id.clone(),
    };

    // Each agent receives a message once; `to` wins over `cc`.
    let mut recipients: Vec<(&AgentRef, &str)> = Vec::new();
    for (list, role) in [(&to, "to"), (&cc, "cc")] {
        for agent in list.iter() {
            if !recipients.iter().any(|(a, _)| a.id == agent.id) {
                recipients.push((agent, role));
            }
        }
    }
    let addrs = |role: &str| -> Vec<String> {
        recipients
            .iter()
            .filter(|(_, r)| *r == role)
            .map(|(a, _)| a.address.clone())
            .collect()
    };

    // Recipients on other nodes travel through the hub; requests stay node-local.
    let mut foreign_nodes: Vec<String> = recipients
        .iter()
        .map(|(a, _)| a.node_id.clone())
        .filter(|n| n != tx.node_id)
        .collect();
    foreign_nodes.sort();
    foreign_nodes.dedup();
    if !foreign_nodes.is_empty() {
        if input.request.is_some() {
            return Err(Error::Invalid(
                "requests are node-local: open them only with agents on this node".into(),
            ));
        }
        for node in &foreign_nodes {
            crate::federation::allowed_towards(tx, node, crate::federation::Perm::SendMail)?;
        }
    }
    let mut recipient_nodes = vec![tx.node_id.to_string()];
    recipient_nodes.extend(foreign_nodes);
    let request_id = input.request.as_ref().map(|_| new_id());
    let event_id = tx.emit(
        &sender.address,
        "message.sent",
        json!({
            "message_id": message_id,
            "thread_id": thread_id,
            "reply_to": input.reply_to,
            "from": sender.address,
            "to": addrs("to"),
            "cc": addrs("cc"),
            "subject": input.subject,
            "body": input.body,
            "importance": input.importance.as_str(),
            "ack_required": input.ack_required,
            "request_id": request_id,
            "sender_id": sender.id,
            "recipients": recipients
                .iter()
                .map(|(a, role)| json!({ "agent_id": a.id, "node_id": a.node_id, "role": role }))
                .collect::<Vec<_>>(),
            "recipient_nodes": recipient_nodes,
        }),
    )?;
    tx.execute(
        "INSERT INTO messages (message_id, node_id, thread_id, reply_to, sender_id, subject, body,
           importance, ack_required, created_at, event_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            message_id,
            tx.node_id,
            thread_id,
            input.reply_to,
            sender.id,
            input.subject,
            input.body,
            input.importance.as_str(),
            input.ack_required,
            tx.now,
            event_id
        ],
    )?;
    for (agent, role) in &recipients {
        tx.execute(
            "INSERT INTO deliveries (message_id, agent_id, node_id, role) VALUES (?1, ?2, ?3, ?4)",
            params![message_id, agent.id, agent.node_id, role],
        )?;
    }
    if let (Some(req), Some(request_id)) = (&input.request, &request_id) {
        create_request_tx(
            tx,
            request_id,
            &sender,
            &message_id,
            &thread_id,
            &input.subject,
            req.due_in_seconds,
        )?;
    }
    touch(tx, &sender.id)?;
    load_message(tx, &message_id)
}

pub(crate) fn touch(tx: &Tx<'_>, agent_id: &str) -> Result<()> {
    tx.execute(
        "UPDATE agents SET last_seen_at = ?2, status = 'active' WHERE agent_id = ?1",
        params![agent_id, tx.now],
    )?;
    Ok(())
}

impl Store {
    /// Send a message (optionally opening a request) to one or more agents.
    pub fn send_message(&self, input: SendMessage) -> Result<Message> {
        self.write(|tx| send_tx(tx, &input))
    }

    /// Reply within the original message's thread.
    pub fn reply(&self, input: Reply) -> Result<Message> {
        self.write(|tx| {
            let original = load_message(tx, &input.message_id)?;
            let me = resolve_agent(tx, tx.node_id, &input.from)?;
            let mut to = vec![original.from.clone()];
            let mut cc = Vec::new();
            if input.all {
                to.extend(original.to.iter().cloned());
                cc.extend(original.cc.iter().cloned());
            }
            to.retain(|a| *a != me.address);
            cc.retain(|a| *a != me.address);
            if to.is_empty() {
                // Replying to your own message: address the original recipients.
                to = original.to.clone();
            }
            let subject = if original.subject.to_ascii_lowercase().starts_with("re:") {
                original.subject.clone()
            } else {
                format!("Re: {}", original.subject)
            };
            send_tx(
                tx,
                &SendMessage {
                    from: input.from.clone(),
                    to,
                    cc,
                    subject,
                    body: input.body.clone(),
                    importance: input.importance.unwrap_or(original.importance),
                    ack_required: input.ack_required,
                    reply_to: Some(original.message_id.clone()),
                    thread_id: None,
                    request: None,
                },
            )
        })
    }

    /// Messages delivered to `who`, newest first, plus counters and relevant requests.
    pub fn inbox(&self, who: &str, query: InboxQuery) -> Result<Inbox> {
        // Expire stale requests/holds first so the digest is current even without a daemon.
        self.sweep()?;
        self.read(|conn| {
            let me = resolve_agent(conn, self.node_id(), who)?;
            let (unread, urgent_unread, awaiting_ack): (u32, u32, u32) = conn.query_row(
                "SELECT
                   count(*) FILTER (WHERE d.read_at IS NULL),
                   count(*) FILTER (WHERE d.read_at IS NULL AND m.importance IN ('high', 'urgent')),
                   count(*) FILTER (WHERE m.ack_required = 1 AND d.acked_at IS NULL)
                 FROM deliveries d JOIN messages m ON m.message_id = d.message_id
                 WHERE d.agent_id = ?1",
                [&me.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let limit = if query.limit == 0 { 50 } else { query.limit };
            let mut stmt = conn.prepare(
                "SELECT d.message_id, d.role, d.read_at, d.acked_at
                 FROM deliveries d JOIN messages m ON m.message_id = d.message_id
                 WHERE d.agent_id = ?1
                   AND (?2 = 0 OR d.read_at IS NULL)
                   AND (?3 = 0 OR m.importance IN ('high', 'urgent'))
                 ORDER BY m.created_at DESC, m.message_id DESC
                 LIMIT ?4",
            )?;
            let rows = stmt
                .query_map(
                    params![me.id, query.unread_only, query.urgent_only, limit],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let messages = rows
                .into_iter()
                .map(|(id, role, read_at, acked_at)| {
                    Ok(InboxItem {
                        message: load_message(conn, &id)?,
                        role,
                        read_at: fmt_opt(read_at),
                        acked_at: fmt_opt(acked_at),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let requests = list_requests_for_inbox(conn, &me.id)?;
            Ok(Inbox {
                agent: me.address,
                unread,
                urgent_unread,
                awaiting_ack,
                messages,
                requests,
            })
        })
    }

    /// Mark deliveries read, and acknowledged too when `ack` is set.
    pub fn acknowledge(&self, who: &str, message_ids: &[String], ack: bool) -> Result<AckOutcome> {
        if message_ids.is_empty() {
            return Err(Error::Invalid("no message ids given".into()));
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, who)?;
            let mut updated = Vec::new();
            let mut unchanged = Vec::new();
            for id in message_ids {
                let state: Option<(Option<i64>, Option<i64>)> = tx
                    .query_row(
                        "SELECT read_at, acked_at FROM deliveries WHERE message_id = ?1 AND agent_id = ?2",
                        params![id, me.id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((read_at, acked_at)) = state else {
                    return Err(Error::NotFound(format!(
                        "message {id:?} was not delivered to {}",
                        me.address
                    )));
                };
                let mark_read = read_at.is_none();
                let mark_ack = ack && acked_at.is_none();
                if !mark_read && !mark_ack {
                    unchanged.push(id.clone());
                    continue;
                }
                let kind = if mark_ack { "delivery.acked" } else { "delivery.read" };
                let message_node: String = tx.query_row(
                    "SELECT node_id FROM messages WHERE message_id = ?1",
                    [id],
                    |r| r.get(0),
                )?;
                tx.emit(
                    &me.address,
                    kind,
                    json!({ "message_id": id, "agent_id": me.id, "message_node": message_node }),
                )?;
                tx.execute(
                    "UPDATE deliveries SET
                       read_at = coalesce(read_at, ?3),
                       acked_at = CASE WHEN ?4 THEN coalesce(acked_at, ?3) ELSE acked_at END
                     WHERE message_id = ?1 AND agent_id = ?2",
                    params![id, me.id, tx.now, mark_ack],
                )?;
                updated.push(id.clone());
            }
            touch(tx, &me.id)?;
            Ok(AckOutcome {
                agent: me.address,
                updated,
                unchanged,
            })
        })
    }

    /// Threads `who` has sent to or received in, most recently active first.
    pub fn threads(&self, who: &str, limit: u32) -> Result<Vec<ThreadSummary>> {
        self.read(|conn| {
            let me = resolve_agent(conn, self.node_id(), who)?;
            let limit = if limit == 0 { 50 } else { limit };
            let mut stmt = conn.prepare(
                "SELECT m.thread_id, count(*), max(m.created_at),
                   (SELECT count(*) FROM deliveries d JOIN messages m2 ON m2.message_id = d.message_id
                     WHERE m2.thread_id = m.thread_id AND d.agent_id = ?1 AND d.read_at IS NULL),
                   (SELECT subject FROM messages f WHERE f.message_id = m.thread_id)
                 FROM messages m
                 WHERE m.thread_id IN (
                   SELECT thread_id FROM messages WHERE sender_id = ?1
                   UNION
                   SELECT m3.thread_id FROM deliveries d3 JOIN messages m3 ON m3.message_id = d3.message_id
                    WHERE d3.agent_id = ?1)
                 GROUP BY m.thread_id
                 ORDER BY max(m.created_at) DESC
                 LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![me.id, limit], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, u32>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter()
                .map(|(thread_id, messages, last_at, unread, subject)| {
                    let participants = thread_participants(conn, &thread_id)?;
                    Ok(ThreadSummary {
                        subject: subject.unwrap_or_default(),
                        thread_id,
                        messages,
                        unread,
                        participants,
                        last_at: fmt_ts(last_at),
                    })
                })
                .collect()
        })
    }

    /// All messages in a thread, oldest first.
    pub fn thread(&self, thread_id: &str) -> Result<Thread> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT message_id FROM messages WHERE thread_id = ?1 ORDER BY created_at, message_id",
            )?;
            let ids = stmt
                .query_map([thread_id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if ids.is_empty() {
                return Err(Error::NotFound(format!("unknown thread {thread_id:?}")));
            }
            let messages = ids
                .iter()
                .map(|id| load_message(conn, id))
                .collect::<Result<Vec<_>>>()?;
            Ok(Thread {
                thread_id: thread_id.to_string(),
                messages,
            })
        })
    }

    pub fn message(&self, message_id: &str) -> Result<Message> {
        self.read(|conn| load_message(conn, message_id))
    }
}

fn thread_participants(conn: &Connection, thread_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT a.address, NULL FROM agents a
         LEFT JOIN projects p ON p.project_id = a.project_id
         WHERE a.agent_id IN (
           SELECT sender_id FROM messages WHERE thread_id = ?1
           UNION
           SELECT d.agent_id FROM deliveries d JOIN messages m ON m.message_id = d.message_id
            WHERE m.thread_id = ?1)
         ORDER BY a.address",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok(address(
            &r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.as_deref(),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
