//! Timestamps are stored as integer milliseconds since the Unix epoch (UTC) and rendered
//! as RFC 3339 strings at the API boundary.

use chrono::{DateTime, SecondsFormat};

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn fmt_ts(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
        .unwrap_or_else(|| ms.to_string())
}

pub fn fmt_opt(ms: Option<i64>) -> Option<String> {
    ms.map(fmt_ts)
}
