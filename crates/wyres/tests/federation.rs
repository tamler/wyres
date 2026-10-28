//! Federation on one machine: a hub and spokes, each with its own database, syncing over
//! real HTTP through the hub.

use std::net::SocketAddr;

use tokio_util::sync::CancellationToken;
use wyres::fed;
use wyres::serve::{http_router, HttpOptions};
use wyres_core::*;

struct Node {
    _dir: tempfile::TempDir,
    store: Store,
}

fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    Node { _dir: dir, store }
}

fn register(store: &Store, name: &str, program: Program, project: Option<&str>) {
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

struct Hub {
    node: Node,
    url: String,
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

fn start_hub() -> Hub {
    let node = node("hub");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        federation: true,
        ..Default::default()
    };
    let app = http_router(node.store.clone(), ct.clone(), &opts);
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Hub {
        node,
        url: format!("http://{addr}"),
        ct,
        _rt: rt,
    }
}

fn join(hub: &Hub, spoke: &Node, perms: &[Perm]) {
    let code = hub.node.store.create_invite(&hub.url, perms, 3600).unwrap();
    let peer = fed::join(&spoke.store, &code).unwrap();
    assert_eq!(peer.role, "hub");
    assert_eq!(peer.display_name, "hub");
}

fn sync(n: &Node) -> fed::SyncReport {
    let r = fed::sync(&n.store).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);
    assert!(r.pull_rejected.is_empty(), "{:?}", r.pull_rejected);
    r
}

fn send(store: &Store, from: &str, to: &str, subject: &str) -> Result<Message> {
    store.send_message(SendMessage {
        from: from.into(),
        to: vec![to.into()],
        subject: subject.into(),
        body: format!("body: {subject}"),
        ..Default::default()
    })
}

fn summaries(store: &Store) -> Vec<String> {
    store
        .activity(ActivityQuery {
            limit: 500,
            ..Default::default()
        })
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.summary)
        .collect()
}

#[test]
fn pairing_rules() {
    let hub = start_hub();
    let unnamed = {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    };
    let code = hub
        .node
        .store
        .create_invite(&hub.url, &Perm::ALL, 3600)
        .unwrap();
    // A node still called "local" must be named first.
    let err = fed::join(&unnamed.1, &code).unwrap_err();
    assert!(err.to_string().contains("wyres node name"), "{err}");

    let laptop = node("laptop");
    fed::join(&laptop.store, &code).unwrap();
    // Invites are single use.
    let again = node("laptop2");
    let err = fed::join(&again.store, &code).unwrap_err();
    assert!(
        err.to_string().contains("invalid, used or expired invite"),
        "{err}"
    );
    // Names are unique across the federation.
    let dup = node("laptop");
    let code2 = hub
        .node
        .store
        .create_invite(&hub.url, &Perm::ALL, 3600)
        .unwrap();
    let err = fed::join(&dup.store, &code2).unwrap_err();
    assert!(err.to_string().contains("already known"), "{err}");
    // Garbage and expired codes.
    assert!(fed::join(&again.store, "wyres-invite:nope").is_err());
    assert!(hub
        .node
        .store
        .create_invite(&hub.url, &Perm::ALL, 0)
        .is_err());
    assert!("remote_reap".parse::<Perm>().is_err());

    let peers = hub.node.store.peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].role, "spoke");
    assert_eq!(peers[0].display_name, "laptop");
    let mine = laptop.store.peers().unwrap();
    assert_eq!(
        mine[0].fingerprint,
        hub.node.store.node().unwrap().fingerprint
    );
    hub.ct.cancel();
}

