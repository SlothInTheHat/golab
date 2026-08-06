//! Phase 5 end-to-end: the operations the dashboard performs.
//!
//! The dashboard is a thin client over these, so testing them through the CLI
//! covers both — and keeps the two surfaces honest with each other.

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
        ok(dir.path(), &["--agent", a, "agent", "register", a]);
    }
    dir
}

#[test]
fn pausing_an_agent_stops_new_work_without_taking_its_current_work() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "first", "--priority", "9", "--symbol", "charge"]);
    ok(d, &["task", "add", "second", "--priority", "5"]);
    ok(d, &["--agent", "alice", "task", "next"]);

    let paused = json(d, &["agent", "pause", "alice"]);
    assert_eq!(paused["paused"], true);
    assert_eq!(
        run(d, &["--agent", "alice", "task", "next"]).status.code(),
        Some(1),
        "a paused agent is handed nothing"
    );
    assert_eq!(
        json(d, &["lease", "list"]).as_array().unwrap().len(),
        1,
        "but it keeps what it already holds"
    );
    // Anyone else is unaffected.
    assert!(run(d, &["--agent", "bob", "task", "next"]).status.success());

    let resumed = json(d, &["agent", "resume", "alice"]);
    assert_eq!(resumed["paused"], false);
}

#[test]
fn reassigning_a_task_moves_its_lease_to_the_new_owner() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "work", "--symbol", "charge"]);
    ok(d, &["--agent", "alice", "task", "next"]);
    let before = json(d, &["lease", "list"]);
    let lease_id = before[0]["id"].as_str().unwrap().to_string();
    assert_eq!(before[0]["agent"], "alice");

    ok(d, &["task", "assign", "T1", "--to", "bob"]);

    let after = json(d, &["lease", "list"]);
    assert_eq!(after.as_array().unwrap().len(), 1);
    assert_eq!(after[0]["agent"], "bob", "the lease moved with the task");
    assert_eq!(after[0]["id"], lease_id, "same lease, never released");

    // And enforcement follows ownership.
    std::fs::write(
        d.join("src/pay.ts"),
        "export function charge(x: number) { return x + 1; }\n\
         export function refund(x: number) { return -x; }\n",
    )
    .unwrap();
    assert!(run(d, &["--agent", "bob", "check"]).status.success());
    assert_eq!(run(d, &["--agent", "alice", "check"]).status.code(), Some(1));
}

#[test]
fn priority_changes_reorder_the_queue() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "low", "--priority", "1"]);
    ok(d, &["task", "add", "high", "--priority", "9"]);
    assert_eq!(json(d, &["schedule"])["plan"]["waves"][0]["tasks"][0]["title"], "high");

    // What dragging a task to the top does.
    ok(d, &["task", "priority", "T1", "99"]);
    assert_eq!(json(d, &["schedule"])["plan"]["waves"][0]["tasks"][0]["title"], "low");
    assert_eq!(
        json(d, &["--agent", "alice", "task", "next"])["id"],
        "T1",
        "and the scheduler honours it"
    );
}

#[test]
fn throughput_summarises_what_the_swarm_did() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a", "--symbol", "charge"]);
    ok(d, &["task", "add", "b", "--symbol", "refund"]);
    ok(d, &["--agent", "alice", "task", "next"]);
    ok(d, &["--agent", "alice", "task", "done", "T1"]);
    // A denial, so contention shows up.
    ok(d, &["--agent", "bob", "lease", "acquire", "refund"]);
    assert_eq!(
        run(d, &["--agent", "alice", "lease", "acquire", "refund"])
            .status
            .code(),
        Some(1)
    );

    let t = json(d, &["throughput"]);
    assert_eq!(t["tasks_completed"], 1);
    assert_eq!(t["tasks_started"], 1);
    assert_eq!(t["leases_denied"], 1);
    assert!(t["mean_task_secs"].as_f64().is_some());
    assert_eq!(t["completed_series"].as_array().unwrap().len(), 30);

    let human = ok(d, &["throughput", "--minutes", "5"]);
    assert!(human.contains("last 5 minutes"), "{human}");
}

