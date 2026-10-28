//! MCP surface: a small set of tools over the core store, served by `rmcp`.

use crate::reap::{self, OrphanReport, ReapPlan};
use crate::remote::{self, RemotePeer};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::{tool, tool_handler, tool_router, ErrorData, Json, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use wyres_core::{
    AckOutcome, AcquireHolds, ActivityFeed, ActivityKind, ActivityQuery, ActivitySummary, Agent,
    AuditRecord, HeartbeatReport, Hold, HoldConflict, HoldFilter, Importance, Inbox, InboxQuery,
    LeaseMode, LeaseOutcome, Message, NewRequest, Program, Project, RegisterAgent,
    RegisterResource, Reply, Request, RequestAction, RequestUpdate, Resource, ResourceFilter,
    ResourceKind, ResourceRef, SendMessage, SessionInfo, SessionPhase, Since, Store, Thread,
    ThreadSummary,
};

#[derive(Clone)]
pub struct WyresMcp {
    store: Store,
    tool_router: ToolRouter<Self>,
}

type ToolResult<T> = Result<Json<T>, String>;

impl WyresMcp {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            tool_router: Self::tool_router(),
        }
    }

    /// Run a store call off the async runtime; errors become structured tool errors.
    async fn run<T, F>(&self, f: F) -> ToolResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> wyres_core::Result<T> + Send + 'static,
    {
        let store = self.store.clone();
        match tokio::task::spawn_blocking(move || f(&store)).await {
            Ok(Ok(v)) => Ok(Json(v)),
            Ok(Err(e)) => Err(serde_json::json!({ "error": e.body() }).to_string()),
            Err(e) => Err(
                serde_json::json!({ "error": { "code": "internal", "message": e.to_string() } })
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EnsureProjectArgs {
    /// Absolute path of the project (repository) root.
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterAgentArgs {
    /// Agent name, unique on this machine (letters, digits, '-', '_', '.').
    pub name: String,
    /// The program running this agent.
    pub program: Program,
    /// Absolute project path; lets the same name exist per project as `name@project`.
    pub project: Option<String>,
    /// Model identifier, if known.
    pub model: Option<String>,
    /// Short description of what this agent is working on.
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendMessageArgs {
    /// Sender: your agent name (or `name@project`).
    pub from: String,
    /// Recipients. Optional when replying (defaults to the original sender).
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    /// Required for new messages; replies default to "Re: <original subject>".
    pub subject: Option<String>,
    pub body: String,
    /// low | normal | high | urgent (default normal).
    pub importance: Option<Importance>,
    /// Ask recipients to explicitly acknowledge.
    #[serde(default)]
    pub ack_required: bool,
    /// Message id to reply to; the reply joins its thread.
    pub reply_to: Option<String>,
    /// When replying without explicit recipients, reply to everyone on the original.
    #[serde(default)]
    pub reply_all: bool,
    /// Also open a request (owned work item) that recipients can claim.
    #[serde(default)]
    pub create_request: bool,
    /// For requests: mark stale if not done within this many seconds.
    pub due_in_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FetchInboxArgs {
    pub agent: String,
    /// Only unread messages (default true).
    pub unread_only: Option<bool>,
    /// Only high/urgent messages.
    #[serde(default)]
    pub urgent_only: bool,
    /// Maximum messages to return (default 20).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThreadsArgs {
    /// Return every message in this thread.
    pub thread_id: Option<String>,
    /// Otherwise list threads this agent is part of.
    pub agent: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ThreadsOut {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<Thread>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threads: Option<Vec<ThreadSummary>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AcknowledgeArgs {
    pub agent: String,
    pub message_ids: Vec<String>,
    /// true: mark read and acknowledged (default); false: only mark read.
    pub ack: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClaimRequestArgs {
    pub agent: String,
    pub request_id: String,
    /// claim (default) | release | complete | cancel.
    pub action: Option<RequestAction>,
    /// Completion or cancellation note; on completion it is mailed to the requester.
    pub note: Option<String>,
    /// On claim: new due time, in seconds from now.
    pub due_in_seconds: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HoldAction {
    #[default]
    Acquire,
    Check,
    Renew,
    Release,
    List,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HoldPathsArgs {
    pub agent: String,
    /// acquire (default) | check | renew | release | list.
    #[serde(default)]
    pub action: HoldAction,
    /// Project absolute path or slug (required for acquire/check; filters list).
    pub project: Option<String>,
    /// Paths or globs relative to the project root (acquire/check).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Renew/release only these holds (default: all of the agent's holds).
    #[serde(default)]
    pub hold_ids: Vec<String>,
    /// Lease length in seconds (default 1800, clamped to 60..14400).
    pub ttl_seconds: Option<i64>,
    /// Exclusive holds (default) conflict with any overlapping hold; shared ones only with exclusive.
    pub exclusive: Option<bool>,
    pub reason: Option<String>,
}

#[derive(Debug, Default, Serialize, JsonSchema)]
pub struct HoldPathsOut {
    pub granted: Vec<Hold>,
    pub conflicts: Vec<HoldConflict>,
    /// Overlapping holds on other nodes (same project name): warnings, never blocking.
    pub warnings: Vec<HoldConflict>,
    pub renewed: Vec<Hold>,
    pub released: Vec<String>,
    pub holds: Vec<Hold>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HeartbeatArgs {
    pub agent: String,
    /// Session is starting: records a session.started event.
    #[serde(default)]
    pub start: bool,
    /// With end: keep resource leases (for servers meant to outlive the session).
    #[serde(default)]
    pub keep_resources: bool,
    /// Session is ending: release all holds and mark the agent offline.
    #[serde(default)]
    pub end: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActivityFeedArgs {
    /// Start point: duration (30m, 2h, 1d), RFC 3339 time, or an event id cursor.
    /// Omit for the most recent items.
    pub since: Option<String>,
    /// Maximum items (default 50, max 500).
    pub limit: Option<u32>,
    /// Only these categories.
    #[serde(default)]
    pub kinds: Vec<ActivityKind>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ActivitySummarizeArgs {
    /// Window start: duration (30m, 2h, 1d), RFC 3339 time, or event id. Default 1h.
    pub since: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResourceRegisterArgs {
    pub agent: String,
    pub kind: ResourceKind,
    /// Identity on this machine, e.g. `ollama@11434`, `vite@5173`, `tcp:127.0.0.1:5432`, `metal:0`.
    pub name: String,
    /// How to reach it, e.g. `http://127.0.0.1:11434`.
    pub endpoint: Option<String>,
    /// Listening port (derived from name/endpoint when omitted).
    pub port: Option<u16>,
    /// Process id (enables orphan matching and human-approved reaping).
    pub pid: Option<u32>,
    /// Project absolute path or slug.
    pub project: Option<String>,
    pub purpose: Option<String>,
    /// Exclusive resources cannot be borrowed by others (default: shared).
    #[serde(default)]
    pub exclusive: bool,
    /// Lease length in seconds (default 1800, clamped to 60..86400); renew with resource_heartbeat.
    pub ttl_seconds: Option<i64>,
    /// Free-form object: model, backend (metal|mlx|cpu|cuda), estimated_bytes, cmdline, ...
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResourceFindArgs {
    pub kind: Option<ResourceKind>,
    /// Case-insensitive substring of name, endpoint or purpose (e.g. `ollama`, `5173`).
    pub query: Option<String>,
    /// Project absolute path or slug.
    pub project: Option<String>,
    #[serde(default)]
    pub include_released: bool,
    /// Only resources owned by this node (default includes other nodes' resources).
    #[serde(default)]
    pub local_only: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ResourcesOut {
    pub resources: Vec<Resource>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResourceBorrowArgs {
    pub agent: String,
    /// Resource id, or `kind:name` such as `model_server:ollama@11434`.
    pub resource: String,
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResourceReleaseArgs {
    pub agent: String,
    /// Resource id, or `kind:name`.
    pub resource: String,
    /// Owner only: the resource has been shut down; end every lease and mark it released.
    #[serde(default)]
    pub stopped: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResourceHeartbeatArgs {
    pub agent: String,
    /// Renew only these (ids or `kind:name`); default: all of the agent's leases.
    #[serde(default)]
    pub resources: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OrphansListArgs {
    /// Also scan for listening processes no registered resource accounts for (default true).
    pub scan: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResourceReapArgs {
    pub agent: String,
    /// Resource id, `kind:name`, or `pid:N` for an unregistered listener.
    pub target: String,
    /// Why it should be reaped (shown to the human).
    pub reason: Option<String>,
    /// Grace period to evaluate against (default 600 seconds).
    pub grace_seconds: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ReapRequestOut {
    pub plan: ReapPlan,
    /// Always false: agents cannot reap. A human runs the command below.
    pub executed: bool,
    pub human_command: String,
}

fn resource_ref(raw: &str) -> wyres_core::Result<ResourceRef> {
    ResourceRef::parse(raw)
}

#[tool_router(router = tool_router)]
impl WyresMcp {
    #[tool(description = "Register a project by its absolute path (idempotent). Returns its slug.")]
    async fn ensure_project(
        &self,
        Parameters(a): Parameters<EnsureProjectArgs>,
    ) -> ToolResult<Project> {
        self.run(move |s| s.ensure_project("mcp", &a.path)).await
    }

    #[tool(
        description = "Register or update an agent identity and mark it active. Call at session start."
    )]
    async fn register_agent(
        &self,
        Parameters(a): Parameters<RegisterAgentArgs>,
    ) -> ToolResult<Agent> {
        self.run(move |s| {
            s.register_agent(RegisterAgent {
                name: a.name,
                program: a.program,
                project: a.project,
                model: a.model,
                description: a.description,
            })
        })
        .await
    }

    #[tool(
        description = "Send a message to other agents, or reply in a thread with reply_to. \
        Set create_request to open a claimable request."
    )]
    async fn send_message(
        &self,
        Parameters(a): Parameters<SendMessageArgs>,
    ) -> ToolResult<Message> {
        self.run(move |s| match (&a.reply_to, a.to.is_empty()) {
            (Some(parent), true) if !a.create_request => s.reply(Reply {
                from: a.from,
                message_id: parent.clone(),
                body: a.body,
                all: a.reply_all,
                importance: a.importance,
                ack_required: a.ack_required,
            }),
            _ => {
                let subject = match (a.subject, &a.reply_to) {
                    (Some(sub), _) => sub,
                    (None, Some(parent)) => {
                        let original = s.message(parent)?;
                        format!("Re: {}", original.subject.trim_start_matches("Re: "))
                    }
                    (None, None) => {
                        return Err(wyres_core::Error::Invalid("subject is required".into()))
                    }
                };
                s.send_message(SendMessage {
                    from: a.from,
                    to: a.to,
                    cc: a.cc,
                    subject,
                    body: a.body,
                    importance: a.importance.unwrap_or_default(),
                    ack_required: a.ack_required,
                    reply_to: a.reply_to,
                    thread_id: None,
                    request: a.create_request.then_some(NewRequest {
                        due_in_seconds: a.due_in_seconds,
                    }),
                })
            }
        })
        .await
    }

    #[tool(
        description = "Fetch an agent's inbox: messages (unread by default), counters, and open \
        requests addressed to or owned by the agent."
    )]
    async fn fetch_inbox(&self, Parameters(a): Parameters<FetchInboxArgs>) -> ToolResult<Inbox> {
        self.run(move |s| {
            s.inbox(
                &a.agent,
                InboxQuery {
                    unread_only: a.unread_only.unwrap_or(true),
                    urgent_only: a.urgent_only,
                    limit: a.limit.unwrap_or(20),
                },
            )
        })
        .await
    }

    #[tool(
        description = "Read a whole thread by thread_id, or list the threads an agent is part of."
    )]
    async fn threads(&self, Parameters(a): Parameters<ThreadsArgs>) -> ToolResult<ThreadsOut> {
        self.run(move |s| match (a.thread_id, a.agent) {
            (Some(id), _) => Ok(ThreadsOut {
                thread: Some(s.thread(&id)?),
                threads: None,
            }),
            (None, Some(agent)) => Ok(ThreadsOut {
                thread: None,
                threads: Some(s.threads(&agent, a.limit.unwrap_or(20))?),
            }),
            (None, None) => Err(wyres_core::Error::Invalid("give thread_id or agent".into())),
        })
        .await
    }

    #[tool(description = "Mark delivered messages read and (by default) acknowledged.")]
    async fn acknowledge(
        &self,
        Parameters(a): Parameters<AcknowledgeArgs>,
    ) -> ToolResult<AckOutcome> {
        self.run(move |s| s.acknowledge(&a.agent, &a.message_ids, a.ack.unwrap_or(true)))
            .await
    }

    #[tool(
        description = "Change a request's ownership: claim (default), release, complete, or cancel \
        (creator only). Stale requests may be claimed by anyone."
    )]
    async fn claim_request(
        &self,
        Parameters(a): Parameters<ClaimRequestArgs>,
    ) -> ToolResult<Request> {
        self.run(move |s| {
            s.update_request(RequestUpdate {
                agent: a.agent,
                request_id: a.request_id,
                action: a.action.unwrap_or(RequestAction::Claim),
                note: a.note,
                due_in_seconds: a.due_in_seconds,
            })
        })
        .await
    }

    #[tool(
        description = "Advisory path holds (leases on files/globs within a project). acquire is \
        all-or-nothing and reports conflicts; check reports conflicts without acquiring; renew, \
        release and list manage your holds."
    )]
    async fn hold_paths(
        &self,
        Parameters(a): Parameters<HoldPathsArgs>,
    ) -> ToolResult<HoldPathsOut> {
        self.run(move |s| {
            let need_project = || {
                a.project.clone().ok_or_else(|| {
                    wyres_core::Error::Invalid("project is required for this action".into())
                })
            };
            let mut out = HoldPathsOut::default();
            match a.action {
                HoldAction::Acquire => {
                    let report = s.acquire_holds(AcquireHolds {
                        agent: a.agent.clone(),
                        project: need_project()?,
                        paths: a.paths.clone(),
                        ttl_seconds: a.ttl_seconds,
                        exclusive: a.exclusive.unwrap_or(true),
                        reason: a.reason.clone(),
                    })?;
                    out.granted = report.granted;
                    out.conflicts = report.conflicts;
                    out.warnings = report.warnings;
                }
                HoldAction::Check => {
                    out.conflicts = s.check_holds(
                        &a.agent,
                        &need_project()?,
                        &a.paths,
                        a.exclusive.unwrap_or(true),
                    )?;
                }
                HoldAction::Renew => out.renewed = s.renew_holds(&a.agent, &a.hold_ids)?,
                HoldAction::Release => out.released = s.release_holds(&a.agent, &a.hold_ids)?,
                HoldAction::List => {
                    out.holds = s.holds(HoldFilter {
                        project: a.project.clone(),
                        agent: None,
                    })?
                }
            }
            Ok(out)
        })
        .await
    }

    #[tool(
        description = "Session heartbeat: marks the agent alive, renews its holds, and returns \
        unread/request counts. With end=true, releases all holds and marks the agent offline."
    )]
    async fn heartbeat(
        &self,
        Parameters(a): Parameters<HeartbeatArgs>,
    ) -> ToolResult<HeartbeatReport> {
        self.run(move |s| {
            let phase = if a.end {
                SessionPhase::End
            } else if a.start {
                SessionPhase::Start
            } else {
                SessionPhase::Beat
            };
            s.session(
                &a.agent,
                phase,
                SessionInfo {
                    keep_resources: a.keep_resources,
                    ..Default::default()
                },
            )
        })
        .await
    }

    #[tool(
        description = "Register a running resource (model server, dev server, port, GPU, \
        container, long job) and take its owner lease. Call resource_find first: if it is \
        already registered, borrow it instead of starting a duplicate."
    )]
    async fn resource_register(
        &self,
        Parameters(a): Parameters<ResourceRegisterArgs>,
    ) -> ToolResult<LeaseOutcome> {
        self.run(move |s| {
            s.register_resource(RegisterResource {
                agent: a.agent,
                kind: a.kind,
                name: a.name,
                endpoint: a.endpoint,
                port: a.port,
                pid: a.pid,
                project: a.project,
                purpose: a.purpose,
                mode: if a.exclusive {
                    LeaseMode::Exclusive
                } else {
                    LeaseMode::Shared
                },
                ttl_seconds: a.ttl_seconds,
                metadata: a.metadata,
            })
        })
        .await
    }

    #[tool(
        description = "Find registered resources before starting a server or job. Returns \
        live and lapsed resources with owner, leases (refcount), endpoint and status."
    )]
    async fn resource_find(
        &self,
        Parameters(a): Parameters<ResourceFindArgs>,
    ) -> ToolResult<ResourcesOut> {
        self.run(move |s| {
            Ok(ResourcesOut {
                resources: s.find_resources(ResourceFilter {
                    kind: a.kind,
                    query: a.query,
                    project: a.project,
                    include_released: a.include_released,
                    local_only: a.local_only,
                })?,
            })
        })
        .await
    }

    #[tool(
        description = "Borrow a shared resource (reference-counted lease with TTL). \
        Exclusive resources refuse; borrowing a lapsed resource adopts it; borrowing again renews."
    )]
    async fn resource_borrow(
        &self,
        Parameters(a): Parameters<ResourceBorrowArgs>,
    ) -> ToolResult<LeaseOutcome> {
        self.run(move |s| s.borrow_resource(&a.agent, &resource_ref(&a.resource)?, a.ttl_seconds))
            .await
    }

    #[tool(
        description = "Release your lease on a resource (ownership passes to the oldest \
        borrower). Owners pass stopped=true after shutting it down."
    )]
    async fn resource_release(
        &self,
        Parameters(a): Parameters<ResourceReleaseArgs>,
    ) -> ToolResult<LeaseOutcome> {
        self.run(move |s| s.release_resource(&a.agent, &resource_ref(&a.resource)?, a.stopped))
            .await
    }

    #[tool(description = "Renew your resource leases (all, or the listed ones).")]
    async fn resource_heartbeat(
        &self,
        Parameters(a): Parameters<ResourceHeartbeatArgs>,
    ) -> ToolResult<ResourcesOut> {
        self.run(move |s| {
            let targets = a
                .resources
                .iter()
                .map(|r| resource_ref(r))
                .collect::<wyres_core::Result<Vec<_>>>()?;
            Ok(ResourcesOut {
                resources: s.heartbeat_resources(&a.agent, &targets)?,
            })
        })
        .await
    }

    #[tool(
        description = "List orphan candidates: registered resources whose leases lapsed, and \
        (by default) listening processes that no registered resource accounts for."
    )]
    async fn orphans_list(
        &self,
        Parameters(a): Parameters<OrphansListArgs>,
    ) -> ToolResult<OrphanReport> {
        self.run(move |s| reap::orphans(s, a.scan.unwrap_or(true)))
            .await
    }

    #[tool(
        description = "Request a reap. This never terminates anything: it returns the reap \
        plan and safety checks, records the request in the activity feed, and gives the command \
        a human must run to confirm."
    )]
    async fn resource_reap(
        &self,
        Parameters(a): Parameters<ResourceReapArgs>,
    ) -> ToolResult<ReapRequestOut> {
        self.run(move |s| {
            let grace = a.grace_seconds.unwrap_or(reap::DEFAULT_GRACE_SECONDS);
            let plan = reap::plan(s, &a.target, grace)
                .map_err(|e| wyres_core::Error::Invalid(format!("{e:#}")))?;
            s.note_reap_request(
                &a.agent,
                &a.target,
                a.reason.as_deref().unwrap_or("orphaned"),
            )?;
            Ok(ReapRequestOut {
                human_command: format!("wyres reap {} --grace {grace} --confirm", a.target),
                plan,
                executed: false,
            })
        })
        .await
    }

    #[tool(
        description = "Read the activity feed: who is doing what (mail, requests, holds, \
        sessions), oldest first. Items have one-line summaries and ids, never message bodies. \
        Pass next_since back as since to page forward."
    )]
    async fn activity_feed(
        &self,
        Parameters(a): Parameters<ActivityFeedArgs>,
    ) -> ToolResult<ActivityFeed> {
        self.run(move |s| {
            let since = a.since.map(|v| Since::parse(&v, s.now())).transpose()?;
            s.activity(ActivityQuery {
                since,
                limit: a.limit.unwrap_or(50),
                kinds: a.kinds,
            })
        })
        .await
    }

    #[tool(
        description = "Summarize recent activity (default: last hour): event counts per \
        category, active agents, request and hold totals, highlights, and a short text digest."
    )]
    async fn activity_summarize(
        &self,
        Parameters(a): Parameters<ActivitySummarizeArgs>,
    ) -> ToolResult<ActivitySummary> {
        self.run(move |s| {
            let since = a.since.map(|v| Since::parse(&v, s.now())).transpose()?;
            s.activity_summary(since)
        })
        .await
    }
}

