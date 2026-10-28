//! Hook packs: install a tiny POSIX wrapper into each client's hook config, and the
//! `wyres hook run` entry point that the wrapper calls to record sessions and print a
//! capped inbox + activity digest.
//!
//! Every client is treated the same way: session start registers the agent (if needed),
//! records `session.started` and prints a digest; each prompt renews holds and mentions
//! only mail that arrived since the last notice; session end releases holds.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use wyres_core::{InboxQuery, Program, RegisterAgent, SessionInfo, SessionPhase, Since, Store};

/// Substring that identifies hook entries Wyres owns inside client configs.
pub const MARKER: &str = "wyres-hook.sh";
/// Upper bound on injected digest text.
pub const MAX_DIGEST_CHARS: usize = 1500;
const MAX_STDIN_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Client {
    Claude,
    Codex,
    Grok,
    Cursor,
}

impl Client {
    pub const ALL: [Client; 4] = [Client::Claude, Client::Codex, Client::Grok, Client::Cursor];

    pub fn as_str(self) -> &'static str {
        match self {
            Client::Claude => "claude",
            Client::Codex => "codex",
            Client::Grok => "grok",
            Client::Cursor => "cursor",
        }
    }

    fn program(self) -> Program {
        match self {
            Client::Claude => Program::Claude,
            Client::Codex => Program::Codex,
            Client::Grok => Program::Grok,
            Client::Cursor => Program::Cursor,
        }
    }

    /// The user-level config file this client reads hooks from.
    pub fn config_path(self, home: &Path) -> PathBuf {
        match self {
            Client::Claude => home.join(".claude").join("settings.json"),
            Client::Codex => home.join(".codex").join("hooks.json"),
            Client::Grok => home.join(".grok").join("hooks").join("wyres.json"),
            Client::Cursor => home.join(".cursor").join("hooks.json"),
        }
    }

    /// (client event key, wyres hook event, timeout seconds)
    fn events(self) -> [(&'static str, HookEvent, u32); 3] {
        match self {
            Client::Cursor => [
                ("sessionStart", HookEvent::SessionStart, 10),
                ("beforeSubmitPrompt", HookEvent::Prompt, 5),
                ("sessionEnd", HookEvent::SessionEnd, 3),
            ],
            _ => [
                ("SessionStart", HookEvent::SessionStart, 10),
                ("UserPromptSubmit", HookEvent::Prompt, 5),
                ("SessionEnd", HookEvent::SessionEnd, 3),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookEvent {
    SessionStart,
    Prompt,
    SessionEnd,
}

impl HookEvent {
    fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "session-start",
            HookEvent::Prompt => "prompt",
            HookEvent::SessionEnd => "session-end",
        }
    }
}

/// Locations used by install/uninstall.
#[derive(Debug, Clone)]
pub struct HookEnv {
    /// The user's home directory (client configs live under it).
    pub home: PathBuf,
    /// The Wyres data directory (the wrapper script lives under it).
    pub wyres_home: PathBuf,
    /// The `wyres` binary the wrapper should call.
    pub exe: PathBuf,
}

impl HookEnv {
    pub fn detect(wyres_home: PathBuf) -> Result<HookEnv> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(HookEnv {
            home,
            wyres_home,
            exe: std::env::current_exe().context("locating the wyres binary")?,
        })
    }

    pub fn script_path(&self) -> PathBuf {
        self.wyres_home.join("hooks").join(MARKER)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Create,
    Update,
    Delete,
    Unchanged,
}

/// One planned or applied file change.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    pub path: String,
    pub change: ChangeKind,
    /// Where the previous version was copied before writing (when applied).
    pub backup: Option<String>,
    #[serde(skip)]
    after: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookReport {
    pub dry_run: bool,
    pub changes: Vec<FileChange>,
    pub notes: Vec<String>,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn script_text(env: &HookEnv) -> String {
    format!(
        "#!/bin/sh\n\
         # Wyres hook wrapper, written by `wyres hook install`; removed by `wyres hook uninstall`.\n\
         # It always exits 0, so a missing or failing wyres never blocks the agent.\n\
         WYRES_BIN={exe}\n\
         if [ ! -x \"$WYRES_BIN\" ]; then\n\
         \x20 WYRES_BIN=$(command -v wyres 2>/dev/null) || exit 0\n\
         fi\n\
         if [ -z \"${{WYRES_HOME:-}}\" ]; then\n\
         \x20 WYRES_HOME={home}\n\
         \x20 export WYRES_HOME\n\
         fi\n\
         \"$WYRES_BIN\" hook run \"$@\" 2>/dev/null\n\
         exit 0\n",
        exe = sh_quote(&env.exe.to_string_lossy()),
        home = sh_quote(&env.wyres_home.to_string_lossy()),
    )
}

fn command_for(env: &HookEnv, client: Client, event: HookEvent) -> String {
    format!(
        "{} {} --client {}",
        sh_quote(&env.script_path().to_string_lossy()),
        event.as_str(),
        client.as_str()
    )
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains(MARKER))
}

fn read_json(path: &Path) -> Result<Option<(String, Map<String, Value>)>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some((text, Map::new())));
    }
    let value: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON; not modifying it (fix or move it, then retry)",
            path.display()
        )
    })?;
    match value {
        Value::Object(map) => Ok(Some((text, map))),
        _ => bail!("{} is not a JSON object; not modifying it", path.display()),
    }
}

