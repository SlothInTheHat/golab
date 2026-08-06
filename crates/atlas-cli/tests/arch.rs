//! The architecture picture, through the real binary.
//!
//! The unit tests in `arch.rs` pin how the graph is derived. What matters here
//! is that the same picture reaches a terminal and a script — the rule that a
//! `Store` method is not finished until both the CLI and `--json` can see it.

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

/// Two services, one importing the other, one of them touching a table.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let w = |rel: &str, body: &str| {
        let p = dir.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    w("api/package.json", r#"{"name":"api","version":"1.0.0"}"#);
    w(
        "api/src/routes.ts",
        "import { record } from '../../lib/src/ledger';\n\
         export function registerRoutes(app) { app.post('/payments', createPayment); }\n\
         export function createPayment(req) {\n\
           db.query('INSERT INTO payments (amount) VALUES ($1)', [req.amount]);\n\
           return record(req.amount);\n\
         }\n",
    );
    w("lib/package.json", r#"{"name":"lib","version":"1.0.0"}"#);
    w(
        "lib/src/ledger.ts",
        "export function record(x) { return x; }\n",
    );
    w("db/schema.sql", "CREATE TABLE payments (id INTEGER);\n");

    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

fn node_id(g: &Value, name: &str) -> String {
    g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == name)
        .unwrap_or_else(|| panic!("no node named {name} in {g}"))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn the_picture_is_services_and_the_arrows_between_them() {
    let dir = workspace();
    let d = dir.path();

    let text = ok(d, &["arch"]);
    assert!(text.contains("api"), "{text}");
    assert!(text.contains("lib"), "{text}");
    assert!(
        text.contains("payments"),
        "the database is a box people point at: {text}"
    );

    let g = cli_json(d, &["arch"]);
    let api = node_id(&g, "api");
    let lib = node_id(&g, "lib");
    let deps: Vec<&Value> = g["edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "imports")
        .collect();
    assert_eq!(deps.len(), 1, "one arrow, not one per import: {deps:?}");
    assert_eq!(deps[0]["from"], api);
    assert_eq!(deps[0]["to"], lib);
}

#[test]
fn a_node_carries_who_is_inside_it_right_now() {
    let dir = workspace();
    let d = dir.path();
    ok(d, &["--agent", "alice", "swarm", "join", "alice"]);
    ok(d, &["--agent", "alice", "lease", "acquire", "record"]);

    let g = cli_json(d, &["arch"]);
    let lib = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "lib")
        .unwrap()
        .clone();
    assert_eq!(lib["workers"], serde_json::json!(["alice"]));
    assert_eq!(lib["leases"], 1);

    // And the terminal says so too, not just the JSON.
    let text = ok(d, &["arch"]);
    assert!(
        text.lines().any(|l| l.contains("lib") && l.contains('●')),
        "an occupied box has to read differently at a glance: {text}"
    );
}

#[test]
fn drilling_into_a_node_names_its_neighbours_rather_than_their_ids() {
    let dir = workspace();
    let d = dir.path();
    let g = cli_json(d, &["arch"]);
    let api = node_id(&g, "api");

    let text = ok(d, &["arch", "--node", &api]);
    assert!(
        text.contains("lib"),
        "an id is not an answer to 'what does this depend on': {text}"
    );
    assert!(!text.contains("s_"), "raw symbol ids should not leak: {text}");

    let detail = cli_json(d, &["arch", "--node", &api]);
    assert_eq!(detail["depends_on"][0]["name"], "lib");
    assert_eq!(detail["tables"][0]["name"], "payments");
    assert!(
        detail["routes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "createPayment"),
        "and what it exposes: {detail}"
    );
}

#[test]
fn depth_changes_how_much_of_the_repository_is_shown() {
    let dir = workspace();
    let d = dir.path();

    let shallow = cli_json(d, &["arch"]);
    let deep = cli_json(d, &["arch", "--depth", "3"]);
    assert!(
        deep["nodes"].as_array().unwrap().len() > shallow["nodes"].as_array().unwrap().len()
    );
    assert!(deep["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["kind"] == "file"));
}

#[test]
fn directory_names_are_qualified_so_five_src_boxes_are_distinguishable() {
    let dir = workspace();
    let d = dir.path();
    let g = cli_json(d, &["arch", "--depth", "2"]);

    let dirs: Vec<String> = g["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["kind"] == "dir")
        .map(|n| n["name"].as_str().unwrap().to_string())
        .collect();
    assert!(dirs.contains(&"api/src".to_string()), "{dirs:?}");
    assert!(dirs.contains(&"lib/src".to_string()), "{dirs:?}");
}

#[test]
fn an_unknown_node_fails_loudly_rather_than_printing_an_empty_panel() {
    let dir = workspace();
    let out = run(dir.path(), &["arch", "--node", "dir:R1:nowhere"]);
    assert_eq!(out.status.code(), Some(2), "an error, not a legitimate no");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such node"));
}
