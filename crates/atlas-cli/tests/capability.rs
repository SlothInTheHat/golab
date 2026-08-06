//! Capability-based scheduling through the real binary: an agent declares a
//! role, a task can require one, and `continue` only hands matching work to
//! a matching agent — a hard gate, unlike `assign` which is the deliberate
//! human-override path around it.

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
        "export function charge(x: number) { return x; }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

#[test]
fn a_mismatched_agent_never_sees_a_gated_task() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "backend-1", "swarm", "join", "backend-1", "--capability", "backend"]);
    ok(d, &["task", "add", "write the tests", "--priority", "9", "--capability", "testing"]);

    let claimed = json_ok_or_no(d, &["--agent", "backend-1", "continue"]);
    assert_eq!(claimed["task"], serde_json::Value::Null);
}

#[test]
fn a_matching_agent_claims_it() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "qa-1", "swarm", "join", "qa-1", "--capability", "testing"]);
    ok(d, &["task", "add", "write the tests", "--priority", "9", "--capability", "testing"]);

    let claimed = json(d, &["--agent", "qa-1", "continue"]);
    assert_eq!(claimed["title"], "write the tests");
}

#[test]
fn assign_overrides_the_gate() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "backend-1", "swarm", "join", "backend-1", "--capability", "backend"]);
    ok(d, &["task", "add", "write the tests", "--priority", "9", "--capability", "testing"]);

    let assigned = json(d, &["assign", "T1", "--to", "backend-1"]);
    assert_eq!(assigned["assignee"], "backend-1");
}

#[test]
fn an_unknown_capability_is_rejected() {
    let dir = workspace();
    let d = dir.path();
    assert_eq!(
        run(d, &["task", "add", "x", "--capability", "wizard"]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(d, &["--agent", "a", "swarm", "join", "a", "--capability", "wizard"]).status.code(),
        Some(2)
    );
}