#[test]
fn mail_feed_resources_and_holds_across_nodes() {
    let hub = start_hub();
    let laptop = node("laptop"); // full permissions
    let runner = node("runner"); // mail only
    let quiet = node("quiet"); // feed only
    join(&hub, &laptop, &Perm::ALL);
    join(&hub, &runner, &[Perm::SendMail]);
    join(&hub, &quiet, &[Perm::ReadFeed]);

    register(&hub.node.store, "alice", Program::Claude, Some("/h/shop"));
    register(&laptop.store, "bob", Program::Codex, Some("/b/shop"));
    register(&runner.store, "carol", Program::GrokBot, None);
    register(&quiet.store, "dave", Program::Grok, None);
    for n in [&laptop, &runner, &quiet, &laptop] {
        sync(n);
    }

    // Directory: every node can address every agent as name@project@node.
    let carol = laptop.store.agent("carol@@runner").unwrap();
    assert_eq!(carol.address, "carol@@runner");
    assert_eq!(carol.node.as_deref(), Some("runner"));
    assert_eq!(
        laptop.store.agent("alice@shop@hub").unwrap().program,
        "claude"
    );
    assert_eq!(
        runner.store.agent("bob@shop@laptop").unwrap().program,
        "codex"
    );
    // Agents act only on their own node.
    let err = send(&laptop.store, "carol@@runner", "bob", "spoof").unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // Cross-node mail: laptop -> runner through the hub, reply back, ack propagates.
    let first = send(&laptop.store, "bob", "carol@@runner", "Need a GPU").unwrap();
    assert_eq!(first.to, vec!["carol@@runner"]);
    sync(&laptop);
    sync(&runner);
    let inbox = runner.store.inbox("carol", InboxQuery::default()).unwrap();
    assert_eq!(inbox.unread, 1);
    let got = &inbox.messages[0].message;
    assert_eq!(got.from, "bob@shop@laptop");
    assert_eq!(got.body, "body: Need a GPU");
    let reply = runner
        .store
        .reply(Reply {
            from: "carol".into(),
            message_id: got.message_id.clone(),
            body: "use mine".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reply.to, vec!["bob@shop@laptop"]);
    runner
        .store
        .acknowledge("carol", std::slice::from_ref(&got.message_id), true)
        .unwrap();
    sync(&runner);
    sync(&laptop);
    let bob = laptop.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(bob.messages[0].message.subject, "Re: Need a GPU");
    assert_eq!(bob.messages[0].message.thread_id, first.thread_id);
    let thread = laptop.store.thread(&first.thread_id).unwrap();
    assert_eq!(thread.messages.len(), 2);
    assert!(laptop
        .store
        .events(500)
        .unwrap()
        .iter()
        .any(|e| e.kind == "delivery.acked" && e.node_id == runner.store.node_id()));
    // Mail stays private to the nodes involved: the feed-only node never saw it.
    assert!(quiet.store.message(&first.message_id).is_err());
    // Requests are node-local.
    let err = laptop
        .store
        .send_message(SendMessage {
            from: "bob".into(),
            to: vec!["carol@@runner".into()],
            subject: "do this".into(),
            body: String::new(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
    // A node without send_mail cannot mail other nodes.
    sync(&quiet);
    let err = send(&quiet.store, "dave", "bob@shop@laptop", "hi").unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // Resources: visible across nodes, borrowable only when reachable and permitted.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = hub
        .node
        .store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::ModelServer,
            name: format!("ollama@{port}"),
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            port: None,
            pid: None,
            project: None,
            purpose: Some("shared llm".into()),
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap()
        .resource;
    let dead = hub
        .node
        .store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::DevServer,
            name: "vite@1".into(),
            endpoint: Some("http://127.0.0.1:1".into()),
            port: None,
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap()
        .resource;
    sync(&laptop);
    sync(&runner);
    let seen = laptop
        .store
        .find_resources(ResourceFilter {
            query: Some("ollama".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].node.as_deref(), Some("hub"));
    assert_eq!(seen[0].owner.as_deref(), Some("alice@shop@hub"));
    assert_eq!(seen[0].reachable, Some(true));
    let local_only = ResourceFilter {
        local_only: true,
        ..Default::default()
    };
    assert!(laptop.store.find_resources(local_only).unwrap().is_empty());

    let id = ResourceRef::Id(shared.resource_id.clone());
    let borrowed = laptop.store.borrow_resource("bob", &id, Some(600)).unwrap();
    assert_eq!(borrowed.outcome, "borrowed");
    let err = laptop
        .store
        .borrow_resource("bob", &ResourceRef::Id(dead.resource_id.clone()), None)
        .unwrap_err();
    assert!(err.to_string().contains("not reachable"), "{err}");
    // runner was not granted borrow_resource (it does not even see the feed).
    let err = runner
        .store
        .borrow_resource("carol", &id, None)
        .unwrap_err();
    assert_eq!(err.code(), "not_found");
    // Only the owning node can stop or reap it.
    assert_eq!(
        laptop
            .store
            .release_resource("bob", &id, true)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    let plan = wyres::reap::plan(&laptop.store, &shared.resource_id, 0).unwrap();
    assert!(!plan.eligible);
    assert!(plan.blockers[0].contains("owning node"));

    sync(&laptop);
    let at_hub = hub.node.store.resource(&id).unwrap();
    assert_eq!(at_hub.refcount, 2, "{:?}", at_hub.leases);
    assert!(at_hub.leases.iter().any(|l| l.agent == "bob@shop@laptop"));
    laptop.store.release_resource("bob", &id, false).unwrap();
    sync(&laptop);
    assert_eq!(hub.node.store.resource(&id).unwrap().refcount, 1);
    drop(listener);

    // Path holds are node-local: a clash across nodes is a warning, never a lock.
    hub.node
        .store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: "shop".into(),
            paths: vec!["src/**".into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
        })
        .unwrap();
    sync(&laptop);
    let mine = laptop
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: "shop".into(),
            paths: vec!["src/main.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
        })
        .unwrap();
    assert_eq!(mine.granted.len(), 1);
    assert!(mine.conflicts.is_empty());
    assert_eq!(mine.warnings.len(), 1);
    assert_eq!(mine.warnings[0].held.agent, "alice@shop@hub");

    // Merged activity feed: the hub sees spokes' activity; spokes with read_feed see the hub's.
    sync(&laptop);
    let at_hub = summaries(&hub.node.store);
    assert!(
        at_hub
            .iter()
            .any(|s| s.starts_with("[laptop] bob@shop holds shop:src/main.rs")),
        "{at_hub:?}"
    );
    sync(&laptop);
    sync(&quiet);
    let at_laptop = summaries(&laptop.store);
    assert!(
        at_laptop
            .iter()
            .any(|s| s.starts_with("[hub] alice@shop holds shop:src/**")),
        "{at_laptop:?}"
    );
    let at_quiet = summaries(&quiet.store);
    assert!(at_quiet
        .iter()
        .any(|s| s.starts_with("[hub] alice@shop holds")));
    let at_runner = summaries(&runner.store);
    assert!(
        !at_runner.iter().any(|s| s.contains("holds shop:")),
        "runner has no read_feed: {at_runner:?}"
    );
    hub.ct.cancel();
}

#[test]
fn signatures_authority_and_revocation() {
    let hub = start_hub();
    let laptop = node("laptop");
    join(&hub, &laptop, &Perm::ALL);
    register(&laptop.store, "bob", Program::Codex, None);

    // A tampered event is rejected; so is one claiming another origin.
    let (events, _) = laptop.store.outbound_events(0, 100).unwrap();
    let registered = events
        .iter()
        .find(|e| e.kind == "agent.registered")
        .unwrap();
    let mut forged = registered.clone();
    forged.payload = forged.payload.replace("bob", "eve");
    let r = hub
        .node
        .store
        .ingest(laptop.store.node_id(), &[forged], &[])
        .unwrap();
    assert_eq!(r.rejected.len(), 1);
    assert!(
        r.rejected[0].reason.contains("signature"),
        "{:?}",
        r.rejected
    );
    let mut stolen = registered.clone();
    stolen.node_id = hub.node.store.node_id().to_string();
    let r = hub
        .node
        .store
        .ingest(laptop.store.node_id(), &[stolen], &[])
        .unwrap();
    assert_eq!(r.rejected.len(), 1);

    // Normal sync works, and is idempotent.
    let first = sync(&laptop);
    assert!(first.pushed > 0);
    let again = sync(&laptop);
    assert_eq!(again.pushed, 0);
    assert_eq!(again.applied, 0);

    // After revocation the hub refuses the spoke.
    hub.node.store.revoke_peer("laptop").unwrap();
    let err = fed::sync(&laptop.store).unwrap_err();
    assert!(err.to_string().contains("401"), "{err}");
    let audit = hub.node.store.audit_log(100).unwrap();
    assert!(audit
        .iter()
        .any(|a| a.operation.contains("/fed/") && a.outcome == "denied"));
    hub.ct.cancel();
}

#[test]
fn federation_cli() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_wyres"))
            .args(args)
            .env("WYRES_HOME", dir.path())
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    };
    let (ok, _, err) = run(&["pair", "invite", "--url", "https://hub.example"]);
    assert!(!ok);
    assert!(err.contains("wyres node name"), "{err}");
    assert!(run(&["node", "name", "studio"]).0);
    let (ok, out, _) = run(&["--json", "node", "show"]);
    assert!(ok);
    let node: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(node["display_name"], "studio");
    let (ok, out, err) = run(&[
        "--json",
        "pair",
        "invite",
        "--url",
        "https://hub.example",
        "--perms",
        "read_feed",
        "--expires",
        "30m",
    ]);
    assert!(ok, "{err}");
    let invite: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(invite["code"]
        .as_str()
        .unwrap()
        .starts_with("wyres-invite:"));
    let (ok, _, err) = run(&[
        "pair",
        "invite",
        "--url",
        "https://hub.example",
        "--perms",
        "remote_reap",
    ]);
    assert!(!ok);
    assert!(err.contains("no remote reap"), "{err}");
    let (ok, out, _) = run(&["peers"]);
    assert!(ok);
    assert!(out.contains("not paired"));
    let (ok, _, err) = run(&["sync"]);
    assert!(!ok);
    assert!(err.contains("no hub"), "{err}");
}