fn object_entry<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>> {
    let v = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    v.as_object_mut()
        .with_context(|| format!("\"{key}\" is not an object; not modifying it"))
}

fn array_entry<'a>(map: &'a mut Map<String, Value>, key: &str) -> Result<&'a mut Vec<Value>> {
    let v = map
        .entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    v.as_array_mut()
        .with_context(|| format!("hooks.{key} is not an array; not modifying it"))
}

/// Add or refresh our entries in a config, leaving every other key and hook alone.
fn install_into(client: Client, env: &HookEnv, root: &mut Map<String, Value>) -> Result<()> {
    if client == Client::Cursor && !root.contains_key("version") {
        root.insert("version".into(), json!(1));
    }
    let hooks = object_entry(root, "hooks")?;
    for (key, event, timeout) in client.events() {
        let command = command_for(env, client, event);
        let groups = array_entry(hooks, key)?;
        let ours = json!({ "type": "command", "command": command, "timeout": timeout });
        if client == Client::Cursor {
            // Cursor: a flat list of hook definitions per event.
            match groups.iter_mut().find(|h| is_ours(h)) {
                Some(h) => *h = json!({ "command": command, "timeout": timeout }),
                None => groups.push(json!({ "command": command, "timeout": timeout })),
            }
            continue;
        }
        // Claude / Codex / Grok: matcher groups, each with a `hooks` array.
        let existing = groups.iter_mut().find_map(|g| {
            g.get_mut("hooks")
                .and_then(Value::as_array_mut)
                .and_then(|hs| hs.iter_mut().find(|h| is_ours(h)))
        });
        match existing {
            Some(h) => *h = ours,
            None => groups.push(json!({ "hooks": [ours] })),
        }
    }
    Ok(())
}

/// Remove our entries; drop groups and event lists only when we emptied them.
fn uninstall_from(client: Client, root: &mut Map<String, Value>) {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for (key, _, _) in client.events() {
        let Some(groups) = hooks.get_mut(key).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = groups.len();
        if client == Client::Cursor {
            groups.retain(|h| !is_ours(h));
        } else {
            groups.retain_mut(|g| {
                let Some(hs) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
                    return true;
                };
                let n = hs.len();
                hs.retain(|h| !is_ours(h));
                !(hs.is_empty() && n > 0)
            });
        }
        if groups.is_empty() && before > 0 {
            hooks.remove(key);
        }
    }
}

fn contains_ours(root: &Map<String, Value>) -> bool {
    serde_json::to_string(root).is_ok_and(|s| s.contains(MARKER))
}

fn render(root: &Map<String, Value>) -> Result<String> {
    Ok(serde_json::to_string_pretty(root)? + "\n")
}

fn plan_config(client: Client, env: &HookEnv, install: bool) -> Result<FileChange> {
    let path = client.config_path(&env.home);
    let existing = read_json(&path)?;
    let (before, mut root) = match existing {
        Some((text, map)) => (Some(text), map),
        None => (None, Map::new()),
    };
    if client == Client::Grok && before.is_some() && !root.is_empty() && !contains_ours(&root) {
        bail!(
            "{} exists and was not written by wyres; not modifying it",
            path.display()
        );
    }
    let (change, after) = if install {
        install_into(client, env, &mut root)?;
        let after = render(&root)?;
        let change = match &before {
            None => ChangeKind::Create,
            Some(b) if b.trim() == after.trim() => ChangeKind::Unchanged,
            Some(_) => ChangeKind::Update,
        };
        (change, Some(after))
    } else {
        match &before {
            None => (ChangeKind::Unchanged, None),
            Some(b) => {
                uninstall_from(client, &mut root);
                let grok_now_empty = client == Client::Grok
                    && root
                        .get("hooks")
                        .and_then(Value::as_object)
                        .is_none_or(|h| h.is_empty());
                if grok_now_empty {
                    (ChangeKind::Delete, None)
                } else {
                    let after = render(&root)?;
                    if b.trim() == after.trim() || !contains_ours_text(b) {
                        (ChangeKind::Unchanged, None)
                    } else {
                        (ChangeKind::Update, Some(after))
                    }
                }
            }
        }
    };
    Ok(FileChange {
        path: path.display().to_string(),
        change,
        backup: None,
        after,
    })
}

