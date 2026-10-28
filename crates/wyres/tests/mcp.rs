//! MCP tools exercised through real rmcp clients: in-process and over loopback HTTP.

use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use wyres::mcp::WyresMcp;
use wyres_core::{Clock, Store};

type Client = RunningService<RoleClient, ClientConfig>;

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().cloned().expect("object args");
    client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .expect("tool call")
}

async fn call_ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

/// Error results carry `{"error": {"code", "message"}}` as text.
async fn call_err(client: &Client, tool: &str, args: Value) -> String {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let body: Value = serde_json::from_str(&text).unwrap();
    body["error"]["code"].as_str().unwrap().to_string()
}

async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = WyresMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

#[tokio::test]
async fn exposes_the_tool_surface() {
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;
    let info = client.peer_info().expect("server info");
    assert_eq!(info.server_info.as_ref().unwrap().name, "wyres");
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "acknowledge",
            "activity_feed",
            "activity_summarize",
            "claim_request",
            "ensure_project",
            "fetch_inbox",
            "heartbeat",
            "hold_paths",
            "orphans_list",
            "register_agent",
            "resource_borrow",
            "resource_find",
            "resource_heartbeat",
            "resource_reap",
            "resource_register",
            "resource_release",
            "send_message",
            "threads"
        ]
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn full_flow_over_mcp() {
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;

    let project = call_ok(&client, "ensure_project", json!({ "path": "/work/site" })).await;
    assert_eq!(project["slug"], "site");
    for (name, program) in [("claude-1", "claude"), ("codex-1", "codex")] {
        let a = call_ok(
            &client,
            "register_agent",
            json!({ "name": name, "program": program }),
        )
        .await;
        assert_eq!(a["status"], "active");
    }
    // Unknown programs are rejected by the input schema.
    let bad = client
        .call_tool(
            CallToolRequestParams::new("register_agent").with_arguments(
                json!({ "name": "x", "program": "notreal" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await;
    assert!(bad.is_err() || bad.unwrap().is_error == Some(true));

    // send -> inbox -> ack
    let sent = call_ok(
        &client,
        "send_message",
        json!({
            "from": "claude-1", "to": ["codex-1"], "subject": "Migrate DB",
            "body": "Run the migration please", "importance": "urgent",
            "create_request": true
        }),
    )
    .await;
    let message_id = sent["message_id"].as_str().unwrap().to_string();
    let request_id = sent["request_id"].as_str().unwrap().to_string();

    let inbox = call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await;
    assert_eq!(inbox["unread"], 1);
    assert_eq!(inbox["urgent_unread"], 1);
    assert_eq!(inbox["messages"][0]["subject"], "Migrate DB");
    assert_eq!(inbox["requests"][0]["status"], "open");

    let ack = call_ok(
        &client,
        "acknowledge",
        json!({ "agent": "codex-1", "message_ids": [message_id] }),
    )
    .await;
    assert_eq!(ack["updated"].as_array().unwrap().len(), 1);
    let inbox = call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await;
    assert_eq!(inbox["unread"], 0);
    assert!(inbox["messages"].as_array().unwrap().is_empty());

    // reply without explicit recipients goes back to the sender, in-thread
    let reply = call_ok(
        &client,
        "send_message",
        json!({ "from": "codex-1", "reply_to": message_id, "body": "on it" }),
    )
    .await;
    assert_eq!(reply["to"], json!(["claude-1"]));
    assert_eq!(reply["subject"], "Re: Migrate DB");
    let thread = call_ok(
        &client,
        "threads",
        json!({ "thread_id": sent["thread_id"] }),
    )
    .await;
    assert_eq!(thread["thread"]["messages"].as_array().unwrap().len(), 2);
    let threads = call_ok(&client, "threads", json!({ "agent": "claude-1" })).await;
    assert_eq!(threads["threads"][0]["messages"], 2);

    // request claim -> done
    let claimed = call_ok(
        &client,
        "claim_request",
        json!({ "agent": "codex-1", "request_id": request_id }),
    )
    .await;
    assert_eq!(claimed["status"], "claimed");
    assert_eq!(
        call_err(
            &client,
            "claim_request",
            json!({ "agent": "claude-1", "request_id": request_id })
        )
        .await,
        "conflict"
    );
    let done = call_ok(
        &client,
        "claim_request",
        json!({ "agent": "codex-1", "request_id": request_id, "action": "complete", "note": "migrated" }),
    )
    .await;
    assert_eq!(done["status"], "done");

    // hold acquire -> conflict -> release
    let got = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "claude-1", "project": "/work/site", "paths": ["src/**"], "ttl_seconds": 600 }),
    )
    .await;
    assert_eq!(got["granted"].as_array().unwrap().len(), 1);
    let clash = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "project": "site", "paths": ["src/app.ts"] }),
    )
    .await;
    assert!(clash["granted"].as_array().unwrap().is_empty());
    assert_eq!(clash["conflicts"][0]["held"]["agent"], "claude-1");
    let check = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "action": "check", "project": "site", "paths": ["README.md"] }),
    )
    .await;
    assert!(check["conflicts"].as_array().unwrap().is_empty());
    let listed = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "action": "list" }),
    )
    .await;
    assert_eq!(listed["holds"].as_array().unwrap().len(), 1);

    let hb = call_ok(&client, "heartbeat", json!({ "agent": "claude-1" })).await;
    assert_eq!(hb["renewed_holds"].as_array().unwrap().len(), 1);
    let end = call_ok(
        &client,
        "heartbeat",
        json!({ "agent": "claude-1", "end": true }),
    )
    .await;
    assert_eq!(end["status"], "offline");
    assert_eq!(end["released_holds"].as_array().unwrap().len(), 1);
    let retry = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "project": "site", "paths": ["src/app.ts"] }),
    )
    .await;
    assert_eq!(retry["granted"].as_array().unwrap().len(), 1);

    let feed = call_ok(
        &client,
        "activity_feed",
        json!({ "kinds": ["request"], "since": "1h" }),
    )
    .await;
    let kinds: Vec<&str> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["request.created", "request.claimed", "request.completed"]
    );
    let summary = call_ok(&client, "activity_summarize", json!({})).await;
    assert_eq!(summary["requests"]["open"], 0);
    assert!(summary["text"].as_str().unwrap().contains("event(s) since"));

    assert_eq!(
        call_err(&client, "fetch_inbox", json!({ "agent": "ghost" })).await,
        "not_found"
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn streamable_http_on_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let ct = CancellationToken::new();
    let app = wyres::serve::http_router(store.clone(), ct.clone());
    let addr = wyres::serve::local_bind("127.0.0.1:0").unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let ct = ct.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct.cancelled_owned().await })
                .await
                .unwrap();
        }
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let agent = call_ok(
        &client,
        "register_agent",
        json!({ "name": "grok-bot", "program": "grok_bot", "project": "/work/site" }),
    )
    .await;
    assert_eq!(agent["address"], "grok-bot@site");
    // Written through HTTP, visible to any other connection on the same database.
    assert_eq!(store.agent("grok-bot").unwrap().program, "grok_bot");

    client.cancel().await.unwrap();
    ct.cancel();
    server.await.unwrap();
}
