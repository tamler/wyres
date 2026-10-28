//! MCP surface: a small set of tools over the core store, served by `rmcp`.

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_handler, tool_router, Json, ServerHandler};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use wyres_core::{
    AckOutcome, AcquireHolds, Agent, HeartbeatReport, Hold, HoldConflict, HoldFilter, Importance,
    Inbox, InboxQuery, Message, NewRequest, Program, Project, RegisterAgent, Reply, Request,
    RequestAction, RequestUpdate, SendMessage, Store, Thread, ThreadSummary,
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
    pub renewed: Vec<Hold>,
    pub released: Vec<String>,
    pub holds: Vec<Hold>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HeartbeatArgs {
    pub agent: String,
    /// Session is ending: release all holds and mark the agent offline.
    #[serde(default)]
    pub end: bool,
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
        self.run(move |s| s.heartbeat(&a.agent, a.end)).await
    }
}

#[tool_handler(router = self.tool_router, name = "wyres", instructions = "Wyres is a local inbox shared by coding agents on this machine. \
Call register_agent once per session (and heartbeat periodically), fetch_inbox at the start of \
work, acknowledge messages you have handled, claim requests before working on them, and \
hold_paths before editing files another agent might touch. Holds are advisory leases that \
expire unless renewed by heartbeat.")]
impl ServerHandler for WyresMcp {}
