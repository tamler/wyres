//! Wyres daemon and CLI: MCP tools over the core store, served on stdio or loopback HTTP.

pub mod cli;
pub mod hooks;
pub mod mcp;
mod output;
pub mod reap;
pub mod scan;
pub mod serve;