#[tool_handler(router = self.tool_router, name = "wyres", instructions = "Wyres is a local inbox shared by coding agents on this machine. \
Call register_agent once per session (and heartbeat periodically), fetch_inbox at the start of \
work, acknowledge messages you have handled, claim requests before working on them, and \
hold_paths before editing files another agent might touch. Before starting a server or job, \
call resource_find and borrow what is already running. Holds and leases expire unless renewed \
by heartbeat.")]
impl ServerHandler for WyresMcp {
    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let peer = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<RemotePeer>())
            .cloned();
        let Some(peer) = peer else {
            // Local caller (stdio, or loopback HTTP without --auth): no extra checks.
            let tcc = ToolCallContext::new(self, request, context);
            return self.tool_router.call(tcc).await;
        };
        let tool = request.name.to_string();
        let detail = remote::redact(request.arguments.as_ref());
        let mut record = AuditRecord {
            token_id: Some(peer.scope.token_id.clone()),
            agent: Some(peer.scope.agent.clone()),
            operation: format!("tools/call {tool}"),
            outcome: "ok".into(),
            detail: Some(detail),
            remote: peer.remote.clone(),
        };
        let store = self.store.clone();
        let mut args = request.arguments.take().unwrap_or_default();
        let checked = tokio::task::spawn_blocking(move || {
            remote::authorize(&store, &peer.scope, &tool, &mut args).map(|_| args)
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string()));
        match checked {
            Ok(args) => request.arguments = Some(args),
            Err(reason) => {
                record.outcome = "denied".into();
                record.detail = Some(format!(
                    "{reason}; args: {}",
                    record.detail.unwrap_or_default()
                ));
                remote::audit(&self.store, record).await;
                let body =
                    serde_json::json!({ "error": { "code": "forbidden", "message": reason } });
                return Ok(
                    CallToolResult::error(vec![ContentBlock::text(body.to_string())]).into(),
                );
            }
        }
        let result = self
            .tool_router
            .call(ToolCallContext::new(self, request, context))
            .await;
        let failed = match &result {
            Ok(CallToolResponse::Complete(r)) => r.is_error == Some(true),
            Ok(_) => false,
            Err(_) => true,
        };
        if failed {
            record.outcome = "error".into();
        }
        remote::audit(&self.store, record).await;
        result
    }
}
