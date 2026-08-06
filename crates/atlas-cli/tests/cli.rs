//! End-to-end tests driving the real `atlas` binary.
//!
//! The lease tests in atlas-core prove the logic; these prove the property
//! that actually matters in production, where agents are separate OS processes
//! contending through the database rather than threads sharing a `Store`.

use std::path::Path;
use std::process::{Command, Output};

fn atlas() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_atlas"));
    // Keep a stray developer environment out of the tests.
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
        "atlas {args:?} failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

const SOURCE: &str = r#"
export class PaymentService {
  async processPayment(id: string) {
    return charge(id);
  }
  async refund(id: string) {
    return 0;
  }
}

export function charge(id: string) {
  return 1;
}
"#;

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/pay.ts"), SOURCE).unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["scan"]);
    dir
}

#[test]
fn scan_indexes_the_workspace() {
    let dir = workspace();
    let json: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["--json", "symbols"])).unwrap();
    let names: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["fqn"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"PaymentService.processPayment"), "{names:?}");
    assert!(names.contains(&"charge"), "{names:?}");
}

/// The Phase 0 success criterion, across processes: eight agents race for one
/// function and exactly one may win.
#[test]
fn only_one_of_many_processes_wins_the_same_symbol() {
    let dir = workspace();
    let root = dir.path().to_path_buf();

    let children: Vec<_> = (0..8)
        .map(|i| {
            atlas()
                .current_dir(&root)
                .args([
                    "--json",
                    "--agent",
                    &format!("agent-{i}"),
                    "lease",
                    "acquire",
                    "PaymentService.processPayment",
                    "--ttl",
                    "120",
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn")
        })
        .collect();

    let mut granted = 0;
    let mut denied = 0;
    for child in children {
        let out = child.wait_with_output().expect("wait");
        match out.status.code() {
            Some(0) => granted += 1,
            Some(1) => denied += 1,
            other => panic!(
                "unexpected exit {other:?}: {}",
                String::from_utf8_lossy(&out.stdout)
            ),
        }
    }
    assert_eq!(granted, 1, "exactly one agent may hold the lease");
    assert_eq!(denied, 7);

    let leases: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["--json", "lease", "list"])).unwrap();
    assert_eq!(leases.as_array().unwrap().len(), 1);
}

/// Disjoint work must stay parallel — the point is coordination, not a lock.
#[test]
fn different_symbols_are_leased_concurrently() {
    let dir = workspace();
    let root = dir.path().to_path_buf();
    let targets = ["PaymentService.processPayment", "PaymentService.refund", "charge"];

    let children: Vec<_> = targets
        .iter()
        .enumerate()
        .map(|(i, sym)| {
            atlas()
                .current_dir(&root)
                .args([
                    "--json",
                    "--agent",
                    &format!("agent-{i}"),
                    "lease",
                    "acquire",
                    sym,
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("spawn")
        })
        .collect();

    for child in children {
        let out = child.wait_with_output().expect("wait");
        assert_eq!(out.status.code(), Some(0));
    }
    let leases: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["--json", "lease", "list"])).unwrap();
    assert_eq!(leases.as_array().unwrap().len(), 3);
}