fn contains_ours_text(text: &str) -> bool {
    text.contains(MARKER)
}

fn timestamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    wyres_core::fmt_ts(ms)
        .chars()
        .filter(char::is_ascii_digit)
        .collect()
}

fn apply(change: &mut FileChange) -> Result<()> {
    let path = PathBuf::from(&change.path);
    if matches!(change.change, ChangeKind::Update | ChangeKind::Delete) && path.exists() {
        let backup = PathBuf::from(format!("{}.wyres-backup-{}", change.path, timestamp()));
        std::fs::copy(&path, &backup).with_context(|| format!("backing up {}", path.display()))?;
        change.backup = Some(backup.display().to_string());
    }
    match change.change {
        ChangeKind::Unchanged => {}
        ChangeKind::Delete => std::fs::remove_file(&path)?,
        ChangeKind::Create | ChangeKind::Update => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("wyres-tmp");
            std::fs::write(&tmp, change.after.as_deref().unwrap_or_default())?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
}

fn plan_script(env: &HookEnv, install: bool, still_used: bool) -> FileChange {
    let path = env.script_path();
    let current = std::fs::read_to_string(&path).ok();
    let (change, after) = if install {
        let text = script_text(env);
        match &current {
            None => (ChangeKind::Create, Some(text)),
            Some(c) if *c == text => (ChangeKind::Unchanged, None),
            Some(_) => (ChangeKind::Update, Some(text)),
        }
    } else if current.is_some() && !still_used {
        (ChangeKind::Delete, None)
    } else {
        (ChangeKind::Unchanged, None)
    };
    FileChange {
        path: path.display().to_string(),
        change,
        backup: None,
        after,
    }
}

/// Install (or with `install = false`, uninstall) hooks for `clients`.
pub fn install(
    env: &HookEnv,
    clients: &[Client],
    install: bool,
    dry_run: bool,
) -> Result<HookReport> {
    // Plan everything first so a bad config aborts before anything is written.
    let mut changes = clients
        .iter()
        .map(|c| plan_config(*c, env, install))
        .collect::<Result<Vec<_>>>()?;
    let still_used = !install
        && Client::ALL
            .iter()
            .filter(|c| !clients.contains(c))
            .any(|c| {
                std::fs::read_to_string(c.config_path(&env.home))
                    .is_ok_and(|t| contains_ours_text(&t))
            });
    let script = plan_script(env, install, still_used);
    // The script is written before configs reference it, and removed after.
    if install {
        changes.insert(0, script);
    } else {
        changes.push(script);
    }
    if !dry_run {
        for change in &mut changes {
            apply(change)?;
        }
        if install {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    env.script_path(),
                    std::fs::Permissions::from_mode(0o755),
                )?;
            }
        }
    }
    let mut notes = Vec::new();
    if install {
        if clients.contains(&Client::Codex) {
            notes.push(
                "Codex runs new hooks only after review: open Codex and trust them under /hooks."
                    .to_string(),
            );
        }
        if clients.contains(&Client::Grok) {
            notes.push(
                "Grok also reads Claude and Cursor hook files; wyres skips those copies when the Grok pack is installed, so each event runs once."
                    .to_string(),
            );
        }
        if clients.contains(&Client::Cursor) {
            notes.push(
                "Cursor only accepts injected context at session start; prompt hooks there just renew holds."
                    .to_string(),
            );
        }
    }
    Ok(HookReport {
        dry_run,
        changes,
        notes,
    })
}

// ---------------------------------------------------------------------------
// `wyres hook run`

/// What the client told us on stdin (all optional; clients differ).
#[derive(Debug, Default)]
pub struct HookInput {
    pub cwd: Option<PathBuf>,
    pub session_id: Option<String>,
}

