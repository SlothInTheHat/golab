//! Review, end to end through the real binary: submit keeps leases, approve
//! releases them and unblocks dependents, reject reopens the task and
//! notifies the assignee. `golab task done` keeps working unchanged for
//! anyone who doesn't opt into this.

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
        "export function charge(x: number) { return x; }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    for a in ["author", "reviewer"] {
        ok(dir.path(), &["--agent", a, "agent", "register", a]);
    }
    dir
}

#[test]
fn submit_then_approve_releases_leases_and_unblocks_dependents() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(d, &["--agent", "author", "task", "next"]);

    // Someone is blocked on this task finishing.
    ok(
        d,
        &[
            "--agent", "waiter", "request", "depend", "--on-task", "T1", "--to", "author",
        ],
    );

    let submitted = json(d, &["--agent", "author", "review", "submit", "T1"]);
    assert_eq!(submitted["state"], "review");
    assert_eq!(json(d, &["lease", "list"]).as_array().unwrap().len(), 1, "leases held through review");

    let approved = json(d, &["--agent", "reviewer", "review", "approve", "T1"]);
    assert_eq!(approved["state"], "done");
    assert!(json(d, &["lease", "list"]).as_array().unwrap().is_empty());

    let waiting = json(d, &["--agent", "waiter", "request", "outbox", "--all"]);
    assert_eq!(waiting[0]["state"], "fulfilled", "the dependency should resolve on full approval");
}

#[test]
fn submit_then_reject_reopens_the_task_and_notifies_the_assignee() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(d, &["--agent", "author", "task", "next"]);
    ok(d, &["--agent", "author", "review", "submit", "T1"]);

    let rejected = json(
        d,
        &["--agent", "reviewer", "review", "reject", "T1", "missing a null check"],
    );
    assert_eq!(rejected["state"], "running");
    assert_eq!(rejected["assignee"], "author");
    assert_eq!(
        json(d, &["lease", "list"]).as_array().unwrap().len(),
        1,
        "the assignee keeps their leases through a rejection"
    );

    let inbox = json(d, &["--agent", "author", "request", "inbox"]);
    assert_eq!(inbox[0]["kind"], "review");
    assert_eq!(inbox[0]["body"]["reason"], "missing a null check");
}

/// The regression test for the bug the plan's review found: `plan()` used to
/// silently drop `Review`-state tasks from every wave, so `observe` (and the
/// dashboard) would show nothing in flight the moment a task was submitted.
#[test]
fn review_state_tasks_are_visible_in_observe_and_schedule() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(d, &["--agent", "author", "task", "next"]);
    ok(d, &["--agent", "author", "review", "submit", "T1"]);

    let schedule = json(d, &["schedule"]);
    assert_eq!(schedule["plan"]["in_review"][0]["id"], "T1");

    let observed = ok(d, &["observe"]);
    assert!(observed.contains("in review"), "{observed}");

    let listed = json(d, &["review", "list"]);
    assert_eq!(listed[0]["id"], "T1");
}

#[test]
fn direct_task_done_still_works_without_opting_into_review() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(d, &["--agent", "author", "task", "next"]);
    let done = json(d, &["--agent", "author", "task", "done", "T1"]);
    assert_eq!(done["state"], "done");
}

#[test]
fn only_the_assignee_can_submit_and_they_cannot_approve_without_force() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "fix charge", "--symbol", "charge"]);
    ok(d, &["--agent", "author", "task", "next"]);

    assert_eq!(
        run(d, &["--agent", "reviewer", "review", "submit", "T1"]).status.code(),
        Some(2)
    );
    ok(d, &["--agent", "author", "review", "submit", "T1"]);

    assert_eq!(
        run(d, &["--agent", "author", "review", "approve", "T1"]).status.code(),
        Some(2)
    );
    assert!(run(d, &["--agent", "author", "review", "approve", "T1", "--force"])
        .status
        .success());
}
