//! Editor hook installation and the callbacks themselves, through the real
//! binary.
//!
//! Two claims are being pinned here. First, that installing into somebody's
//! `.claude/settings.json` never costs them anything they had. Second, that a
//! `PreToolUse` denial actually refuses the edit *and* explains itself in a
//! way the model can act on — which is the whole reason the guard exists.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

fn golab() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_golab"));
    c.env_remove("GOLAB_AGENT");
    // Pin the derived identity: `<tool>-<user>` would otherwise depend on
    // whoever is running the suite.
    c.env("GOLAB_USER", "tester");
    c
}

fn run(dir: &Path, args: &[&str]) -> Output {
    golab()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run golab")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "golab {args:?} failed ({:?}):\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn cli_json(dir: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(dir, &full)).expect("valid json")
}

/// Run a hook callback the way an editor does: payload on stdin.
fn hook(dir: &Path, args: &[&str], payload: &Value) -> Output {
    let mut child = golab()
        .current_dir(dir)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(serde_json::to_string(payload).unwrap().as_bytes())
        .unwrap();
    child.wait_with_output().expect("hook output")
}

/// The agent name a Claude Code session derives with GOLAB_USER=tester.
const DERIVED: &str = "claude-code-tester";

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/pay.ts"),
        "export function charge(x: number) { return x; }\n\
         export function refund(x: number) { return -x; }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

fn edit_payload(dir: &Path, file: &str) -> Value {
    json!({
        "session_id": "sess-abc123",
        "cwd": dir.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "Edit",
        "tool_input": { "file_path": dir.join(file).to_string_lossy(), "old_string": "a", "new_string": "b" }
    })
}

#[test]
fn the_guard_hook_denies_an_edit_someone_else_holds() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice"]);
    ok(d, &["--agent", "alice", "lease", "acquire", "charge"]);

    // The editor's own agent has not joined and does not need to: the guard
    // is asked on the first keystroke, which may well be before anything else.
    let out = hook(d, &["hook", "guard"], &edit_payload(d, "src/pay.ts"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Structured decision, for a client that reads it.
    let decision: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("hook stdout was not JSON ({e}): {stdout}"));
    let hso = &decision["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "PreToolUse");
    assert_eq!(hso["permissionDecision"], "deny");
    let reason = hso["permissionDecisionReason"].as_str().unwrap();
    assert!(reason.contains("alice"), "must name the holder: {reason}");
    assert!(
        reason.contains("lease-transfer"),
        "and must say how to get to yes: {reason}"
    );

    // And the older contract, for a client that does not: exit 2 blocks the
    // call and stderr is fed back to the model.
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr.contains("alice"), "{stderr}");
}

#[test]
fn the_guard_hook_lets_an_uncontended_edit_through_silently() {
    let dir = workspace();
    let d = dir.path();

    let out = hook(d, &["hook", "guard"], &edit_payload(d, "src/pay.ts"));
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "an explicit allow would short-circuit the user's own permission rules"
    );
}

#[test]
fn the_guard_hook_covers_multiedit_not_just_edit() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice"]);
    ok(d, &["--agent", "alice", "lease", "acquire", "charge"]);

    // An agent that can reach around the guard by picking a different editing
    // tool is not guarded at all.
    let payload = json!({
        "session_id": "s1",
        "cwd": d.to_string_lossy(),
        "hook_event_name": "PreToolUse",
        "tool_name": "MultiEdit",
        "tool_input": { "edits": [
            { "file_path": d.join("src/other.ts").to_string_lossy() },
            { "file_path": d.join("src/pay.ts").to_string_lossy() }
        ] }
    });
    let out = hook(d, &["hook", "guard"], &payload);
    assert_eq!(
        out.status.code(),
        Some(2),
        "one denied file in a batch refuses the whole call"
    );
}