#[test]
fn unknown_agents_and_tasks_are_rejected() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a"]);
    assert_eq!(run(d, &["agent", "pause", "nobody"]).status.code(), Some(2));
    assert_eq!(
        run(d, &["task", "assign", "T1", "--to", "nobody"]).status.code(),
        Some(2)
    );
    assert_eq!(run(d, &["task", "priority", "T99", "5"]).status.code(), Some(2));
}

/// The dashboard reads this one payload for most of what it draws.
#[test]
fn status_carries_everything_the_dashboard_needs() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "a", "--symbol", "charge"]);
    ok(d, &["--agent", "alice", "task", "next"]);
    ok(d, &["agent", "pause", "bob"]);

    let s = json(d, &["status"]);
    let bob = s["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "bob")
        .unwrap();
    assert_eq!(bob["paused"], true, "the pause button needs this");
    assert!(s["leases"][0]["expires_at"].as_i64().is_some(), "for the countdown");
    assert!(s["tasks"][0]["priority"].as_i64().is_some(), "for drag ordering");
    assert!(s["schedule"]["startable_now"].as_i64().is_some());
    assert!(s["knowledge"]["routes"].as_array().is_some());
    assert!(
        s["progress"].as_array().is_some(),
        "the agents panel joins this in for per-agent activity"
    );
}

/// The critical-path and what-runs-next panels both read `/api/schedule`,
/// which is exactly what `atlas schedule` prints.
#[test]
fn the_plan_carries_what_the_schedule_panels_draw() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "first", "--priority", "9", "--symbol", "charge"]);
    ok(d, &["task", "add", "second", "--priority", "5", "--dep", "T1"]);
    ok(d, &["--agent", "bob", "lease", "acquire", "charge"]);

    let plan = json(d, &["schedule"])["plan"].clone();
    assert!(plan["critical_path"].as_array().is_some(), "no new endpoint needed for it");
    assert!(plan["cycles"].as_array().is_some());

    let wave = plan["waves"][0]["tasks"].as_array().expect("wave 0");
    let first = wave.iter().find(|t| t["id"] == "T1").expect("T1 in wave 0");
    assert_eq!(
        first["contended_by"]["holder"], "bob",
        "the panel greys out a held task and names who holds it: {first}"
    );
    assert!(first["contended_by"]["seconds_until_free"].as_i64().is_some());
}

/// The dashboard recomputes "startable" in one line of JS rather than making a
/// second request. If that expression and the scheduler ever disagree, the
/// panel silently lies about what can run.
#[test]
fn the_dashboards_startable_expression_agrees_with_the_scheduler() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "free", "--priority", "9", "--symbol", "refund"]);
    ok(d, &["task", "add", "held", "--priority", "8", "--symbol", "charge"]);
    ok(d, &["task", "add", "later", "--priority", "7", "--dep", "T1"]);
    ok(d, &["--agent", "bob", "lease", "acquire", "charge"]);

    let plan = json(d, &["schedule"])["plan"].clone();
    // `t.ready && !t.contended_by`, verbatim from dashboard.html.
    let js_startable = plan["waves"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|w| w["tasks"].as_array().unwrap())
        .filter(|t| t["ready"] == true && t["contended_by"].is_null())
        .count() as i64;

    assert_eq!(
        js_startable,
        plan["startable_now"].as_i64().unwrap(),
        "the JS shortcut and ScheduledTask::startable() must not drift"
    );
}

/// The connected-tools panel and the tri-state presence dot both read this.
#[test]
fn sessions_carry_what_the_connected_tools_panel_draws() {
    let dir = workspace();
    let d = dir.path();

    assert!(
        json(d, &["session", "list"]).as_array().unwrap().is_empty(),
        "registered agents are not connections; only a tool attaching is"
    );

    // An agent that is online without a session is the "heartbeating but no
    // tool" state — a bare CLI loop, drawn as a hollow dot.
    ok(d, &["--agent", "alice", "agent", "heartbeat"]);
    let alice = json(d, &["swarm", "list"]);
    let alice = alice
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "alice")
        .unwrap();
    assert_eq!(alice["online"], true);
    assert!(
        json(d, &["session", "list", "--live"]).as_array().unwrap().is_empty(),
        "online and attached are different facts, and the dot distinguishes them"
    );
}
