//! Editing somebody else's config file without breaking it.
//!
//! `.claude/settings.json` and `.mcp.json` belong to the user, not to us. They
//! will already contain hooks for other tools, permission rules, model choices
//! and keys we have never heard of. So: parse the whole document, touch only
//! the entries we own, write the rest back byte-equivalent, and if the file is
//! there but unparseable, **fail** rather than replace it. Losing somebody's
//! configuration to a tool that was supposed to be helping is not recoverable.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

/// Read `path` as JSON (missing or empty means `{}`), apply `f`, write it back.
pub fn edit_json_file(path: &Path, f: impl FnOnce(&mut Value) -> Result<()>) -> Result<()> {
    let mut doc = read_json_file(path)?;
    if !doc.is_object() {
        anyhow::bail!(
            "{} is not a JSON object; refusing to overwrite it",
            path.display()
        );
    }
    f(&mut doc)?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn read_json_file(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON. Fix or move it first — refusing to overwrite \
             a config file we cannot read",
            path.display()
        )
    })
}

/// Add one hook entry under `settings.hooks[event]`.
///
/// Idempotent on the command string, which is what makes installing twice a
/// no-op: an entry whose `hooks[].command` we already wrote is left alone
/// rather than duplicated.
///
/// Returns true when the document changed.
pub fn upsert_hook(
    settings: &mut Value,
    event: &str,
    matcher: Option<&str>,
    command: &str,
    timeout: u64,
) -> bool {
    let hooks = settings
        .as_object_mut()
        .expect("object checked by edit_json_file")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let Some(hooks) = hooks.as_object_mut() else {
        return false;
    };
    let entries = hooks.entry(event).or_insert_with(|| json!([]));
    let Some(entries) = entries.as_array_mut() else {
        return false;
    };

    if entries.iter().any(|e| has_command(e, command)) {
        return false;
    }

    let mut entry = Map::new();
    if let Some(m) = matcher {
        entry.insert("matcher".to_string(), json!(m));
    }
    entry.insert(
        "hooks".to_string(),
        json!([{ "type": "command", "command": command, "timeout": timeout }]),
    );
    entries.push(Value::Object(entry));
    true
}

/// Remove every hook whose command contains `marker`, and prune what that
/// empties.
///
/// Matching on a substring rather than the whole command on purpose: the
/// installed command embeds an absolute exe path, so a user who moved or
/// rebuilt the binary would otherwise be unable to uninstall what they
/// installed. The marker is the subcommand tail (`" hook guard"`), which no
/// other tool's config will contain.
pub fn remove_hooks_matching(settings: &mut Value, marker: &str) -> usize {
    let Some(obj) = settings.as_object_mut() else {
        return 0;
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return 0;
    };

    let mut removed = 0;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(entries) = hooks.get_mut(&event).and_then(|e| e.as_array_mut()) else {
            continue;
        };
        let before = entries.len();
        entries.retain(|e| !mentions(e, marker));
        removed += before - entries.len();
        if entries.is_empty() {
            hooks.remove(&event);
        }
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    removed
}

fn has_command(entry: &Value, command: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|list| {
            list.iter()
                .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))
        })
}

fn mentions(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|list| {
            list.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(marker))
            })
        })
}

/// Register an MCP server under `mcpServers`, replacing our own entry only.
pub fn upsert_mcp_server(doc: &mut Value, name: &str, entry: Value) -> bool {
    let servers = doc
        .as_object_mut()
        .expect("object checked by edit_json_file")
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return false;
    };
    if servers.get(name) == Some(&entry) {
        return false;
    }
    servers.insert(name.to_string(), entry);
    true
}

pub fn remove_mcp_server(doc: &mut Value, name: &str) -> bool {
    let Some(obj) = doc.as_object_mut() else {
        return false;
    };
    let Some(servers) = obj.get_mut("mcpServers").and_then(|s| s.as_object_mut()) else {
        return false;
    };
    let removed = servers.remove(name).is_some();
    if servers.is_empty() {
        obj.remove("mcpServers");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installing_twice_adds_one_entry() {
        let mut s = json!({});
        assert!(upsert_hook(&mut s, "PreToolUse", Some("Edit"), "/bin/atlas hook guard", 10));
        assert!(!upsert_hook(&mut s, "PreToolUse", Some("Edit"), "/bin/atlas hook guard", 10));
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn an_unrelated_hook_for_another_tool_survives_both_ways() {
        let mut s = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "some-other-tool check" }] }
                ]
            }
        });
        upsert_hook(&mut s, "PreToolUse", Some("Edit"), "/bin/atlas hook guard", 10);
        assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);

        assert_eq!(remove_hooks_matching(&mut s, " hook guard"), 1);
        let left = s["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0]["hooks"][0]["command"], "some-other-tool check");
    }

    #[test]
    fn uninstalling_prunes_what_it_empties() {
        let mut s = json!({ "model": "opus" });
        upsert_hook(&mut s, "SessionStart", None, "/bin/atlas hook session-start", 15);
        assert_eq!(remove_hooks_matching(&mut s, " hook "), 1);
        assert!(
            s.get("hooks").is_none(),
            "an empty hooks map is clutter we put there: {s}"
        );
        assert_eq!(s["model"], "opus", "and everything else is untouched");
    }

    #[test]
    fn uninstall_matches_even_after_the_binary_moved() {
        let mut s = json!({});
        upsert_hook(&mut s, "PreToolUse", Some("Edit"), "/old/path/atlas hook guard", 10);
        // Rebuilt somewhere else; the marker is what identifies ours.
        assert_eq!(remove_hooks_matching(&mut s, " hook guard"), 1);
    }

    #[test]
    fn a_missing_file_reads_as_an_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert_eq!(read_json_file(&path).unwrap(), json!({}));

        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(read_json_file(&path).unwrap(), json!({}));
    }

    #[test]
    fn a_malformed_config_is_an_error_not_an_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let err = edit_json_file(&path, |_| Ok(())).expect_err("must refuse");
        assert!(format!("{err:#}").contains("not valid JSON"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ this is not json",
            "losing somebody's config is not a recoverable mistake"
        );
    }

    #[test]
    fn unrelated_keys_and_nesting_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"permissions":{"allow":["Bash(npm install:*)"]},"model":"opus"}"#,
        )
        .unwrap();

        edit_json_file(&path, |s| {
            upsert_hook(s, "SessionEnd", None, "/bin/atlas hook session-end", 15);
            Ok(())
        })
        .unwrap();

        let back = read_json_file(&path).unwrap();
        assert_eq!(back["permissions"]["allow"][0], "Bash(npm install:*)");
        assert_eq!(back["model"], "opus");
        assert!(back["hooks"]["SessionEnd"].is_array());
    }

    #[test]
    fn an_mcp_server_entry_replaces_only_its_own_key() {
        let mut doc = json!({ "mcpServers": { "other": { "command": "other-server" } } });
        assert!(upsert_mcp_server(
            &mut doc,
            "atlas",
            json!({ "command": "/bin/atlas", "args": ["mcp"] })
        ));
        assert_eq!(doc["mcpServers"]["other"]["command"], "other-server");

        // Re-running the installer with identical config changes nothing.
        assert!(!upsert_mcp_server(
            &mut doc,
            "atlas",
            json!({ "command": "/bin/atlas", "args": ["mcp"] })
        ));

        assert!(remove_mcp_server(&mut doc, "atlas"));
        assert!(doc["mcpServers"]["other"].is_object());
    }
}
