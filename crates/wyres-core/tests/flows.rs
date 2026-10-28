//! End-to-end flows over the core store, covering the Milestone 1 definition of done.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use wyres_core::*;

const T0: i64 = 1_800_000_000_000;

struct Fixture {
    store: Store,
    clock: Arc<AtomicI64>,
    project: Project,
}

impl Fixture {
    fn new() -> Fixture {
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_in_memory(clock).unwrap();
        let project = store.ensure_project("test", "/work/app").unwrap();
        for (name, program) in [
            ("alice", Program::Claude),
            ("bob", Program::Codex),
            ("carol", Program::Grok),
        ] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fixture {
            store,
            clock: handle,
            project,
        }
    }

    fn advance_secs(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    fn send(&self, from: &str, to: &[&str], subject: &str) -> Message {
        self.store
            .send_message(SendMessage {
                from: from.into(),
                to: to.iter().map(|s| s.to_string()).collect(),
                subject: subject.into(),
                body: format!("body of {subject}"),
                ..Default::default()
            })
            .unwrap()
    }

    fn hold(&self, agent: &str, paths: &[&str], exclusive: bool, ttl: i64) -> HoldReport {
        self.store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: self.project.abs_path.clone(),
                paths: paths.iter().map(|s| s.to_string()).collect(),
                ttl_seconds: Some(ttl),
                exclusive,
                reason: Some("editing".into()),
            })
            .unwrap()
    }
}

fn kinds(store: &Store) -> Vec<String> {
    store
        .events(1000)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[test]
fn fresh_home_creates_db_with_node_and_event_log() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("wyres-home");
    let store = Store::open(&home).unwrap();
    assert!(home.join(DB_FILE).exists());
    let node = store.node().unwrap();
    assert_eq!(node.node_id.len(), 26, "node_id is a ULID");
    let events = store.events(10).unwrap();
    assert_eq!(events[0].kind, "node.created");
    assert_eq!(events[0].node_id, node.node_id);
    drop(store);

    // Re-opening keeps the same node identity.
    let again = Store::open(&home).unwrap();
    assert_eq!(again.node_id(), node.node_id);
    assert_eq!(again.events(10).unwrap().len(), 1);
}

#[test]
fn event_log_is_chained() {
    let fx = Fixture::new();
    fx.send("alice", &["bob"], "hello");
    let events = fx.store.events(1000).unwrap();
    assert!(events[0].prev_id.is_none());
    for pair in events.windows(2) {
        assert_eq!(pair[1].prev_id.as_deref(), Some(pair[0].event_id.as_str()));
        assert_eq!(pair[1].node_id, fx.store.node_id());
    }
}

#[test]
fn register_agent_is_idempotent_and_touches_last_seen() {
    let fx = Fixture::new();
    let first = fx.store.agent("alice").unwrap();
    assert_eq!(first.program, "claude");
    fx.advance_secs(30);
    let again = fx
        .store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: Some("opus".into()),
            description: None,
        })
        .unwrap();
    assert_eq!(again.agent_id, first.agent_id);
    assert_eq!(again.model.as_deref(), Some("opus"));
    assert_ne!(again.last_seen_at, first.last_seen_at);
    assert_eq!(fx.store.agents().unwrap().len(), 3);
}

