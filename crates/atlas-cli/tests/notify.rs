//! Proactive API-change notification through the real binary: `atlas scan
//! <path>` goes through `scan_and_notify`, so an agent whose active work
//! depends on a changed endpoint hears about it without polling.

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

/// `get_receipt` calls the routed handler `get_payment`, which is the shape
/// `impact()` can see: something in-repo depending on an endpoint.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/api.py"),
        "@app.get(\"/payments/{id}\")\n\
         def get_payment(id):\n\
         \x20   return 1\n\n\
         @app.get(\"/payments/{id}/receipt\")\n\
         def get_receipt(id):\n\
         \x20   return format(get_payment(id))\n",
    )
    .unwrap();
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

#[test]
fn changing_an_api_symbol_broadcasts_to_agents_in_its_impact_radius() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "watcher-agent", "agent", "register", "watcher-agent"]);
    ok(d, &["--agent", "watcher-agent", "lease", "acquire", "get_receipt"]);

    std::fs::write(
        d.join("src/api.py"),
        "@app.get(\"/payments/{id}\")\n\
         def get_payment(id):\n\
         \x20   return 2\n\n\
         @app.get(\"/payments/{id}/receipt\")\n\
         def get_receipt(id):\n\
         \x20   return format(get_payment(id))\n",
    )
    .unwrap();
    // A targeted rescan, same as the watcher does on a save.
    ok(d, &["scan", "src/api.py"]);

    let inbox = json(d, &["--agent", "watcher-agent", "request", "inbox"]);
    assert_eq!(inbox.as_array().unwrap().len(), 1, "{inbox:?}");
    assert_eq!(inbox[0]["kind"], "api-change");
    assert_eq!(inbox[0]["body"]["symbol"], "src/api.py:get_payment");
}

#[test]
fn a_full_scan_does_not_notify() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "watcher-agent", "agent", "register", "watcher-agent"]);
    ok(d, &["--agent", "watcher-agent", "lease", "acquire", "get_receipt"]);

    std::fs::write(
        d.join("src/api.py"),
        "@app.get(\"/payments/{id}\")\n\
         def get_payment(id):\n\
         \x20   return 2\n\n\
         @app.get(\"/payments/{id}/receipt\")\n\
         def get_receipt(id):\n\
         \x20   return format(get_payment(id))\n",
    )
    .unwrap();
    // No paths given = a full scan, which intentionally skips the diff.
    ok(d, &["scan"]);

    let inbox = json(d, &["--agent", "watcher-agent", "request", "inbox"]);
    assert!(inbox.as_array().unwrap().is_empty());
}

#[test]
fn the_watcher_daemon_notifies_on_a_live_save() {
    let dir = workspace();
    let d = dir.path().to_path_buf();
    ok(&d, &["--agent", "watcher-agent", "agent", "register", "watcher-agent"]);
    ok(&d, &["--agent", "watcher-agent", "lease", "acquire", "get_receipt"]);

    let mut child = atlas()
        .current_dir(&d)
        .args(["index", "--watch"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn watcher");

    // Give the watcher a moment to start observing the tree.
    std::thread::sleep(std::time::Duration::from_millis(600));
    std::fs::write(
        d.join("src/api.py"),
        "@app.get(\"/payments/{id}\")\n\
         def get_payment(id):\n\
         \x20   return 2\n\n\
         @app.get(\"/payments/{id}/receipt\")\n\
         def get_receipt(id):\n\
         \x20   return format(get_payment(id))\n",
    )
    .unwrap();

    // Poll for the notice rather than sleeping a fixed amount.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        let inbox = json(&d, &["--agent", "watcher-agent", "request", "inbox"]);
        if !inbox.as_array().unwrap().is_empty() {
            found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(found, "the watcher should have noticed and notified within 10s");
}

#[test]
fn a_goal_linked_api_change_opens_a_followup_task_automatically() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["goal", "add", "Support payments"]);
    ok(d, &["goal", "decompose", "G1", "--task", "wire the handler", "--symbol", "get_payment"]);

    std::fs::write(
        d.join("src/api.py"),
        "@app.get(\"/payments/{id}\")\n\
         def get_payment(id):\n\
         \x20   return 2\n\n\
         @app.get(\"/payments/{id}/receipt\")\n\
         def get_receipt(id):\n\
         \x20   return format(get_payment(id))\n",
    )
    .unwrap();
    ok(d, &["scan", "src/api.py"]);

    let show = json(d, &["goal", "show", "G1"]);
    let tasks = show["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 2, "the original task plus one auto-opened follow-up: {tasks:?}");
    assert!(
        tasks.iter().any(|t| t["title"].as_str().unwrap().contains("get_receipt")),
        "{tasks:?}"
    );
}
