//! Authenticated access for remote peers (e.g. Grok Bot through a tunnel).
//!
//! With `wyres serve --auth`, every `/mcp` and `/activity` request needs
//! `Authorization: Bearer <token>`. A token acts as exactly one agent and may only touch
//! projects matching its globs. Every remote call is written to the audit log, and reaping
//! is never available remotely.

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use wyres_core::{ActivityKind, ActivityQuery, AuditRecord, Since, Store, TokenScope};

/// The authenticated caller, attached to the request by [`require_token`].
#[derive(Debug, Clone)]
pub struct RemotePeer {
    pub scope: TokenScope,
    pub remote: Option<String>,
}

fn remote_addr(headers: &HeaderMap, req: &Request) -> Option<String> {
    for h in ["cf-connecting-ip", "x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            return Some(format!("{h}: {}", v.split(',').next().unwrap_or(v).trim()));
        }
    }
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.to_string())
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
}

pub async fn audit(store: &Store, record: AuditRecord) {
    let store = store.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Err(e) = store.record_audit(record) {
            tracing::warn!(error = %e, "failed to write audit log");
        }
    })
    .await;
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(json!({ "error": { "code": "unauthorized", "message": message } })),
    )
        .into_response()
}

/// Axum middleware: verify the bearer token and attach a [`RemotePeer`].
pub async fn require_token(State(store): State<Store>, mut req: Request, next: Next) -> Response {
    let operation = format!("{} {}", req.method(), req.uri().path());
    let remote = remote_addr(req.headers(), &req);
    let Some(token) = bearer(req.headers()) else {
        audit(
            &store,
            AuditRecord {
                operation,
                outcome: "denied".into(),
                detail: Some("missing bearer token".into()),
                remote,
                ..Default::default()
            },
        )
        .await;
        return unauthorized("missing bearer token");
    };
    let verify_store = store.clone();
    let verified = tokio::task::spawn_blocking(move || verify_store.verify_token(&token)).await;
    match verified {
        Ok(Ok(scope)) => {
            req.extensions_mut().insert(RemotePeer { scope, remote });
            next.run(req).await
        }
        _ => {
            audit(
                &store,
                AuditRecord {
                    operation,
                    outcome: "denied".into(),
                    detail: Some("invalid or revoked token".into()),
                    remote,
                    ..Default::default()
                },
            )
            .await;
            unauthorized("invalid or revoked token")
        }
    }
}

