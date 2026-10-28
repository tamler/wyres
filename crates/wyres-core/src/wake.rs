//! Wake targets: where to nudge an agent's terminal when it has mail (optional adapters).
//! Wyres only stores the target; the CLI runs the adapter (`tmux`, `cmux`, `herdr`).

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::identity::resolve_agent;
use crate::store::Store;
use crate::time::fmt_ts;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WakeVia {
    Tmux,
    Cmux,
    Herdr,
}

impl WakeVia {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeVia::Tmux => "tmux",
            WakeVia::Cmux => "cmux",
            WakeVia::Herdr => "herdr",
        }
    }
}

impl std::str::FromStr for WakeVia {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "tmux" => Ok(WakeVia::Tmux),
            "cmux" => Ok(WakeVia::Cmux),
            "herdr" => Ok(WakeVia::Herdr),
            _ => Err(Error::Invalid(format!(
                "unknown wake adapter {s:?}; expected tmux, cmux or herdr"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct WakeTarget {
    pub agent: String,
    pub via: WakeVia,
    /// tmux pane (`%3`, `session:1.0`), cmux surface id, or Herdr agent/pane.
    pub target: String,
    /// True when captured automatically by a session hook.
    pub auto: bool,
    pub updated_at: String,
}

fn validate_target(target: &str) -> Result<()> {
    if target.trim().is_empty() || target.len() > 200 || target.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "wake target must be 1-200 printable characters".into(),
        ));
    }
    Ok(())
}

impl Store {
    /// Set where to wake an agent. Automatic captures never replace a manual setting.
    pub fn set_wake_target(
        &self,
        agent: &str,
        via: WakeVia,
        target: &str,
        auto: bool,
    ) -> Result<Option<WakeTarget>> {
        validate_target(target)?;
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let manual_exists = tx
                .query_row(
                    "SELECT 1 FROM wake_targets WHERE agent_id = ?1 AND auto = 0",
                    [&me.id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if auto && manual_exists {
                return Ok(None);
            }
            tx.execute(
                "INSERT INTO wake_targets (agent_id, via, target, auto, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(agent_id) DO UPDATE SET via = excluded.via, target = excluded.target,
                   auto = excluded.auto, updated_at = excluded.updated_at",
                params![me.id, via.as_str(), target, auto, tx.now],
            )?;
            Ok(Some(WakeTarget {
                agent: me.address,
                via,
                target: target.to_string(),
                auto,
                updated_at: fmt_ts(tx.now),
            }))
        })
    }

    pub fn clear_wake_target(&self, agent: &str) -> Result<bool> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            Ok(tx.execute("DELETE FROM wake_targets WHERE agent_id = ?1", [&me.id])? > 0)
        })
    }

    pub fn wake_target(&self, agent: &str) -> Result<Option<WakeTarget>> {
        Ok(self
            .wake_targets()?
            .into_iter()
            .find(|t| t.agent == self.agent(agent).map(|a| a.address).unwrap_or_default()))
    }

    pub fn wake_targets(&self) -> Result<Vec<WakeTarget>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT a.address, w.via, w.target, w.auto, w.updated_at FROM wake_targets w
                 JOIN agents a ON a.agent_id = w.agent_id ORDER BY a.address",
            )?;
            let rows = stmt.query_map([], |r| {
                let via: String = r.get(1)?;
                Ok(WakeTarget {
                    agent: r.get(0)?,
                    via: via.parse().unwrap_or(WakeVia::Tmux),
                    target: r.get(2)?,
                    auto: r.get(3)?,
                    updated_at: fmt_ts(r.get(4)?),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}
