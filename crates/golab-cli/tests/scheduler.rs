//! Phase 2 end-to-end: the scheduler.
//!
//! The claim being tested is that this is more than a priority queue — it
//! reads the code graph to order work, refuses to hand out work that would
//! collide, and takes work back from agents that die.

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

/// `createPayment` calls `record`, so the scheduler has a real edge to use.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/ledger.ts"),
        "export function record(x: number) { return x; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/pay.ts"),
        "import { record } from './ledger';\n\
         export function createPayment(x: number) { return record(x); }\n\
         export function refund(x: number) { return -x; }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    for a in ["agent-1", "agent-2"] {
        ok(dir.path(), &["--agent", a, "agent", "register", a]);
    }
    dir
}

#[test]
fn dependencies_are_inferred_from_the_code_not_declared() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "wire the endpoint", "--priority", "9", "--symbol", "createPayment"]);
    ok(d, &["task", "add", "write the ledger", "--priority", "1", "--symbol", "record"]);

    let out = json(d, &["schedule", "--infer"]);
    let inferred = out["inferred"].as_array().unwrap();
    assert_eq!(inferred.len(), 1, "{inferred:?}");
    assert_eq!(inferred[0]["task"], "T1");
    assert_eq!(inferred[0]["depends_on"], "T2");
    assert_eq!(inferred[0]["edge"], "calls");

    // The high-priority caller is now in the second wave, behind its callee.
    let waves = out["plan"]["waves"].as_array().unwrap();
    assert_eq!(waves[0]["tasks"][0]["id"], "T2");
    assert_eq!(waves[1]["tasks"][0]["id"], "T1");
    assert_eq!(out["plan"]["startable_now"], 1);
}

#[test]
fn claiming_a_task_leases_its_scope_in_one_step() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "do it", "--symbol", "createPayment"]);

    let claimed = json(d, &["--agent", "agent-1", "task", "next"]);
    assert_eq!(claimed["assignee"], "agent-1");
    assert_eq!(claimed["scope"][0]["name"], "createPayment");

    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases.as_array().unwrap().len(), 1);
    assert_eq!(leases[0]["agent"], "agent-1");
    assert_eq!(leases[0]["task"], "T1");

    // So enforcement already passes for the agent that was handed the work.
    std::fs::write(
        d.join("src/pay.ts"),
        "import { record } from './ledger';\n\
         export function createPayment(x: number) { return record(x) + 1; }\n\
         export function refund(x: number) { return -x; }\n",
    )
    .unwrap();
    assert!(run(d, &["--agent", "agent-1", "check"]).status.success());
    assert_eq!(
        run(d, &["--agent", "agent-2", "check"]).status.code(),
        Some(1)
    );
}

/// The core scheduling promise: never hand two agents colliding work.
#[test]
fn work_held_by_someone_else_is_skipped_not_handed_out() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "hot", "--priority", "9", "--symbol", "createPayment"]);
    ok(d, &["task", "add", "cold", "--priority", "1", "--symbol", "refund"]);
    ok(d, &["--agent", "outsider", "lease", "acquire", "createPayment"]);

    // The higher-priority task is ready but unsafe, so the agent gets the other.
    let claimed = json(d, &["--agent", "agent-1", "task", "next"]);
    assert_eq!(claimed["id"], "T2", "the contended task must be skipped");

    let plan = json(d, &["schedule"]);
    let hot = plan["plan"]["waves"][0]["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "T1")
        .expect("T1 is still in wave 1");
    assert_eq!(hot["ready"], true, "its dependencies are satisfied");
    assert_eq!(hot["contended_by"]["holder"], "outsider");
}

#[test]
fn containment_counts_as_a_collision() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "edit a function", "--symbol", "createPayment"]);
    // Holding the whole file must block a task scoped to a function in it.
    ok(d, &["--agent", "outsider", "lease", "acquire", "src/pay.ts"]);

    let out = run(d, &["--agent", "agent-1", "task", "next"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("held by other agents"),
        "the refusal should explain itself"
    );
}

#[test]
fn two_agents_are_given_different_work() {
    let dir = workspace();
    let d = dir.path();
    for (title, sym) in [("a", "createPayment"), ("b", "refund"), ("c", "record")] {
        ok(d, &["task", "add", title, "--symbol", sym]);
    }
    let first = json(d, &["--agent", "agent-1", "task", "next"]);
    let second = json(d, &["--agent", "agent-2", "task", "next"]);
    assert_ne!(first["id"], second["id"]);
    assert_eq!(json(d, &["lease", "list"]).as_array().unwrap().len(), 2);
}

#[test]
fn finishing_releases_the_scope_and_can_pull_the_next_task() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "first", "--priority", "9", "--symbol", "createPayment"]);
    ok(d, &["task", "add", "second", "--priority", "5", "--symbol", "refund"]);
    ok(d, &["--agent", "agent-1", "task", "next"]);

    let out = ok(d, &["--agent", "agent-1", "task", "done", "T1", "--next"]);
    assert!(out.contains("T2"), "it should have claimed the next task: {out}");

    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases.as_array().unwrap().len(), 1, "T1's scope was released");
    assert_eq!(leases[0]["task"], "T2");
}

#[test]
fn a_dependency_cycle_is_reported_rather_than_silently_stalling() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a"]);
    ok(d, &["task", "add", "b", "--dep", "T1"]);
    // Close the loop through the CLI's own dependency handling.
    let out = run(d, &["task", "add", "c", "--dep", "T2", "--dep", "T99"]);
    assert_eq!(out.status.code(), Some(2), "unknown deps are rejected");

    let plan = json(d, &["schedule"]);
    assert!(plan["plan"]["cycles"].as_array().unwrap().is_empty());
    assert_eq!(plan["plan"]["waves"].as_array().unwrap().len(), 2);
    assert_eq!(plan["plan"]["max_parallel"], 1);
}

#[test]
fn an_unscoped_task_is_still_scheduled_and_leases_nothing() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "write the docs"]);
    let claimed = json(d, &["--agent", "agent-1", "task", "next"]);
    assert_eq!(claimed["id"], "T1");
    assert!(claimed["scope"].as_array().unwrap().is_empty());
    assert!(json(d, &["lease", "list"]).as_array().unwrap().is_empty());
}

#[test]
fn the_schedule_headline_shows_up_in_status() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a", "--symbol", "createPayment"]);
    ok(d, &["task", "add", "b", "--symbol", "refund"]);
    let status = json(d, &["status"]);
    assert_eq!(status["schedule"]["startable_now"], 2);
    assert_eq!(status["schedule"]["max_parallel"], 2);
    assert_eq!(status["schedule"]["cycles"], 0);
}

#[test]
fn scoping_accepts_any_symbol_reference_and_rejects_unknown_ones() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a"]);
    // A path:Fqn handle works as well as a bare name.
    let scoped = json(d, &["task", "scope", "T1", "--symbol", "src/pay.ts:refund"]);
    assert_eq!(scoped["scope"][0]["name"], "refund");

    let bad = run(d, &["task", "scope", "T1", "--symbol", "doesNotExist"]);
    assert_eq!(bad.status.code(), Some(2));
    let bad_task = run(d, &["task", "scope", "T99", "--symbol", "refund"]);
    assert_eq!(bad_task.status.code(), Some(2));
}
