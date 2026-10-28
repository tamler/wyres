//! Resource CLI, orphan scanning, and human-approved reaping against real processes.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use wyres::mcp::WyresMcp;
use wyres_core::Store;

fn wyres(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wyres"))
        .args(args)
        .env("WYRES_HOME", home)
        .env_remove("WYRES_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = wyres(home, &all);
    assert!(
        out.status.success(),
        "wyres {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn sleeper() -> Child {
    Command::new("sleep")
        .arg("300")
        .stdin(Stdio::null())
        .spawn()
        .unwrap()
}

fn setup(home: &Path) {
    json(home, &["agent", "register", "alice", "--program", "claude"]);
    json(home, &["agent", "register", "bob", "--program", "codex"]);
}

#[test]
fn resource_cli_flow() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    setup(home);
    let reg = json(
        home,
        &[
            "resource",
            "register",
            "dev_server",
            "vite@5173",
            "--agent",
            "alice",
            "--endpoint",
            "http://127.0.0.1:5173",
            "--purpose",
            "frontend",
            "--meta",
            "framework=vite",
        ],
    );
    assert_eq!(reg["outcome"], "registered");
    assert_eq!(reg["resource"]["port"], 5173);
    assert_eq!(reg["resource"]["metadata"]["framework"], "vite");

    let found = json(home, &["resources", "--query", "5173"]);
    assert_eq!(found.as_array().unwrap().len(), 1);

    // A second agent finds it and borrows instead of starting another.
    let dup = wyres(
        home,
        &[
            "--json",
            "resource",
            "register",
            "dev_server",
            "vite@5173",
            "--agent",
            "bob",
        ],
    );
    assert!(!dup.status.success());
    let err: Value = serde_json::from_slice(&dup.stdout).unwrap();
    assert_eq!(err["error"]["code"], "conflict");
    let b = json(home, &["borrow", "dev_server:vite@5173", "--agent", "bob"]);
    assert_eq!(b["resource"]["refcount"], 2);
    json(home, &["resource", "heartbeat", "--agent", "bob"]);
    let rel = json(
        home,
        &["release", "dev_server:vite@5173", "--agent", "alice"],
    );
    assert_eq!(rel["outcome"], "transferred");
    assert_eq!(rel["resource"]["owner"], "bob");

    // Session end releases resource leases unless --keep.
    json(home, &["heartbeat", "--agent", "bob", "--end", "--keep"]);
    assert_eq!(json(home, &["resources"])[0]["status"], "live");
    json(home, &["heartbeat", "--agent", "bob", "--end"]);
    assert_eq!(json(home, &["resources"])[0]["status"], "lapsed");

    let orphans = json(home, &["orphans", "--no-scan"]);
    let list = orphans["orphans"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["source"], "lapsed_lease");
    assert!(orphans["scan"].is_null());

    // Human output renders.
    let text = wyres(home, &["resources"]);
    assert!(String::from_utf8_lossy(&text.stdout).contains("vite@5173"));
}

#[test]
fn reap_requires_lapse_grace_and_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    setup(home);
    let mut child = sleeper();
    let pid = child.id().to_string();
    let reg = json(
        home,
        &[
            "resource", "register", "long_job", "eval-run", "--agent", "alice", "--pid", &pid,
        ],
    );
    let id = reg["resource"]["resource_id"].as_str().unwrap().to_string();

    // Live: blocked even with --confirm.
    let plan = json(home, &["reap", &id, "--grace", "0"]);
    assert_eq!(plan["eligible"], false);
    assert!(plan["blockers"][0].as_str().unwrap().contains("is live"));
    let refused = wyres(home, &["reap", &id, "--grace", "0", "--confirm"]);
    assert!(!refused.status.success());
    assert!(child.try_wait().unwrap().is_none(), "still running");

    json(home, &["release", &id, "--agent", "alice"]);
    // Lapsed but inside the grace period: blocked.
    let plan = json(home, &["reap", &id]);
    assert_eq!(plan["eligible"], false);
    assert!(plan["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b.as_str().unwrap().contains("grace period is 600s")));

    // Eligible, but without --confirm nothing happens.
    let plan = json(home, &["reap", &id, "--grace", "0"]);
    assert_eq!(plan["eligible"], true, "{plan}");
    assert_eq!(plan["executed"], false);
    assert!(plan["command"].as_str().unwrap().contains("sleep 300"));
    assert!(child.try_wait().unwrap().is_none(), "plan must not signal");

    let done = json(home, &["reap", &id, "--grace", "0", "--confirm"]);
    assert_eq!(done["executed"], true);
    let status = child.wait().unwrap();
    assert!(!status.success(), "terminated by signal");
    let all = json(home, &["resources", "--all"]);
    assert_eq!(all[0]["status"], "released");
    let feed = json(home, &["activity", "--kinds", "resource"]);
    assert!(feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["kind"] == "resource.reaped"));
}

#[tokio::test]
async fn mcp_reap_never_terminates() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    setup(&home);
    let mut child = sleeper();
    let pid = child.id();
    let reg = json(
        &home,
        &[
            "resource",
            "register",
            "long_job",
            "train",
            "--agent",
            "alice",
            "--pid",
            &pid.to_string(),
        ],
    );
    let id = reg["resource"]["resource_id"].as_str().unwrap().to_string();
    json(&home, &["release", &id, "--agent", "alice"]);

    let store = Store::open(&home).unwrap();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = WyresMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    let client = ClientConfig::default().serve(client_io).await.unwrap();
    let args =
        json!({ "agent": "bob", "target": id, "grace_seconds": 0, "reason": "lease lapsed" });
    let res = client
        .call_tool(
            CallToolRequestParams::new("resource_reap")
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    let out = res.structured_content.unwrap();
    assert_eq!(out["executed"], false);
    assert_eq!(out["plan"]["eligible"], true);
    assert!(out["human_command"].as_str().unwrap().contains("--confirm"));
    assert!(child.try_wait().unwrap().is_none(), "MCP must never signal");

    let orphans = client
        .call_tool(
            CallToolRequestParams::new("orphans_list")
                .with_arguments(json!({ "scan": false }).as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(orphans["orphans"][0]["target"], id.as_str());
    client.cancel().await.unwrap();

    let feed = json(&home, &["activity", "--kinds", "resource"]);
    assert!(feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["summary"]
            .as_str()
            .unwrap()
            .contains("bob asked a human to reap")));
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn scan_finds_unregistered_listeners() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    setup(home);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let me = std::process::id();

    let report = json(home, &["orphans"]);
    if report["scan"]["scanner"] == "none" {
        eprintln!("no lsof/ss on this machine; skipping scan assertions");
        return;
    }
    let ours =
        |r: &Value| {
            r["orphans"].as_array().unwrap().iter().any(|o| {
                o["source"] == "unregistered_listener" && o["port"] == port && o["pid"] == me
            })
        };
    assert!(ours(&report), "{report}");
    // A pid target plan for it is computed (this process is young; grace blocks it).
    let plan = json(home, &["reap", &format!("pid:{me}"), "--grace", "3600"]);
    assert_eq!(plan["eligible"], false);

    // Once registered, it is no longer an orphan.
    json(
        home,
        &[
            "resource",
            "register",
            "port",
            &format!("tcp:127.0.0.1:{port}"),
            "--agent",
            "alice",
        ],
    );
    assert!(!ours(&json(home, &["orphans"])));
    drop(listener);
}