/// Arguments for the audit log: message bodies and long strings are not recorded.
pub fn redact(args: Option<&Map<String, Value>>) -> String {
    fn walk(key: &str, v: &Value) -> Value {
        match v {
            Value::String(s) if key == "body" || key == "note" => {
                Value::String(format!("<{} bytes>", s.len()))
            }
            Value::String(s) if s.chars().count() > 200 => {
                Value::String(format!("{}…", s.chars().take(200).collect::<String>()))
            }
            Value::Object(m) => {
                Value::Object(m.iter().map(|(k, v)| (k.clone(), walk(k, v))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(key, x)).collect()),
            other => other.clone(),
        }
    }
    match args {
        Some(m) => walk("", &Value::Object(m.clone())).to_string(),
        None => "{}".into(),
    }
}

fn s<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// The acting agent must be the token's own agent; it is filled in when omitted.
fn acting_agent(
    store: &Store,
    scope: &TokenScope,
    args: &mut Map<String, Value>,
    field: &str,
) -> Result<(), String> {
    match s(args, field) {
        None => {
            args.insert(field.into(), Value::String(scope.agent.clone()));
            Ok(())
        }
        Some(who) if store.is_token_agent(scope, who).unwrap_or(false) => Ok(()),
        Some(who) => Err(format!("this token acts as {}, not {who}", scope.agent)),
    }
}

/// Check (and fill in) a remote peer's tool arguments. Returns a denial reason.
pub fn authorize(
    store: &Store,
    scope: &TokenScope,
    tool: &str,
    args: &mut Map<String, Value>,
) -> Result<(), String> {
    let err = |e: wyres_core::Error| e.to_string();
    let acting =
        |args: &mut Map<String, Value>, field: &str| acting_agent(store, scope, args, field);
    let project = |value: Option<&str>, required: bool| -> Result<(), String> {
        match value {
            None if required => Err("remote peers must name a project".into()),
            None => Ok(()),
            Some(p) => {
                if store.project_allowed(scope, p).map_err(err)? {
                    Ok(())
                } else {
                    Err(format!(
                        "project {p:?} is outside this token's project globs"
                    ))
                }
            }
        }
    };
    match tool {
        "resource_reap" => Err("reaping is never available to remote peers".into()),
        "activity_feed" | "activity_summarize" | "resource_find" => Ok(()),
        "orphans_list" => {
            // Remote peers see lapsed registrations only, never a process scan.
            args.insert("scan".into(), Value::Bool(false));
            Ok(())
        }
        "ensure_project" => project(s(args, "path"), true),
        "register_agent" => {
            let (name, slug) = match scope.agent.split_once('@') {
                Some((n, p)) => (n.to_string(), Some(p.to_string())),
                None => (scope.agent.clone(), None),
            };
            if s(args, "name") != Some(name.as_str()) {
                return Err(format!("this token can only register {}", scope.agent));
            }
            match (s(args, "project"), &slug) {
                (None, None) => Ok(()),
                (Some(p), Some(_)) => {
                    project(Some(p), true)?;
                    let same = store.projects().map_err(err)?.into_iter().any(|x| {
                        (x.abs_path == p || x.slug == p) && Some(&x.slug) == slug.as_ref()
                    });
                    if same {
                        Ok(())
                    } else {
                        Err(format!("this token can only register {}", scope.agent))
                    }
                }
                _ => Err(format!("this token can only register {}", scope.agent)),
            }
        }
        "send_message" => acting(args, "from"),
        "fetch_inbox" | "acknowledge" | "claim_request" | "heartbeat" | "resource_borrow"
        | "resource_release" | "resource_heartbeat" => acting(args, "agent"),
        "hold_paths" => {
            acting(args, "agent")?;
            project(s(args, "project"), true)
        }
        "resource_register" => {
            acting(args, "agent")?;
            project(s(args, "project"), false)
        }
        "threads" => match s(args, "thread_id") {
            Some(t) => {
                if store.in_thread(&scope.agent_id, t).map_err(err)? {
                    Ok(())
                } else {
                    Err("remote peers can only read threads they are part of".into())
                }
            }
            None => acting(args, "agent"),
        },
        other => Err(format!("tool {other:?} is not available to remote peers")),
    }
}

#[derive(Debug, Deserialize)]
pub struct ActivityParams {
    pub since: Option<String>,
    pub limit: Option<u32>,
    /// Comma-separated categories.
    pub kinds: Option<String>,
}

/// `GET /activity?since=1h&limit=50&kinds=mail,request` — the same feed as MCP `activity_feed`.
pub async fn activity(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    Query(params): Query<ActivityParams>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let query_store = store.clone();
    let detail = format!(
        "since={:?} limit={:?} kinds={:?}",
        params.since, params.limit, params.kinds
    );
    let result = tokio::task::spawn_blocking(move || {
        let since = params
            .since
            .as_deref()
            .map(|v| Since::parse(v, query_store.now()))
            .transpose()?;
        let kinds = params
            .kinds
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .filter(|k| !k.trim().is_empty())
            .map(|k| k.trim().parse::<ActivityKind>())
            .collect::<wyres_core::Result<Vec<_>>>()?;
        query_store.activity(ActivityQuery {
            since,
            limit: params.limit.unwrap_or(50),
            kinds,
        })
    })
    .await;
    let (status, body, outcome) = match result {
        Ok(Ok(feed)) => (StatusCode::OK, json!(feed), "ok"),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            json!({ "error": e.body() }),
            "error",
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": { "code": "internal", "message": e.to_string() } }),
            "error",
        ),
    };
    if let Some(peer) = peer {
        audit(
            &store,
            AuditRecord {
                token_id: Some(peer.scope.token_id.clone()),
                agent: Some(peer.scope.agent.clone()),
                operation: "GET /activity".into(),
                outcome: outcome.into(),
                detail: Some(detail),
                remote: peer.remote.clone(),
            },
        )
        .await;
    }
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_bodies_and_long_strings() {
        let args =
            json!({ "from": "a", "body": "secret text", "subject": "x".repeat(300), "to": ["b"] });
        let out = redact(args.as_object());
        assert!(!out.contains("secret text"));
        assert!(out.contains("<11 bytes>"));
        assert!(out.len() < 400);
        assert!(out.contains("\"to\":[\"b\"]"));
    }
}
