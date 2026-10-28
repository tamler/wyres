//! `wyres wake` adapters, exercised with fake tmux / cmux / herdr binaries on PATH.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    log: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.path().join("calls.log");
        for tool in ["tmux", "cmux", "herdr"] {
            let script = bin.join(tool);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nprintf '%s' \"{tool}\" >> '{log}'\nfor a in \"$@\"; do printf ' [%s]' \"$a\" >> '{log}'; done\necho >> '{log}'\n",
                    log = log.display()
                ),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Env {
            home: dir.path().join("home"),
            _dir: dir,
            bin,
            log,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_wyres"));
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.args(args)
            .env("WYRES_HOME", &self.home)
            .env("HOME", &self.home)
            .env("PATH", path)
            .env_remove("WYRES_AGENT")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("HERDR_BIN_PATH")
            .env_remove("GROK_HOOK_EVENT")
            .stdin(Stdio::null());
        c
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.cmd(&all).output().unwrap();
        assert!(
            out.status.success(),
            "wyres {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn setup(env: &Env) {
    env.json(&["agent", "register", "alice", "--program", "claude"]);
    env.json(&["agent", "register", "bob", "--program", "codex"]);
}

#[test]
fn wakes_only_with_something_waiting() {
    let env = Env::new();
    setup(&env);
    env.json(&[
        "wake", "set", "--agent", "bob", "--via", "tmux", "--target", "%3",
    ]);

    let skipped = env.json(&["wake", "--agent", "bob"]);
    assert_eq!(skipped[0]["sent"], false);
    assert!(skipped[0]["skipped"]
        .as_str()
        .unwrap()
        .contains("nothing unread"));
    assert!(env.calls().is_empty());

    let sent = env.json(&[
        "send",
        "--from",
        "alice",
        "--to",
        "bob",
        "--subject",
        "Rebase",
        "-i",
        "urgent",
    ]);
    let dry = env.json(&["wake", "--agent", "bob", "--dry-run"]);
    assert_eq!(dry[0]["commands"][0][0], "tmux");
    assert!(env.calls().is_empty(), "dry run runs nothing");

    let woke = env.json(&["wake", "--agent", "bob"]);
    assert_eq!(woke[0]["sent"], true);
    let calls = env.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0]
        .starts_with("tmux [send-keys] [-t] [%3] [-l] [Wyres: 1 unread message(s) (1 urgent)"));
    assert!(calls[0].contains("\"Rebase\""));
    assert_eq!(calls[1], "tmux [send-keys] [-t] [%3] [Enter]");

    // Once read, there is nothing to wake for unless forced; --message overrides the text.
    env.json(&[
        "ack",
        "--agent",
        "bob",
        sent["message_id"].as_str().unwrap(),
    ]);
    assert_eq!(env.json(&["wake", "--agent", "bob"])[0]["sent"], false);
    let forced = env.json(&["wake", "--agent", "bob", "--force", "--message", "standup"]);
    assert_eq!(forced[0]["sent"], true);
    assert_eq!(env.calls()[2], "tmux [send-keys] [-t] [%3] [-l] [standup]");
}

#[test]
fn adapters_and_targets() {
    let env = Env::new();
    setup(&env);
    env.json(&[
        "send",
        "--from",
        "alice",
        "--to",
        "bob",
        "--subject",
        "hello",
    ]);

    // Herdr defaults to the agent's name; cmux sends to a surface or notifies.
    env.json(&["wake", "--agent", "bob", "--via", "herdr"]);
    env.json(&[
        "wake",
        "--agent",
        "bob",
        "--via",
        "cmux",
        "--target",
        "surf-7",
        "--message",
        "ping",
    ]);
    env.json(&[
        "wake",
        "--agent",
        "bob",
        "--via",
        "cmux",
        "--target",
        "notify",
        "--message",
        "ping",
    ]);
    let calls = env.calls();
    assert!(
        calls[0].starts_with("herdr [agent] [prompt] [bob] [Wyres:"),
        "{calls:?}"
    );
    assert_eq!(calls[1], "cmux [send] [--surface] [surf-7] [ping\\n]");
    assert_eq!(calls[2], "cmux [notify] [--title] [Wyres] [--body] [ping]");

    // A session-start hook inside tmux records the pane automatically...
    let mut hook = env
        .cmd(&["hook", "run", "session-start", "--client", "codex"])
        .env("TMUX_PANE", "%9")
        .env("WYRES_AGENT", "bob")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    drop(hook.stdin.take());
    assert!(hook.wait().unwrap().success());
    let targets = env.json(&["wake", "list"]);
    assert_eq!(targets[0]["target"], "%9");
    assert_eq!(targets[0]["auto"], true);
    // ...but never replaces a target set by hand.
    env.json(&[
        "wake", "set", "--agent", "bob", "--via", "tmux", "--target", "work:1.0",
    ]);
    let mut hook = env
        .cmd(&["hook", "run", "session-start", "--client", "codex"])
        .env("TMUX_PANE", "%10")
        .env("WYRES_AGENT", "bob")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    assert!(hook.wait().unwrap().success());
    assert_eq!(env.json(&["wake", "list"])[0]["target"], "work:1.0");

    // --all wakes agents with targets and something waiting (alice has neither).
    let all = env.json(&["wake", "--all"]);
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["agent"], "bob");
    assert_eq!(all[0]["sent"], true);

    // Missing target is reported, not guessed (except Herdr).
    let none = env.json(&["wake", "--agent", "alice", "--force"]);
    assert!(none[0]["skipped"]
        .as_str()
        .unwrap()
        .contains("no wake target"));
    assert!(env.json(&["wake", "clear", "--agent", "bob"])["cleared"] == true);
    let bad = env
        .cmd(&[
            "wake", "set", "--agent", "bob", "--via", "screen", "--target", "x",
        ])
        .output()
        .unwrap();
    assert!(!bad.status.success());
}

#[test]
fn missing_adapter_binary_is_an_error() {
    let env = Env::new();
    setup(&env);
    env.json(&[
        "send",
        "--from",
        "alice",
        "--to",
        "bob",
        "--subject",
        "hello",
    ]);
    let out = env
        .cmd(&["wake", "--agent", "bob", "--via", "herdr"])
        .env("HERDR_BIN_PATH", Path::new("/nonexistent/herdr"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is it installed"));
}

#[test]
fn herdr_plugin_script_is_valid_sh() {
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integrations/herdr/wyres-herdr.sh");
    let status = Command::new("sh").arg("-n").arg(&script).status().unwrap();
    assert!(status.success());
    let manifest = std::fs::read_to_string(script.with_file_name("herdr-plugin.toml")).unwrap();
    for needle in [
        "id = \"wyres.inbox\"",
        "min_herdr_version",
        "[[actions]]",
        "[[panes]]",
    ] {
        assert!(manifest.contains(needle), "{needle}");
    }
}
