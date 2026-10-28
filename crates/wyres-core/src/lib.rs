//! Wyres core: a local-first, vendor-neutral inbox for coding agents.
//!
//! Every mutation appends an event to an append-only log (ULID ids, chained by
//! `prev_id`) and updates query projections in the same SQLite transaction. All durable
//! rows carry the owning `node_id` so a later federation layer can sync without a
//! breaking migration.

mod activity;
pub mod crypto;
mod error;
mod federation;
mod holds;
mod identity;
mod mail;
mod requests;
mod resources;
mod store;
mod time;
mod tokens;
mod wake;

pub use activity::{
    ActivityFeed, ActivityItem, ActivityKind, ActivityQuery, ActivitySummary, AgentPresence,
    RequestCounts, Since,
};
pub use error::{Error, ErrorBody, Result};
pub use federation::{
    reachable, IngestReport, Invite, NodeEntry, PairRequest, PairResponse, Peer, Perm,
    PullResponse, PushRequest, Rejected, WireEvent,
};
pub use holds::{
    patterns_overlap, AcquireHolds, HeartbeatReport, Hold, HoldConflict, HoldFilter, HoldReport,
    SessionInfo, SessionPhase, SweepReport, DEFAULT_HOLD_TTL_SECONDS, MAX_HOLD_TTL_SECONDS,
    MIN_HOLD_TTL_SECONDS,
};
pub use identity::{Agent, Program, Project, RegisterAgent};
pub use mail::{
    AckOutcome, Importance, Inbox, InboxItem, InboxQuery, Message, NewRequest, Reply, SendMessage,
    Thread, ThreadSummary,
};
pub use requests::{Request, RequestAction, RequestFilter, RequestStatus, RequestUpdate};
pub use resources::{
    derive_port, Lease, LeaseMode, LeaseOutcome, RegisterResource, Resource, ResourceFilter,
    ResourceKind, ResourceRef, DEFAULT_LEASE_TTL_SECONDS, MAX_LEASE_TTL_SECONDS,
    MIN_LEASE_TTL_SECONDS,
};
pub use store::{default_home, Clock, Event, NodeInfo, Store, DB_FILE};
pub use time::fmt_ts;
pub use tokens::{AuditEntry, AuditRecord, NewToken, Token, TokenScope};
pub use wake::{WakeTarget, WakeVia};
