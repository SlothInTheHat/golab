//! Goals, end to end through the real binary: the top of the abstraction
//! stack this reframing adds. A human names a goal; agents decompose it into
//! scoped tasks; the goal aggregates their progress.

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

/// Two services, so `suggest` has more than one group to find:
/// `payments-api` calls into `ledger-lib`.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("api/src")).unwrap();
    std::fs::create_dir_all(dir.path().join("lib/src")).unwrap();
    std::fs::write(dir.path().join("api/package.json"), r#"{"name":"payments-api"}"#).unwrap();
    std::fs::write(dir.path().join("lib/package.json"), r#"{"name":"ledger-lib"}"#).unwrap();
    std::fs::write(
        dir.path().join("lib/src/ledger.ts"),
        "export function record(x: number) { return x; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("api/src/routes.ts"),
        "import { record } from '../../lib/src/ledger';\n\
         export function createPayment(x: number) { return record(x); }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    ok(dir.path(), &["--agent", "alice", "swarm", "join", "alice"]);
    ok(dir.path(), &["--agent", "bob", "swarm", "join", "bob"]);
    dir
}

#[test]
fn decomposing_a_goal_creates_scoped_tasks() {
    let dir = workspace();
    let d = dir.path();
    let goal = json(d, &["goal", "add", "Implement refunds", "--priority", "9"]);
    assert_eq!(goal["id"], "G1");

    let t1 = json(
        d,
        &["goal", "decompose", "G1", "--task", "wire the endpoint", "--symbol", "createPayment"],
    );
    assert_eq!(t1["id"], "T1");

    // The scope is real: claiming the task leases it.
    let claimed = json(d, &["--agent", "alice", "continue", "--goal", "G1"]);
    assert_eq!(claimed["id"], "T1");
    assert_eq!(claimed["scope"][0]["name"], "createPayment");
    assert_eq!(json(d, &["lease", "list"]).as_array().unwrap().len(), 1);
}

#[test]
fn goal_progress_reflects_task_completion() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "Implement refunds"]);
    ok(d, &["goal", "decompose", "G1", "--task", "a", "--symbol", "createPayment"]);
    ok(d, &["goal", "decompose", "G1", "--task", "b", "--symbol", "record"]);

    assert_eq!(json(d, &["goal", "show", "G1"])["progress"]["done"], 0);

    ok(d, &["--agent", "alice", "continue", "--goal", "G1"]);
    ok(d, &["--agent", "alice", "task", "done", "T1"]);

    let show = json(d, &["goal", "show", "G1"]);
    assert_eq!(show["progress"]["total"], 2);
    assert_eq!(show["progress"]["done"], 1);
    assert_eq!(show["progress"]["percent"], 50.0);
}

#[test]
fn goal_show_lists_its_tasks() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "Implement refunds"]);
    ok(d, &["goal", "decompose", "G1", "--task", "a", "--symbol", "createPayment"]);
    ok(d, &["task", "add", "unrelated"]); // not under the goal

    let show = json(d, &["goal", "show", "G1"]);
    let ids: Vec<&str> = show["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["T1"], "the unrelated task must not show up under G1");
}

#[test]
fn suggest_proposes_tasks_grouped_by_service() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "Implement refunds"]);

    let suggestions = json(d, &["goal", "suggest", "G1", "--near", "createPayment", "--depth", "2"]);
    let titles: Vec<&str> = suggestions
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"Update payments-api"), "{titles:?}");

    // --apply actually creates the tasks instead of just previewing.
    assert!(json(d, &["goal", "show", "G1"])["tasks"].as_array().unwrap().is_empty());
    ok(d, &["goal", "suggest", "G1", "--near", "createPayment", "--apply"]);
    assert!(!json(d, &["goal", "show", "G1"])["tasks"].as_array().unwrap().is_empty());
}

#[test]
fn plan_proposes_tasks_from_the_goal_title_alone() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "Handle payments"]);

    let suggestions = json(d, &["goal", "plan", "G1", "--depth", "2"]);
    let titles: Vec<&str> = suggestions
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["title"].as_str().unwrap())
        .collect();
    assert!(titles.contains(&"Update payments-api"), "{titles:?}");

    ok(d, &["goal", "plan", "G1", "--apply"]);
    assert!(!json(d, &["goal", "show", "G1"])["tasks"].as_array().unwrap().is_empty());
}

#[test]
fn plan_with_no_keyword_match_reports_nothing_and_exits_1() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "Improve team morale"]);

    let out = run(d, &["--json", "goal", "plan", "G1"]);
    assert_eq!(out.status.code(), Some(1));
    let suggestions: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(suggestions.as_array().unwrap().is_empty());
}

#[test]
fn unknown_goals_are_rejected_everywhere() {
    let dir = workspace();
    let d = dir.path();
    assert_eq!(run(d, &["goal", "show", "G99"]).status.code(), Some(2));
    assert_eq!(
        run(d, &["goal", "decompose", "G99", "--task", "x"]).status.code(),
        Some(2)
    );
    assert_eq!(
        run(d, &["goal", "suggest", "G99", "--near", "createPayment"]).status.code(),
        Some(2)
    );
    assert_eq!(run(d, &["goal", "plan", "G99"]).status.code(), Some(2));
}

#[test]
fn done_and_abandon_change_goal_state() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "a"]);
    ok(d, &["goal", "add", "b"]);

    let done = json(d, &["goal", "done", "G1"]);
    assert_eq!(done["state"], "done");
    let abandoned = json(d, &["goal", "abandon", "G2"]);
    assert_eq!(abandoned["state"], "abandoned");
}