#[test]
fn check_blocks_unleased_edits_and_allows_leased_ones() {
    let dir = workspace();
    let edited = SOURCE.replace("return charge(id);", "return charge(id) + 1;");

    // Nobody holds anything: the edit is rejected.
    std::fs::write(dir.path().join("src/pay.ts"), &edited).unwrap();
    let out = run(dir.path(), &["--agent", "agent-1", "check"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("processPayment"), "{text}");

    // With the lease, the same edit passes.
    assert!(run(
        dir.path(),
        &[
            "--agent",
            "agent-1",
            "lease",
            "acquire",
            "PaymentService.processPayment"
        ]
    )
    .status
    .success());
    let out = run(dir.path(), &["--agent", "agent-1", "check"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // But not for anyone else.
    let out = run(dir.path(), &["--agent", "agent-2", "check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("agent-1"));
}

#[test]
fn a_crashed_agents_lease_expires_and_the_symbol_frees_up() {
    let dir = workspace();
    // A one-second TTL with no heartbeat is exactly what a crash looks like.
    assert!(run(
        dir.path(),
        &["--agent", "crashed", "lease", "acquire", "charge", "--ttl", "1"]
    )
    .status
    .success());
    assert_eq!(
        run(dir.path(), &["--agent", "other", "lease", "acquire", "charge"])
            .status
            .code(),
        Some(1)
    );

    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        run(dir.path(), &["--agent", "other", "lease", "acquire", "charge"])
            .status
            .success(),
        "the lease should have expired on its own"
    );
}

#[test]
fn releasing_hands_the_symbol_to_the_first_waiter() {
    let dir = workspace();
    ok(dir.path(), &["--agent", "holder", "lease", "acquire", "charge"]);
    // Two agents queue, in order.
    assert_eq!(
        run(dir.path(), &["--agent", "waiter-1", "lease", "acquire", "charge"])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(
        run(dir.path(), &["--agent", "waiter-2", "lease", "acquire", "charge"])
            .status
            .code(),
        Some(1)
    );

    let queue = ok(dir.path(), &["--json", "lease", "queue", "charge"]);
    let queue: serde_json::Value = serde_json::from_str(&queue).unwrap();
    assert_eq!(queue[0]["agent"], "waiter-1");

    ok(dir.path(), &["--agent", "holder", "lease", "release", "--all"]);
    assert_eq!(
        run(dir.path(), &["--agent", "waiter-2", "lease", "acquire", "charge"])
            .status
            .code(),
        Some(1),
        "waiter-2 must not jump the queue"
    );
    assert!(
        run(dir.path(), &["--agent", "waiter-1", "lease", "acquire", "charge"])
            .status
            .success()
    );
}

#[test]
fn leases_survive_rescans_of_edited_code() {
    let dir = workspace();
    ok(
        dir.path(),
        &["--agent", "agent-1", "lease", "acquire", "PaymentService.processPayment"],
    );
    std::fs::write(
        dir.path().join("src/pay.ts"),
        SOURCE.replace("return charge(id);", "return charge(id) + 42;"),
    )
    .unwrap();
    ok(dir.path(), &["scan"]);

    let leases: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["--json", "lease", "list"])).unwrap();
    assert_eq!(leases.as_array().unwrap().len(), 1, "identity is not content");
    assert_eq!(leases[0]["agent"], "agent-1");
}

#[test]
fn deleting_a_leased_symbol_retires_the_lease() {
    let dir = workspace();
    ok(dir.path(), &["--agent", "agent-1", "lease", "acquire", "charge"]);
    std::fs::write(
        dir.path().join("src/pay.ts"),
        "export class PaymentService {\n  async refund(id: string) { return 0; }\n}\n",
    )
    .unwrap();
    ok(dir.path(), &["scan"]);

    let leases: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["--json", "lease", "list"])).unwrap();
    assert!(leases.as_array().unwrap().is_empty());

    let events = ok(dir.path(), &["watch", "--once", "--since", "1"]);
    assert!(events.contains("lease.dropped"), "{events}");
}

#[test]
fn the_task_graph_hands_out_unblocked_work_only() {
    let dir = workspace();
    ok(dir.path(), &["task", "add", "backend", "--priority", "5"]);
    ok(
        dir.path(),
        &["task", "add", "frontend", "--priority", "9", "--dep", "T1"],
    );

    let claimed: serde_json::Value = serde_json::from_str(&ok(
        dir.path(),
        &["--json", "--agent", "agent-1", "task", "next"],
    ))
    .unwrap();
    assert_eq!(claimed["id"], "T1", "the higher-priority task is blocked");

    // Nothing else is available until T1 is done.
    assert_eq!(
        run(dir.path(), &["--agent", "agent-2", "task", "next"])
            .status
            .code(),
        Some(1)
    );
    ok(dir.path(), &["--agent", "agent-1", "task", "done", "T1"]);
    let claimed: serde_json::Value = serde_json::from_str(&ok(
        dir.path(),
        &["--json", "--agent", "agent-2", "task", "next"],
    ))
    .unwrap();
    assert_eq!(claimed["id"], "T2");
}

#[test]
fn agents_share_memory_and_messages() {
    let dir = workspace();
    ok(dir.path(), &["--agent", "claude-1", "agent", "register", "claude-1"]);
    ok(
        dir.path(),
        &[
            "--agent",
            "claude-1",
            "memory",
            "set",
            "decision/auth",
            "JWT, 15 minute expiry",
            "--tag",
            "architecture",
        ],
    );
    assert!(ok(dir.path(), &["memory", "get", "decision/auth"]).contains("JWT"));

    ok(
        dir.path(),
        &[
            "--agent",
            "claude-1",
            "msg",
            "send",
            "--to",
            "cursor-1",
            "--subject",
            "need-interface",
            "--body",
            r#"{"methods":["authorize","capture"]}"#,
        ],
    );
    let inbox: serde_json::Value = serde_json::from_str(&ok(
        dir.path(),
        &["--json", "--agent", "cursor-1", "msg", "inbox"],
    ))
    .unwrap();
    assert_eq!(inbox[0]["subject"], "need-interface");
    assert_eq!(inbox[0]["body"]["methods"][0], "authorize");
}

/// The hook must not depend on `atlas` being on PATH: git runs hooks with a
/// minimal environment, and a bare name fails the commit for the wrong reason.
#[test]
fn the_installed_hook_points_at_a_real_binary() {
    let dir = workspace();
    std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
    ok(dir.path(), &["hook", "install"]);

    let script = std::fs::read_to_string(dir.path().join(".git/hooks/pre-commit")).unwrap();
    let exe = script
        .lines()
        .find_map(|l| l.strip_prefix("exec \""))
        .and_then(|l| l.split('"').next())
        .expect("the hook should exec an absolute path");
    assert!(
        std::path::Path::new(exe).exists(),
        "hook points at {exe}, which does not exist"
    );
    assert!(script.contains("ATLAS_SKIP"), "there should be an escape hatch");

    ok(dir.path(), &["hook", "uninstall"]);
    assert!(!dir.path().join(".git/hooks/pre-commit").exists());
}

#[test]
fn commands_outside_a_workspace_explain_themselves() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(dir.path(), &["status"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("atlas init"));
}

#[test]
fn agent_identity_falls_back_to_the_registered_default() {
    let dir = workspace();
    ok(dir.path(), &["agent", "register", "solo", "--kind", "claude"]);
    // No --agent flag: the workspace default applies.
    assert!(run(dir.path(), &["lease", "acquire", "charge"])
        .status
        .success());
    let leases: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["--json", "lease", "list"])).unwrap();
    assert_eq!(leases[0]["agent"], "solo");
}
