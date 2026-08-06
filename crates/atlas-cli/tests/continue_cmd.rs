//! `atlas continue`, end to end: the per-agent loop verb. Idle, it claims the
//! next startable task (optionally scoped to a goal); already working, it
//! renews the lease it holds instead of reaching for something new.

use std::path::Path;
use std::process::{Command, Output};

fn atlas() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_atlas"));
    c.env_remove("ATLAS_AGENT");
    c
}

fn run(dir: &Path, args: &[&str]) -> Output {
    atlas()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("failed to run atlas")
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, args);
    assert!(
        out.status.success(),
        "atlas {args:?} failed ({:?}):\n{}\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(dir, &full)).expect("valid json")
}

/// Like `json`, but also accepts exit code 1 — `continue` finding nothing
/// startable is a legitimate "no", not an error.
fn json_ok_or_no(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = run(dir, &full);
    assert!(
        matches!(out.status.code(), Some(0) | Some(1)),
        "atlas {full:?} errored ({:?}):\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid json")
}

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
    for a in ["alice", "bob"] {
        ok(dir.path(), &["--agent", a, "swarm", "join", a]);
    }
    dir
}

#[test]
fn continue_claims_when_idle_and_renews_when_already_working() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);

    let claimed = json(d, &["--agent", "alice", "continue"]);
    assert_eq!(claimed["id"], "T1");
    assert_eq!(json(d, &["lease", "list"])[0]["agent"], "alice");

    // Calling it again while still on T1 renews rather than claiming afresh.
    let renewed = json(d, &["--agent", "alice", "continue"]);
    assert_eq!(renewed["task"], "T1");
    assert!(!renewed["renewed"].as_array().unwrap().is_empty());

    // Nothing new got claimed — still exactly one lease, still alice.
    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases.as_array().unwrap().len(), 1);
    assert_eq!(leases[0]["agent"], "alice");
}

#[test]
fn continue_reports_nothing_when_no_task_is_startable() {
    let dir = workspace();
    let d = dir.path();
    let claimed = json_ok_or_no(d, &["--agent", "alice", "continue"]);
    assert_eq!(claimed["task"], serde_json::Value::Null);
}

#[test]
fn continue_respects_the_goal_filter() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "refunds"]);
    ok(d, &["goal", "decompose", "G1", "--task", "in goal", "--symbol", "charge"]);
    ok(d, &["task", "add", "outside goal", "--symbol", "refund"]);

    let claimed = json(d, &["--agent", "alice", "continue", "--goal", "G1"]);
    assert_eq!(claimed["title"], "in goal", "must not pick the task outside the goal");

    // The task outside the goal is still available to a plain continue.
    let claimed2 = json(d, &["--agent", "bob", "continue"]);
    assert_eq!(claimed2["title"], "outside goal");
}

#[test]
fn continue_with_an_empty_goal_finds_nothing() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "empty"]);
    ok(d, &["task", "add", "unrelated", "--symbol", "charge"]);

    let claimed = json_ok_or_no(d, &["--agent", "alice", "continue", "--goal", "G1"]);
    assert_eq!(claimed["task"], serde_json::Value::Null);
}