#[test]
fn agent_names_disambiguate_with_project() {
    let fx = Fixture::new();
    for path in ["/work/one", "/work/two"] {
        fx.store
            .register_agent(RegisterAgent {
                name: "dave".into(),
                program: Program::Cursor,
                project: Some(path.into()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let err = fx.store.agent("dave").unwrap_err();
    assert_eq!(err.code(), "ambiguous");
    let one = fx.store.agent("dave@one").unwrap();
    assert_eq!(one.address, "dave@one");
    let msg = fx.send("alice", &["dave@two"], "hi two");
    assert_eq!(msg.to, vec!["dave@two"]);
    assert_eq!(fx.store.agent("nobody").unwrap_err().code(), "not_found");
}

#[test]
fn rejects_bad_names_and_paths() {
    let fx = Fixture::new();
    let bad = fx.store.register_agent(RegisterAgent {
        name: "has space".into(),
        program: Program::Other,
        project: None,
        model: None,
        description: None,
    });
    assert_eq!(bad.unwrap_err().code(), "invalid");
    assert_eq!(
        fx.store
            .ensure_project("t", "relative/path")
            .unwrap_err()
            .code(),
        "invalid"
    );
    assert_eq!("gpt".parse::<Program>().unwrap_err().code(), "invalid");
}

#[test]
fn send_inbox_ack() {
    let fx = Fixture::new();
    let msg = fx
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            cc: vec!["carol".into(), "bob".into()],
            subject: "API change".into(),
            body: "Please review".into(),
            importance: Importance::Urgent,
            ack_required: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(msg.to, vec!["bob"]);
    assert_eq!(msg.cc, vec!["carol"], "bob is not duplicated into cc");
    assert_eq!(msg.thread_id, msg.message_id);

    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.unread, 1);
    assert_eq!(inbox.urgent_unread, 1);
    assert_eq!(inbox.awaiting_ack, 1);
    assert_eq!(inbox.messages[0].message.subject, "API change");
    assert_eq!(inbox.messages[0].role, "to");
    assert!(fx
        .store
        .inbox("alice", InboxQuery::default())
        .unwrap()
        .messages
        .is_empty());

    // Read only.
    let out = fx
        .store
        .acknowledge("bob", std::slice::from_ref(&msg.message_id), false)
        .unwrap();
    assert_eq!(out.updated, vec![msg.message_id.clone()]);
    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!((inbox.unread, inbox.awaiting_ack), (0, 1));
    let unread_only = InboxQuery {
        unread_only: true,
        ..Default::default()
    };
    assert!(fx
        .store
        .inbox("bob", unread_only)
        .unwrap()
        .messages
        .is_empty());

    // Then acknowledge; repeating is a no-op.
    fx.store
        .acknowledge("bob", std::slice::from_ref(&msg.message_id), true)
        .unwrap();
    let again = fx
        .store
        .acknowledge("bob", std::slice::from_ref(&msg.message_id), true)
        .unwrap();
    assert_eq!(again.unchanged, vec![msg.message_id.clone()]);
    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.awaiting_ack, 0);
    assert!(inbox.messages[0].acked_at.is_some());

    // Carol's copy is independent.
    assert_eq!(
        fx.store
            .inbox("carol", InboxQuery::default())
            .unwrap()
            .unread,
        1
    );
    // Alice cannot ack a message she did not receive.
    let err = fx
        .store
        .acknowledge("alice", &[msg.message_id], true)
        .unwrap_err();
    assert_eq!(err.code(), "not_found");

    let k = kinds(&fx.store);
    assert!(k.contains(&"message.sent".to_string()));
    assert!(k.contains(&"delivery.read".to_string()));
    assert!(k.contains(&"delivery.acked".to_string()));
}

#[test]
fn unknown_recipient_is_rejected_without_side_effects() {
    let fx = Fixture::new();
    let before = fx.store.events(1000).unwrap().len();
    let err = fx
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into(), "ghost".into()],
            subject: "x".into(),
            body: "y".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.code(), "not_found");
    assert_eq!(fx.store.events(1000).unwrap().len(), before);
    assert_eq!(
        fx.store.inbox("bob", InboxQuery::default()).unwrap().unread,
        0
    );
}

