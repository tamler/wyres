//! Process scanning for orphan detection: listening TCP sockets via `lsof` (macOS first,
//! also Linux) with `ss` as a Linux fallback, and per-process details via `ps`.
//! Read-only: nothing here signals or changes a process.

use std::collections::BTreeSet;
use std::process::Command;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, schemars::JsonSchema)]
pub struct Listener {
    pub pid: u32,
    pub port: u16,
    /// Short command name as reported by the scanner.
    pub command: String,
    pub user: Option<String>,
    /// Bound address, e.g. `127.0.0.1:5173` or `*:11434`.
    pub address: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ScanResult {
    /// `lsof`, `ss`, or `none`.
    pub scanner: String,
    pub listeners: Vec<Listener>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct ProcInfo {
    pub pid: u32,
    pub user: String,
    pub elapsed_seconds: i64,
    pub command: String,
}

/// Process names that are never orphan candidates (system services and shells).
const SYSTEM_PROCESSES: &[&str] = &[
    "launchd",
    "systemd",
    "systemd-resolved",
    "init",
    "sshd",
    "rapportd",
    "ControlCenter",
    "ControlCe",
    "sharingd",
    "mDNSResponder",
    "mDNSResp",
    "cupsd",
    "WindowServer",
    "loginwindow",
    "identityservicesd",
    "remoted",
    "AirPlayXPCHelper",
    "AirPlayXP",
    "UserEventAgent",
    "UserEvent",
    "netbiosd",
    "configd",
    "kdc",
    "dnsmasq",
    "avahi-daemon",
    "containerd",
    "dockerd",
    "tailscaled",
    "wyres",
];

/// True for processes that must never be listed or reaped as orphans.
pub fn is_system(command: &str) -> bool {
    let base = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default();
    SYSTEM_PROCESSES.contains(&base)
}

fn port_of(address: &str) -> Option<u16> {
    address.rsplit(':').next()?.parse().ok()
}

/// Parse `lsof -nP -iTCP -sTCP:LISTEN -FpcLn` field output.
pub fn parse_lsof(text: &str) -> Vec<Listener> {
    let mut out = BTreeSet::new();
    let (mut pid, mut command, mut user) = (None::<u32>, String::new(), None::<String>);
    for line in text.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => {
                pid = value.parse().ok();
                command.clear();
                user = None;
            }
            "c" => command = value.to_string(),
            "L" => user = Some(value.to_string()),
            "n" => {
                // `n` may carry `local->remote` for connected sockets; listeners have none.
                let local = value.split("->").next().unwrap_or_default();
                if let (Some(pid), Some(port)) = (pid, port_of(local)) {
                    out.insert(Listener {
                        pid,
                        port,
                        command: command.clone(),
                        user: user.clone(),
                        address: local.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    dedupe(out)
}

/// Parse `ss -ltnpH` output (Linux fallback).
pub fn parse_ss(text: &str) -> Vec<Listener> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(local) = cols.get(3) else { continue };
        let Some(port) = port_of(local) else { continue };
        let users = cols.get(5).copied().unwrap_or_default();
        // users:(("name",pid=123,fd=4),("name",pid=124,fd=4))
        for entry in users.split("),(") {
            let name = entry.split('"').nth(1).unwrap_or_default().to_string();
            let pid = entry
                .split("pid=")
                .nth(1)
                .and_then(|r| r.split([',', ')']).next())
                .and_then(|p| p.parse().ok());
            if let Some(pid) = pid {
                out.insert(Listener {
                    pid,
                    port,
                    command: name,
                    user: None,
                    address: local.to_string(),
                });
            }
        }
    }
    dedupe(out)
}

/// One entry per (pid, port); IPv4 and IPv6 binds of the same port collapse.
fn dedupe(set: BTreeSet<Listener>) -> Vec<Listener> {
    let mut seen = BTreeSet::new();
    set.into_iter()
        .filter(|l| seen.insert((l.pid, l.port)))
        .collect()
}

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    // lsof and ps exit 1 when nothing matches; only a failure with a message is an error.
    if !out.status.success() && out.stdout.is_empty() && !out.stderr.is_empty() {
        return Err(format!(
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Scan listening TCP sockets with the best available tool.
pub fn scan_listeners() -> ScanResult {
    match run("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-FpcLn"]) {
        Ok(text) => ScanResult {
            scanner: "lsof".into(),
            listeners: parse_lsof(&text),
            error: None,
        },
        Err(lsof_err) => match run("ss", &["-ltnpH"]) {
            Ok(text) => ScanResult {
                scanner: "ss".into(),
                listeners: parse_ss(&text),
                error: None,
            },
            Err(ss_err) => ScanResult {
                scanner: "none".into(),
                listeners: Vec::new(),
                error: Some(format!("no scanner available ({lsof_err}; {ss_err})")),
            },
        },
    }
}

/// Parse `ps` elapsed time: `[[dd-]hh:]mm:ss`.
pub fn parse_etime(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let (days, rest) = match raw.split_once('-') {
        Some((d, r)) => (d.parse::<i64>().ok()?, r),
        None => (0, raw),
    };
    let parts: Vec<i64> = rest
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let (h, m, s) = match parts.as_slice() {
        [m, s] => (0, *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    Some(days * 86_400 + h * 3600 + m * 60 + s)
}

/// Owner, age and full command line of a running process.
pub fn process_info(pid: u32) -> Option<ProcInfo> {
    let out = run(
        "ps",
        &["-o", "user=,etime=,command=", "-p", &pid.to_string()],
    )
    .ok()?;
    let line = out.lines().find(|l| !l.trim().is_empty())?;
    let mut it = line.split_whitespace();
    let user = it.next()?.to_string();
    let elapsed_seconds = parse_etime(it.next()?)?;
    let command = it.collect::<Vec<_>>().join(" ");
    Some(ProcInfo {
        pid,
        user,
        elapsed_seconds,
        command,
    })
}

/// Whether a process is running (exited-but-unreaped zombies count as gone).
pub fn alive(pid: u32) -> bool {
    run("ps", &["-o", "stat=", "-p", &pid.to_string()])
        .map(|out| {
            let stat = out.trim();
            !stat.is_empty() && !stat.starts_with('Z')
        })
        .unwrap_or(false)
}

/// The user running wyres.
pub fn current_user() -> Option<String> {
    run("id", &["-un"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsof_fields() {
        let text = "p412\ncollama\nLdev\nn127.0.0.1:11434\np977\ncnode\nLdev\nn*:5173\nn[::1]:5173\np88\ncrapportd\nLdev\nn*:49152\n";
        let ls = parse_lsof(text);
        assert_eq!(ls.len(), 3);
        let ollama = ls.iter().find(|l| l.pid == 412).unwrap();
        assert_eq!((ollama.port, ollama.command.as_str()), (11434, "ollama"));
        assert_eq!(ollama.user.as_deref(), Some("dev"));
        assert_eq!(
            ls.iter().filter(|l| l.pid == 977).count(),
            1,
            "v4/v6 collapse"
        );
    }

    #[test]
    fn parses_ss_output() {
        let text = "LISTEN 0 4096 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=123,fd=5))\n\
                    LISTEN 0 511 *:3000 *:* users:((\"node\",pid=200,fd=20),(\"node\",pid=201,fd=20))\n\
                    LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n";
        let ls = parse_ss(text);
        assert_eq!(ls.len(), 3);
        assert!(ls
            .iter()
            .any(|l| l.pid == 123 && l.port == 5432 && l.command == "postgres"));
        assert!(ls.iter().any(|l| l.pid == 201 && l.port == 3000));
    }

    #[test]
    fn parses_etime() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("12:34"), Some(754));
        assert_eq!(parse_etime("01:00:00"), Some(3600));
        assert_eq!(
            parse_etime("2-03:04:05"),
            Some(2 * 86_400 + 3 * 3600 + 4 * 60 + 5)
        );
        assert_eq!(parse_etime("garbage"), None);
    }

    #[test]
    fn system_allowlist() {
        assert!(is_system("launchd"));
        assert!(is_system("/usr/sbin/sshd -D"));
        assert!(!is_system("ollama"));
        assert!(!is_system("llama-server"));
    }
}
