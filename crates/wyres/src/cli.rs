//! The `wyres` command line. Every command accepts `--json` for scripts and hooks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use wyres_core::{
    AcquireHolds, HoldFilter, Importance, InboxQuery, NewRequest, Program, RegisterAgent, Reply,
    RequestAction, RequestFilter, RequestStatus, RequestUpdate, SendMessage, Store,
};

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
        /// HTTP bind address; must be loopback.
        #[arg(long, default_value = crate::serve::DEFAULT_BIND)]
        bind: String,
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
        #[arg(long)]
        end: bool,
    },
    /// Expire lapsed holds and mark overdue requests stale now.
    Sweep,
    /// Show the most recent event-log records.
    Events {
        #[arg(long, default_value_t = 20)]
        limit: u32,
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
                crate::serve::serve_http(store, bind, every).await
            }
        })?;
        return Ok(ExitCode::SUCCESS);
    }

    let store = open(home)?;
    let code = match cli.command {
        Command::Serve { .. } => unreachable!("handled above"),
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
        Command::Heartbeat { agent, end } => {
            let hb = store.heartbeat(&agent.agent, end)?;
            emit(json, &hb, output::heartbeat)
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
