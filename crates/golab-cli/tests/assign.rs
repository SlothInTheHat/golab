//! `golab assign`, end to end: the top-level verb that replaces reaching for
//! `golab lease acquire` directly. Also exercises the bug Part 1 of this
//! pass fixed — a scoped-but-never-claimed task used to reassign with zero
//! leases, which affected the dashboard's reassign button too.

use std::path::Path;
use std::process::{Command, Output};

fn golab() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_golab"));
    c.env_remove("GOLAB_AGENT");
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

fn json(dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(dir, &full)).expect("valid json")
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
fn assigning_a_never_claimed_scoped_task_leases_it_atomically() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);

    let assigned = json(d, &["assign", "T1", "--to", "alice"]);
    assert_eq!(assigned["assignee"], "alice");
    assert_eq!(assigned["state"], "running");

    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases.as_array().unwrap().len(), 1, "the scope must actually be leased");
    assert_eq!(leases[0]["agent"], "alice");

    // And enforcement follows: alice may edit it, nobody else may.
    std::fs::write(
        d.join("src/pay.ts"),
        "export function charge(x: number) { return x + 1; }\n\
         export function refund(x: number) { return -x; }\n",
    )
    .unwrap();
    assert!(run(d, &["--agent", "alice", "check"]).status.success());
    assert_eq!(run(d, &["--agent", "bob", "check"]).status.code(), Some(1));
}

#[test]
fn assigning_into_a_conflict_is_refused_without_preempt() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(d, &["--agent", "bob", "lease", "acquire", "charge"]);

    let out = run(d, &["assign", "T1", "--to", "alice"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("held by someone else")
            || String::from_utf8_lossy(&out.stderr).contains("held by someone else")
    );

    // Nothing changed: the task is still unassigned, bob still holds it.
    let task = json(d, &["task", "list"]);
    assert_eq!(task[0]["assignee"], serde_json::Value::Null);
    assert_eq!(json(d, &["lease", "list"])[0]["agent"], "bob");
}

#[test]
fn assigning_with_preempt_takes_a_lower_priority_holders_scope() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(
        d,
        &["--agent", "bob", "lease", "acquire", "charge", "--priority", "1"],
    );

    let assigned = json(
        d,
        &["assign", "T1", "--to", "alice", "--preempt", "--priority", "9"],
    );
    assert_eq!(assigned["assignee"], "alice");
    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases.as_array().unwrap().len(), 1);
    assert_eq!(leases[0]["agent"], "alice");
}

#[test]
fn assigning_a_goal_picks_its_next_startable_task() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "refunds", "--priority", "9"]);
    ok(d, &["goal", "decompose", "G1", "--task", "high", "--priority", "9", "--symbol", "charge"]);
    ok(d, &["goal", "decompose", "G1", "--task", "low", "--priority", "1", "--symbol", "refund"]);

    let assigned = json(d, &["assign", "G1", "--to", "alice"]);
    assert_eq!(assigned["title"], "high", "the highest-priority startable task under the goal");
}

#[test]
fn unknown_agents_and_tasks_are_rejected() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a"]);
    assert_eq!(run(d, &["assign", "T1", "--to", "nobody"]).status.code(), Some(2));
    assert_eq!(run(d, &["assign", "T99", "--to", "alice"]).status.code(), Some(2));
}
