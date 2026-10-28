//! Federation transport: hub endpoints (`/fed/pair`, `/fed/push`, `/fed/pull`) and the spoke
//! client that dials out to them (`wyres pair join`, `wyres sync`).
//!
//! Node-to-node requests are signed with the sending node's key over method, path, time and
//! body; the hub checks them against the key it pinned at pairing.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use wyres_core::{
    AuditRecord, IngestReport, PairRequest, PairResponse, Peer, PullResponse, PushRequest,
    Rejected, Store,
};

const H_NODE: &str = "x-wyres-node";
const H_TIME: &str = "x-wyres-time";
const H_SIG: &str = "x-wyres-signature";
const BATCH: u32 = 500;

/// Routes a hub serves when started with `wyres serve --federation`.
pub fn router(store: Store) -> axum::Router {
    axum::Router::new()
        .route("/fed/pair", axum::routing::post(pair))
        .route("/fed/push", axum::routing::post(push))
        .route("/fed/pull", axum::routing::get(pull))
        .with_state(store)
}

fn error(status: StatusCode, e: &wyres_core::Error) -> Response {
    (status, axum::Json(json!({ "error": e.body() }))).into_response()
}

fn status_for(e: &wyres_core::Error) -> StatusCode {
    match e.code() {
        "forbidden" => StatusCode::FORBIDDEN,
        "not_found" => StatusCode::NOT_FOUND,
        "invalid" => StatusCode::BAD_REQUEST,
        "conflict" | "ambiguous" => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> wyres_core::Result<T> + Send + 'static,
) -> wyres_core::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(wyres_core::Error::Invalid(format!("task failed: {e}"))))
}

async fn record(
    store: &Store,
    node: Option<String>,
    operation: &str,
    outcome: &str,
    detail: String,
) {
    crate::remote::audit(
        store,
        AuditRecord {
            token_id: None,
            agent: node.map(|n| format!("node:{n}")),
            operation: operation.to_string(),
            outcome: outcome.to_string(),
            detail: Some(detail),
            remote: None,
        },
    )
    .await;
}

async fn pair(State(store): State<Store>, axum::Json(req): axum::Json<PairRequest>) -> Response {
    let node = req.node.node_id.clone();
    let s = store.clone();
    match blocking(move || s.accept_pairing(&req)).await {
        Ok(resp) => {
            record(&store, Some(node), "POST /fed/pair", "ok", "paired".into()).await;
            axum::Json(resp).into_response()
        }
        Err(e) => {
            record(
                &store,
                Some(node),
                "POST /fed/pair",
                "denied",
                e.to_string(),
            )
            .await;
            error(status_for(&e), &e)
        }
    }
}

/// Verify the signature headers; returns the calling node id.
async fn authenticate(
    store: &Store,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<String, Response> {
    let get = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (Some(node), Some(time), Some(sig)) = (get(H_NODE), get(H_TIME), get(H_SIG)) else {
        let e = wyres_core::Error::Forbidden("missing node signature headers".into());
        return Err(error(StatusCode::UNAUTHORIZED, &e));
    };
    let time: i64 = time.parse().unwrap_or_default();
    let path = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let (s, m, b, n) = (
        store.clone(),
        method.to_string(),
        body.to_vec(),
        node.clone(),
    );
    match blocking(move || s.verify_peer_request(&n, &m, &path, time, &b, &sig)).await {
        Ok(_) => Ok(node),
        Err(e) => {
            record(
                store,
                Some(node),
                &format!("{method} {}", uri.path()),
                "denied",
                e.to_string(),
            )
            .await;
            Err(error(StatusCode::UNAUTHORIZED, &e))
        }
    }
}

async fn push(
    State(store): State<Store>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let node = match authenticate(&store, &method, &uri, &headers, &body).await {
        Ok(n) => n,
        Err(r) => return r,
    };
    let req: PushRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let e = wyres_core::Error::Invalid(format!("invalid push body: {e}"));
            return error(StatusCode::BAD_REQUEST, &e);
        }
    };
    let (s, n) = (store.clone(), node.clone());
    match blocking(move || s.ingest(&n, &req.events, &[])).await {
        Ok(report) => {
            let detail = format!(
                "accepted {}, duplicates {}, rejected {}",
                report.accepted,
                report.duplicates,
                report.rejected.len()
            );
            record(&store, Some(node), "POST /fed/push", "ok", detail).await;
            axum::Json(report).into_response()
        }
        Err(e) => error(status_for(&e), &e),
    }
}

