//! Role detection: what a symbol is *for*.
//!
//! The plan asks for APIs and Tests as first-class nodes. They are not a
//! separate kind of thing, though — an endpoint is a function that happens to
//! be routed, and a test is a function that happens to assert. So roles are a
//! facet layered onto the symbols the parser already found.
//!
//! Detection is heuristic and cross-framework by design. Writing a tree-sitter
//! query per web framework per language does not scale past the first two, and
//! the signal here (a string literal beginning with `/` next to an HTTP verb)
//! is both stable and language-independent. Where a heuristic is unsure it
//! records nothing rather than guessing.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

use crate::model::Role;
use crate::parse::ParsedSymbol;

/// A detected role for one symbol in a file.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleHit {
    /// Index into the file's symbol list.
    pub symbol: usize,
    pub role: Role,
    pub meta: Value,
}

struct Patterns {
    /// `.get("/x"` — Express, Flask/FastAPI decorators, chi, Hono.
    verb_call: Regex,
    /// `.route("/x"` / `HandleFunc("/x"` — method comes from nearby text.
    route_call: Regex,
    /// Spring's `@GetMapping("/x")`.
    java_mapping: Regex,
    /// ASP.NET's `[HttpGet("x")]`.
    csharp_attr: Regex,
    /// Python's `methods=["POST"]`.
    py_methods: Regex,
    /// axum's `get(handler)` inside a `.route(...)` call.
    axum_verb: Regex,
    /// The handler named right after the path: `app.get("/x", getThing)`.
    handler_arg: Regex,
    /// Spring's `method = RequestMethod.POST`.
    spring_method: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        // Requiring the path to start with `/` is what keeps `map.get("key")`
        // out of the API list.
        verb_call: Regex::new(
            r#"(?i)\.\s*(get|post|put|patch|delete|head|options)\s*\(\s*["'`](/[^"'`]*)"#,
        )
        .expect("verb_call"),
        route_call: Regex::new(
            r#"(?i)\.\s*(?:route|handle|handlefunc|add_url_rule)\s*\(\s*["'`](/[^"'`]*)"#,
        )
        .expect("route_call"),
        java_mapping: Regex::new(
            r#"@(Get|Post|Put|Patch|Delete|Request)Mapping\s*\(\s*(?:value\s*=\s*)?["']([^"']*)"#,
        )
        .expect("java_mapping"),
        csharp_attr: Regex::new(
            r#"\[Http(Get|Post|Put|Patch|Delete)(?:\s*\(\s*["']([^"']*)["'])?"#,
        )
        .expect("csharp_attr"),
        py_methods: Regex::new(r#"(?i)methods\s*=\s*\[([^\]]*)\]"#).expect("py_methods"),
        axum_verb: Regex::new(
            r#"(?i)\b(get|post|put|patch|delete|head|options)\s*\(\s*([A-Za-z_$][\w$]*)"#,
        )
        .expect("axum_verb"),
        handler_arg: Regex::new(r#"^["'`]\s*,\s*(?:async\s+)?(?:[\w$]+\.)*([A-Za-z_$][\w$]*)"#)
            .expect("handler_arg"),
        spring_method: Regex::new(r#"RequestMethod\.(\w+)"#).expect("spring_method"),
    })
}

/// Find the HTTP endpoints declared in a file.
pub fn endpoints(source: &[u8], symbols: &[ParsedSymbol]) -> Vec<RoleHit> {
    let Ok(text) = std::str::from_utf8(source) else {
        return Vec::new();
    };
    let p = patterns();
    let mut hits: Vec<Hit> = Vec::new();

    for c in p.verb_call.captures_iter(text) {
        let m = c.get(0).expect("whole match");
        hits.push(Hit {
            byte: m.start(),
            method: c[1].to_uppercase(),
            path: c[2].to_string(),
            handler: handler_after(p, text, m.end()),
        });
    }
    for c in p.route_call.captures_iter(text) {
        let m = c.get(0).expect("whole match");
        // The verb lives after the path: `methods=["POST"]`, `get(handler)`.
        let tail_end = (m.end() + 160).min(text.len());
        let tail = text.get(m.end()..tail_end).unwrap_or("");
        let axum = p.axum_verb.captures(tail);
        let method = p
            .py_methods
            .captures(tail)
            .map(|c| {
                c[1].split(',')
                    .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .filter(|s| !s.is_empty())
            .or_else(|| axum.as_ref().map(|c| c[1].to_uppercase()))
            .unwrap_or_else(|| "ANY".to_string());
        // axum names the handler inside the verb call; Go, right after the path.
        let handler = axum
            .map(|c| c[2].to_string())
            .or_else(|| handler_after(p, text, m.end()));
        hits.push(Hit {
            byte: m.start(),
            method,
            path: c[1].to_string(),
            handler,
        });
    }
    for c in p.java_mapping.captures_iter(text) {
        let m = c.get(0).expect("whole match");
        let verb = &c[1];
        let method = if verb == "Request" {
            let tail_end = (m.end() + 160).min(text.len());
            p.spring_method
                .captures(text.get(m.end()..tail_end).unwrap_or(""))
                .map(|c| c[1].to_uppercase())
                .unwrap_or_else(|| "ANY".to_string())
        } else {
            verb.to_uppercase()
        };
        hits.push(Hit {
            byte: m.start(),
            method,
            path: c[2].to_string(),
            handler: None,
        });
    }
    for c in p.csharp_attr.captures_iter(text) {
        let m = c.get(0).expect("whole match");
        let path = c.get(2).map(|g| g.as_str()).unwrap_or("").to_string();
        hits.push(Hit {
            byte: m.start(),
            method: c[1].to_uppercase(),
            path,
            handler: None,
        });
    }

    let mut out = Vec::new();
    for hit in hits {
        // A route written in a comment is documentation, not an endpoint.
        if in_comment(text, hit.byte) {
            continue;
        }
        let Some(symbol) = attribute(&hit, symbols) else {
            continue;
        };
        out.push(RoleHit {
            symbol,
            role: Role::Api,
            meta: json!({ "method": hit.method, "path": hit.path }),
        });
    }
    out.sort_by_key(|h| h.symbol);
    out.dedup_by(|a, b| a.symbol == b.symbol && a.meta == b.meta);
    out
}

/// One route declaration found in the text, before it is tied to a symbol.
struct Hit {
    byte: usize,
    method: String,
    path: String,
    /// The handler named at the call site, if the framework names one.
    handler: Option<String>,
}

/// Is `byte` preceded on its own line by a comment marker?
///
/// Cheap, and enough: every language atlas indexes marks line comments with
/// one of these, and block-comment bodies are conventionally continued with
/// `*`. It is what stops a doc comment describing a route from being indexed
/// as one.
fn in_comment(text: &str, byte: usize) -> bool {
    let line_start = text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let before = text[line_start..byte].trim_start();
    before.starts_with("//")
        || before.starts_with('#')
        || before.starts_with('*')
        || before.starts_with("--")
        || before.starts_with("///")
}

fn handler_after(p: &Patterns, text: &str, path_end: usize) -> Option<String> {
    let tail_end = (path_end + 80).min(text.len());
    let tail = text.get(path_end..tail_end)?;
    p.handler_arg.captures(tail).map(|c| c[1].to_string())
}

/// Which symbol a route declaration belongs to.
///
/// Usually the enclosing function. Decorators and attributes sit *before* the
/// function they annotate, though, so fall back to the next symbol that starts
/// nearby.
fn attribute(hit: &Hit, symbols: &[ParsedSymbol]) -> Option<usize> {
    // `app.post("/payments", createPayment)` — the endpoint is the handler,
    // not the function that happens to register it.
    if let Some(handler) = &hit.handler {
        if let Some(i) = symbols.iter().position(|s| &s.name == handler) {
            return Some(i);
        }
    }
    let byte = hit.byte;
    let mut innermost: Option<usize> = None;
    for (i, s) in symbols.iter().enumerate() {
        if s.start_byte <= byte && byte < s.end_byte {
            match innermost {
                Some(b) if symbols[b].start_byte >= s.start_byte => {}
                _ => innermost = Some(i),
            }
        }
    }
    if let Some(i) = innermost {
        return Some(i);
    }
    // A decorator: take the next symbol, if it starts close enough that it is
    // plausibly the thing being annotated.
    symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| s.start_byte >= byte && s.start_byte - byte < 400)
        .min_by_key(|(_, s)| s.start_byte)
        .map(|(i, _)| i)
}

/// Whether a path looks like test code, by the conventions every ecosystem
/// happens to share.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/__tests__/")
        || lower.starts_with("tests/")
        || lower.starts_with("test/")
        || base.starts_with("test_")
        || base.ends_with("_test.py")
        || base.ends_with("_test.go")
        || base.ends_with("_test.rs")
        || base.ends_with("test.java")
        || base.ends_with("tests.cs")
        || [".test.", ".spec."].iter().any(|m| base.contains(m))
}

/// Tests declared in a file, whether by path convention, naming convention or
/// an annotation on the symbol itself.
pub fn tests(path: &str, source: &[u8], symbols: &[ParsedSymbol]) -> Vec<RoleHit> {
    let by_path = is_test_path(path);
    let text = std::str::from_utf8(source).unwrap_or("");
    let mut out = Vec::new();

    for (i, s) in symbols.iter().enumerate() {
        if !s.kind.is_callable() {
            continue;
        }
        let name = s.name.as_str();
        // Look back only as far as the previous symbol ends, or `#[test]` on
        // one function would tag the next one too.
        let floor = symbols
            .iter()
            .filter(|o| o.end_byte <= s.start_byte)
            .map(|o| o.end_byte)
            .max()
            .unwrap_or(0);
        let annotated = preceding(text, s.start_byte, floor);
        let framework = if annotated.contains("#[test]") || annotated.contains("#[tokio::test]") {
            Some("rust")
        } else if annotated.contains("@Test") {
            Some("junit")
        } else if annotated.contains("[Fact]") || annotated.contains("[Test]") {
            Some("xunit")
        } else if name.starts_with("test_") || name.starts_with("Test") {
            Some("naming")
        } else if by_path {
            Some("path")
        } else {
            None
        };
        if let Some(framework) = framework {
            out.push(RoleHit {
                symbol: i,
                role: Role::Test,
                meta: json!({ "framework": framework }),
            });
        }
    }
    out
}

/// The text between `floor` and `byte` — at most a couple of lines — which is
/// where decorators and attributes live.
fn preceding(text: &str, byte: usize, floor: usize) -> &str {
    let start = byte.saturating_sub(200).max(floor).min(byte);
    // Do not slice through a UTF-8 boundary.
    let start = (start..=byte).find(|i| text.is_char_boundary(*i)).unwrap_or(byte);
    text.get(start..byte.min(text.len())).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_path;

    fn detect(path: &str, src: &[u8]) -> Vec<(String, String)> {
        let pf = parse_path(path, src).unwrap();
        endpoints(src, &pf.symbols)
            .into_iter()
            .map(|h| {
                (
                    pf.symbols[h.symbol].fqn.clone(),
                    format!("{} {}", h.meta["method"].as_str().unwrap(), h.meta["path"].as_str().unwrap()),
                )
            })
            .collect()
    }

    #[test]
    fn express_routes_attach_to_their_handler() {
        let src = br#"
export function registerRoutes(app) {
  app.get('/payments/:id', getPayment);
  app.post('/payments', createPayment);
}
"#;
        let found = detect("routes.ts", src);
        assert!(found.contains(&("registerRoutes".into(), "GET /payments/:id".into())), "{found:?}");
        assert!(found.contains(&("registerRoutes".into(), "POST /payments".into())), "{found:?}");
    }

    #[test]
    fn python_decorators_attach_to_the_function_below_them() {
        let src = br#"
@app.get("/health")
def health():
    return "ok"

@app.route("/payments", methods=["POST", "PUT"])
def create_payment():
    return 1
"#;
        let found = detect("api.py", src);
        assert!(found.contains(&("health".into(), "GET /health".into())), "{found:?}");
        assert!(
            found.contains(&("create_payment".into(), "POST|PUT /payments".into())),
            "{found:?}"
        );
    }

    #[test]
    fn spring_and_aspnet_annotations_are_understood() {
        let java = br#"
class PaymentController {
  @GetMapping("/payments/{id}")
  public Payment get(String id) { return null; }
}
"#;
        let found = detect("PaymentController.java", java);
        assert!(
            found.iter().any(|(_, r)| r == "GET /payments/{id}"),
            "{found:?}"
        );

        let cs = br#"
class PaymentController {
  [HttpPost("/payments")]
  public int Create() { return 1; }
}
"#;
        let found = detect("PaymentController.cs", cs);
        assert!(found.iter().any(|(_, r)| r == "POST /payments"), "{found:?}");
    }

    #[test]
    fn axum_routes_pick_up_the_verb_from_the_handler_argument() {
        let src = br#"
pub fn router() -> Router {
    Router::new().route("/payments", get(list_payments))
}
"#;
        let found = detect("router.rs", src);
        assert!(found.iter().any(|(_, r)| r == "GET /payments"), "{found:?}");
    }

    #[test]
    fn a_route_in_a_comment_is_documentation_not_an_endpoint() {
        let src = br#"
// Handles app.get("/legacy") from the old router.
/// See also app.post("/docs-only").
export function actualHandler(req) {
  return 1;
}
export function register(app) {
  app.get('/real', actualHandler);
}
"#;
        let found = detect("routes.ts", src);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1, "GET /real");
    }