#[test]
fn reply_and_threads() {
    let fx = Fixture::new();
    let first = fx
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            cc: vec!["carol".into()],
            subject: "Schema".into(),
            body: "v1?".into(),
            ..Default::default()
        })
        .unwrap();
    fx.advance_secs(1);
    let reply = fx
        .store
        .reply(Reply {
            from: "bob".into(),
            message_id: first.message_id.clone(),
            body: "looks good".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(reply.thread_id, first.thread_id);
    assert_eq!(reply.reply_to.as_deref(), Some(first.message_id.as_str()));
    assert_eq!(reply.subject, "Re: Schema");
    assert_eq!(reply.to, vec!["alice"]);
    assert!(reply.cc.is_empty());

    fx.advance_secs(1);
    let all = fx
        .store
        .reply(Reply {
            from: "alice".into(),
            message_id: reply.message_id.clone(),
            body: "shipping".into(),
            all: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(all.subject, "Re: Schema");
    assert_eq!(all.to, vec!["bob"]);

    let thread = fx.store.thread(&first.thread_id).unwrap();
    let bodies: Vec<_> = thread.messages.iter().map(|m| m.body.as_str()).collect();
    assert_eq!(bodies, vec!["v1?", "looks good", "shipping"]);

    let threads = fx.store.threads("bob", 10).unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].subject, "Schema");
    assert_eq!(threads[0].messages, 3);
    assert_eq!(threads[0].unread, 2);
    assert_eq!(threads[0].participants, vec!["alice", "bob", "carol"]);

    assert_eq!(fx.store.thread("nope").unwrap_err().code(), "not_found");
}

#[test]
fn request_claim_done() {
    let fx = Fixture::new();
    let msg = fx
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into(), "carol".into()],
            subject: "Fix flaky test".into(),
            body: "tests/net.rs times out".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let rid = msg.request_id.clone().expect("request created");
    let req = fx.store.request(&rid).unwrap();
    assert_eq!(req.status, RequestStatus::Open);
    assert_eq!(req.owner, None);
    assert_eq!(req.created_by, "alice");

    let inbox = fx.store.inbox("carol", InboxQuery::default()).unwrap();
    assert_eq!(inbox.requests.len(), 1);

    let update = |agent: &str, action| {
        fx.store.update_request(RequestUpdate {
            agent: agent.into(),
            request_id: rid.clone(),
            action,
            note: None,
            due_in_seconds: None,
        })
    };

    let claimed = update("bob", RequestAction::Claim).unwrap();
    assert_eq!(claimed.status, RequestStatus::Claimed);
    assert_eq!(claimed.owner.as_deref(), Some("bob"));
    assert_eq!(
        update("carol", RequestAction::Claim).unwrap_err().code(),
        "conflict"
    );
    assert_eq!(
        update("carol", RequestAction::Release).unwrap_err().code(),
        "forbidden"
    );
    assert_eq!(
        update("bob", RequestAction::Cancel).unwrap_err().code(),
        "forbidden"
    );
    // Claimed requests leave carol's inbox but stay in bob's.
    assert!(fx
        .store
        .inbox("carol", InboxQuery::default())
        .unwrap()
        .requests
        .is_empty());
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .requests
            .len(),
        1
    );

    let released = update("bob", RequestAction::Release).unwrap();
    assert_eq!(released.status, RequestStatus::Open);
    assert_eq!(released.owner, None);
    update("carol", RequestAction::Claim).unwrap();

    let done = fx
        .store
        .update_request(RequestUpdate {
            agent: "carol".into(),
            request_id: rid.clone(),
            action: RequestAction::Complete,
            note: Some("fixed in abc123".into()),
            due_in_seconds: None,
        })
        .unwrap();
    assert_eq!(done.status, RequestStatus::Done);
    assert_eq!(done.owner.as_deref(), Some("carol"));
    assert!(done.closed_at.is_some());
    assert_eq!(
        update("carol", RequestAction::Claim).unwrap_err().code(),
        "conflict"
    );

    // The creator is told, in the same thread.
    let inbox = fx.store.inbox("alice", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages[0].message.subject, "Done: Fix flaky test");
    assert_eq!(inbox.messages[0].message.body, "fixed in abc123");
    assert_eq!(inbox.messages[0].message.thread_id, msg.thread_id);

    let filter = RequestFilter {
        status: Some(RequestStatus::Done),
        ..Default::default()
    };
    assert_eq!(fx.store.requests(filter).unwrap().len(), 1);
    for kind in [
        "request.created",
        "request.claimed",
        "request.released",
        "request.completed",
    ] {
        assert!(kinds(&fx.store).contains(&kind.to_string()), "{kind}");
    }
}

