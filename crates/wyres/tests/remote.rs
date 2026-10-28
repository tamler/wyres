//! Authenticated remote access: bearer tokens, scoping, audit log, GET /activity.

use std::net::SocketAddr;

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wyres::serve::{http_router, HttpOptions};
use wyres_core::{Program, RegisterAgent, SendMessage, Store};

type Client = RunningService<RoleClient, ClientConfig>;

struct Server {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    ct: CancellationToken,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/secret/vault").unwrap();
    for (name, program, project) in [
        ("grok-bot", Program::GrokBot, None),
        ("alice", Program::Claude, Some("/work/shop")),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: project.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        auth: true,
        ..Default::default()
    };
    let app = http_router(store.clone(), ct.clone(), &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Server {
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        ct,
    }
}

async fn connect(base: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn error_code(res: &CallToolResult) -> Option<String> {
    if res.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&text).unwrap();
    v["error"]["code"].as_str().map(str::to_string)
}

#[tokio::test]
async fn bearer_tokens_scope_and_audit_remote_calls() {
    let srv = start().await;
    let http = reqwest::Client::new();

    // No or bad token: 401 on /mcp and /activity; /health stays open.
    let r = http
        .get(format!("{}/activity", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = http
        .post(format!("{}/mcp", srv.base))
        .header("authorization", "Bearer wyr_nope_nope")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let health: Value = serde_json::from_str(
        &http
            .get(format!("{}/health", srv.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(health["auth"], true);

    let token = srv
        .store
        .create_token("grok-bot", &["/work/*".into()], Some("bot".into()), None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;

    // Acting agent is filled in, and only the token's agent is allowed.
    let inbox = call(&client, "fetch_inbox", json!({})).await;
    assert_eq!(error_code(&inbox), None);
    assert_eq!(inbox.structured_content.unwrap()["agent"], "grok-bot");
    let other = call(&client, "fetch_inbox", json!({ "agent": "alice" })).await;
    assert_eq!(error_code(&other).as_deref(), Some("forbidden"));

    let sent = call(
        &client,
        "send_message",
        json!({ "to": ["alice"], "subject": "status?", "body": "TOP SECRET BODY" }),
    )
    .await;
    assert_eq!(error_code(&sent), None);
    let spoof = call(
        &client,
        "send_message",
        json!({ "from": "alice", "to": ["grok-bot"], "subject": "x", "body": "y" }),
    )
    .await;
    assert_eq!(error_code(&spoof).as_deref(), Some("forbidden"));

    // Project globs.
    let ok = call(
        &client,
        "hold_paths",
        json!({ "project": "/work/shop", "paths": ["docs/**"] }),
    )
    .await;
    assert_eq!(error_code(&ok), None);
    for args in [
        json!({ "project": "/secret/vault", "paths": ["a"] }),
        json!({ "project": "vault", "paths": ["a"] }),
        json!({ "action": "list" }),
    ] {
        let denied = call(&client, "hold_paths", args).await;
        assert_eq!(error_code(&denied).as_deref(), Some("forbidden"));
    }
    let denied = call(&client, "ensure_project", json!({ "path": "/elsewhere/x" })).await;
    assert_eq!(error_code(&denied).as_deref(), Some("forbidden"));

    // Never reap remotely; orphan listing never scans processes.
    let reap = call(&client, "resource_reap", json!({ "target": "pid:1" })).await;
    assert_eq!(error_code(&reap).as_deref(), Some("forbidden"));
    let orphans = call(&client, "orphans_list", json!({ "scan": true })).await;
    assert!(orphans.structured_content.unwrap()["scan"].is_null());

    // Threads: only ones the token's agent is part of.
    let private = srv
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["alice".into()],
            subject: "note to self".into(),
            body: "private".into(),
            ..Default::default()
        })
        .unwrap();
    let t = call(
        &client,
        "threads",
        json!({ "thread_id": private.thread_id }),
    )
    .await;
    assert_eq!(error_code(&t).as_deref(), Some("forbidden"));

    // Feed reads are allowed, over MCP and HTTP.
    let feed = call(&client, "activity_feed", json!({ "kinds": ["mail"] })).await;
    assert_eq!(error_code(&feed), None);
    let r = http
        .get(format!("{}/activity?since=1h&kinds=mail,hold", srv.base))
        .header("authorization", format!("Bearer {}", token.secret))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    assert!(!body["items"].as_array().unwrap().is_empty());
    assert!(!body.to_string().contains("TOP SECRET BODY"));

    // Audit log: every remote call, denials included, without message bodies.
    let audit = srv.store.audit_log(100).unwrap();
    let ops: Vec<(&str, &str)> = audit
        .iter()
        .map(|e| (e.operation.as_str(), e.outcome.as_str()))
        .collect();
    assert!(ops.contains(&("GET /activity", "denied")));
    assert!(ops.contains(&("POST /mcp", "denied")));
    assert!(ops.contains(&("tools/call send_message", "ok")));
    assert!(ops.contains(&("tools/call send_message", "denied")));
    assert!(ops.contains(&("tools/call resource_reap", "denied")));
    assert!(ops.contains(&("GET /activity", "ok")));
    assert!(audit
        .iter()
        .filter(|e| e.token_id.is_some())
        .all(|e| e.agent.as_deref() == Some("grok-bot")));
    let all = serde_json::to_string(&audit).unwrap();
    assert!(!all.contains("TOP SECRET BODY"), "bodies are redacted");
    assert!(all.contains("<15 bytes>"));

    // Revocation takes effect on the next request.
    srv.store.revoke_token(&token.token.token_id).unwrap();
    let r = http
        .get(format!("{}/activity", srv.base))
        .header("authorization", format!("Bearer {}", token.secret))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let after = client
        .call_tool(CallToolRequestParams::new("fetch_inbox").with_arguments(Default::default()))
        .await;
    assert!(after.is_err() || after.unwrap().is_error == Some(true));
    srv.ct.cancel();
}

#[tokio::test]
async fn expired_and_unscoped_tokens() {
    let srv = start().await;
    // No project globs: project-scoped calls are denied, mailbox calls work.
    let token = srv.store.create_token("grok-bot", &[], None, None).unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let hold = call(
        &client,
        "hold_paths",
        json!({ "project": "/work/shop", "paths": ["a"] }),
    )
    .await;
    assert_eq!(error_code(&hold).as_deref(), Some("forbidden"));
    let hb = call(&client, "heartbeat", json!({})).await;
    assert_eq!(error_code(&hb), None);

    // Registering itself is fine; registering someone else is not.
    let me = call(
        &client,
        "register_agent",
        json!({ "name": "grok-bot", "program": "grok_bot" }),
    )
    .await;
    assert_eq!(error_code(&me), None);
    let other = call(
        &client,
        "register_agent",
        json!({ "name": "mallory", "program": "other" }),
    )
    .await;
    assert_eq!(error_code(&other).as_deref(), Some("forbidden"));

    assert!(srv
        .store
        .create_token("grok-bot", &["[".into()], None, None)
        .is_err());
    assert!(srv.store.create_token("nobody", &[], None, None).is_err());
    let short = srv
        .store
        .create_token("grok-bot", &[], None, Some(1))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(srv.store.verify_token(&short.secret).is_err());
    assert!(srv.store.verify_token("not-a-token").is_err());
    let tampered = format!("{}x", token.secret);
    assert!(srv.store.verify_token(&tampered).is_err());
    srv.ct.cancel();
}

#[test]
fn token_cli_and_bind_rules() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_wyres"))
            .args(args)
            .env("WYRES_HOME", dir.path())
            .output()
            .unwrap()
    };
    assert!(
        run(&["agent", "register", "grok-bot", "--program", "grok_bot"])
            .status
            .success()
    );
    let out = run(&[
        "--json",
        "token",
        "create",
        "--agent",
        "grok-bot",
        "--project",
        "/work/*",
        "--expires",
        "30d",
    ]);
    assert!(out.status.success());
    let created: Value = serde_json::from_slice(&out.stdout).unwrap();
    let secret = created["secret"].as_str().unwrap();
    assert!(secret.starts_with("wyr_"));
    let list = run(&["--json", "token", "list"]);
    let list_text = String::from_utf8_lossy(&list.stdout).to_string();
    assert!(!list_text.contains(secret), "secrets are never listed");
    let listed: Value = serde_json::from_str(&list_text).unwrap();
    assert_eq!(listed[0]["agent"], "grok-bot");
    assert!(listed[0]["expires_at"].is_string());
    let id = created["token_id"].as_str().unwrap();
    assert!(run(&["token", "revoke", id]).status.success());
    let listed: Value = serde_json::from_slice(&run(&["--json", "token", "list"]).stdout).unwrap();
    assert!(listed[0]["revoked_at"].is_string());

    // Non-loopback binds need --auth and --allow-remote-bind.
    let no_auth = run(&["serve", "--bind", "0.0.0.0:0"]);
    assert!(!no_auth.status.success());
    let auth_only = run(&["serve", "--auth", "--bind", "0.0.0.0:0"]);
    assert!(!auth_only.status.success());
    assert!(String::from_utf8_lossy(&auth_only.stderr).contains("--allow-remote-bind"));
    let flag_only = run(&["serve", "--allow-remote-bind", "--bind", "0.0.0.0:0"]);
    assert!(!flag_only.status.success(), "clap requires --auth");
}
