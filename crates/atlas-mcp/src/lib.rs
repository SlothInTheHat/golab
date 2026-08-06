//! Model Context Protocol adapter.
//!
//! This is the layer that turns atlas from "another CLI" into infrastructure a
//! coding tool is already wired to. Claude Code, Cursor, Codex, Windsurf, Zed
//! and Gemini CLI all speak MCP, so one stdio server reaches every one of them
//! and per-tool work collapses to a config snippet.
//!
//! Two things make it an *adapter* rather than a prompt:
//!
//! - **Lifecycle does not depend on the model.** Registering, heartbeating,
//!   renewing leases, receiving notices and leaving cleanly all happen on the
//!   `initialize` handler and a background thread. A model that calls no tools
//!   at all still participates correctly.
//! - **A refusal is not an error.** atlas's exit-code doctrine — `0` yes, `1` a
//!   legitimate no, `2` broken — maps onto MCP as: denials come back with
//!   `isError: false` and the "no" in `structuredContent`, so the model can
//!   branch on it and go negotiate instead of treating it as a fault.
//!
//! ## stdout is a protocol channel
//!
//! Every byte on stdout must be a JSON-RPC frame. A stray `println!` anywhere
//! in this crate silently corrupts a user's session in a way that is miserable
//! to diagnose, so the lint below makes it a build failure instead.
//! `atlas-core` contains no printing at all, which is what makes calling it
//! from here safe; all of atlas's output lives in `atlas-cli`.

#![deny(clippy::print_stdout, clippy::print_stderr)]

use std::path::PathBuf;

use anyhow::Result;

pub mod jsonrpc;
pub mod notices;
pub mod resources;
pub mod server;
pub mod tools;

/// Protocol versions this server understands, newest first.
///
/// The correct behaviour on `initialize` is to echo the client's requested
/// version when we support it and otherwise offer our newest and let the
/// client decide. Keeping the list in one place makes supporting a new
/// revision a one-line change.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// How often the background thread heartbeats, renews leases and sweeps.
///
/// Twenty seconds against a sixty-second `AGENT_ONLINE_MS`, so an agent
/// survives two consecutive missed ticks — a long build pegging the CPU, a
/// laptop briefly asleep. At thirty a single late tick would flip the agent
/// offline and let `reassign_orphans` hand its work to somebody else while it
/// is still mid-edit.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 20;

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub root: PathBuf,
    /// Overrides the derived agent name.
    pub agent: Option<String>,
    /// Tool slug, when the client's `initialize` does not name itself.
    pub tool: Option<String>,
    pub heartbeat_secs: u64,
    /// Keep leases held when the session ends — for a session that will resume.
    pub keep_leases: bool,
}

impl McpConfig {
    pub fn new(root: PathBuf) -> McpConfig {
        McpConfig {
            root,
            agent: None,
            tool: None,
            heartbeat_secs: DEFAULT_HEARTBEAT_SECS,
            keep_leases: false,
        }
    }
}

/// Serve MCP on stdin/stdout until the client closes stdin.
pub fn serve(cfg: McpConfig) -> Result<()> {
    server::run(cfg, std::io::stdin().lock(), std::io::stdout())
}
