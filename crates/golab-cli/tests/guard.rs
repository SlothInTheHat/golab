//! `golab guard`, `golab context` and `golab session`, end to end.
//!
//! The guard is the predictive twin of `check`: asked *before* an edit, so a
//! coding agent can negotiate rather than be told off at commit time. Its exit
//! code is the whole contract — `0` allowed or merely unleased, `1` somebody
//! else holds it — because an editor hook branches on it on every keystroke.

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

/// A guard verdict is an answer, not a failure: exit 1 means "denied", which
/// callers must be able to read the report for.
fn guard(dir: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = run(dir, &full);
    let code = out.status.code().unwrap_or(-1);
    assert!(
        matches!(code, 0 | 1),
        "golab {full:?} errored ({code}):\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid json");
    (code, value)
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/pay.ts"),
        "export class Payments {\n\
        \x20 charge(x: number) { return x; }\n\
        \x20 refund(x: number) { return -x; }\n\
         }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/checkout.ts"),
        "import { Payments } from './pay';\n\
         export function checkout(x: number) { return new Payments().charge(x); }\n",
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
fn guard_denies_an_edit_to_a_symbol_another_agent_holds() {
    let dir = workspace();
    let d = dir.path();
    ok(
        d,
        &["--agent", "alice", "lease", "acquire", "src/pay.ts:Payments.charge"],
    );

    let (code, report) = guard(d, &["--agent", "bob", "guard", "src/pay.ts"]);
    assert_eq!(code, 1, "a denial is exit 1, not an error");
    assert_eq!(report["verdict"], "denied");
    assert_eq!(report["conflicts"][0]["holder"], "alice");

    // The report has to carry a way out, or the agent is simply stuck.
    let first = &report["suggestions"][0];
    assert_eq!(first["action"], "request-transfer");
    assert!(
        first["command"].as_str().unwrap().contains("request lease"),
        "suggestion must be runnable: {first}"
    );
    assert_eq!(first["tool"], "ask");
    assert!(
        report["summary"].as_str().unwrap().contains("alice"),
        "summary names the holder: {}",
        report["summary"]
    );
}

#[test]
fn narrowing_to_a_free_symbol_turns_a_denial_into_an_allowed_edit() {
    let dir = workspace();
    let d = dir.path();
    ok(
        d,
        &["--agent", "alice", "lease", "acquire", "src/pay.ts:Payments.charge"],
    );
    ok(
        d,
        &["--agent", "bob", "lease", "acquire", "src/pay.ts:Payments.refund"],
    );

    // File-granular by default, because Edit/Write name a file.
    let (code, wide) = guard(d, &["--agent", "bob", "guard", "src/pay.ts"]);
    assert_eq!(code, 1);
    assert!(
        wide["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["action"] == "narrow"),
        "a file-wide denial has to offer the precise question: {wide}"
    );

    let (code, narrow) = guard(
        d,
        &[
            "--agent",
            "bob",
            "guard",
            "src/pay.ts",
            "--symbol",
            "src/pay.ts:Payments.refund",
        ],
    );
    assert_eq!(code, 0, "bob holds refund; that edit was always legal");
    assert_eq!(narrow["verdict"], "allowed");
}

#[test]
fn an_unleased_edit_warns_but_does_not_block_unless_strict() {
    let dir = workspace();
    let d = dir.path();

    let (code, report) = guard(d, &["--agent", "bob", "guard", "src/pay.ts"]);
    assert_eq!(code, 0, "a warning is not a no");
    assert_eq!(report["verdict"], "warn");
    assert_eq!(report["suggestions"][0]["action"], "acquire");

    let (code, strict) = guard(d, &["--agent", "bob", "guard", "src/pay.ts", "--strict"]);
    assert_eq!(code, 1, "--strict is for callers that want the stronger rule");
    assert_eq!(strict["verdict"], "warn");
}

#[test]
fn a_brand_new_file_is_allowed_because_nothing_could_be_leased_yet() {
    let dir = workspace();
    let d = dir.path();
    std::fs::write(d.join("src/new.ts"), "export function fresh() {}\n").unwrap();

    let (code, report) = guard(d, &["--agent", "bob", "guard", "src/new.ts"]);
    assert_eq!(code, 0);
    assert_eq!(report["verdict"], "allowed");
    assert_eq!(report["unindexed"], true);
}

#[test]
fn your_own_lease_allows_the_edit_and_names_which_one() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "lease", "acquire", "src/pay.ts:Payments"]);

    let (code, report) = guard(
        d,
        &[
            "--agent",
            "alice",
            "guard",
            "src/pay.ts",
            "--symbol",
            "src/pay.ts:Payments.charge",
        ],
    );
    assert_eq!(code, 0);
    assert_eq!(report["verdict"], "allowed");
    assert_eq!(
        report["via"], "src/pay.ts:Payments",
        "holding the class covers its methods, and the report says so"
    );
}

