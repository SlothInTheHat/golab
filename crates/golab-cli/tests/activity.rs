//! Live editing: who is in which file, right now, before any of it is
//! committed.
//!
//! The claim being pinned is that this works *without the model choosing to
//! cooperate*. Every producer here is an editor hook — the same callbacks that
//! fire whether or not the agent on the other end ever calls a golab tool. If
//! these pass, a second person watching the dashboard learns that somebody is
//! in `src/pay.ts` from the fact that they tried to edit it, and nothing else.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

fn golab() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_golab"));
    c.env_remove("GOLAB_AGENT");
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
    std::fs::write(
        dir.path().join("src/ledger.ts"),
        "export function record(x: number) { return x; }\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

fn edit_payload(dir: &Path, event: &str, file: &str) -> Value {
    json!({
        "session_id": "sess-abc123",
        "cwd": dir.to_string_lossy(),
        "hook_event_name": event,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": dir.join(file).to_string_lossy(),
            "old_string": "a",
            "new_string": "b"
        }
    })
}

fn rows(dir: &Path) -> Vec<Value> {
    cli_json(dir, &["activity"])
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn asking_permission_is_what_announces_the_edit() {
    let dir = workspace();
    let d = dir.path();

    assert!(rows(d).is_empty(), "nothing is happening yet");

    // The guard hook fires one keystroke *before* the edit lands. Nothing else
    // in the runtime can see this moment.
    hook(d, &["hook", "guard"], &edit_payload(d, "PreToolUse", "src/pay.ts"));

    let live = rows(d);
    assert_eq!(live.len(), 1, "one open window");
    assert_eq!(live[0]["agent"], DERIVED);
    assert_eq!(live[0]["path"], "src/pay.ts");
    assert_eq!(
        live[0]["kind"], "editing",
        "about to edit, and has not yet — the whole point of guarding first"
    );
    assert_eq!(live[0]["live"], true);
}

#[test]
fn a_refused_edit_is_recorded_as_contention_not_as_silence() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice"]);
    ok(d, &["--agent", "alice", "lease", "acquire", "charge"]);

    let out = hook(d, &["hook", "guard"], &edit_payload(d, "PreToolUse", "src/pay.ts"));
    assert_eq!(out.status.code(), Some(2), "the edit is refused");

    let mine: Vec<Value> = rows(d)
        .into_iter()
        .filter(|r| r["agent"] == DERIVED)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(
        mine[0]["kind"], "blocked",
        "two agents reaching for one file within a TTL is exactly what a human needs to see"
    );
    assert_eq!(mine[0]["verdict"], "denied");
}

#[test]
fn a_landed_edit_names_the_symbol_and_not_just_the_file() {
    let dir = workspace();
    let d = dir.path();
    hook(d, &["hook", "post-tool"], &edit_payload(d, "PostToolUse", "src/pay.ts"));

    let live = rows(d);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["kind"], "edited");
    assert_eq!(
        live[0]["symbol_handle"], "src/pay.ts",
        "the window points at an indexed symbol, not just a path string"
    );

    // The regression this fixes: `record_progress` was always called with
    // symbol_id = None, so the dashboard could report a percentage but never
    // what was being worked on.
    let progress = cli_json(d, &["status"]);
    let p = &progress["progress"][0];
    assert_eq!(p["agent"], DERIVED);
    assert!(
        p["symbol_id"].is_string(),
        "progress finally carries the symbol: {p}"
    );
}

#[test]
fn an_unindexed_file_is_still_reported_without_claiming_a_symbol() {
    let dir = workspace();
    let d = dir.path();
    std::fs::write(d.join("src/brand-new.ts"), "export const x = 1;\n").unwrap();

    hook(
        d,
        &["hook", "post-tool"],
        &edit_payload(d, "PostToolUse", "src/brand-new.ts"),
    );

    let live = rows(d);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0]["path"], "src/brand-new.ts");
    assert!(
        live[0]["symbol_id"].is_null(),
        "we can compute the id of a file nobody has scanned, but pointing at it would be a lie"
    );
}

