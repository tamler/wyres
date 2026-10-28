//! Hook packs: install/uninstall against a temporary HOME, and `wyres hook run`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    wyres_home: PathBuf,
    repo: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let wyres_home = home.join(".wyres");
        let repo = dir.path().join("work").join("shop");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        Env {
            _dir: dir,
            home,
            wyres_home,
            repo,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wyres"));
        c.args(args)
            .env("HOME", &self.home)
            .env("WYRES_HOME", &self.wyres_home)
            .env_remove("WYRES_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("WYRES_LOG");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "wyres {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn hook(&self, event: &str, client: &str, stdin: &Value, extra_env: &[(&str, &str)]) -> String {
        let mut cmd = self.cmd(&["hook", "run", event, "--client", client]);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook run must always exit 0");
        String::from_utf8(out.stdout).unwrap()
    }

    fn read(&self, rel: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.home.join(rel)).unwrap()).unwrap()
    }

    fn backups(&self, rel: &str) -> usize {
        let path = self.home.join(rel);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{name}.wyres-backup-"))
            })
            .count()
    }

    fn write(&self, rel: &str, value: &Value) {
        let path = self.home.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }
}

fn commands(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(c)) = m.get("command") {
                    out.push(c.clone());
                }
                m.values().for_each(|x| walk(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(v, &mut out);
    out
}

fn ours(v: &Value) -> usize {
    commands(v)
        .iter()
        .filter(|c| c.contains("wyres-hook.sh"))
        .count()
}

#[test]
fn claude_install_preserves_existing_config_and_is_idempotent() {
    let env = Env::new();
    let original = json!({
        "model": "some-model",
        "permissions": { "allow": ["Bash(ls)"] },
        "hooks": {
            "SessionStart": [{ "hooks": [{ "type": "command", "command": "echo mine" }] }],
            "PreToolUse": [{ "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard.sh" }] }]
        }
    });
    env.write(".claude/settings.json", &original);

    // Dry run writes nothing.
    let dry = env.ok(&[
        "--json",
        "hook",
        "install",
        "--client",
        "claude",
        "--dry-run",
    ]);
    let dry: Value = serde_json::from_str(&dry).unwrap();
    assert_eq!(dry["dry_run"], true);
    assert_eq!(env.read(".claude/settings.json"), original);
    assert!(!env.wyres_home.join("hooks/wyres-hook.sh").exists());

    env.ok(&["hook", "install", "--client", "claude"]);
    let after = env.read(".claude/settings.json");
    assert_eq!(after["model"], "some-model");
    assert_eq!(after["permissions"], original["permissions"]);
    assert_eq!(
        after["hooks"]["PreToolUse"],
        original["hooks"]["PreToolUse"]
    );
    let start = after["hooks"]["SessionStart"].as_array().unwrap();
    assert_eq!(
        start[0]["hooks"][0]["command"], "echo mine",
        "existing hook kept first"
    );
    assert_eq!(
        ours(&after),
        3,
        "SessionStart, UserPromptSubmit, SessionEnd"
    );
    assert!(commands(&after)
        .iter()
        .any(|c| c.ends_with("session-start --client claude")));
    assert_eq!(env.backups(".claude/settings.json"), 1);
    let script = env.wyres_home.join("hooks/wyres-hook.sh");
    let text = std::fs::read_to_string(&script).unwrap();
    assert!(text.starts_with("#!/bin/sh"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&script).unwrap().permissions().mode() & 0o111,
            0o111
        );
    }

    // Second install: no change, no new backup.
    let again: Value =
        serde_json::from_str(&env.ok(&["--json", "hook", "install", "--client", "claude"]))
            .unwrap();
    assert!(again["changes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["change"] == "unchanged"));
    assert_eq!(env.read(".claude/settings.json"), after);
    assert_eq!(env.backups(".claude/settings.json"), 1);

    // Uninstall restores the original content exactly.
    env.ok(&["hook", "uninstall", "--client", "claude"]);
    assert_eq!(env.read(".claude/settings.json"), original);
    assert!(!script.exists(), "script removed when no client uses it");
}

#[test]
fn refuses_to_touch_invalid_or_foreign_files() {
    let env = Env::new();
    let path = env.home.join(".codex/hooks.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ not json").unwrap();
    let out = env.run(&["hook", "install", "--client", "codex,claude"]);
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    // All-or-nothing: claude was not configured either.
    assert!(!env.home.join(".claude/settings.json").exists());

    env.write(
        ".grok/hooks/wyres.json",
        &json!({ "hooks": { "Stop": [] }, "owner": "someone" }),
    );
    let out = env.run(&["hook", "install", "--client", "grok"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not written by wyres"));
}

#[test]
fn installs_every_client_format() {
    let env = Env::new();
    env.write(
        ".cursor/hooks.json",
        &json!({ "version": 1, "hooks": { "afterFileEdit": [{ "command": "fmt.sh" }] } }),
    );
    let out: Value =
        serde_json::from_str(&env.ok(&["--json", "hook", "install", "--client", "all"])).unwrap();
    assert!(out["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.as_str().unwrap().contains("/hooks")));

    let codex = env.read(".codex/hooks.json");
    assert_eq!(ours(&codex), 3);
    assert!(codex["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("prompt --client codex"));

    let grok = env.read(".grok/hooks/wyres.json");
    assert_eq!(ours(&grok), 3);
    assert_eq!(
        grok["hooks"]["SessionEnd"][0]["hooks"][0]["type"],
        "command"
    );

    let cursor = env.read(".cursor/hooks.json");
    assert_eq!(cursor["version"], 1);
    assert_eq!(cursor["hooks"]["afterFileEdit"][0]["command"], "fmt.sh");
    assert!(cursor["hooks"]["sessionStart"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("session-start --client cursor"));
    assert!(cursor["hooks"]["beforeSubmitPrompt"].is_array());

    let status: Value = serde_json::from_str(&env.ok(&["--json", "hook", "status"])).unwrap();
    assert!(status
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["installed"] == true));

    // Uninstalling one client keeps the shared script for the others.
    env.ok(&["hook", "uninstall", "--client", "grok"]);
    assert!(!env.home.join(".grok/hooks/wyres.json").exists());
    assert!(env.wyres_home.join("hooks/wyres-hook.sh").exists());
    env.ok(&["hook", "uninstall", "--client", "all"]);
    assert_eq!(
        env.read(".cursor/hooks.json"),
        json!({ "version": 1, "hooks": { "afterFileEdit": [{ "command": "fmt.sh" }] } })
    );
    assert!(!env.wyres_home.join("hooks/wyres-hook.sh").exists());
}

fn stdin_for(repo: &Path, session: &str) -> Value {
    json!({ "session_id": session, "cwd": repo.join("src"), "hook_event_name": "SessionStart" })
}

#[test]
fn hook_run_records_sessions_and_injects_a_capped_digest() {
    let env = Env::new();
    let repo = env.repo.to_str().unwrap();

    // First session start registers claude@shop from the git root of cwd.
    let digest = env.hook("session-start", "claude", &stdin_for(&env.repo, "s1"), &[]);
    assert!(
        digest.contains("Wyres inbox for claude@shop: 0 unread"),
        "{digest}"
    );

    // Another agent mails it; the next prompt mentions it exactly once.
    env.ok(&[
        "agent",
        "register",
        "codex",
        "--program",
        "codex",
        "--project",
        repo,
    ]);
    env.ok(&[
        "send",
        "--from",
        "codex@shop",
        "--to",
        "claude@shop",
        "--subject",
        "Rebase please",
        "-i",
        "urgent",
        "--body",
        "secret body",
    ]);
    let notice = env.hook("prompt", "claude", &stdin_for(&env.repo, "s1"), &[]);
    assert!(
        notice.contains("1 new message(s) for claude@shop (1 urgent)"),
        "{notice}"
    );
    assert!(notice.contains("Rebase please"));
    assert!(!notice.contains("secret body"));
    assert_eq!(
        env.hook("prompt", "claude", &stdin_for(&env.repo, "s1"), &[]),
        ""
    );

    // A long backlog is capped.
    for i in 0..40 {
        env.ok(&[
            "send",
            "--from",
            "codex@shop",
            "--to",
            "claude@shop",
            "--subject",
            &format!("{i} {}", "x".repeat(200)),
        ]);
    }
    let digest = env.hook("session-start", "claude", &stdin_for(&env.repo, "s2"), &[]);
    assert!(digest.chars().count() <= 1501, "{}", digest.len());
    assert!(digest.contains("41 unread (1 urgent)"), "{digest}");

    // Holds are released at session end, which is recorded.
    env.ok(&[
        "hold",
        "acquire",
        "--agent",
        "claude@shop",
        "--project",
        repo,
        "src/**",
    ]);
    assert_eq!(
        env.hook("session-end", "claude", &stdin_for(&env.repo, "s2"), &[]),
        ""
    );
    let holds: Value = serde_json::from_str(&env.ok(&["--json", "holds"])).unwrap();
    assert!(holds.as_array().unwrap().is_empty());
    let feed: Value =
        serde_json::from_str(&env.ok(&["--json", "activity", "--kinds", "session"])).unwrap();
    let summaries: Vec<&str> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["summary"].as_str().unwrap())
        .collect();
    assert!(summaries.contains(&"claude@shop started a session (claude)"));
    assert!(summaries.contains(&"claude@shop ended a session (claude)"));
}

#[test]
fn cursor_and_grok_specifics() {
    let env = Env::new();
    let start = env.hook(
        "session-start",
        "cursor",
        &json!({ "conversation_id": "c1", "workspace_roots": [env.repo] }),
        &[],
    );
    let v: Value = serde_json::from_str(start.trim()).unwrap();
    assert!(v["additional_context"]
        .as_str()
        .unwrap()
        .contains("cursor@shop"));
    let prompt = env.hook("prompt", "cursor", &json!({}), &[]);
    assert_eq!(
        serde_json::from_str::<Value>(prompt.trim()).unwrap()["continue"],
        true
    );

    // Grok running Claude's hook file acts as grok when no Grok pack is installed...
    let grok_stdin = json!({ "sessionId": "g1", "workspaceRoot": env.repo });
    let out = env.hook(
        "session-start",
        "claude",
        &grok_stdin,
        &[("GROK_HOOK_EVENT", "SessionStart")],
    );
    assert!(out.contains("grok@shop"), "{out}");
    // ...and stands down once the Grok pack is installed, so events run once.
    env.ok(&["hook", "install", "--client", "grok"]);
    let out = env.hook(
        "session-start",
        "claude",
        &grok_stdin,
        &[("GROK_HOOK_EVENT", "SessionStart")],
    );
    assert_eq!(out, "");
}

#[test]
fn hook_run_fails_open() {
    let env = Env::new();
    // Garbage stdin, an explicit unknown agent, and an unwritable home all exit 0.
    let out = env.hook(
        "session-start",
        "codex",
        &json!("garbage"),
        &[("WYRES_AGENT", "nobody-registered")],
    );
    assert_eq!(out, "");
    let mut cmd = env.cmd(&["hook", "run", "prompt", "--client", "claude"]);
    let status = cmd
        .env("WYRES_HOME", "/dev/null/not-a-dir")
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn installed_wrapper_script_runs_under_sh() {
    let env = Env::new();
    env.ok(&["hook", "install", "--client", "codex"]);
    let codex = env.read(".codex/hooks.json");
    let command = codex["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    // Run exactly what Codex would run, with a minimal environment.
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .env_clear()
        .env("HOME", &env.home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_for(&env.repo, "x").to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Wyres inbox for codex@shop"), "{text}");
    // WYRES_HOME baked in at install time was used.
    assert!(env.wyres_home.join("wyres.db").exists());
}
