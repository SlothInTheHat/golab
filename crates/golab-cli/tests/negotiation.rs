//! Phase 3 end-to-end: agents in separate processes negotiating with each
//! other. Nothing here involves a human deciding anything.

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

const SOURCE: &str = r#"
export class PaymentService {
  async processPayment(id: string) { return charge(id); }
  async refund(id: string) { return 0; }
}
export function charge(id: string) { return 1; }
"#;

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/pay.ts"), SOURCE).unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["scan"]);
    for (name, kind) in [("claude-1", "claude"), ("cursor-1", "cursor")] {
        ok(
            dir.path(),
            &["--agent", name, "agent", "register", name, "--kind", kind],
        );
    }
    dir
}

/// The headline flow: blocked agent asks, holder agrees, ownership moves —
/// and the symbol is never unowned in between.
#[test]
fn an_agent_can_ask_for_a_lease_and_be_given_it() {
    let dir = workspace();
    let d = dir.path();
    let held = json(
        d,
        &["--agent", "claude-1", "lease", "acquire", "charge", "--task", "stripe"],
    );
    let original_lease = held["lease"]["id"].as_str().unwrap().to_string();

    let req = json(
        d,
        &[
            "--agent", "cursor-1", "request", "lease", "charge",
            "--reason", "hotfix for prod", "--priority", "9",
        ],
    );
    assert_eq!(req["to"], "claude-1", "routed to the current holder");
    assert_eq!(req["state"], "open");
    assert_eq!(req["body"]["current_task"], "stripe", "the ask carries context");

    let id = req["id"].as_str().unwrap();
    // The holder sees it in an inbox, without being told where to look.
    let inbox = json(d, &["--agent", "claude-1", "request", "inbox"]);
    assert_eq!(inbox[0]["id"], id);

    let accepted = json(d, &["--agent", "claude-1", "request", "accept", id]);
    assert_eq!(accepted["state"], "fulfilled");

    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases.as_array().unwrap().len(), 1);
    assert_eq!(leases[0]["agent"], "cursor-1");
    assert_eq!(
        leases[0]["id"], original_lease,
        "the same lease changed hands; it was never released"
    );
}

/// A queued third party must not be able to steal the symbol during a handoff.
#[test]
fn a_handoff_cannot_be_sniped_by_a_waiting_agent() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "claude-1", "lease", "acquire", "charge"]);
    // A third agent is queued and polling.
    assert_eq!(
        run(d, &["--agent", "third", "lease", "acquire", "charge"])
            .status
            .code(),
        Some(1)
    );

    let req = json(d, &["--agent", "cursor-1", "request", "lease", "charge"]);
    ok(
        d,
        &["--agent", "claude-1", "request", "accept", req["id"].as_str().unwrap()],
    );

    let leases = json(d, &["lease", "list"]);
    assert_eq!(leases[0]["agent"], "cursor-1", "the requester got it, not the queue");
    assert_eq!(
        run(d, &["--agent", "third", "lease", "acquire", "charge"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn a_declined_request_leaves_ownership_alone_and_says_why() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "claude-1", "lease", "acquire", "charge"]);
    let req = json(d, &["--agent", "cursor-1", "request", "lease", "charge"]);
    let id = req["id"].as_str().unwrap();

    let declined = json(
        d,
        &[
            "--agent", "claude-1", "request", "decline", id,
            "--reason", "mid-refactor, ~90s",
        ],
    );
    assert_eq!(declined["state"], "declined");
    assert_eq!(declined["response"]["reason"], "mid-refactor, ~90s");
    assert_eq!(json(d, &["lease", "list"])[0]["agent"], "claude-1");

    // `wait` on a declined request fails fast rather than hanging.
    let out = run(d, &["--agent", "cursor-1", "request", "wait", id, "--timeout", "30"]);
    assert_eq!(out.status.code(), Some(1));
}

