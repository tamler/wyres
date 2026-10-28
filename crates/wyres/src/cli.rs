//! The `wyres` command line. Every command accepts `--json` for scripts and hooks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use wyres_core::{
    AcquireHolds, ActivityKind, ActivityQuery, HoldFilter, Importance, InboxQuery, LeaseMode,
    NewRequest, Program, RegisterAgent, RegisterResource, Reply, RequestAction, RequestFilter,
    RequestStatus, RequestUpdate, ResourceFilter, ResourceKind, ResourceRef, SendMessage,
    SessionInfo, SessionPhase, Since, Store,
};

use crate::hooks::{self, Client, HookEnv, HookEvent};
use crate::output;

#[derive(Debug, Parser)]
#[command(
    name = "wyres",
    version,
    about = "Local-first inbox, requests and path holds for coding agents"
)]
pub struct Cli {
    /// Data directory (default: $WYRES_HOME, else ~/.wyres).
    #[arg(long, global = true, env = "WYRES_HOME", hide_env_values = true)]
    pub home: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct AgentArg {
    /// Acting agent: name or name@project (default: $WYRES_AGENT).
    #[arg(long, short = 'a', env = "WYRES_AGENT")]
    pub agent: String,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the data directory and database if needed; show the local node.
    Init,
    /// Show the local node, database location and registered agents.
    Status,
    /// Run the daemon: MCP over loopback streamable HTTP (default) or stdio.
    Serve {
        /// Serve MCP on stdin/stdout instead of HTTP (for MCP client configs).
        #[arg(long)]
        stdio: bool,
        /// HTTP bind address. Loopback unless --auth and --allow-remote-bind are both given.
        #[arg(long, default_value = crate::serve::DEFAULT_BIND)]
        bind: String,
        /// Require bearer tokens (`wyres token create`) on /mcp and /activity; audit every call.
        #[arg(long)]
        auth: bool,
        /// Allow a non-loopback --bind (requires --auth). Prefer a tunnel instead.
        #[arg(long, requires = "auth")]
        allow_remote_bind: bool,
        /// Extra Host header to accept, e.g. a tunnel hostname (repeatable; requires --auth).
        #[arg(long = "allowed-host", requires = "auth")]
        allowed_hosts: Vec<String>,
        /// Act as a federation hub: serve /fed/* for paired spokes (node-signed requests).
        #[arg(long)]
        federation: bool,
        /// Seconds between sweeps that expire holds and stale requests.
        #[arg(long, default_value_t = 30)]
        sweep_secs: u64,
    },
    /// Register projects or list them.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Register agents, list them, or show one.
    #[command(subcommand)]
    Agent(AgentCmd),
    /// Send a message.
    Send {
        /// Sender: name or name@project (default: $WYRES_AGENT).
        #[arg(long, short = 'f', env = "WYRES_AGENT")]
        from: String,
        /// Recipients (repeat or comma-separate).
        #[arg(long, short = 't', value_delimiter = ',', required = true)]
        to: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        cc: Vec<String>,
        #[arg(long, short = 's')]
        subject: String,
        /// Message body; `-` reads stdin.
        #[arg(long, short = 'b', default_value = "")]
        body: String,
        #[arg(long, short = 'i', default_value = "normal")]
        importance: String,
        /// Ask recipients to acknowledge.
        #[arg(long)]
        ack: bool,
        /// Also open a claimable request.
        #[arg(long)]
        request: bool,
        /// Request due time in seconds (implies --request).
        #[arg(long)]
        due: Option<i64>,
        /// Post into an existing thread.
        #[arg(long)]
        thread: Option<String>,
    },
    /// Reply to a message within its thread.
    Reply {
        message_id: String,
        #[arg(long, short = 'f', env = "WYRES_AGENT")]
        from: String,
        /// Reply body; `-` reads stdin.
        #[arg(long, short = 'b')]
        body: String,
        /// Reply to all original recipients.
        #[arg(long)]
        all: bool,
        #[arg(long, short = 'i')]
        importance: Option<String>,
        #[arg(long)]
        ack: bool,
    },
    /// Show an agent's inbox and open requests.
    Inbox {
        #[command(flatten)]
        agent: AgentArg,
        /// Only unread messages.
        #[arg(long)]
        unread: bool,
        /// Only high/urgent messages.
        #[arg(long)]
        urgent: bool,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Mark messages read and acknowledged.
    Ack {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(required = true)]
        message_ids: Vec<String>,
        /// Only mark read; do not acknowledge.
        #[arg(long)]
        read_only: bool,
    },
    /// List threads an agent is part of.
    Threads {
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show every message in a thread.
    Thread { thread_id: String },
    /// List requests.
    Requests {
        /// open | claimed | done | stale | cancelled
        #[arg(long)]
        status: Option<String>,
        /// Owner name or name@project.
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Claim, release, complete or cancel a request.
    Request {
        /// claim | release | complete | cancel
        action: String,
        request_id: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        note: Option<String>,
        /// On claim: due in this many seconds.
        #[arg(long)]
        due: Option<i64>,
    },
    /// Acquire, check, renew or release path holds.
    #[command(subcommand)]
    Hold(HoldCmd),
    /// List live path holds.
    Holds {
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Session heartbeat: renew holds; with --end release them and go offline.
    Heartbeat {
        #[command(flatten)]
        agent: AgentArg,
        /// Session is starting: record a `session.started` event.
        #[arg(long, conflicts_with = "end")]
        start: bool,
        #[arg(long)]
        end: bool,
        /// With --end: keep resource leases (servers meant to outlive the session).
        #[arg(long, requires = "end")]
        keep: bool,
    },
    /// Find registered resources (model servers, dev servers, ports, GPUs, containers, jobs).
    Resources {
        /// model_server | dev_server | port | gpu | container | long_job
        #[arg(long)]
        kind: Option<String>,
        /// Substring of name, endpoint or purpose.
        #[arg(long, short = 'q')]
        query: Option<String>,
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        /// Include released resources.
        #[arg(long)]
        all: bool,
        /// Only resources owned by this node (default includes other nodes').
        #[arg(long)]
        local: bool,
    },
    /// Register a resource or renew resource leases.
    #[command(subcommand)]
    Resource(ResourceCmd),
    /// Borrow a shared resource (id or kind:name).
    Borrow {
        target: String,
        #[command(flatten)]
        agent: AgentArg,
        /// Lease seconds (default 1800).
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Release your lease on a resource (id or kind:name).
    Release {
        target: String,
        #[command(flatten)]
        agent: AgentArg,
        /// Owner: the resource was shut down; end all leases and mark it released.
        #[arg(long)]
        stopped: bool,
    },
    /// List orphan candidates: lapsed resources and unregistered listeners.
    Orphans {
        /// Skip the listening-process scan (lapsed registered resources only).
        #[arg(long)]
        no_scan: bool,
    },
    /// Reap an orphan (resource id, kind:name, or pid:N). Without --confirm, only shows the plan.
    Reap {
        target: String,
        /// Seconds the target must have been orphaned (or alive, for pid targets).
        #[arg(long, default_value_t = crate::reap::DEFAULT_GRACE_SECONDS)]
        grace: i64,
        /// Human confirmation: actually send SIGTERM. Agents never pass this.
        #[arg(long)]
        confirm: bool,
        /// With --confirm: SIGKILL if the process survives SIGTERM.
        #[arg(long, requires = "confirm")]
        force: bool,
    },
    /// Create, list or revoke bearer tokens for remote peers.
    #[command(subcommand)]
    Token(TokenCmd),
    /// Show the audit log of remote (token-authenticated) calls.
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Show or rename this node.
    #[command(subcommand)]
    Node(NodeCmd),
    /// Pair nodes for federation: invite (hub), join (spoke), revoke.
    #[command(subcommand)]
    Pair(PairCmd),
    /// List paired nodes.
    Peers,
    /// Spoke: push this node's events to the hub and pull everyone else's.
    Sync {
        /// Keep syncing every --every seconds.
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 30)]
        every: u64,
    },
    /// Nudge agents with unread mail through tmux, cmux or Herdr (optional adapters).
    #[command(args_conflicts_with_subcommands = true)]
    Wake {
        #[command(subcommand)]
        cmd: Option<WakeCmd>,
        #[command(flatten)]
        run: WakeRun,
    },
    /// Show the activity feed (who is doing what), or a summary of it.
    Activity {
        /// Start point: a duration (30m, 2h, 1d), an RFC 3339 time, or an event id cursor.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Only these categories: mail, request, hold, resource, session, system, federation.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Print a summary (counts, active agents, highlights) instead of items.
        #[arg(long)]
        summary: bool,
    },
    /// Install, remove or run client hook packs.
    #[command(subcommand)]
    Hook(HookCmd),
    /// Expire lapsed holds and mark overdue requests stale now.
    Sweep,
    /// Show the most recent event-log records.
    Events {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Debug, Args)]
pub struct WakeRun {
    /// Agent to wake (default: $WYRES_AGENT); or use --all.
    #[arg(long, short = 'a', env = "WYRES_AGENT", conflicts_with = "all")]
    agent: Option<String>,
    /// Wake every agent with a stored target that has unread mail or open requests.
    #[arg(long)]
    all: bool,
    /// Adapter: tmux | cmux | herdr (default: the agent's stored target).
    #[arg(long)]
    via: Option<String>,
    /// tmux pane, cmux surface id (or `notify`), or Herdr agent name.
    #[arg(long)]
    target: Option<String>,
    /// Text to send (default: a one-line inbox summary).
    #[arg(long)]
    message: Option<String>,
    /// Wake even when nothing is unread.
    #[arg(long)]
    force: bool,
    /// Print the adapter commands without running them.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum WakeCmd {
    /// Store where to wake an agent (hooks capture tmux/cmux targets automatically).
    Set {
        #[command(flatten)]
        agent: AgentArg,
        /// tmux | cmux | herdr
        #[arg(long)]
        via: String,
        #[arg(long)]
        target: String,
    },
    /// Forget an agent's wake target.
    Clear {
        #[command(flatten)]
        agent: AgentArg,
    },
    /// List stored wake targets.
    List,
}

#[derive(Debug, Subcommand)]
pub enum NodeCmd {
    /// Show this node's id, name and key fingerprint.
    Show,
    /// Rename this node (do this before pairing; names must be unique across nodes).
    Name { name: String },
}

#[derive(Debug, Subcommand)]
pub enum PairCmd {
    /// Hub: print a single-use invite code for a spoke.
    Invite {
        /// URL spokes use to reach this hub (e.g. its tunnel URL).
        #[arg(long)]
        url: String,
        /// Permissions for the spoke: read_feed, send_mail, borrow_resource (comma-separated).
        #[arg(
            long,
            value_delimiter = ',',
            default_value = "read_feed,send_mail,borrow_resource"
        )]
        perms: Vec<String>,
        /// Invite lifetime: seconds or a duration like 30m, 24h.
        #[arg(long, default_value = "1h")]
        expires: String,
    },
    /// Spoke: redeem an invite code and record the hub.
    Join { code: String },
    /// Stop accepting (or using) a paired node, by name or id.
    Revoke { node: String },
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Create a token that acts as one agent within project globs. Printed once.
    Create {
        /// The agent the token acts as (register it first, e.g. with --program grok_bot).
        #[arg(long, short = 'a')]
        agent: String,
        /// Project path or slug glob the token may touch (repeatable), e.g. '/work/*' or shop.
        #[arg(long = "project", short = 'p')]
        projects: Vec<String>,
        /// A note to recognise the token by.
        #[arg(long)]
        label: Option<String>,
        /// Expiry: seconds or a duration like 12h, 30d.
        #[arg(long)]
        expires: Option<String>,
    },
    List,
    /// Revoke a token by id (takes effect on the next request).
    Revoke {
        token_id: String,
    },
}

fn parse_secs(raw: &str) -> anyhow::Result<i64> {
    let raw = raw.trim();
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (num, unit) = raw.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid duration {raw:?}"))?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 604_800,
        _ => anyhow::bail!("invalid duration {raw:?}: use s, m, h, d or w"),
    };
    Ok(n * mult)
}