    #[test]
    fn a_python_decorator_is_not_mistaken_for_a_comment() {
        let src = b"@app.get(\"/health\")\ndef health():\n    return 1\n";
        assert_eq!(detect("api.py", src).len(), 1);
    }

    #[test]
    fn a_map_lookup_is_not_an_endpoint() {
        let src = br#"
function lookup(cache) {
  return cache.get("user-42");
}
"#;
        assert!(detect("cache.ts", src).is_empty());
    }

    #[test]
    fn tests_are_found_by_annotation_naming_and_path() {
        let rust = br#"
#[test]
fn checks_the_thing() { assert!(true); }

fn helper() {}
"#;
        let pf = parse_path("src/lib.rs", rust).unwrap();
        let hits = tests("src/lib.rs", rust, &pf.symbols);
        let named: Vec<&str> = hits.iter().map(|h| pf.symbols[h.symbol].name.as_str()).collect();
        assert_eq!(named, vec!["checks_the_thing"], "helper is not a test");

        let py = b"def test_charges():\n    assert True\n\ndef helper():\n    pass\n";
        let pf = parse_path("app.py", py).unwrap();
        let hits = tests("app.py", py, &pf.symbols);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].meta["framework"], "naming");

        // Everything callable in a test file counts.
        let pf = parse_path("tests/pay_test.go", b"package t\nfunc setup() {}\n").unwrap();
        assert_eq!(tests("tests/pay_test.go", b"package t\nfunc setup() {}\n", &pf.symbols).len(), 1);
    }

    #[test]
    fn test_paths_follow_ecosystem_conventions() {
        for p in [
            "tests/test_pay.py",
            "src/pay_test.go",
            "src/pay.test.ts",
            "src/__tests__/pay.tsx",
            "src/PayTest.java",
            "test/helpers.rb",
        ] {
            assert!(is_test_path(p), "{p} should be a test path");
        }
        for p in ["src/pay.ts", "src/latest.py", "src/contest.go"] {
            assert!(!is_test_path(p), "{p} should not be a test path");
        }
    }
}
