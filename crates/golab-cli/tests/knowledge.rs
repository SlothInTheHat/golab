//! Phase 4 end-to-end: the repository knowledge graph.
//!
//! These drive the real binary against a small multi-service repository with
//! an HTTP API, a database schema, tests and CODEOWNERS — the shapes the graph
//! is supposed to model.

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

const FILES: &[(&str, &str)] = &[
    ("api/package.json", r#"{"name":"payments-api"}"#),
    ("lib/package.json", r#"{"name":"ledger-lib"}"#),
    (
        "db/schema.sql",
        "CREATE TABLE payments (id UUID PRIMARY KEY, amount INTEGER);\n\
         CREATE TABLE audit_log (id UUID);\n",
    ),
    (
        "lib/src/ledger.ts",
        "export function record(id: string, amount: number) {\n\
         \x20 return db.query(\"INSERT INTO payments (id, amount) VALUES ($1, $2)\", [id, amount]);\n\
         }\n",
    ),
    (
        "api/src/routes.ts",
        "import { record } from '../../lib/src/ledger';\n\n\
         export function registerRoutes(app) {\n\
         \x20 app.post('/payments', createPayment);\n\
         \x20 app.get('/payments/:id', getPayment);\n\
         }\n\n\
         export function createPayment(req) {\n\
         \x20 return record(req.id, req.amount);\n\
         }\n\n\
         export function getPayment(req) {\n\
         \x20 return db.query(\"SELECT * FROM payments WHERE id = $1\", [req.id]);\n\
         }\n",
    ),
    (
        "api/tests/routes.test.ts",
        "import { createPayment } from '../src/routes';\n\n\
         export function testCreatePayment() {\n\
         \x20 return createPayment({ id: '1', amount: 10 });\n\
         }\n",
    ),
    (
        "CODEOWNERS",
        "*         @acme/platform\n/api/     @acme/api-team\n*.sql     @acme/data\n",
    ),
];

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in FILES {
        let p = dir.path().join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    ok(dir.path(), &["init"]);
    ok(dir.path(), &["index"]);
    dir
}

#[test]
fn manifests_become_services_with_dependencies() {
    let dir = workspace();
    let services = json(dir.path(), &["services"]);
    let names: Vec<&str> = services
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["service"]["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"payments-api"), "{names:?}");
    assert!(names.contains(&"ledger-lib"), "{names:?}");

    let api = services
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"]["name"] == "payments-api")
        .unwrap();
    assert_eq!(
        api["depends_on"].as_array().unwrap(),
        &vec![serde_json::json!("ledger-lib")],
        "a cross-service import is a service dependency"
    );
}

#[test]
fn endpoints_are_discovered_and_attributed_to_their_handlers() {
    let dir = workspace();
    let api = json(dir.path(), &["api"]);
    let routes: Vec<(String, String)> = api
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                format!(
                    "{} {}",
                    e["meta"]["method"].as_str().unwrap(),
                    e["meta"]["path"].as_str().unwrap()
                ),
                e["name"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    assert!(
        routes.contains(&("POST /payments".into(), "createPayment".into())),
        "{routes:?}"
    );
    assert!(
        routes.contains(&("GET /payments/:id".into(), "getPayment".into())),
        "{routes:?}"
    );
    assert!(
        !routes.iter().any(|(_, name)| name == "registerRoutes"),
        "the handler is the endpoint, not the function that registers it"
    );
}

#[test]
fn tables_know_which_code_touches_them() {
    let dir = workspace();
    let tables = json(dir.path(), &["tables"]);
    let payments = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"]["name"] == "payments")
        .expect("payments table");

    let accessors: Vec<&str> = payments["accessed_by"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "queries")
        .map(|a| a["symbol"]["name"].as_str().unwrap())
        .collect();
    assert!(accessors.contains(&"record"), "{accessors:?}");
    assert!(accessors.contains(&"getPayment"), "{accessors:?}");

    let audit = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"]["name"] == "audit_log")
        .expect("audit_log table");
    assert!(
        audit["accessed_by"].as_array().unwrap().is_empty(),
        "an unused table is worth knowing about too"
    );
}

#[test]
fn test_coverage_is_a_query_and_gaps_are_detectable() {
    let dir = workspace();
    let covered = json(dir.path(), &["tests", "createPayment"]);
    let names: Vec<&str> = covered["tests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["symbol"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["testCreatePayment"]);

    // Nothing reaches getPayment, and the exit code says so — a CI gate.
    let out = run(dir.path(), &["tests", "getPayment"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn imports_resolve_calls_that_a_name_alone_could_not() {
    let dir = workspace();
    // `record` is defined in lib and called from api; the edge must cross.
    let graph = json(dir.path(), &["graph", "record", "--depth", "3"]);
    let reached: Vec<&str> = graph["impact"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["symbol"]["name"].as_str().unwrap())
        .collect();
    assert!(reached.contains(&"createPayment"), "{reached:?}");
    assert!(
        reached.contains(&"testCreatePayment"),
        "impact reaches the test two hops out: {reached:?}"
    );
}

#[test]
fn ownership_combines_codeowners_with_live_leases() {
    let dir = workspace();
    let owners = json(dir.path(), &["owners", "api/src/routes.ts"]);
    assert_eq!(owners["codeowners"][0], "@acme/api-team");
    assert_eq!(owners["rule"], "/api/");
    assert!(owners["leases"].as_array().unwrap().is_empty());

    // The most specific *last* rule wins, as GitHub defines it.
    let sql = json(dir.path(), &["owners", "db/schema.sql"]);
    assert_eq!(sql["codeowners"][0], "@acme/data");

    // A symbol reference works too, and a live lease shows up beside the file.
    ok(
        dir.path(),
        &["--agent", "claude-1", "lease", "acquire", "createPayment"],
    );
    let owners = json(dir.path(), &["owners", "createPayment"]);
    assert_eq!(owners["path"], "api/src/routes.ts");
    assert_eq!(owners["leases"][0]["agent"], "claude-1");
}

#[test]
fn a_service_lease_covers_every_file_inside_it() {
    let dir = workspace();
    ok(
        dir.path(),
        &["--agent", "claude-1", "lease", "acquire", "service:payments-api"],
    );
    // Nobody else can take anything within that service.
    assert_eq!(
        run(
            dir.path(),
            &["--agent", "cursor-1", "lease", "acquire", "createPayment"]
        )
        .status
        .code(),
        Some(1)
    );
    // But the other service is untouched.
    assert!(run(
        dir.path(),
        &["--agent", "cursor-1", "lease", "acquire", "record"]
    )
    .status
    .success());
}

#[test]
fn role_filters_narrow_the_symbol_list() {
    let dir = workspace();
    let apis = json(dir.path(), &["symbols", "--role", "api"]);
    assert_eq!(apis.as_array().unwrap().len(), 2);

    let schema = json(dir.path(), &["symbols", "--role", "schema"]);
    assert!(
        schema.as_array().unwrap().len() >= 2,
        "tables and the schema file both carry the role"
    );

    let bogus = run(dir.path(), &["symbols", "--role", "nonsense"]);
    assert_eq!(bogus.status.code(), Some(2));
}

#[test]
fn status_reports_the_shape_of_the_repository() {
    let dir = workspace();
    let status = json(dir.path(), &["status"]);
    let k = &status["knowledge"];
    assert_eq!(k["services"], 2);
    assert_eq!(k["endpoints"], 2);
    assert_eq!(k["tables"], 2);
    assert!(k["tests"].as_i64().unwrap() >= 1);
    let routes: Vec<&str> = k["routes"].as_array().unwrap().iter().map(|r| r.as_str().unwrap()).collect();
    assert!(routes.contains(&"POST /payments"), "{routes:?}");
}

/// Editing code should update the graph, not just the symbol list.
#[test]
fn reindexing_updates_roles_and_edges() {
    let dir = workspace();
    assert_eq!(json(dir.path(), &["api"]).as_array().unwrap().len(), 2);

    std::fs::write(
        dir.path().join("api/src/routes.ts"),
        "import { record } from '../../lib/src/ledger';\n\n\
         export function registerRoutes(app) {\n\
         \x20 app.post('/payments', createPayment);\n\
         }\n\n\
         export function createPayment(req) {\n\
         \x20 return record(req.id, req.amount);\n\
         }\n",
    )
    .unwrap();
    ok(dir.path(), &["index"]);

    let api = json(dir.path(), &["api"]);
    assert_eq!(
        api.as_array().unwrap().len(),
        1,
        "the deleted route is gone from the graph"
    );
    // And the table accessor that went with it.
    let tables = json(dir.path(), &["tables"]);
    let payments = tables
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["table"]["name"] == "payments")
        .unwrap();
    let accessors: Vec<&str> = payments["accessed_by"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|a| a["kind"] == "queries")
        .map(|a| a["symbol"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(accessors, vec!["record"], "getPayment no longer exists");
}
