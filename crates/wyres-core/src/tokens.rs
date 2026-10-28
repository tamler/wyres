//! Bearer tokens for remote peers and the audit log of remote calls.
//!
//! A token acts as exactly one agent and may only touch projects matching its globs. Only a
//! SHA-256 hash of the secret is stored; the full token is shown once, at creation.

use globset::GlobBuilder;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::identity::{address, resolve_agent};
use crate::store::{new_id, Store};
use crate::time::{fmt_opt, fmt_ts};

const TOKEN_PREFIX: &str = "wyr_";

/// (agent_id, project_globs JSON, secret_hash, expires_at, revoked_at)
type StoredToken = (String, String, String, Option<i64>, Option<i64>);

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Token {
    pub token_id: String,
    pub agent: String,
    pub label: Option<String>,
    pub project_globs: Vec<String>,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// A freshly created token. `secret` is the only copy of the bearer value.
#[derive(Debug, Clone, Serialize)]
pub struct NewToken {
    #[serde(flatten)]
    pub token: Token,
    pub secret: String,
}

/// What a verified bearer token may do.
#[derive(Debug, Clone)]
pub struct TokenScope {
    pub token_id: String,
    pub agent_id: String,
    /// `name` or `name@project`.
    pub agent: String,
    pub project_globs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditEntry {
    pub audit_id: i64,
    pub ts: String,
    pub token_id: Option<String>,
    pub agent: Option<String>,
    /// e.g. `tools/call send_message` or `GET /activity`.
    pub operation: String,
    /// `ok`, `error`, or `denied`.
    pub outcome: String,
    /// Redacted arguments or the error.
    pub detail: Option<String>,
    /// Forwarding header or peer address, when known.
    pub remote: Option<String>,
}

/// Input for [`Store::record_audit`].
#[derive(Debug, Clone, Default)]
pub struct AuditRecord {
    pub token_id: Option<String>,
    pub agent: Option<String>,
    pub operation: String,
    pub outcome: String,
    pub detail: Option<String>,
    pub remote: Option<String>,
}

fn hash(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)
        .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether `candidate` matches any of the token's project globs.
fn glob_allows(globs: &[String], candidates: &[String]) -> bool {
    globs.iter().any(|g| {
        let Ok(glob) = GlobBuilder::new(g).literal_separator(true).build() else {
            return false;
        };
        let m = glob.compile_matcher();
        candidates.iter().any(|c| m.is_match(c))
    })
}

fn token_from_row(r: &Row<'_>) -> rusqlite::Result<Token> {
    let globs: String = r.get(4)?;
    Ok(Token {
        token_id: r.get(0)?,
        agent: address(
            &r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?.as_deref(),
        ),
        label: r.get(3)?,
        project_globs: serde_json::from_str(&globs).unwrap_or_default(),
        created_at: fmt_ts(r.get(5)?),
        expires_at: fmt_opt(r.get(6)?),
        revoked_at: fmt_opt(r.get(7)?),
        last_used_at: fmt_opt(r.get(8)?),
    })
}

const TOKEN_SELECT: &str = "SELECT t.token_id, a.name, p.slug, t.label, t.project_globs,
       t.created_at, t.expires_at, t.revoked_at, t.last_used_at
     FROM tokens t JOIN agents a ON a.agent_id = t.agent_id
     LEFT JOIN projects p ON p.project_id = a.project_id";

fn get_token(conn: &Connection, id: &str) -> Result<Token> {
    conn.query_row(
        &format!("{TOKEN_SELECT} WHERE t.token_id = ?1"),
        [id],
        token_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("unknown token {id:?}")))
}

impl Store {
    /// Create a bearer token that acts as `agent` within `project_globs`.
    pub fn create_token(
        &self,
        agent: &str,
        project_globs: &[String],
        label: Option<String>,
        expires_in_seconds: Option<i64>,
    ) -> Result<NewToken> {
        for g in project_globs {
            GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Invalid(format!("invalid project glob {g:?}: {e}")))?;
        }
        if expires_in_seconds.is_some_and(|s| s <= 0) {
            return Err(Error::Invalid("expiry must be positive".into()));
        }
        let secret_part = random_hex(32)?;
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let token_id = new_id();
            let expires_at = expires_in_seconds.map(|s| tx.now + s * 1000);
            let secret = format!("{TOKEN_PREFIX}{token_id}_{secret_part}");
            tx.emit(
                "human (wyres token create)",
                "token.created",
                json!({
                    "token_id": token_id,
                    "agent_id": me.id,
                    "project_globs": project_globs,
                    "label": label,
                    "expires_at": expires_at,
                }),
            )?;
            tx.execute(
                "INSERT INTO tokens (token_id, node_id, agent_id, label, project_globs, secret_hash,
                   created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    token_id,
                    tx.node_id,
                    me.id,
                    label,
                    serde_json::to_string(project_globs)?,
                    hash(&secret),
                    tx.now,
                    expires_at
                ],
            )?;
            Ok(NewToken {
                token: get_token(tx, &token_id)?,
                secret,
            })
        })
    }

    pub fn tokens(&self) -> Result<Vec<Token>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{TOKEN_SELECT} WHERE t.node_id = ?1 ORDER BY t.created_at DESC"
            ))?;
            let rows = stmt.query_map([self.node_id()], token_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn revoke_token(&self, token_id: &str) -> Result<Token> {
        self.write(|tx| {
            let t = get_token(tx, token_id)?;
            if t.revoked_at.is_none() {
                tx.emit(
                    "human (wyres token revoke)",
                    "token.revoked",
                    json!({ "token_id": token_id }),
                )?;
                tx.execute(
                    "UPDATE tokens SET revoked_at = ?2 WHERE token_id = ?1",
                    params![token_id, tx.now],
                )?;
            }
            get_token(tx, token_id)
        })
    }

    /// Check a bearer value. Unknown, malformed, revoked and expired tokens all fail the
    /// same way so callers learn nothing about which it was.
    pub fn verify_token(&self, bearer: &str) -> Result<TokenScope> {
        let denied = || Error::Forbidden("invalid or revoked token".into());
        let token_id = bearer
            .strip_prefix(TOKEN_PREFIX)
            .and_then(|rest| rest.split_once('_'))
            .map(|(id, _)| id.to_string())
            .ok_or_else(denied)?;
        let digest = hash(bearer);
        self.write(|tx| {
            let row: Option<StoredToken> = tx
                .query_row(
                    "SELECT agent_id, project_globs, secret_hash, expires_at, revoked_at
                     FROM tokens WHERE token_id = ?1",
                    [&token_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((agent_id, globs, stored, expires_at, revoked_at)) = row else {
                return Err(denied());
            };
            let expired = expires_at.is_some_and(|e| e <= tx.now);
            if stored != digest || revoked_at.is_some() || expired {
                return Err(denied());
            }
            tx.execute(
                "UPDATE tokens SET last_used_at = ?2 WHERE token_id = ?1",
                params![token_id, tx.now],
            )?;
            let agent = crate::identity::get_agent(tx, &agent_id)?;
            Ok(TokenScope {
                token_id,
                agent_id,
                agent: agent.address,
                project_globs: serde_json::from_str(&globs).unwrap_or_default(),
            })
        })
    }

    /// Whether a token may act on `project` (an absolute path or slug, existing or new).
    pub fn project_allowed(&self, scope: &TokenScope, project: &str) -> Result<bool> {
        self.read(|conn| {
            let mut candidates = vec![project.trim_end_matches('/').to_string()];
            let known: Option<(String, String)> = conn
                .query_row(
                    "SELECT abs_path, slug FROM projects
                     WHERE node_id = ?1 AND (abs_path = ?2 OR slug = ?2)",
                    params![self.node_id(), candidates[0]],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((path, slug)) = known {
                candidates.push(path);
                candidates.push(slug);
            }
            Ok(glob_allows(&scope.project_globs, &candidates))
        })
    }

    /// Whether `who` resolves to the token's own agent.
    pub fn is_token_agent(&self, scope: &TokenScope, who: &str) -> Result<bool> {
        self.read(|conn| Ok(resolve_agent(conn, self.node_id(), who)?.id == scope.agent_id))
    }

    /// Whether the agent sent or received any message in the thread.
    pub fn in_thread(&self, agent_id: &str, thread_id: &str) -> Result<bool> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM messages m WHERE m.thread_id = ?1 AND (m.sender_id = ?2 OR
                       EXISTS (SELECT 1 FROM deliveries d WHERE d.message_id = m.message_id
                               AND d.agent_id = ?2))
                     LIMIT 1",
                    params![thread_id, agent_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    pub fn record_audit(&self, entry: AuditRecord) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO audit_log (node_id, ts, token_id, agent, operation, outcome, detail, remote)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    tx.node_id,
                    tx.now,
                    entry.token_id,
                    entry.agent,
                    entry.operation,
                    entry.outcome,
                    entry.detail,
                    entry.remote
                ],
            )?;
            Ok(())
        })
    }

    /// Most recent audit entries, newest last.
    pub fn audit_log(&self, limit: u32) -> Result<Vec<AuditEntry>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT audit_id, ts, token_id, agent, operation, outcome, detail, remote FROM (
                   SELECT * FROM audit_log WHERE node_id = ?1 ORDER BY audit_id DESC LIMIT ?2
                 ) ORDER BY audit_id",
            )?;
            let rows = stmt.query_map(params![self.node_id(), limit], |r| {
                Ok(AuditEntry {
                    audit_id: r.get(0)?,
                    ts: fmt_ts(r.get(1)?),
                    token_id: r.get(2)?,
                    agent: r.get(3)?,
                    operation: r.get(4)?,
                    outcome: r.get(5)?,
                    detail: r.get(6)?,
                    remote: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        let g = vec!["/work/*".to_string(), "docs-site".to_string()];
        assert!(glob_allows(&g, &["/work/shop".into()]));
        assert!(!glob_allows(&g, &["/work/a/b".into()]));
        assert!(glob_allows(&g, &["docs-site".into()]));
        assert!(!glob_allows(&g, &["/other/shop".into()]));
        assert!(glob_allows(&["**".to_string()], &["/any/where".into()]));
        assert!(!glob_allows(&[], &["/work/shop".into()]));
    }
}