/// The holder never has to remember who was waiting.
#[test]
fn releasing_a_symbol_answers_the_agent_who_asked_for_it() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "claude-1", "lease", "acquire", "charge"]);
    let req = json(d, &["--agent", "cursor-1", "request", "lease", "charge"]);
    let id = req["id"].as_str().unwrap().to_string();

    ok(d, &["--agent", "claude-1", "lease", "release", "--all"]);

    let after = json(d, &["request", "show", &id]);
    assert_eq!(after["state"], "fulfilled");
    assert_eq!(after["response"]["symbol_free"], true);
    // And the waiting process exits 0 immediately.
    assert!(run(d, &["--agent", "cursor-1", "request", "wait", &id])
        .status
        .success());
}

/// `request lease --wait` blocks in one process while another answers it.
#[test]
fn waiting_and_answering_happen_in_different_processes() {
    let dir = workspace();
    let d = dir.path().to_path_buf();
    ok(&d, &["--agent", "claude-1", "lease", "acquire", "charge"]);

    let waiter = golab()
        .current_dir(&d)
        .args([
            "--json", "--agent", "cursor-1", "request", "lease", "charge",
            "--wait", "30", "--deadline", "60",
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");

    // Poll until the holder's inbox shows the ask, then answer it.
    let mut id = None;
    for _ in 0..100 {
        let inbox = json(&d, &["--agent", "claude-1", "request", "inbox"]);
        if let Some(first) = inbox.as_array().and_then(|a| a.first()) {
            id = Some(first["id"].as_str().unwrap().to_string());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let id = id.expect("the request should have reached the holder");
    ok(&d, &["--agent", "claude-1", "request", "accept", &id]);

    let out = waiter.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "the waiter should exit 0 once granted: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(json(&d, &["lease", "list"])[0]["agent"], "cursor-1");
}

#[test]
fn an_interface_request_is_accepted_then_delivered() {
    let dir = workspace();
    let d = dir.path();
    let req = json(
        d,
        &[
            "--agent", "cursor-1", "request", "interface", "PaymentProvider",
            "--to", "claude-1", "--method", "authorize", "--method", "capture",
            "--method", "refund", "--deadline", "300",
        ],
    );
    let id = req["id"].as_str().unwrap();
    assert_eq!(req["body"]["methods"][2], "refund");

    let accepted = json(d, &["--agent", "claude-1", "request", "accept", id]);
    assert_eq!(accepted["state"], "accepted", "committed, not yet delivered");

    let delivered = json(
        d,
        &[
            "--agent", "claude-1", "request", "fulfill", id,
            "--body", r#"{"version":2,"breaking_changes":false}"#,
        ],
    );
    assert_eq!(delivered["state"], "fulfilled");
    assert_eq!(delivered["response"]["version"], 2);
    assert!(run(d, &["--agent", "cursor-1", "request", "wait", id])
        .status
        .success());
}

#[test]
fn a_dependency_request_clears_when_the_task_is_done() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["task", "add", "provider interface", "--priority", "5"]);
    let req = json(
        d,
        &[
            "--agent", "cursor-1", "request", "depend", "--on-task", "T1",
            "--to", "claude-1", "--note", "need authorize()",
        ],
    );
    let id = req["id"].as_str().unwrap().to_string();
    assert_eq!(req["resource_task"], "T1");

    ok(d, &["--agent", "claude-1", "task", "done", "T1"]);
    let after = json(d, &["request", "show", &id]);
    assert_eq!(after["state"], "fulfilled");
    assert_eq!(after["response"]["task"], "T1");
}

#[test]
fn an_unanswered_request_expires_instead_of_blocking_forever() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "claude-1", "lease", "acquire", "charge"]);
    let req = json(
        d,
        &[
            "--agent", "cursor-1", "request", "lease", "charge", "--deadline", "1",
        ],
    );
    let id = req["id"].as_str().unwrap().to_string();

    // The waiter gives up when the deadline lapses, exit 1, no hang.
    let out = run(d, &["--agent", "cursor-1", "request", "wait", &id, "--timeout", "20"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json(d, &["request", "show", &id])["state"], "expired");
    // Too late to answer.
    assert_eq!(
        run(d, &["--agent", "claude-1", "request", "accept", &id])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn transfer_is_available_directly_without_a_request() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "claude-1", "lease", "acquire", "charge"]);
    let moved = json(
        d,
        &["--agent", "claude-1", "lease", "transfer", "charge", "--to", "cursor-1"],
    );
    assert_eq!(moved["agent"], "cursor-1");
    // Only the holder may give it away.
    assert_eq!(
        run(d, &["--agent", "claude-1", "lease", "transfer", "charge", "--to", "third"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn broadcast_requests_reach_every_other_agent() {
    let dir = workspace();
    let d = dir.path();
    let req = json(
        d,
        &[
            "--agent", "cursor-1", "request", "open",
            "--subject", "who owns the ledger schema?",
            "--body", r#"{"area":"ledger"}"#,
        ],
    );
    let id = req["id"].as_str().unwrap();
    assert!(req["to"].is_null());

    assert_eq!(
        json(d, &["--agent", "claude-1", "request", "inbox"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        json(d, &["--agent", "cursor-1", "request", "inbox"])
            .as_array()
            .unwrap()
            .is_empty(),
        "your own broadcast is not your inbox"
    );
    ok(
        d,
        &["--agent", "claude-1", "request", "fulfill", id, "--body", r#"{"owner":"claude-1"}"#],
    );
    assert_eq!(json(d, &["request", "show", id])["response"]["owner"], "claude-1");
}

#[test]
fn progress_is_visible_to_everyone_and_keeps_the_lease_alive() {
    let dir = workspace();
    let d = dir.path();
    ok(
        d,
        &["--agent", "claude-1", "lease", "acquire", "charge", "--ttl", "60"],
    );
    let before = json(d, &["lease", "list"])[0]["expires_at"].as_i64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));

    ok(
        d,
        &[
            "--agent", "claude-1", "progress", "--percent", "60",
            "--note", "authorize() done", "--eta", "120", "--symbol", "charge",
        ],
    );
    let status = json(d, &["status"]);
    assert_eq!(status["progress"][0]["agent"], "claude-1");
    assert_eq!(status["progress"][0]["percent"], 60);
    assert_eq!(status["progress"][0]["symbol_handle"], "src/pay.ts:charge");

    let after = json(d, &["lease", "list"])[0]["expires_at"].as_i64().unwrap();
    assert!(after > before, "reporting progress is proof of life");
}

/// The whole point of Phase 3: a blocked agent resolves its own blockage.
#[test]
fn a_scripted_agent_loop_negotiates_without_a_human() {
    let dir = workspace();
    let d = dir.path();

    // claude-1 is working on the symbol cursor-1 needs.
    ok(
        d,
        &["--agent", "claude-1", "lease", "acquire", "charge", "--task", "stripe"],
    );

    // cursor-1's loop: try to take it, discover it is held, ask for it.
    let denied = run(d, &["--agent", "cursor-1", "lease", "acquire", "charge"]);
    assert_eq!(denied.status.code(), Some(1));
    let req = json(
        d,
        &["--agent", "cursor-1", "request", "lease", "charge", "--reason", "blocked"],
    );
    let id = req["id"].as_str().unwrap().to_string();

    // claude-1's loop: drain the inbox and answer every ask.
    let inbox = json(d, &["--agent", "claude-1", "request", "inbox"]);
    for r in inbox.as_array().unwrap() {
        ok(
            d,
            &[
                "--agent", "claude-1", "request", "accept",
                r["id"].as_str().unwrap(),
            ],
        );
    }

    // cursor-1 now owns it and can legally edit it.
    assert!(run(d, &["--agent", "cursor-1", "request", "wait", &id])
        .status
        .success());
    std::fs::write(
        d.join("src/pay.ts"),
        SOURCE.replace("return 1;", "return 2;"),
    )
    .unwrap();
    assert!(
        run(d, &["--agent", "cursor-1", "check"]).status.success(),
        "the new owner may edit what it now holds"
    );
    assert_eq!(
        run(d, &["--agent", "claude-1", "check"]).status.code(),
        Some(1),
        "and the old owner may not"
    );
}