async fn pull(
    State(store): State<Store>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let node = match authenticate(&store, &method, &uri, &headers, b"").await {
        Ok(n) => n,
        Err(r) => return r,
    };
    let mut after = 0i64;
    let mut limit = BATCH;
    for pair in uri.query().unwrap_or_default().split('&') {
        match pair.split_once('=') {
            Some(("after", v)) => after = v.parse().unwrap_or(0),
            Some(("limit", v)) => limit = v.parse().unwrap_or(BATCH).clamp(1, 2000),
            _ => {}
        }
    }
    let (s, n) = (store.clone(), node.clone());
    match blocking(move || s.pull_for(&n, after, limit)).await {
        Ok(resp) => {
            let detail = format!("after {after}: {} event(s)", resp.events.len());
            record(&store, Some(node), "GET /fed/pull", "ok", detail).await;
            axum::Json(resp).into_response()
        }
        Err(e) => error(status_for(&e), &e),
    }
}

// ---------------------------------------------------------------------------
// Spoke client

fn agent_for(url: &str) -> ureq::Agent {
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.starts_with(p));
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)));
    if loopback {
        config = config.proxy(None);
    }
    ureq::Agent::new_with_config(config.build())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn read<T: serde::de::DeserializeOwned>(
    mut resp: ureq::http::Response<ureq::Body>,
    what: &str,
) -> Result<T> {
    let status = resp.status();
    let text = resp
        .body_mut()
        .read_to_string()
        .with_context(|| format!("reading {what} response"))?;
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        bail!("{what} failed ({status}): {msg}");
    }
    serde_json::from_str(&text).with_context(|| format!("decoding {what} response"))
}

fn signed(
    store: &Store,
    agent: &ureq::Agent,
    base: &str,
    method: &str,
    path_and_query: &str,
    body: Option<Vec<u8>>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let time = now_ms();
    let bytes = body.clone().unwrap_or_default();
    let sig = store.sign_request(method, path_and_query, time, &bytes);
    let url = format!("{}{path_and_query}", base.trim_end_matches('/'));
    let node = store.node_id().to_string();
    let resp = match body {
        Some(b) => agent
            .post(&url)
            .header(H_NODE, &node)
            .header(H_TIME, time.to_string())
            .header(H_SIG, &sig)
            .header("content-type", "application/json")
            .send(&b[..]),
        None => agent
            .get(&url)
            .header(H_NODE, &node)
            .header(H_TIME, time.to_string())
            .header(H_SIG, &sig)
            .call(),
    };
    resp.map_err(|e| anyhow!("cannot reach the hub at {base}: {e}"))
}

/// Spoke: redeem an invite code with the hub and record it as this node's hub.
pub fn join(store: &Store, code: &str) -> Result<Peer> {
    let (invite, req) = store.prepare_join(code)?;
    let agent = agent_for(&invite.hub_url);
    let resp = agent
        .post(&format!("{}/fed/pair", invite.hub_url))
        .header("content-type", "application/json")
        .send(serde_json::to_vec(&req)?.as_slice())
        .map_err(|e| anyhow!("cannot reach the hub at {}: {e}", invite.hub_url))?;
    let paired: PairResponse = read(resp, "pairing")?;
    Ok(store.complete_join(&invite, &paired)?)
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub hub: String,
    pub pushed: usize,
    pub push_duplicates: usize,
    pub push_rejected: Vec<Rejected>,
    pub pulled: usize,
    pub applied: usize,
    pub pull_duplicates: usize,
    pub pull_rejected: Vec<Rejected>,
}

/// Spoke: push events this node authored, then pull everyone else's, until caught up.
pub fn sync(store: &Store) -> Result<SyncReport> {
    let (hub, url) = store
        .hub()?
        .ok_or_else(|| anyhow!("this node has no hub; pair it with `wyres pair join <code>` (hubs do not sync, spokes dial them)"))?;
    let agent = agent_for(&url);
    let mut report = SyncReport {
        hub: url.clone(),
        ..Default::default()
    };
    // Push.
    loop {
        let (cursor, _) = store.push_cursor(&hub)?;
        let (events, high) = store.outbound_events(cursor, BATCH)?;
        if high == cursor {
            break;
        }
        if !events.is_empty() {
            let body = serde_json::to_vec(&PushRequest { events })?;
            let resp = signed(store, &agent, &url, "POST", "/fed/push", Some(body))?;
            let r: IngestReport = read(resp, "push")?;
            report.pushed += r.accepted;
            report.push_duplicates += r.duplicates;
            report.push_rejected.extend(r.rejected);
        }
        store.set_push_cursor(&hub, high)?;
    }
    // Pull.
    loop {
        let (_, cursor) = store.push_cursor(&hub)?;
        let path = format!("/fed/pull?after={cursor}&limit={BATCH}");
        let resp = signed(store, &agent, &url, "GET", &path, None)?;
        let page: PullResponse = read(resp, "pull")?;
        report.pulled += page.events.len();
        let r = store.ingest(&hub, &page.events, &page.nodes)?;
        report.applied += r.accepted;
        report.pull_duplicates += r.duplicates;
        report.pull_rejected.extend(r.rejected);
        store.set_pull_cursor(&hub, page.cursor)?;
        if !page.more || page.cursor == cursor {
            break;
        }
    }
    Ok(report)
}