#[test]
fn overdue_requests_go_stale_and_can_be_taken_over() {
    let fx = Fixture::new();
    let msg = fx
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into(), "carol".into()],
            subject: "Bump deps".into(),
            body: "".into(),
            request: Some(NewRequest {
                due_in_seconds: Some(600),
            }),
            ..Default::default()
        })
        .unwrap();
    let rid = msg.request_id.unwrap();
    fx.store
        .update_request(RequestUpdate {
            agent: "bob".into(),
            request_id: rid.clone(),
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();
    fx.advance_secs(601);
    let report = fx.store.sweep().unwrap();
    assert_eq!(report.stale_requests, 1);
    let stale = fx.store.request(&rid).unwrap();
    assert_eq!(stale.status, RequestStatus::Stale);
    assert_eq!(stale.owner.as_deref(), Some("bob"));

    let taken = fx
        .store
        .update_request(RequestUpdate {
            agent: "carol".into(),
            request_id: rid.clone(),
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();
    assert_eq!(taken.status, RequestStatus::Claimed);
    assert_eq!(taken.owner.as_deref(), Some("carol"));
    assert_eq!(taken.due_at, None, "past due time is cleared on takeover");

    let cancelled = fx
        .store
        .update_request(RequestUpdate {
            agent: "alice".into(),
            request_id: rid,
            action: RequestAction::Cancel,
            note: Some("not needed".into()),
            due_in_seconds: None,
        })
        .unwrap();
    assert_eq!(cancelled.status, RequestStatus::Cancelled);
}

#[test]
fn hold_acquire_conflict_expire() {
    let fx = Fixture::new();
    let got = fx.hold("alice", &["src/api/**"], true, 600);
    assert_eq!(got.granted.len(), 1);
    assert!(got.conflicts.is_empty());
    assert_eq!(got.granted[0].pattern, "src/api/**");

    // Bob conflicts on an overlapping file; nothing is granted (all-or-nothing).
    let clash = fx.hold("bob", &["docs/readme.md", "src/api/routes.rs"], false, 600);
    assert!(clash.granted.is_empty());
    assert_eq!(clash.conflicts.len(), 1);
    assert_eq!(clash.conflicts[0].path, "src/api/routes.rs");
    assert_eq!(clash.conflicts[0].held.agent, "alice");
    assert!(fx
        .store
        .holds(HoldFilter {
            agent: Some("bob".into()),
            ..Default::default()
        })
        .unwrap()
        .is_empty());

    // Non-overlapping paths are fine.
    assert_eq!(fx.hold("bob", &["docs/**"], true, 600).granted.len(), 1);
    // check_holds reports without acquiring.
    let check = fx
        .store
        .check_holds(
            "carol",
            "app",
            &["src/api/x.rs".into(), "README.md".into()],
            false,
        )
        .unwrap();
    assert_eq!(check.len(), 1);

    // Heartbeat renews; without it the hold lapses and the sweeper expires it.
    fx.advance_secs(500);
    let hb = fx.store.heartbeat("alice", false).unwrap();
    assert_eq!(hb.renewed_holds.len(), 1);
    fx.advance_secs(500);
    assert!(fx
        .hold("carol", &["src/api/routes.rs"], false, 600)
        .granted
        .is_empty());
    fx.advance_secs(101);
    let sweep = fx.store.sweep().unwrap();
    assert_eq!(
        sweep.expired_holds, 1,
        "alice's hold lapsed (bob's was swept earlier)"
    );
    let after = fx.hold("carol", &["src/api/routes.rs"], true, 600);
    assert_eq!(after.granted.len(), 1);
    assert!(kinds(&fx.store).contains(&"hold.expired".to_string()));
}

#[test]
fn shared_holds_coexist_and_release_works() {
    let fx = Fixture::new();
    assert_eq!(
        fx.hold("alice", &["Cargo.toml"], false, 600).granted.len(),
        1
    );
    assert_eq!(fx.hold("bob", &["Cargo.toml"], false, 600).granted.len(), 1);
    let excl = fx.hold("carol", &["Cargo.toml"], true, 600);
    assert_eq!(excl.conflicts.len(), 2);

    // Re-acquiring your own pattern renews it instead of duplicating.
    let again = fx.hold("alice", &["Cargo.toml"], false, 900);
    assert_eq!(again.granted[0].ttl_seconds, 900);
    let alice = HoldFilter {
        agent: Some("alice".into()),
        ..Default::default()
    };
    assert_eq!(fx.store.holds(alice.clone()).unwrap().len(), 1);

    let released = fx.store.release_holds("alice", &[]).unwrap();
    assert_eq!(released.len(), 1);
    assert!(fx.store.holds(alice).unwrap().is_empty());
    assert_eq!(
        fx.store
            .release_holds("bob", &["missing".into()])
            .unwrap_err()
            .code(),
        "not_found"
    );
}

#[test]
fn ttl_is_clamped() {
    let fx = Fixture::new();
    let long = fx.hold("alice", &["a"], true, 999_999);
    assert_eq!(long.granted[0].ttl_seconds, MAX_HOLD_TTL_SECONDS);
    let short = fx.hold("alice", &["b"], true, 1);
    assert_eq!(short.granted[0].ttl_seconds, MIN_HOLD_TTL_SECONDS);
}

#[test]
fn heartbeat_end_releases_holds_and_reports_digest() {
    let fx = Fixture::new();
    fx.hold("bob", &["src/**"], true, 600);
    fx.send("alice", &["bob"], "ping");
    let hb = fx.store.heartbeat("bob", true).unwrap();
    assert_eq!(hb.status, "offline");
    assert_eq!(hb.released_holds.len(), 1);
    assert_eq!(hb.unread, 1);
    assert_eq!(fx.store.agent("bob").unwrap().status, "offline");
    assert!(fx.store.holds(HoldFilter::default()).unwrap().is_empty());
    assert!(kinds(&fx.store).contains(&"session.ended".to_string()));
    // Any later activity marks the agent active again.
    fx.store.heartbeat("bob", false).unwrap();
    assert_eq!(fx.store.agent("bob").unwrap().status, "active");
}

#[test]
fn concurrent_connections_share_one_database() {
    let dir = tempfile::tempdir().unwrap();
    let a = Store::open(dir.path()).unwrap();
    let b = Store::open(dir.path()).unwrap();
    assert_eq!(a.node_id(), b.node_id());
    for name in ["x", "y"] {
        a.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    b.send_message(SendMessage {
        from: "x".into(),
        to: vec!["y".into()],
        subject: "across connections".into(),
        body: String::new(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(a.inbox("y", InboxQuery::default()).unwrap().unread, 1);
}