impl HookInput {
    pub fn parse(raw: &str) -> HookInput {
        let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let cwd = s("cwd")
            .or_else(|| s("workspaceRoot"))
            .or_else(|| {
                v.get("workspace_roots")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .map(PathBuf::from);
        let session_id = s("session_id")
            .or_else(|| s("sessionId"))
            .or_else(|| s("conversation_id"));
        HookInput { cwd, session_id }
    }

    fn read_stdin() -> HookInput {
        let stdin = std::io::stdin();
        if stdin.is_terminal() {
            return HookInput::default();
        }
        let mut raw = String::new();
        let _ = stdin.lock().take(MAX_STDIN_BYTES).read_to_string(&mut raw);
        HookInput::parse(&raw)
    }
}

/// Nearest ancestor containing `.git`, else `dir` itself.
pub fn project_root(dir: &Path) -> PathBuf {
    dir.ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Grok also executes Claude/Cursor hook files. When the Grok pack is installed, those
/// copies stand down so each event runs once; otherwise they act on Grok's behalf.
fn effective_client(client: Client, env: &HookEnv) -> Option<Client> {
    if client == Client::Grok || std::env::var_os("GROK_HOOK_EVENT").is_none() {
        return Some(client);
    }
    let grok_pack = std::fs::read_to_string(Client::Grok.config_path(&env.home))
        .is_ok_and(|t| contains_ours_text(&t));
    if grok_pack {
        None
    } else {
        Some(Client::Grok)
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.chars().count() > max {
        let cut = s
            .char_indices()
            .nth(max.saturating_sub(1))
            .map_or(s.len(), |(i, _)| i);
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// Resolve (and register on first use) the agent a hook acts for.
fn hook_agent(store: &Store, client: Client, root: &Path) -> Result<String> {
    if let Some(agent) = std::env::var("WYRES_AGENT")
        .ok()
        .filter(|a| !a.trim().is_empty())
    {
        return Ok(agent);
    }
    let project = store.ensure_project(client.as_str(), &root.to_string_lossy())?;
    let address = format!("{}@{}", client.as_str(), project.slug);
    if store.agent(&address).is_err() {
        store.register_agent(RegisterAgent {
            name: client.as_str().to_string(),
            program: client.program(),
            project: Some(project.abs_path),
            model: None,
            description: None,
        })?;
    }
    Ok(address)
}

/// Inbox + activity digest for session start, capped at [`MAX_DIGEST_CHARS`].
pub fn session_digest(store: &Store, agent: &str) -> Result<String> {
    let inbox = store.inbox(
        agent,
        InboxQuery {
            unread_only: true,
            urgent_only: false,
            limit: 5,
        },
    )?;
    let mut out = format!(
        "Wyres inbox for {}: {} unread ({} urgent), {} awaiting ack, {} open request(s).",
        inbox.agent,
        inbox.unread,
        inbox.urgent_unread,
        inbox.awaiting_ack,
        inbox.requests.len()
    );
    for item in &inbox.messages {
        let m = &item.message;
        let tag = match m.importance {
            wyres_core::Importance::Urgent => "[urgent] ",
            wyres_core::Importance::High => "[high] ",
            _ => "",
        };
        out.push_str(&format!(
            "\n- {tag}from {}: \"{}\" (message {})",
            m.from, m.subject, m.message_id
        ));
    }
    for r in inbox.requests.iter().take(5) {
        out.push_str(&format!(
            "\n- request {} \"{}\" from {} ({}{})",
            r.request_id,
            r.title,
            r.created_by,
            r.status.as_str(),
            r.owner
                .as_deref()
                .map(|o| format!(", owner {o}"))
                .unwrap_or_default()
        ));
    }
    let summary = store.activity_summary(Some(Since::Time(store.now() - 3_600_000)))?;
    out.push_str("\nLast hour: ");
    out.push_str(summary.text.lines().next().unwrap_or_default());
    for h in summary.highlights.iter().rev().take(3).rev() {
        out.push_str("\n- ");
        out.push_str(h);
    }
    out.push_str(&format!(
        "\nWyres MCP tools fetch_inbox, acknowledge and claim_request (or `wyres inbox --agent {}`) show details.",
        inbox.agent
    ));
    Ok(truncate(out, MAX_DIGEST_CHARS))
}

fn last_notified_path(env_home: &Path, agent: &str) -> PathBuf {
    let safe: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    env_home
        .join("hooks")
        .join("state")
        .join(format!("{safe}.last"))
}

/// One line about unread mail that arrived since the previous notice (or nothing).
pub fn prompt_notice(store: &Store, wyres_home: &Path, agent: &str) -> Result<Option<String>> {
    let inbox = store.inbox(
        agent,
        InboxQuery {
            unread_only: true,
            urgent_only: false,
            limit: 20,
        },
    )?;
    let state = last_notified_path(wyres_home, agent);
    let last = std::fs::read_to_string(&state).unwrap_or_default();
    let last = last.trim();
    let fresh: Vec<_> = inbox
        .messages
        .iter()
        .filter(|i| i.message.message_id.as_str() > last)
        .collect();
    let Some(newest) = fresh.first() else {
        return Ok(None);
    };
    if let Some(dir) = state.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&state, &newest.message.message_id)?;
    let urgent = fresh
        .iter()
        .filter(|i| {
            matches!(
                i.message.importance,
                wyres_core::Importance::Urgent | wyres_core::Importance::High
            )
        })
        .count();
    let subjects: Vec<String> = fresh
        .iter()
        .take(3)
        .map(|i| format!("\"{}\" from {}", i.message.subject, i.message.from))
        .collect();
    let line = format!(
        "Wyres: {} new message(s) for {} ({} urgent): {}. fetch_inbox has details.",
        fresh.len(),
        inbox.agent,
        urgent,
        subjects.join(", ")
    );
    Ok(Some(truncate(line, 500)))
}

/// Run a hook event. Returns the text to print on stdout (possibly empty).
pub fn run(
    store: &Store,
    env: &HookEnv,
    event: HookEvent,
    client: Client,
    input: HookInput,
) -> Result<String> {
    let Some(client) = effective_client(client, env) else {
        return Ok(String::new());
    };
    let dir = match input.cwd {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let root = project_root(&dir);
    let agent = hook_agent(store, client, &root)?;
    let info = SessionInfo {
        client: Some(client.as_str().to_string()),
        session_id: input.session_id,
    };
    let text = match event {
        HookEvent::SessionStart => {
            store.session(&agent, SessionPhase::Start, info)?;
            let digest = session_digest(store, &agent)?;
            // New mail shown here should not be announced again on the next prompt.
            let _ = prompt_notice(store, &env.wyres_home, &agent);
            if client == Client::Cursor {
                json!({ "additional_context": digest }).to_string()
            } else {
                digest
            }
        }
        HookEvent::Prompt => {
            store.session(&agent, SessionPhase::Beat, info)?;
            if client == Client::Cursor {
                // Cursor's prompt hook cannot add context; just let the prompt through.
                json!({ "continue": true }).to_string()
            } else {
                prompt_notice(store, &env.wyres_home, &agent)?.unwrap_or_default()
            }
        }
        HookEvent::SessionEnd => {
            store.session(&agent, SessionPhase::End, info)?;
            String::new()
        }
    };
    Ok(text)
}

/// Entry point used by the CLI: never fails, so hooks always fail open.
pub fn run_fail_open(
    store: Result<Store>,
    env: Result<HookEnv>,
    event: HookEvent,
    client: Client,
) -> String {
    let input = HookInput::read_stdin();
    let attempt = || -> Result<String> { run(&store?, &env?, event, client, input) };
    match attempt() {
        Ok(text) => text,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "wyres hook failed; continuing");
            match (event, client) {
                (HookEvent::Prompt, Client::Cursor) => json!({ "continue": true }).to_string(),
                _ => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_client_payloads() {
        let claude = HookInput::parse(
            r#"{"session_id":"a","cwd":"/w/repo","hook_event_name":"SessionStart"}"#,
        );
        assert_eq!(claude.cwd.as_deref(), Some(Path::new("/w/repo")));
        assert_eq!(claude.session_id.as_deref(), Some("a"));
        let grok = HookInput::parse(r#"{"sessionId":"b","workspaceRoot":"/w/g"}"#);
        assert_eq!(grok.cwd.as_deref(), Some(Path::new("/w/g")));
        assert_eq!(grok.session_id.as_deref(), Some("b"));
        let cursor = HookInput::parse(r#"{"conversation_id":"c","workspace_roots":["/w/c"]}"#);
        assert_eq!(cursor.cwd.as_deref(), Some(Path::new("/w/c")));
        assert_eq!(cursor.session_id.as_deref(), Some("c"));
        let junk = HookInput::parse("not json");
        assert!(junk.cwd.is_none() && junk.session_id.is_none());
    }

    #[test]
    fn quotes_for_sh() {
        assert_eq!(sh_quote("/a b/c"), "'/a b/c'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate("ééééé".into(), 3), "éé…");
        assert_eq!(truncate("abc".into(), 3), "abc");
    }
}
