//! Drive the `wyres` binary the way hooks and scripts do.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn wyres(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wyres"))
        .args(args)
        .env("WYRES_HOME", home)
        .env_remove("WYRES_AGENT")
        .env_remove("WYRES_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run wyres")
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = wyres(home, &all);
    assert!(
        out.status.success(),
        "wyres {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json output")
}

#[test]
fn init_creates_home_with_node() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let status = json(&home, &["init"]);
    assert_eq!(status["node"]["node_id"].as_str().unwrap().len(), 26);
    assert!(home.join("wyres.db").exists());
    let events = json(&home, &["events"]);
    assert_eq!(events[0]["kind"], "node.created");
}

#[test]
fn mail_requests_and_holds_from_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_s = repo.to_str().unwrap();

    json(
        &home,
        &["agent", "register", "alice", "--program", "claude"],
    );
    json(&home, &["agent", "register", "bob", "--program", "grok"]);

    let sent = json(
        &home,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "Review PR",
            "--body",
            "please",
            "--due",
            "3600",
        ],
    );
    let mid = sent["message_id"].as_str().unwrap().to_string();
    let rid = sent["request_id"].as_str().unwrap().to_string();

    let inbox = json(&home, &["inbox", "--agent", "bob", "--unread"]);
    assert_eq!(inbox["unread"], 1);
    assert_eq!(inbox["requests"][0]["request_id"], rid.as_str());

    json(&home, &["ack", "--agent", "bob", &mid]);
    let reply = json(&home, &["reply", &mid, "--from", "bob", "--body", "lgtm"]);
    assert_eq!(reply["thread_id"], sent["thread_id"]);
    let thread = json(&home, &["thread", sent["thread_id"].as_str().unwrap()]);
    assert_eq!(thread["messages"].as_array().unwrap().len(), 2);

    let claimed = json(&home, &["request", "claim", &rid, "--agent", "bob"]);
    assert_eq!(claimed["owner"], "bob");
    let done = json(
        &home,
        &[
            "request", "complete", &rid, "--agent", "bob", "--note", "merged",
        ],
    );
    assert_eq!(done["status"], "done");
    let listed = json(&home, &["requests", "--status", "done"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let held = json(
        &home,
        &[
            "hold",
            "acquire",
            "--agent",
            "alice",
            "--project",
            repo_s,
            "src/**",
            "--ttl",
            "600",
        ],
    );
    assert_eq!(held["granted"].as_array().unwrap().len(), 1);

    // A conflicting acquire exits 3 and grants nothing.
    let clash = wyres(
        &home,
        &[
            "--json",
            "hold",
            "acquire",
            "--agent",
            "bob",
            "--project",
            repo_s,
            "src/lib.rs",
        ],
    );
    assert_eq!(clash.status.code(), Some(3));
    let clash: Value = serde_json::from_slice(&clash.stdout).unwrap();
    assert_eq!(clash["conflicts"][0]["held"]["agent"], "alice");

    let hb = json(&home, &["heartbeat", "--agent", "alice", "--end"]);
    assert_eq!(hb["released_holds"].as_array().unwrap().len(), 1);
    let check = wyres(
        &home,
        &[
            "hold",
            "check",
            "--agent",
            "bob",
            "--project",
            repo_s,
            "src/lib.rs",
        ],
    );
    assert!(check.status.success());

    // Human output works too.
    let text = wyres(&home, &["inbox", "--agent", "alice"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("Done: Review PR"));
}

#[test]
fn errors_are_structured_in_json_mode() {
    let dir = tempfile::tempdir().unwrap();
    let out = wyres(dir.path(), &["--json", "inbox", "--agent", "nobody"]);
    assert!(!out.status.success());
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}

#[test]
fn serve_refuses_non_loopback_binds() {
    let dir = tempfile::tempdir().unwrap();
    let out = wyres(dir.path(), &["serve", "--bind", "0.0.0.0:7466"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}