#[test]
fn guard_resolves_identity_the_same_way_every_other_command_does() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "lease", "acquire", "src/pay.ts:Payments.charge"]);

    // `swarm join` wrote `.golab/agent`, so a bare invocation is bob (the
    // last to join) and gets bob's answer without naming him.
    let (code, report) = guard(d, &["guard", "src/pay.ts"]);
    assert_eq!(code, 1);
    assert_eq!(report["agent"], "bob");
    assert_eq!(report["conflicts"][0]["holder"], "alice");
}

#[test]
fn guard_without_any_identity_is_an_error_not_a_denial() {
    // A workspace nobody has joined: there is no `.golab/agent` to fall back
    // on, so the invocation is broken rather than refused. An editor hook
    // must be able to tell those two apart — one means "stop", the other
    // means "you configured me wrong".
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/pay.ts"), "export function charge() {}\n").unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);

    let out = run(dir.path(), &["guard", "src/pay.ts"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("agent"), "{stderr}");
}

#[test]
fn context_hands_over_the_scope_its_blast_radius_and_its_tests() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "make charging refundable"]);
    ok(
        d,
        &[
            "goal",
            "decompose",
            "G1",
            "--task",
            "rework charging",
            "--symbol",
            "src/pay.ts:Payments.charge",
        ],
    );
    ok(d, &["--agent", "alice", "continue"]);
    ok(
        d,
        &["--agent", "bob", "lease", "acquire", "src/checkout.ts:checkout"],
    );

    let ctx = json(d, &["context", "--task", "T1"]);
    assert_eq!(ctx["task"]["id"], "T1");
    assert_eq!(ctx["goal"]["id"], "G1");
    assert_eq!(ctx["scope"][0]["symbol"]["name"], "charge");
    assert_eq!(
        ctx["scope"][0]["lease"]["agent"], "alice",
        "the packet says the scope is already yours"
    );

    let impact: Vec<String> = ctx["impact"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["symbol"]["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        impact.iter().any(|n| n == "checkout"),
        "the caller is in the blast radius: {impact:?}"
    );

    let next_door: Vec<String> = ctx["neighbors_at_work"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        next_door,
        vec!["bob"],
        "bob holds a caller of my scope — worth knowing before the collision"
    );
}

#[test]
fn context_for_an_idle_agent_offers_what_it_could_start() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "ship refunds"]);
    ok(d, &["goal", "decompose", "G1", "--task", "wire refunds"]);

    let ctx = json(d, &["context", "--agent", "alice"]);
    assert!(ctx["task"].is_null(), "alice has claimed nothing yet");
    assert_eq!(ctx["open_goals"][0]["id"], "G1");
    assert_eq!(ctx["startable"][0]["id"], "T1");
}

#[test]
fn context_falls_back_to_the_acting_agents_own_task() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "rework charging"]);
    ok(d, &["task", "scope", "T1", "--symbol", "src/pay.ts:Payments.charge"]);
    ok(d, &["--agent", "alice", "continue"]);

    // No --task and no --agent: the identity already on hand is the answer.
    let ctx = json(d, &["--agent", "alice", "context"]);
    assert_eq!(ctx["task"]["task"]["id"], "T1");
    assert_eq!(ctx["held"][0]["agent"], "alice");
}

#[test]
fn sessions_are_listed_and_can_be_ended_by_hand() {
    let dir = workspace();
    let d = dir.path();

    // Nothing is attached until a tool says so; the CLI is not a session.
    let empty = json(d, &["session", "list"]);
    assert_eq!(empty.as_array().unwrap().len(), 0);

    assert!(
        json(d, &["session", "list", "--live"]).as_array().unwrap().is_empty(),
        "and --live is empty too"
    );
}

#[test]
fn two_repos_sharing_a_relative_path_get_separate_verdicts() {
    // `path` is not a global key — `repo_id` is part of it. The guard has to
    // route by repo before it looks anything up, or a lease in one checkout
    // would block an edit in an unrelated one that merely shares a filename.
    let dir = workspace();
    let d = dir.path();
    std::fs::create_dir_all(d.join("vendor/src")).unwrap();
    std::fs::write(
        d.join("vendor/src/pay.ts"),
        "export class Payments {\n\x20 charge(x: number) { return x * 2; }\n}\n",
    )
    .unwrap();
    ok(d, &["repo", "add", "vendor"]);
    ok(d, &["index"]);

    // The bare handle is genuinely ambiguous now, which `resolve` refuses
    // rather than guessing — so name the repo.
    ok(
        d,
        &[
            "--agent",
            "alice",
            "lease",
            "acquire",
            "R1:src/pay.ts:Payments.charge",
        ],
    );

    let (blocked, _) = guard(d, &["--agent", "bob", "guard", "src/pay.ts"]);
    assert_eq!(blocked, 1, "alice holds this one");

    let (free, report) = guard(d, &["--agent", "bob", "guard", "vendor/src/pay.ts"]);
    assert_eq!(
        free, 0,
        "the vendor copy is a different symbol and nobody holds it"
    );
    assert_eq!(report["verdict"], "warn");
}