#[test]
fn a_multi_file_call_opens_a_window_on_every_file_it_touched() {
    let dir = workspace();
    let d = dir.path();
    let payload = json!({
        "session_id": "sess-abc123",
        "cwd": d.to_string_lossy(),
        "hook_event_name": "PostToolUse",
        "tool_name": "MultiEdit",
        "tool_input": {
            "edits": [
                { "file_path": d.join("src/pay.ts").to_string_lossy() },
                { "file_path": d.join("src/ledger.ts").to_string_lossy() },
            ]
        }
    });
    hook(d, &["hook", "post-tool"], &payload);

    let mut paths: Vec<String> = rows(d)
        .iter()
        .map(|r| r["path"].as_str().unwrap().to_string())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["src/ledger.ts", "src/pay.ts"],
        "leaving the others reading 'editing' until they expire would be a lie for a whole minute"
    );
}

#[test]
fn the_window_moves_rather_than_piling_up() {
    let dir = workspace();
    let d = dir.path();
    for _ in 0..5 {
        hook(d, &["hook", "guard"], &edit_payload(d, "PreToolUse", "src/pay.ts"));
        hook(
            d,
            &["hook", "post-tool"],
            &edit_payload(d, "PostToolUse", "src/pay.ts"),
        );
    }

    let all = cli_json(d, &["activity", "--all"]);
    assert_eq!(
        all.as_array().unwrap().len(),
        1,
        "ten callbacks on one file is one row, not ten"
    );
    assert_eq!(all[0]["kind"], "edited", "showing the latest state");
}

#[test]
fn two_agents_editing_two_files_are_two_windows() {
    let dir = workspace();
    let d = dir.path();

    // A second identity, reported through the CLI's own agent flag rather than
    // the hook's derived one.
    hook(d, &["hook", "guard"], &edit_payload(d, "PreToolUse", "src/pay.ts"));
    golab()
        .current_dir(d)
        .env("GOLAB_AGENT", "alice")
        .args(["hook", "post-tool"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map(|mut c| {
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(
                    serde_json::to_string(&edit_payload(d, "PostToolUse", "src/ledger.ts"))
                        .unwrap()
                        .as_bytes(),
                )
                .unwrap();
            c.wait_with_output().unwrap()
        })
        .unwrap();

    let live = rows(d);
    assert_eq!(live.len(), 2, "two people, two files, two windows: {live:?}");

    let filtered = cli_json(d, &["activity", "--agent", DERIVED]);
    assert_eq!(filtered.as_array().unwrap().len(), 1);
    assert_eq!(filtered[0]["path"], "src/pay.ts");
}

#[test]
fn leaving_closes_the_window() {
    let dir = workspace();
    let d = dir.path();
    let start = json!({
        "session_id": "sess-abc123",
        "cwd": d.to_string_lossy(),
        "hook_event_name": "SessionStart",
    });
    hook(d, &["hook", "session-start"], &start);
    hook(d, &["hook", "guard"], &edit_payload(d, "PreToolUse", "src/pay.ts"));
    assert_eq!(rows(d).len(), 1);

    hook(
        d,
        &["hook", "session-end"],
        &json!({ "session_id": "sess-abc123", "hook_event_name": "SessionEnd" }),
    );
    assert!(
        rows(d).is_empty(),
        "closing the editor is not still editing"
    );
}

#[test]
fn the_event_bus_carries_the_edit_window() {
    let dir = workspace();
    let d = dir.path();
    hook(d, &["hook", "guard"], &edit_payload(d, "PreToolUse", "src/pay.ts"));

    // `--json watch` is one JSON object per line, backlog included.
    let raw = ok(d, &["--json", "watch", "--once", "--tail", "50"]);
    let started = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("each line is an event"))
        .filter(|e| e["kind"] == "activity.started")
        .count();
    assert_eq!(
        started, 1,
        "everything interesting goes through the bus, or the dashboard cannot see it"
    );
}
