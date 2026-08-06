//! Workers: everyone in the workspace, human and AI, through the real binary.
//!
//! Also the home of one test that looks like it is about nothing: that
//! `atlas --version` runs at all. See `the_binary_starts_at_all` below — it
//! guards a failure mode that took out every single command at once and looked
//! nothing like its cause.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

fn atlas() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_atlas"));
    c.env_remove("ATLAS_AGENT");
    c.env("ATLAS_USER", "tester");
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

fn cli_json(dir: &Path, args: &[&str]) -> Value {
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
    dir
}

fn find<'a>(ws: &'a [Value], name: &str) -> &'a Value {
    ws.iter()
        .find(|w| w["name"] == name)
        .unwrap_or_else(|| panic!("no worker {name} in {ws:?}"))
}

/// The whole binary, on the simplest possible input.
///
/// `clap`'s derive builds every subcommand in one generated function. In a
/// debug build that is a single very large stack frame, and Windows fixes the
/// main thread's stack at 1 MB where Linux gives 8 — so past about fifty
/// subcommands, *every* invocation died with "has overflowed its stack",
/// including `--version`, while `cargo build --release` was perfectly fine.
///
/// `main` now runs everything on a thread whose stack it sizes itself. This
/// test costs a millisecond and would have caught it immediately.
#[test]
fn the_binary_starts_at_all() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["--version"], vec!["--help"], vec!["workers", "--help"]] {
        let out = run(dir.path(), &args);
        assert!(
            out.status.success(),
            "atlas {args:?} exited {:?} — if this is a stack overflow, see STACK_SIZE in main.rs\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("overflowed"),
            "atlas {args:?} overflowed its stack"
        );
    }
}

#[test]
fn a_person_and_their_assistant_are_both_workers_and_told_apart() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice", "--kind", "human"]);
    ok(d, &["--agent", "claude-1", "swarm", "join", "claude-1", "--kind", "claude-code"]);
    ok(d, &["--agent", "ci", "swarm", "join", "ci", "--kind", "ci-runner"]);

    let ws = cli_json(d, &["workers"]);
    let ws = ws.as_array().unwrap();
    assert_eq!(find(ws, "alice")["type"], "human");
    assert_eq!(find(ws, "claude-1")["type"], "ai");
    assert_eq!(find(ws, "ci")["type"], "service");

    // ...and the terminal says so too, not only the JSON.
    let text = ok(d, &["workers"]);
    assert!(text.contains("human"), "{text}");
    assert!(text.contains("ai"), "{text}");
}

#[test]
fn filtering_by_type_narrows_the_list() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice", "--kind", "human"]);
    ok(d, &["--agent", "claude-1", "swarm", "join", "claude-1", "--kind", "claude-code"]);

    let humans = cli_json(d, &["workers", "--type", "human"]);
    assert_eq!(humans.as_array().unwrap().len(), 1);
    assert_eq!(humans[0]["name"], "alice");
}

#[test]
fn a_worker_carries_its_goal_task_and_progress() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice", "--kind", "human"]);
    ok(d, &["goal", "add", "Ship refunds", "--priority", "9"]);
    ok(d, &["goal", "decompose", "G1", "--task", "charge it", "--symbol", "charge"]);
    ok(d, &["assign", "T1", "--to", "alice"]);
    ok(d, &["--agent", "alice", "progress", "--percent", "40", "--note", "halfway"]);

    let ws = cli_json(d, &["workers"]);
    let alice = find(ws.as_array().unwrap(), "alice");
    assert_eq!(alice["status"], "working");
    assert_eq!(alice["task"], "T1");
    assert_eq!(alice["goal"], "G1");
    assert_eq!(alice["task_title"], "charge it");
    assert_eq!(alice["percent"], 40);
    assert_eq!(alice["note"], "halfway");
}

#[test]
fn a_worker_reads_as_blocked_when_its_scope_is_taken_away() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice", "--kind", "human"]);
    ok(d, &["--agent", "bob", "swarm", "join", "bob", "--kind", "human"]);
    ok(d, &["task", "add", "rework charge", "--priority", "5", "--symbol", "charge"]);
    ok(d, &["assign", "T1", "--to", "bob"]);

    // A higher-priority claim takes the symbol out from under bob. He still
    // owns the task and cannot proceed — the state that previously showed up
    // as him simply having no task at all.
    ok(
        d,
        &[
            "--agent", "alice", "lease", "acquire", "charge", "--preempt", "--priority", "9",
        ],
    );

    let ws = cli_json(d, &["workers"]);
    let bob = find(ws.as_array().unwrap(), "bob");
    assert_eq!(bob["status"], "blocked");
    assert_eq!(
        bob["blocked_by"], "alice",
        "a colour is not actionable; a name is"
    );
    assert_eq!(bob["task"], "T1", "he still owns the work");
}

#[test]
fn an_attached_tool_is_named_on_the_worker() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice", "--kind", "human"]);
    assert!(
        find(cli_json(d, &["workers"]).as_array().unwrap(), "alice")["tool"].is_null(),
        "nothing is attached yet"
    );

    // `session` is what an editor or an MCP adapter opens; the CLI can too.
    ok(d, &["--agent", "alice", "swarm", "join", "alice", "--kind", "cursor"]);
    let ws = cli_json(d, &["workers"]);
    assert_eq!(
        find(ws.as_array().unwrap(), "alice")["kind"],
        "cursor",
        "the registered kind is kept even when no session is open"
    );
}

/// Two different absences, told apart.
///
/// Leaving is deliberate and removes you; going quiet is not, and leaves a row
/// that `--all` can still show. A dashboard needs the distinction — "bob quit"
/// and "bob's laptop went to sleep mid-task" call for different reactions.
#[test]
fn leaving_removes_a_worker_but_going_quiet_only_hides_it() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "quitter", "swarm", "join", "quitter", "--kind", "human"]);
    ok(d, &["--agent", "napper", "swarm", "join", "napper", "--kind", "human"]);

    ok(d, &["--agent", "quitter", "swarm", "leave", "quitter"]);
    let all = cli_json(d, &["workers", "--all"]);
    let all = all.as_array().unwrap();
    assert!(
        !all.iter().any(|w| w["name"] == "quitter"),
        "leaving is deliberate: {all:?}"
    );
    assert!(all.iter().any(|w| w["name"] == "napper"));

    // Backdate the heartbeat rather than waiting a minute for it.
    let db = d.join(".atlas").join("runtime.db");
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "UPDATE agents SET heartbeat_at = heartbeat_at - 120000 WHERE name = 'napper'",
        [],
    )
    .unwrap();
    drop(conn);

    let live = cli_json(d, &["workers"]);
    assert!(
        !live.as_array().unwrap().iter().any(|w| w["name"] == "napper"),
        "gone quiet drops out of the default view"
    );
    let all = cli_json(d, &["workers", "--all"]);
    let napper = find(all.as_array().unwrap(), "napper");
    assert_eq!(
        napper["status"], "offline",
        "but is still there to be asked about"
    );
    assert!(napper["silent_for"].as_i64().unwrap() >= 120);
}
