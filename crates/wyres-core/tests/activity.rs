//! Activity feed and session lifecycle events.

use std::sync::atomic::Ordering;

use wyres_core::*;

const T0: i64 = 1_800_000_000_000;

fn setup() -> (Store, std::sync::Arc<std::sync::atomic::AtomicI64>) {
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
    for (name, program) in [("alice", Program::Claude), ("bob", Program::Grok)] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: Some("/work/app".into()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    (store, handle)
}

#[test]
fn feed_summarizes_without_bodies() {
    let (store, clock) = setup();
    let start = store
        .session(
            "alice",
            SessionPhase::Start,
            SessionInfo {
                client: Some("claude".into()),
                session_id: Some("s-1".into()),
            },
        )
        .unwrap();
    assert_eq!(start.status, "active");
    clock.fetch_add(1000, Ordering::SeqCst);
    let msg = store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "Ship it".into(),
            body: "SECRET BODY TEXT".into(),
            importance: Importance::Urgent,
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    store
        .acknowledge("bob", std::slice::from_ref(&msg.message_id), true)
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: "app".into(),
            paths: vec!["src/**".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
        })
        .unwrap();
    store.heartbeat("bob", true).unwrap();

    let feed = store.activity(ActivityQuery::default()).unwrap();
    let summaries: Vec<&str> = feed.items.iter().map(|i| i.summary.as_str()).collect();
    let text = summaries.join("\n");
    assert!(!text.contains("SECRET BODY TEXT"), "{text}");
    assert!(
        text.contains("alice@app started a session (claude)"),
        "{text}"
    );
    assert!(
        text.contains("alice@app sent request to bob@app: Ship it [urgent]"),
        "{text}"
    );
    assert!(text.contains("bob@app acknowledged \"Ship it\""), "{text}");
    assert!(
        text.contains("bob@app holds app:src/** (exclusive)"),
        "{text}"
    );
    assert!(
        text.contains("bob@app released 1 hold(s) at session end"),
        "{text}"
    );
    assert!(text.contains("bob@app ended a session"), "{text}");
    let started = feed
        .items
        .iter()
        .find(|i| i.kind == "session.started")
        .unwrap();
    assert_eq!(started.category, ActivityKind::Session);
    assert_eq!(started.refs["session_id"], "s-1");
    // Oldest first, ending at the cursor.
    assert_eq!(
        feed.next_since.as_deref(),
        Some(feed.items.last().unwrap().event_id.as_str())
    );
}

#[test]
fn feed_filters_pages_and_limits() {
    let (store, clock) = setup();
    for i in 0..5 {
        clock.fetch_add(1000, Ordering::SeqCst);
        store
            .send_message(SendMessage {
                from: "alice".into(),
                to: vec!["bob".into()],
                subject: format!("m{i}"),
                body: String::new(),
                ..Default::default()
            })
            .unwrap();
    }
    let mail_only = ActivityQuery {
        kinds: vec![ActivityKind::Mail],
        ..Default::default()
    };
    let feed = store.activity(mail_only).unwrap();
    assert_eq!(feed.items.len(), 5);
    assert!(feed.items.iter().all(|i| i.category == ActivityKind::Mail));

    // Without since, the newest N; with an event-id cursor, what came after it.
    let last2 = store
        .activity(ActivityQuery {
            limit: 2,
            kinds: vec![ActivityKind::Mail],
            ..Default::default()
        })
        .unwrap();
    assert!(last2.items[1].summary.ends_with("m4"));
    let first = store
        .activity(ActivityQuery {
            kinds: vec![ActivityKind::Mail],
            limit: 1,
            since: Some(Since::Time(T0)),
        })
        .unwrap();
    assert!(first.items[0].summary.ends_with("m0"));
    let next = store
        .activity(ActivityQuery {
            kinds: vec![ActivityKind::Mail],
            limit: 10,
            since: Some(Since::After(first.next_since.clone().unwrap())),
        })
        .unwrap();
    assert_eq!(next.items.len(), 4);
    assert!(next.items[0].summary.ends_with("m1"));

    // Time window.
    let recent = store
        .activity(ActivityQuery {
            since: Some(Since::parse("2s", store.now()).unwrap()),
            kinds: vec![ActivityKind::Mail],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(recent.items.len(), 3);

    let bad = store.activity(ActivityQuery {
        since: Some(Since::After("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())),
        ..Default::default()
    });
    assert_eq!(bad.unwrap_err().code(), "not_found");
}

#[test]
fn summary_counts_and_digest() {
    let (store, clock) = setup();
    store
        .session("alice", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "Please review".into(),
            body: String::new(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: "app".into(),
            paths: vec!["docs/**".into()],
            ttl_seconds: None,
            exclusive: false,
            reason: None,
        })
        .unwrap();
    let sum = store.activity_summary(None).unwrap();
    assert_eq!(sum.counts["mail"], 1);
    assert_eq!(sum.counts["request"], 1);
    assert_eq!(sum.counts["hold"], 1);
    assert_eq!(sum.requests.open, 1);
    assert_eq!(sum.live_holds, 1);
    assert!(sum.agents.iter().any(|a| a.agent == "alice@app"));
    assert!(sum.text.contains("1 open"), "{}", sum.text);
    assert!(sum.highlights.iter().any(|h| h.contains("Please review")));

    // Two hours later only the hold's expiry is in the window; request state remains.
    clock.fetch_add(2 * 3_600_000, Ordering::SeqCst);
    let later = store.activity_summary(None).unwrap();
    assert_eq!(later.highlights.len(), 1, "{:?}", later.highlights);
    assert!(later.highlights[0].contains("expired"));
    assert_eq!(later.live_holds, 0);
    assert_eq!(later.requests.open, 1);
}
