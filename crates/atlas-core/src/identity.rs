//! Who an adapter says it is.
//!
//! Deterministic on purpose: an MCP server and an editor hook are separate
//! processes that never talk to each other, and both have to arrive at the
//! same agent name from the same inputs, with no shared file and no lookup.
//!
//! The derivation deliberately **does not** consult `.atlas/agent`. That file
//! is the *workspace* default for bare CLI invocations — one name, written by
//! `atlas swarm join`. Two coding tools open in one repository would both read
//! it and both believe they were the same agent, which is precisely the
//! multi-person case this exists to support. Tools derive; humans use the file.

/// Reduce anything to a safe, stable agent-name fragment.
///
/// Agent names are a primary key and end up in shell commands, JSON and hook
/// config, so they stay boring: lowercase, `[a-z0-9-]`, no runs, no edges.
pub fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    // Long enough to stay readable, short enough for a table column and a
    // terminal line.
    trimmed.chars().take(32).collect::<String>().trim_matches('-').to_string()
}

/// `USERNAME` on Windows, `USER` elsewhere.
pub fn os_user() -> String {
    for key in ["ATLAS_USER", "USERNAME", "USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return v;
            }
        }
    }
    "local".to_string()
}

/// Derive the agent name a coding tool should register under.
///
/// Precedence: `explicit` (a `--as` flag) → `$ATLAS_AGENT` → `<tool>-<user>`.
///
/// The fallback is what makes the common case need no configuration at all:
/// Alice on Claude Code and Bob on Cursor become `claude-code-alice` and
/// `cursor-bob` in the same workspace without either of them naming themselves.
pub fn derive_agent(tool: &str, explicit: Option<&str>) -> String {
    if let Some(name) = explicit.map(slug).filter(|s| !s.is_empty()) {
        return name;
    }
    if let Some(name) = std::env::var("ATLAS_AGENT")
        .ok()
        .map(|s| slug(&s))
        .filter(|s| !s.is_empty())
    {
        return name;
    }
    let tool = slug(tool);
    let tool = if tool.is_empty() { "agent".to_string() } else { tool };
    let user = slug(&os_user());
    if user.is_empty() {
        tool
    } else {
        slug(&format!("{tool}-{user}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `derive_agent` reads process-wide env, so these must not interleave.
    /// A mutex rather than one big test keeps the failure messages specific.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env(agent: Option<&str>, user: &str, f: impl FnOnce()) {
        let _lock = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let prev_agent = std::env::var("ATLAS_AGENT").ok();
        let prev_user = std::env::var("ATLAS_USER").ok();
        match agent {
            Some(a) => std::env::set_var("ATLAS_AGENT", a),
            None => std::env::remove_var("ATLAS_AGENT"),
        }
        std::env::set_var("ATLAS_USER", user);
        f();
        match prev_agent {
            Some(a) => std::env::set_var("ATLAS_AGENT", a),
            None => std::env::remove_var("ATLAS_AGENT"),
        }
        match prev_user {
            Some(u) => std::env::set_var("ATLAS_USER", u),
            None => std::env::remove_var("ATLAS_USER"),
        }
    }

    #[test]
    fn slugging_is_boring_on_purpose() {
        assert_eq!(slug("Claude Code"), "claude-code");
        assert_eq!(slug("  Cursor  "), "cursor");
        assert_eq!(slug("a//b__c"), "a-b-c");
        assert_eq!(slug("!!!"), "");
        assert_eq!(slug("Zed_IDE v0.9"), "zed-ide-v0-9");
        assert!(slug(&"x".repeat(100)).chars().count() <= 32);
    }

    #[test]
    fn an_explicit_name_wins_over_everything() {
        with_env(Some("from-env"), "alice", || {
            assert_eq!(derive_agent("claude-code", Some("CI Runner")), "ci-runner");
        });
    }

    #[test]
    fn the_environment_wins_over_the_derived_default() {
        with_env(Some("ci"), "alice", || {
            assert_eq!(derive_agent("claude-code", None), "ci");
        });
    }

    #[test]
    fn an_empty_explicit_name_falls_through() {
        with_env(None, "alice", || {
            assert_eq!(derive_agent("Cursor", Some("  ")), "cursor-alice");
        });
    }

    #[test]
    fn the_default_is_tool_and_user() {
        with_env(None, "Alice", || {
            assert_eq!(derive_agent("Claude Code", None), "claude-code-alice");
            assert_eq!(derive_agent("Cursor", None), "cursor-alice");
        });
    }

    #[test]
    fn two_processes_derive_the_same_name_from_the_same_inputs() {
        with_env(None, "alice", || {
            // The MCP server and the editor hook never speak to each other;
            // agreeing on identity is what lets them share a session's work.
            assert_eq!(
                derive_agent("claude-code", None),
                derive_agent("Claude Code", None)
            );
        });
    }

    #[test]
    fn different_tools_are_different_agents_for_the_same_user() {
        with_env(None, "alice", || {
            assert_ne!(derive_agent("claude-code", None), derive_agent("cursor", None));
        });
    }

    #[test]
    fn a_nameless_tool_still_yields_something_usable() {
        with_env(None, "alice", || {
            assert_eq!(derive_agent("", None), "agent-alice");
        });
    }
}
