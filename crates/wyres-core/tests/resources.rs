//! Resource registry: leases, refcounts, TTL expiry, adoption, transfer, session end.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use wyres_core::*;

const T0: i64 = 1_800_000_000_000;

fn setup() -> (Store, Arc<AtomicI64>) {
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
    store.ensure_project("t", "/work/lab").unwrap();
    for (name, program) in [
        ("alice", Program::Claude),
        ("bob", Program::Codex),
        ("carol", Program::GrokBot),
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
    (store, handle)
}

fn ollama(store: &Store, agent: &str, mode: LeaseMode, ttl: i64) -> Result<LeaseOutcome> {
    store.register_resource(RegisterResource {
        agent: agent.into(),
        kind: ResourceKind::ModelServer,
        name: "ollama@11434".into(),
        endpoint: Some("http://127.0.0.1:11434".into()),
        port: None,
        pid: Some(4242),
        project: Some("lab".into()),
        purpose: Some("local llm".into()),
        mode,
        ttl_seconds: Some(ttl),
        metadata: Some(
            serde_json::json!({ "backend": "metal", "estimated_bytes": 8_000_000_000u64 }),
        ),
    })
}

fn name_ref() -> ResourceRef {
    ResourceRef::Name(ResourceKind::ModelServer, "ollama@11434".into())
}

#[test]
fn register_find_borrow_release() {
    let (store, _) = setup();
    let reg = ollama(&store, "alice", LeaseMode::Shared, 600).unwrap();
    assert_eq!(reg.outcome, "registered");
    assert_eq!(reg.resource.port, Some(11434), "port derived from endpoint");
    assert_eq!(reg.resource.owner.as_deref(), Some("alice"));
    assert_eq!(reg.resource.status, "live");
    assert_eq!(reg.resource.metadata["backend"], "metal");
    assert_eq!(reg.lease.as_ref().unwrap().role, "owner");

    // Find before start.
    let found = store
        .find_resources(ResourceFilter {
            query: Some("OLLAMA".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(found.len(), 1);
    let by_kind = ResourceFilter {
        kind: Some(ResourceKind::DevServer),
        ..Default::default()
    };
    assert!(store.find_resources(by_kind).unwrap().is_empty());

    // A duplicate registration is refused while it is live.
    let dup = ollama(&store, "bob", LeaseMode::Shared, 600).unwrap_err();
    assert_eq!(dup.code(), "conflict");
    assert!(dup.to_string().contains("borrow it instead"));

    // Borrow (by kind:name and by id), with refcounts.
    let b = store.borrow_resource("bob", &name_ref(), None).unwrap();
    assert_eq!(b.outcome, "borrowed");
    assert_eq!(b.resource.refcount, 2);
    let id = ResourceRef::Id(reg.resource.resource_id.clone());
    let c = store.borrow_resource("carol", &id, Some(120)).unwrap();
    assert_eq!(c.resource.refcount, 3);
    assert_eq!(
        store
            .borrow_resource("bob", &name_ref(), None)
            .unwrap()
            .outcome,
        "renewed"
    );

    // The owner leaves: the oldest borrower takes over.
    let left = store.release_resource("alice", &id, false).unwrap();
    assert_eq!(left.outcome, "transferred");
    assert_eq!(left.resource.owner.as_deref(), Some("bob"));
    assert_eq!(left.resource.refcount, 2);
    assert!(left.lease.is_none());
    assert_eq!(
        store
            .release_resource("alice", &id, false)
            .unwrap_err()
            .code(),
        "not_found"
    );

    // Only the owner can declare it stopped.
    assert_eq!(
        store
            .release_resource("carol", &id, true)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    let stopped = store.release_resource("bob", &id, true).unwrap();
    assert_eq!(stopped.resource.status, "released");
    assert_eq!(stopped.resource.refcount, 0);
    assert!(store
        .find_resources(ResourceFilter::default())
        .unwrap()
        .is_empty());
    let all = ResourceFilter {
        include_released: true,
        ..Default::default()
    };
    assert_eq!(store.find_resources(all).unwrap().len(), 1);
    // Released identities can be registered again as a new resource.
    let again = ollama(&store, "carol", LeaseMode::Shared, 600).unwrap();
    assert_eq!(again.outcome, "registered");
    assert_ne!(again.resource.resource_id, reg.resource.resource_id);
}

#[test]
fn exclusive_resources_refuse_borrowers() {
    let (store, _) = setup();
    store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::Gpu,
            name: "metal:0".into(),
            endpoint: None,
            port: None,
            pid: None,
            project: None,
            purpose: Some("fine-tune".into()),
            mode: LeaseMode::Exclusive,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap();
    let r = ResourceRef::parse("gpu:metal:0").unwrap();
    let err = store.borrow_resource("bob", &r, None).unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert!(err.to_string().contains("exclusive to alice"));
}

#[test]
fn leases_expire_lapse_and_can_be_adopted() {
    let (store, clock) = setup();
    let reg = ollama(&store, "alice", LeaseMode::Shared, 600).unwrap();
    store
        .borrow_resource("bob", &name_ref(), Some(120))
        .unwrap();

    clock.fetch_add(121_000, Ordering::SeqCst);
    let sweep = store.sweep().unwrap();
    assert_eq!(sweep.expired_leases, 1, "bob's short lease");
    assert_eq!(store.resource(&name_ref()).unwrap().refcount, 1);

    // Heartbeat keeps alice's lease alive past its original expiry.
    clock.fetch_add(400_000, Ordering::SeqCst);
    let renewed = store.heartbeat_resources("alice", &[]).unwrap();
    assert_eq!(renewed.len(), 1);
    clock.fetch_add(400_000, Ordering::SeqCst);
    assert_eq!(store.resource(&name_ref()).unwrap().status, "live");

    // Without heartbeats it lapses: an orphan candidate, still listed.
    clock.fetch_add(601_000, Ordering::SeqCst);
    let lapsed = store.lapsed_resources().unwrap();
    assert_eq!(lapsed.len(), 1);
    assert_eq!(lapsed[0].status, "lapsed");
    assert!(lapsed[0].owner.is_none());
    assert!(lapsed[0].lapsed_since.is_some());
    // Lapsed since the lease's real expiry (renewed at +521s with a 600s TTL), not the sweep.
    clock.fetch_add(30_000, Ordering::SeqCst);
    let ms = store
        .lapsed_for_ms(&reg.resource.resource_id)
        .unwrap()
        .unwrap();
    assert_eq!(ms, (121 + 400 + 400 + 601 + 30 - 521 - 600) * 1000);

    // Registering the same identity adopts it; so would borrowing.
    let adopted = ollama(&store, "carol", LeaseMode::Shared, 600).unwrap();
    assert_eq!(adopted.outcome, "adopted");
    assert_eq!(adopted.resource.resource_id, reg.resource.resource_id);
    assert_eq!(adopted.resource.owner.as_deref(), Some("carol"));
    assert!(store.lapsed_resources().unwrap().is_empty());
    assert_eq!(
        store.lapsed_for_ms(&reg.resource.resource_id).unwrap(),
        None
    );

    let kinds: Vec<String> = store
        .events(1000)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    for k in [
        "resource.registered",
        "resource.borrowed",
        "resource.lease_expired",
        "resource.renewed",
        "resource.adopted",
    ] {
        assert!(kinds.contains(&k.to_string()), "{k}");
    }
}

#[test]
fn session_end_releases_resources_unless_kept() {
    let (store, _) = setup();
    ollama(&store, "alice", LeaseMode::Shared, 600).unwrap();
    store.borrow_resource("bob", &name_ref(), None).unwrap();

    // A plain heartbeat renews resource leases too.
    let hb = store.heartbeat("bob", false).unwrap();
    assert_eq!(hb.resource_leases, 1);

    // keep_resources leaves the lease in place.
    let kept = store
        .session(
            "bob",
            SessionPhase::End,
            SessionInfo {
                keep_resources: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(kept.status, "offline");
    assert_eq!(store.resource(&name_ref()).unwrap().refcount, 2);

    let end = store.heartbeat("alice", true).unwrap();
    assert_eq!(end.resource_leases, 1);
    let r = store.resource(&name_ref()).unwrap();
    assert_eq!(r.owner.as_deref(), Some("bob"), "ownership passed on");
    store.heartbeat("bob", true).unwrap();
    assert_eq!(store.resource(&name_ref()).unwrap().status, "lapsed");

    let feed = store.activity(ActivityQuery {
        kinds: vec![ActivityKind::Resource],
        ..Default::default()
    });
    let text: Vec<String> = feed.unwrap().items.into_iter().map(|i| i.summary).collect();
    let text = text.join("\n");
    assert!(
        text.contains("alice registered model_server ollama@11434 (shared) for local llm"),
        "{text}"
    );
    assert!(
        text.contains("bob borrowed model_server ollama@11434"),
        "{text}"
    );
    assert!(
        text.contains("model_server ollama@11434 now owned by bob"),
        "{text}"
    );
    assert!(
        text.contains("alice released 1 resource lease(s) at session end"),
        "{text}"
    );
}

#[test]
fn reap_requests_and_records_are_logged() {
    let (store, _) = setup();
    let reg = ollama(&store, "alice", LeaseMode::Shared, 600).unwrap();
    store
        .note_reap_request("bob", "model_server:ollama@11434", "lease lapsed")
        .unwrap();
    store
        .record_reap(
            "human (wyres reap --confirm)",
            Some(&reg.resource.resource_id),
            Some(4242),
            "pid 4242 ollama serve",
        )
        .unwrap();
    let r = store
        .find_resources(ResourceFilter {
            include_released: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(r[0].status, "released");
    let summary = store.activity_summary(None).unwrap();
    assert!(summary
        .highlights
        .iter()
        .any(|h| h == "bob asked a human to reap model_server:ollama@11434: lease lapsed"));
    assert!(summary
        .highlights
        .iter()
        .any(|h| h.contains("reaped pid 4242 ollama serve")));
}

#[test]
fn validates_input() {
    let (store, _) = setup();
    let bad = store.register_resource(RegisterResource {
        agent: "alice".into(),
        kind: ResourceKind::Port,
        name: " ".into(),
        endpoint: None,
        port: None,
        pid: None,
        project: None,
        purpose: None,
        mode: LeaseMode::Shared,
        ttl_seconds: None,
        metadata: None,
    });
    assert_eq!(bad.unwrap_err().code(), "invalid");
    assert_eq!(
        "gpus".parse::<ResourceKind>().unwrap_err().code(),
        "invalid"
    );
    let missing = store.borrow_resource("alice", &ResourceRef::Id("nope".into()), None);
    assert_eq!(missing.unwrap_err().code(), "not_found");
}
