//! Multi-repo workspaces through the real binary: a workspace can register
//! more than one repository, `golab index`/`golab scan` cover all of them,
//! and a goal's tasks can scope to symbols in either — while `assign`,
//! `continue`, `review` and the lease layer stay entirely unaware repos
//! exist at all, exactly as the design intends.

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

/// One workspace root with two independent repos underneath it: `frontend`
/// (a Node service) and `backend` (a Python service), each with a symbol
/// named `handler` so a naive implementation would collide them.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("frontend/src")).unwrap();
    std::fs::create_dir_all(dir.path().join("backend/src")).unwrap();
    std::fs::write(
        dir.path().join("frontend/src/index.ts"),
        "export function handler() { return 'front'; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("backend/src/index.py"),
        "def handler():\n    return 'back'\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    dir
}

#[test]
fn repo_add_and_list_round_trip() {
    let dir = workspace();
    let d = dir.path();
    let added = json(d, &["repo", "add", "frontend", "--name", "web"]);
    assert_eq!(added["id"], "R2");
    assert_eq!(added["name"], "web");

    let repos = json(d, &["repo", "list"]);
    let ids: Vec<&str> = repos.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["R1", "R2"], "R1 is registered automatically by init");
}

#[test]
fn an_absolute_repo_path_is_rejected() {
    let dir = workspace();
    let d = dir.path();
    let absolute = if cfg!(windows) { r"C:\somewhere" } else { "/somewhere" };
    assert_eq!(run(d, &["repo", "add", absolute]).status.code(), Some(2));
}

#[test]
fn index_covers_every_registered_repo() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["repo", "add", "frontend", "--name", "web"]);
    ok(d, &["repo", "add", "backend", "--name", "api"]);

    ok(d, &["index"]);

    // Both repos' symbols are indexed, distinguishable by their repo-qualified handle.
    assert!(json(d, &["symbols", "handler"]).as_array().unwrap().len() >= 2);
    assert!(run(d, &["show", "web:src/index.ts:handler"]).status.success());
    assert!(run(d, &["show", "api:src/index.py:handler"]).status.success());
}

#[test]
fn scanning_one_repo_leaves_the_others_leases_untouched() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["repo", "add", "frontend", "--name", "web"]);
    ok(d, &["repo", "add", "backend", "--name", "api"]);
    ok(d, &["index"]);

    ok(d, &["--agent", "alice", "lease", "acquire", "api:src/index.py:handler"]);
    assert_eq!(json(d, &["lease", "list"]).as_array().unwrap().len(), 1);

    // Rescan only the frontend file.
    ok(d, &["scan", "frontend/src/index.ts"]);

    assert_eq!(
        json(d, &["lease", "list"]).as_array().unwrap().len(),
        1,
        "the backend lease must survive a scan scoped to the frontend repo"
    );
}

#[test]
fn a_goal_can_span_symbols_in_either_repo() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["repo", "add", "frontend", "--name", "web"]);
    ok(d, &["repo", "add", "backend", "--name", "api"]);
    ok(d, &["index"]);

    ok(d, &["goal", "add", "wire the handlers"]);
    ok(
        d,
        &["goal", "decompose", "G1", "--task", "frontend handler", "--symbol", "web:src/index.ts:handler"],
    );
    ok(
        d,
        &["goal", "decompose", "G1", "--task", "backend handler", "--symbol", "api:src/index.py:handler"],
    );

    let show = json(d, &["goal", "show", "G1"]);
    assert_eq!(show["tasks"].as_array().unwrap().len(), 2);
}