#[test]
fn a_guard_mode_can_be_narrowed_to_one_contract() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice"]);
    ok(d, &["--agent", "alice", "lease", "acquire", "charge"]);
    let payload = edit_payload(d, "src/pay.ts");

    let json_only = hook(d, &["hook", "guard", "--mode", "json"], &payload);
    assert_eq!(json_only.status.code(), Some(0), "json mode says no in the body");
    assert!(!String::from_utf8_lossy(&json_only.stdout).trim().is_empty());

    let exit_only = hook(d, &["hook", "guard", "--mode", "exit"], &payload);
    assert_eq!(exit_only.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&exit_only.stdout).trim().is_empty());
    assert!(!String::from_utf8_lossy(&exit_only.stderr).trim().is_empty());
}

#[test]
fn the_guard_hook_never_fails_an_edit_because_golab_is_unhappy() {
    // No workspace at all. A coordination layer that bricks somebody's editor
    // when it cannot answer has failed at the job it exists to do.
    let dir = tempfile::tempdir().unwrap();
    let out = hook(
        dir.path(),
        &["hook", "guard"],
        &edit_payload(dir.path(), "src/pay.ts"),
    );
    assert_eq!(out.status.code(), Some(0));

    // And a payload shape we do not recognise.
    let ws = workspace();
    let out = hook(ws.path(), &["hook", "guard"], &json!({ "hook_event_name": "PreToolUse" }));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn session_start_joins_the_workspace_and_hands_back_orientation() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "make charging refundable"]);
    ok(d, &["goal", "decompose", "G1", "--task", "wire refunds", "--symbol", "refund"]);
    ok(
        d,
        &["memory", "set", "rounding", "always round half up", "--tag", "convention"],
    );

    let out = hook(
        d,
        &["hook", "session-start"],
        &json!({ "session_id": "sess-1", "cwd": d.to_string_lossy(), "hook_event_name": "SessionStart" }),
    );
    assert_eq!(out.status.code(), Some(0));

    let decision: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let context = decision["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("orientation text");
    assert!(context.contains("check_edit"), "the rule has to be stated: {context}");
    assert!(context.contains("ready to start"), "{context}");
    assert!(
        context.contains("round half up"),
        "a decision the team already made should not be rediscovered: {context}"
    );

    // Joining happened without the model doing anything.
    let swarm = cli_json(d, &["swarm", "list"]);
    assert!(
        swarm.as_array().unwrap().iter().any(|a| a["name"] == DERIVED),
        "{swarm}"
    );
    let sessions = cli_json(d, &["session", "list", "--live"]);
    assert_eq!(sessions.as_array().unwrap()[0]["transport"], "hook");
}

#[test]
fn session_end_closes_the_session_the_start_hook_opened() {
    let dir = workspace();
    let d = dir.path();
    let start = json!({ "session_id": "sess-xyz", "cwd": d.to_string_lossy(), "hook_event_name": "SessionStart" });
    hook(d, &["hook", "session-start"], &start);
    assert_eq!(cli_json(d, &["session", "list", "--live"]).as_array().unwrap().len(), 1);

    // A separate process with no memory of the first: the host's own id is
    // the only thing tying them together.
    let out = hook(
        d,
        &["hook", "session-end"],
        &json!({ "session_id": "sess-xyz", "hook_event_name": "SessionEnd", "reason": "exit" }),
    );
    assert_eq!(out.status.code(), Some(0));
    assert!(cli_json(d, &["session", "list", "--live"]).as_array().unwrap().is_empty());
}

#[test]
fn post_tool_publishes_progress_so_liveness_is_not_the_models_job() {
    let dir = workspace();
    let d = dir.path();
    hook(
        d,
        &["hook", "session-start"],
        &json!({ "session_id": "s1", "cwd": d.to_string_lossy() }),
    );

    let out = hook(
        d,
        &["hook", "post-tool"],
        &json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": d.join("src/pay.ts").to_string_lossy() }
        }),
    );
    assert_eq!(out.status.code(), Some(0));

    let status = cli_json(d, &["status"]);
    let progress = status["progress"].as_array().unwrap();
    assert!(
        progress.iter().any(|p| p["agent"] == DERIVED
            && p["note"].as_str().unwrap_or("").contains("pay.ts")),
        "the edit itself should publish progress: {progress:?}"
    );
}