#[derive(Debug, Subcommand)]
pub enum ResourceCmd {
    /// Register a running resource and take the owner lease.
    Register {
        /// model_server | dev_server | port | gpu | container | long_job
        kind: String,
        /// Identity, e.g. ollama@11434, vite@5173, tcp:127.0.0.1:5432, metal:0.
        name: String,
        #[command(flatten)]
        agent: AgentArg,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        pid: Option<u32>,
        /// Project path or slug.
        #[arg(long, short = 'p')]
        project: Option<String>,
        #[arg(long)]
        purpose: Option<String>,
        /// Refuse borrowers.
        #[arg(long)]
        exclusive: bool,
        /// Lease seconds (default 1800, max 86400).
        #[arg(long)]
        ttl: Option<i64>,
        /// Metadata as key=value (repeatable), e.g. --meta model=llama3 --meta backend=metal.
        #[arg(long = "meta", value_name = "KEY=VALUE")]
        meta: Vec<String>,
    },
    /// Renew your resource leases (all, or the given ids / kind:name).
    Heartbeat {
        #[command(flatten)]
        agent: AgentArg,
        targets: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ClientSel {
    Claude,
    Codex,
    Grok,
    Cursor,
    All,
}

fn clients(sel: &[ClientSel]) -> Vec<Client> {
    let mut out = Vec::new();
    for s in sel {
        let add: &[Client] = match s {
            ClientSel::Claude => &[Client::Claude],
            ClientSel::Codex => &[Client::Codex],
            ClientSel::Grok => &[Client::Grok],
            ClientSel::Cursor => &[Client::Cursor],
            ClientSel::All => &Client::ALL,
        };
        for c in add {
            if !out.contains(c) {
                out.push(*c);
            }
        }
    }
    out
}

#[derive(Debug, Subcommand)]
pub enum HookCmd {
    /// Add Wyres hooks to client configs (idempotent; backs up files it changes).
    Install {
        /// Clients to configure (repeat or comma-separate).
        #[arg(long, value_delimiter = ',', required = true)]
        client: Vec<ClientSel>,
        /// Show what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove Wyres hooks from client configs, leaving other hooks untouched.
    Uninstall {
        #[arg(long, value_delimiter = ',', required = true)]
        client: Vec<ClientSel>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Show which clients have Wyres hooks installed.
    Status,
    /// Called by the hook wrapper; reads the client's JSON on stdin. Always exits 0.
    Run {
        event: HookEvent,
        #[arg(long)]
        client: Client,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    /// Register a project by path (default: current directory).
    Ensure {
        path: Option<PathBuf>,
    },
    List,
}

#[derive(Debug, Subcommand)]
pub enum AgentCmd {
    /// Register or update an agent.
    Register {
        name: String,
        /// claude | grok | codex | cursor | grok_bot | other
        #[arg(long)]
        program: String,
        /// Project path to scope the name to (enables name@project).
        #[arg(long)]
        project: Option<PathBuf>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    List,
    Show {
        name: String,
    },
}

#[derive(Debug, Args)]
pub struct ProjectArg {
    /// Project path or slug (default: current directory).
    #[arg(long, short = 'p')]
    pub project: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum HoldCmd {
    /// Acquire holds on paths/globs (exit code 3 on conflict; nothing is granted).
    Acquire {
        #[command(flatten)]
        agent: AgentArg,
        #[command(flatten)]
        project: ProjectArg,
        #[arg(required = true)]
        paths: Vec<String>,
        /// Lease seconds (default 1800).
        #[arg(long)]
        ttl: Option<i64>,
        /// Shared instead of exclusive.
        #[arg(long)]
        shared: bool,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Report conflicts for paths without acquiring (exit code 3 on conflict).
    Check {
        #[command(flatten)]
        agent: AgentArg,
        #[command(flatten)]
        project: ProjectArg,
        #[arg(required = true)]
        paths: Vec<String>,
        #[arg(long)]
        shared: bool,
    },
    /// Renew your holds (all, or the given ids).
    Renew {
        #[command(flatten)]
        agent: AgentArg,
        hold_ids: Vec<String>,
    },
    /// Release your holds (all, or the given ids).
    Release {
        #[command(flatten)]
        agent: AgentArg,
        hold_ids: Vec<String>,
    },
}

/// Exit status used when holds conflict, so hooks can branch on it.
pub const EXIT_CONFLICT: u8 = 3;

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(matches!(cli.command, Command::Serve { .. }));
    let json = cli.json;
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            let body = match err.downcast_ref::<wyres_core::Error>() {
                Some(e) => e.body(),
                None => wyres_core::ErrorBody {
                    code: "error",
                    message: format!("{err:#}"),
                },
            };
            if json {
                println!("{}", serde_json::json!({ "error": body }));
            } else {
                eprintln!("wyres: {}", body.message);
            }
            ExitCode::FAILURE
        }
    }
}

fn init_tracing(serving: bool) {
    let default = if serving { "info" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_env("WYRES_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn open(home: Option<&Path>) -> anyhow::Result<Store> {
    let home = match home {
        Some(h) => h.to_path_buf(),
        None => wyres_core::default_home()?,
    };
    Ok(Store::open(&home)?)
}

fn body_arg(body: String) -> anyhow::Result<String> {
    if body == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        Ok(body)
    }
}

fn abs(path: &Path) -> anyhow::Result<String> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(p.to_string_lossy().into_owned())
}

/// Resolve `--project`: a path (ensured on the fly) or an existing slug; default cwd.
fn project_key(store: &Store, actor: &str, project: Option<String>) -> anyhow::Result<String> {
    let looks_like_path = |p: &str| p.contains('/') || p.contains('\\') || p == "." || p == "..";
    match project {
        Some(p) if !looks_like_path(&p) => Ok(p),
        Some(p) => Ok(store.ensure_project(actor, &abs(Path::new(&p))?)?.abs_path),
        None => Ok(store
            .ensure_project(actor, &abs(&std::env::current_dir()?)?)?
            .abs_path),
    }
}

fn emit<T: Serialize>(json: bool, value: &T, human: impl FnOnce(&T) -> String) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).unwrap_or_else(|e| e.to_string())
        );
    } else {
        let text = human(value);
        if !text.is_empty() {
            println!("{}", text.trim_end());
        }
    }
    ExitCode::SUCCESS
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let json = cli.json;
    let home = cli.home.as_deref();
    if let Command::Serve {
        stdio,
        bind,
        auth,
        allow_remote_bind,
        allowed_hosts,
        federation,
        sweep_secs,
    } = &cli.command
    {
        let store = open(home)?;
        let every = Duration::from_secs((*sweep_secs).max(1));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            if *stdio {
                crate::serve::serve_stdio(store, every).await
            } else {
                let opts = crate::serve::HttpOptions {
                    auth: *auth,
                    allow_remote_bind: *allow_remote_bind,
                    allowed_hosts: allowed_hosts.clone(),
                    federation: *federation,
                };
                crate::serve::serve_http(store, bind, opts, every).await
            }
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    let wyres_home = || -> anyhow::Result<PathBuf> {
        Ok(match home {
            Some(h) => h.to_path_buf(),
            None => wyres_core::default_home()?,
        })
    };
    if let Command::Hook(cmd) = &cli.command {
        return hook_command(cmd, json, wyres_home);
    }

    let store = open(home)?;
    let code = match cli.command {
        Command::Serve { .. } | Command::Hook(_) => unreachable!("handled above"),
        Command::Init | Command::Status => {
            let status = output::Status {
                node: store.node()?,
                db_path: store
                    .db_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                projects: store.projects()?,
                agents: store.agents()?,
            };
            emit(json, &status, output::status)
        }
        Command::Project(ProjectCmd::Ensure { path }) => {
            let path = match path {
                Some(p) => abs(&p)?,
                None => abs(&std::env::current_dir()?)?,
            };
            let p = store.ensure_project("cli", &path)?;
            emit(json, &p, |p| format!("{}  {}", p.slug, p.abs_path))
        }
        Command::Project(ProjectCmd::List) => {
            let ps = store.projects()?;
            emit(json, &ps, |ps| {
                ps.iter()
                    .map(|p| format!("{}  {}\n", p.slug, p.abs_path))
                    .collect()
            })
        }
        Command::Agent(AgentCmd::Register {
            name,
            program,
            project,
            model,
            description,
        }) => {
            let project = project.as_deref().map(abs).transpose()?;
            let a = store.register_agent(RegisterAgent {
                name,
                program: program.parse::<Program>()?,
                project,
                model,
                description,
            })?;
            emit(json, &a, |a| {
                format!("registered {} ({})", a.address, a.program)
            })
        }
        Command::Agent(AgentCmd::List) => {
            let agents = store.agents()?;
            emit(json, &agents, |a| output::agents(a))
        }
        Command::Agent(AgentCmd::Show { name }) => {
            let a = store.agent(&name)?;
            emit(json, &a, |a| output::agents(std::slice::from_ref(a)))
        }
        Command::Send {
            from,
            to,
            cc,
            subject,
            body,
            importance,
            ack,
            request,
            due,
            thread,
        } => {
            let m = store.send_message(SendMessage {
                from,
                to,
                cc,
                subject,
                body: body_arg(body)?,
                importance: importance.parse::<Importance>()?,
                ack_required: ack,
                reply_to: None,
                thread_id: thread,
                request: (request || due.is_some()).then_some(NewRequest {
                    due_in_seconds: due,
                }),
            })?;
            emit(json, &m, output::sent)
        }
        Command::Reply {
            message_id,
            from,
            body,
            all,
            importance,
            ack,
        } => {
            let m = store.reply(Reply {
                from,
                message_id,
                body: body_arg(body)?,
                all,
                importance: importance.map(|i| i.parse()).transpose()?,
                ack_required: ack,
            })?;
            emit(json, &m, output::sent)
        }
        Command::Inbox {
            agent,
            unread,
            urgent,
            limit,
        } => {
            let inbox = store.inbox(
                &agent.agent,
                InboxQuery {
                    unread_only: unread,
                    urgent_only: urgent,
                    limit,
                },
            )?;
            emit(json, &inbox, output::inbox)
        }
        Command::Ack {
            agent,
            message_ids,
            read_only,
        } => {
            let out = store.acknowledge(&agent.agent, &message_ids, !read_only)?;
            emit(json, &out, |o| {
                format!(
                    "{} updated, {} unchanged",
                    o.updated.len(),
                    o.unchanged.len()
                )
            })
        }
        Command::Threads { agent, limit } => {
            let threads = store.threads(&agent.agent, limit)?;
            emit(json, &threads, |t| output::threads(t))
        }
        Command::Thread { thread_id } => {
            let thread = store.thread(&thread_id)?;
            emit(json, &thread, output::thread)
        }
        Command::Requests {
            status,
            owner,
            limit,
        } => {
            let reqs = store.requests(RequestFilter {
                status: status.map(|s| s.parse::<RequestStatus>()).transpose()?,
                owner,
                limit,
            })?;
            emit(json, &reqs, |r| output::requests(r))
        }
        Command::Request {
            action,
            request_id,
            agent,
            note,
            due,
        } => {
            let r = store.update_request(RequestUpdate {
                agent: agent.agent,
                request_id,
                action: action.parse::<RequestAction>()?,
                note,
                due_in_seconds: due,
            })?;
            emit(json, &r, |r| output::requests(std::slice::from_ref(r)))
        }
        Command::Hold(HoldCmd::Acquire {
            agent,
            project,
            paths,
            ttl,
            shared,
            reason,
        }) => {
            let project = project_key(&store, &agent.agent, project.project)?;
            let report = store.acquire_holds(AcquireHolds {
                agent: agent.agent,
                project,
                paths,
                ttl_seconds: ttl,
                exclusive: !shared,
                reason,
            })?;
            let conflicted = !report.conflicts.is_empty();
            emit(json, &report, output::hold_report);
            if conflicted {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                ExitCode::SUCCESS
            }
        }
        Command::Hold(HoldCmd::Check {
            agent,
            project,
            paths,
            shared,
        }) => {
            let project = project_key(&store, &agent.agent, project.project)?;
            let conflicts = store.check_holds(&agent.agent, &project, &paths, !shared)?;
            let conflicted = !conflicts.is_empty();
            emit(json, &conflicts, |c| {
                if c.is_empty() {
                    "no conflicts".to_string()
                } else {
                    output::conflicts(c)
                }
            });
            if conflicted {
                ExitCode::from(EXIT_CONFLICT)
            } else {
                ExitCode::SUCCESS
            }
        }
        Command::Hold(HoldCmd::Renew { agent, hold_ids }) => {
            let holds = store.renew_holds(&agent.agent, &hold_ids)?;
            emit(json, &holds, |h| output::holds(h))
        }
        Command::Hold(HoldCmd::Release { agent, hold_ids }) => {
            let ids = store.release_holds(&agent.agent, &hold_ids)?;
            emit(json, &ids, |ids| format!("released {} hold(s)", ids.len()))
        }
        Command::Holds { project, agent } => {
            let holds = store.holds(HoldFilter { project, agent })?;
            emit(json, &holds, |h| output::holds(h))
        }
        Command::Heartbeat {
            agent,
            start,
            end,
            keep,
        } => {
            let phase = match (start, end) {
                (true, _) => SessionPhase::Start,
                (_, true) => SessionPhase::End,
                _ => SessionPhase::Beat,
            };
            let hb = store.session(
                &agent.agent,
                phase,
                SessionInfo {
                    keep_resources: keep,
                    ..Default::default()
                },
            )?;
            emit(json, &hb, output::heartbeat)
        }
        Command::Resources {
            kind,
            query,
            project,
            all,
            local,
        } => {
            let rs = store.find_resources(ResourceFilter {
                kind: kind.map(|k| k.parse::<ResourceKind>()).transpose()?,
                query,
                project,
                include_released: all,
                local_only: local,
            })?;
            emit(json, &rs, |r| output::resources(r))
        }
        Command::Resource(ResourceCmd::Register {
            kind,
            name,
            agent,
            endpoint,
            port,
            pid,
            project,
            purpose,
            exclusive,
            ttl,
            meta,
        }) => {
            let mut metadata = serde_json::Map::new();
            for kv in meta {
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--meta expects KEY=VALUE, got {kv:?}"))?;
                metadata.insert(k.to_string(), serde_json::Value::String(v.to_string()));
            }
            let project = project
                .map(|p| project_key(&store, &agent.agent, Some(p)))
                .transpose()?;
            let out = store.register_resource(RegisterResource {
                agent: agent.agent,
                kind: kind.parse()?,
                name,
                endpoint,
                port,
                pid,
                project,
                purpose,
                mode: if exclusive {
                    LeaseMode::Exclusive
                } else {
                    LeaseMode::Shared
                },
                ttl_seconds: ttl,
                metadata: Some(serde_json::Value::Object(metadata)),
            })?;
            emit(json, &out, output::lease_outcome)
        }
        Command::Resource(ResourceCmd::Heartbeat { agent, targets }) => {
            let targets = targets
                .iter()
                .map(|t| ResourceRef::parse(t))
                .collect::<Result<Vec<_>, _>>()?;
            let rs = store.heartbeat_resources(&agent.agent, &targets)?;
            emit(json, &rs, |r| output::resources(r))
        }
        Command::Borrow { target, agent, ttl } => {
            let out = store.borrow_resource(&agent.agent, &ResourceRef::parse(&target)?, ttl)?;
            emit(json, &out, output::lease_outcome)
        }
        Command::Release {
            target,
            agent,
            stopped,
        } => {
            let out =
                store.release_resource(&agent.agent, &ResourceRef::parse(&target)?, stopped)?;
            emit(json, &out, output::lease_outcome)
        }
        Command::Orphans { no_scan } => {
            let report = crate::reap::orphans(&store, !no_scan)?;
            emit(json, &report, output::orphans)
        }
        Command::Reap {
            target,
            grace,
            confirm,
            force,
        } => {
            let plan = crate::reap::plan(&store, &target, grace)?;
            if confirm {
                let done = crate::reap::execute(&store, plan, force)?;
                emit(json, &done, output::reap_plan)
            } else {
                emit(json, &plan, output::reap_plan)
            }
        }
        Command::Token(TokenCmd::Create {
            agent,
            projects,
            label,
            expires,
        }) => {
            let expires = expires.as_deref().map(parse_secs).transpose()?;
            let t = store.create_token(&agent, &projects, label, expires)?;
            emit(json, &t, |t| {
                format!(
                    "token {} for {} (projects: {})\n\n{}\n\nThis is the only time the token is shown. \
                     Give it to the remote peer as `Authorization: Bearer <token>`.",
                    t.token.token_id,
                    t.token.agent,
                    if t.token.project_globs.is_empty() {
                        "none".to_string()
                    } else {
                        t.token.project_globs.join(", ")
                    },
                    t.secret
                )
            })
        }
        Command::Token(TokenCmd::List) => {
            let ts = store.tokens()?;
            emit(json, &ts, |ts| output::tokens(ts))
        }
        Command::Token(TokenCmd::Revoke { token_id }) => {
            let t = store.revoke_token(&token_id)?;
            emit(json, &t, |t| {
                format!("revoked token {} ({})", t.token_id, t.agent)
            })
        }
        Command::Node(NodeCmd::Show) => {
            let n = store.node()?;
            emit(json, &n, |n| {
                format!(
                    "node {} ({})\nkey fingerprint {}",
                    n.display_name, n.node_id, n.fingerprint
                )
            })
        }
        Command::Node(NodeCmd::Name { name }) => {
            let n = store.set_node_name(&name)?;
            emit(json, &n, |n| format!("this node is now {}", n.display_name))
        }
        Command::Pair(PairCmd::Invite {
            url,
            perms,
            expires,
        }) => {
            let perms = perms
                .iter()
                .map(|p| p.parse::<wyres_core::Perm>())
                .collect::<Result<Vec<_>, _>>()?;
            let code = store.create_invite(&url, &perms, parse_secs(&expires)?)?;
            let node = store.node()?;
            let out = serde_json::json!({ "code": code, "hub": node.display_name, "fingerprint": node.fingerprint });
            emit(json, &out, |_| {
                format!(
                    "invite for hub {} (key {}), single use:\n\n{code}\n\nOn the spoke: wyres pair join <code>",
                    node.display_name, node.fingerprint
                )
            })
        }
        Command::Pair(PairCmd::Join { code }) => {
            let peer = crate::fed::join(&store, &code)?;
            emit(json, &peer, |p| {
                format!(
                    "paired with hub {} at {} (key {}); run `wyres sync`",
                    p.display_name,
                    p.url.as_deref().unwrap_or("-"),
                    p.fingerprint
                )
            })
        }
        Command::Pair(PairCmd::Revoke { node }) => {
            let p = store.revoke_peer(&node)?;
            emit(json, &p, |p| format!("revoked {}", p.display_name))
        }
        Command::Peers => {
            let ps = store.peers()?;
            emit(json, &ps, |p| output::peers(p))
        }
        Command::Sync { watch, every } => loop {
            let report = crate::fed::sync(&store);
            match report {
                Ok(r) => {
                    emit(json, &r, output::sync_report);
                }
                Err(e) if watch => eprintln!("wyres: sync failed: {e:#}"),
                Err(e) => return Err(e),
            }
            if !watch {
                break ExitCode::SUCCESS;
            }
            std::thread::sleep(Duration::from_secs(every.max(5)));
        },
        Command::Wake {
            cmd: Some(WakeCmd::Set { agent, via, target }),
            ..
        } => {
            let t = store.set_wake_target(&agent.agent, via.parse()?, &target, false)?;
            emit(json, &t, |t| match t {
                Some(t) => format!("wake {} via {} at {}", t.agent, t.via.as_str(), t.target),
                None => String::new(),
            })
        }
        Command::Wake {
            cmd: Some(WakeCmd::Clear { agent }),
            ..
        } => {
            let cleared = store.clear_wake_target(&agent.agent)?;
            emit(json, &serde_json::json!({ "cleared": cleared }), |_| {
                if cleared {
                    "cleared".into()
                } else {
                    "no wake target was set".into()
                }
            })
        }
        Command::Wake {
            cmd: Some(WakeCmd::List),
            ..
        } => {
            let ts = store.wake_targets()?;
            emit(json, &ts, |ts| output::wake_targets(ts))
        }
        Command::Wake { cmd: None, run } => {
            let opts = crate::wake::WakeOptions {
                via: run.via.as_deref().map(str::parse).transpose()?,
                target: run.target,
                message: run.message,
                force: run.force,
                dry_run: run.dry_run,
            };
            let plans = if run.all {
                crate::wake::wake_all(&store, &opts)?
            } else {
                let agent = run
                    .agent
                    .ok_or_else(|| anyhow::anyhow!("give --agent (or set WYRES_AGENT) or --all"))?;
                vec![crate::wake::wake(&store, &agent, &opts)?]
            };
            emit(json, &plans, |p| output::wake_plans(p))
        }
        Command::Audit { limit } => {
            let entries = store.audit_log(limit)?;
            emit(json, &entries, |e| output::audit(e))
        }
        Command::Activity {
            since,
            limit,
            kinds,
            summary,
        } => {
            let since = since.map(|s| Since::parse(&s, store.now())).transpose()?;
            if summary {
                let sum = store.activity_summary(since)?;
                emit(json, &sum, |s| s.text.clone())
            } else {
                let feed = store.activity(ActivityQuery {
                    since,
                    limit,
                    kinds: kinds
                        .iter()
                        .map(|k| k.parse::<ActivityKind>())
                        .collect::<Result<_, _>>()?,
                })?;
                emit(json, &feed, output::activity)
            }
        }
        Command::Sweep => {
            let r = store.sweep()?;
            emit(json, &r, |r| {
                format!(
                    "expired {} hold(s), {} request(s) went stale",
                    r.expired_holds, r.stale_requests
                )
            })
        }
        Command::Events { limit } => {
            let events = store.events(limit)?;
            emit(json, &events, |e| output::events(e))
        }
    };
    Ok(code)
}

fn hook_command(
    cmd: &HookCmd,
    json: bool,
    wyres_home: impl Fn() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<ExitCode> {
    match cmd {
        HookCmd::Run { event, client } => {
            // Fail open: whatever happens, print what we have and exit 0.
            let store = wyres_home().and_then(|h| Ok(Store::open(&h)?));
            let env = wyres_home().and_then(HookEnv::detect);
            let text = hooks::run_fail_open(store, env, *event, *client);
            if !text.is_empty() {
                println!("{text}");
            }
            Ok(ExitCode::SUCCESS)
        }
        HookCmd::Install { client, dry_run } | HookCmd::Uninstall { client, dry_run } => {
            let install = matches!(cmd, HookCmd::Install { .. });
            let env = HookEnv::detect(wyres_home()?)?;
            let report = hooks::install(&env, &clients(client), install, *dry_run)?;
            Ok(emit(json, &report, output::hook_report))
        }
        HookCmd::Status => {
            let env = HookEnv::detect(wyres_home()?)?;
            let rows: Vec<output::HookStatus> = Client::ALL
                .iter()
                .map(|c| {
                    let path = c.config_path(&env.home);
                    output::HookStatus {
                        client: c.as_str().to_string(),
                        installed: std::fs::read_to_string(&path)
                            .is_ok_and(|t| t.contains(hooks::MARKER)),
                        config: path.display().to_string(),
                    }
                })
                .collect();
            Ok(emit(json, &rows, |r| output::hook_status(r)))
        }
    }
}
