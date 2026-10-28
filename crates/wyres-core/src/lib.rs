//! Wyres core: a local-first, vendor-neutral inbox for coding agents.
//!
//! Every mutation appends an event to an append-only log (ULID ids, chained by
//! `prev_id`) and updates query projections in the same SQLite transaction. All durable
//! rows carry the owning `node_id` so a later federation layer can sync without a
//! breaking migration.

mod error;
mod holds;
mod identity;
mod mail;
mod requests;
mod store;
mod time;

pub use error::{Error, ErrorBody, Result};
pub use holds::{
    patterns_overlap, AcquireHolds, HeartbeatReport, Hold, HoldConflict, HoldFilter, HoldReport,
    SweepReport, DEFAULT_HOLD_TTL_SECONDS, MAX_HOLD_TTL_SECONDS, MIN_HOLD_TTL_SECONDS,
};
pub use identity::{Agent, Program, Project, RegisterAgent};
pub use mail::{
    AckOutcome, Importance, Inbox, InboxItem, InboxQuery, Message, NewRequest, Reply, SendMessage,
    Thread, ThreadSummary,
};
pub use requests::{Request, RequestAction, RequestFilter, RequestStatus, RequestUpdate};
pub use store::{default_home, Clock, Event, NodeInfo, Store, DB_FILE};
pub use time::fmt_ts;