#[test]
fn installing_claude_code_hooks_merges_and_is_idempotent() {
    let dir = workspace();
    let d = dir.path();
    let settings = d.join(".claude").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        r#"{
  "model": "opus",
  "permissions": { "allow": ["Bash(npm install:*)"] },
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "somebody-elses-tool" }] }
    ]
  }
}"#,
    )
    .unwrap();

    ok(d, &["hook", "install", "--claude-code"]);
    ok(d, &["hook", "install", "--claude-code"]);

    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["model"], "opus", "unrelated keys survive");
    assert_eq!(after["permissions"]["allow"][0], "Bash(npm install:*)");

    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 2, "installed twice, added once: {pre:?}");
    assert!(
        pre.iter()
            .any(|e| e["hooks"][0]["command"] == "somebody-elses-tool"),
        "another tool's hook must survive: {pre:?}"
    );
    for event in ["SessionStart", "PostToolUse", "SessionEnd"] {
        assert_eq!(
            after["hooks"][event].as_array().map(|a| a.len()),
            Some(1),
            "{event} missing"
        );
    }

    ok(d, &["hook", "uninstall", "--claude-code"]);
    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    assert_eq!(after["model"], "opus");
    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0]["hooks"][0]["command"], "somebody-elses-tool");
    assert!(after["hooks"].get("SessionStart").is_none());
}

#[test]
fn a_config_we_cannot_read_is_never_overwritten() {
    let dir = workspace();
    let d = dir.path();
    let settings = d.join(".claude").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{ not json at all").unwrap();

    let out = run(d, &["hook", "install", "--claude-code"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        "{ not json at all",
        "losing somebody's config is not a recoverable mistake"
    );
}

#[test]
fn installing_the_mcp_server_registers_it_without_a_hardcoded_root() {
    let dir = workspace();
    let d = dir.path();
    std::fs::write(
        d.join(".mcp.json"),
        r#"{"mcpServers":{"other":{"command":"other-server"}}}"#,
    )
    .unwrap();

    ok(d, &["hook", "install", "--mcp", "--as", "alice"]);
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(d.join(".mcp.json")).unwrap()).unwrap();

    assert_eq!(doc["mcpServers"]["other"]["command"], "other-server");
    let golab = &doc["mcpServers"]["golab"];
    assert!(golab["command"].as_str().unwrap().contains("golab"));
    let args: Vec<&str> = golab["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args, vec!["mcp", "--as", "alice"]);
    assert!(
        !args.contains(&"--root"),
        "an absolute root would break this config for anyone else's checkout"
    );

    ok(d, &["hook", "uninstall", "--mcp"]);
    let doc: Value =
        serde_json::from_str(&std::fs::read_to_string(d.join(".mcp.json")).unwrap()).unwrap();
    assert!(doc["mcpServers"]["golab"].is_null());
    assert_eq!(doc["mcpServers"]["other"]["command"], "other-server");
}

#[test]
fn bare_hook_install_still_means_the_git_hook() {
    let dir = workspace();
    let d = dir.path();
    std::fs::create_dir_all(d.join(".git").join("hooks")).unwrap();

    ok(d, &["hook", "install"]);
    let script = std::fs::read_to_string(d.join(".git/hooks/pre-commit")).unwrap();
    assert!(script.contains("golab"), "{script}");
    assert!(
        !d.join(".claude").join("settings.json").exists(),
        "adding flags must not change what the bare command has always done"
    );

    ok(d, &["hook", "uninstall"]);
    assert!(!d.join(".git/hooks/pre-commit").exists());
}
