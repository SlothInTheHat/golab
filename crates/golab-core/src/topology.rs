//! Services and ownership: the two things about a repository that are true
//! above the level of any one file.
//!
//! **Services** come from manifests. A repository with three `Cargo.toml`s or
//! a `packages/*/package.json` is three deployable units, and agents should be
//! able to reason — and take leases — at that granularity.
//!
//! **Ownership** comes from CODEOWNERS. It is modelled as an attribute of a
//! path rather than as edges to person-nodes: people are not code, and making
//! them graph nodes would make every traversal walk through them.

use std::collections::BTreeMap;
use std::path::Path;

use globset::{Glob, GlobMatcher};

/// A deployable or publishable unit discovered from a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    /// Directory containing the manifest, repo-relative. Empty means the root.
    pub dir: String,
    /// The manifest that declared it, repo-relative.
    pub manifest: String,
    pub ecosystem: String,
}

const MANIFESTS: &[(&str, &str)] = &[
    ("Cargo.toml", "cargo"),
    ("package.json", "npm"),
    ("go.mod", "go"),
    ("pyproject.toml", "python"),
    ("build.gradle", "gradle"),
    ("pom.xml", "maven"),
];

/// Is this file a manifest the indexer should read?
pub fn is_manifest(path: &str) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    MANIFESTS.iter().any(|(name, _)| *name == base)
}

/// Read a manifest into a service, if it names one.
///
/// A Cargo workspace root with no `[package]` is not a service — it is a
/// container for them — so it yields `None` rather than a phantom node.
pub fn read_manifest(root: &Path, rel_path: &str) -> Option<Service> {
    let base = rel_path.rsplit('/').next()?;
    let ecosystem = MANIFESTS.iter().find(|(n, _)| *n == base).map(|(_, e)| *e)?;
    let text = std::fs::read_to_string(root.join(rel_path)).ok()?;
    let dir = match rel_path.rfind('/') {
        Some(i) => rel_path[..i].to_string(),
        None => String::new(),
    };

    let name = match ecosystem {
        "npm" => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("name")?.as_str().map(|s| s.to_string())),
        "go" => text
            .lines()
            .find_map(|l| l.trim().strip_prefix("module ").map(|m| m.trim().to_string())),
        "maven" => between(&text, "<artifactId>", "</artifactId>"),
        "gradle" => Some(dir_name(&dir)),
        // Cargo and pyproject are both TOML with the name in a known table.
        _ => toml_name(&text, if ecosystem == "cargo" { "package" } else { "project" })
            .or_else(|| toml_name(&text, "tool.poetry")),
    }?;

    let name = name.trim().trim_matches('"').to_string();
    if name.is_empty() {
        return None;
    }
    // Go modules are full URLs; the last segment is the useful name.
    let name = if ecosystem == "go" {
        name.rsplit('/').next().unwrap_or(&name).to_string()
    } else {
        name
    };

    Some(Service {
        name,
        dir,
        manifest: rel_path.to_string(),
        ecosystem: ecosystem.to_string(),
    })
}

/// `name = "x"` inside `[table]`, without pulling in a TOML parser for two
/// fields. Stops at the next table header so `[dependencies] name = ...`
/// cannot be mistaken for the package name.
fn toml_name(text: &str, table: &str) -> Option<String> {
    let mut in_table = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_table = line.trim_start_matches('[').trim_end_matches(']') == table;
            continue;
        }
        if !in_table {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn between(text: &str, open: &str, close: &str) -> Option<String> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(text[start..end].to_string())
}

fn dir_name(dir: &str) -> String {
    if dir.is_empty() {
        "root".to_string()
    } else {
        dir.rsplit('/').next().unwrap_or(dir).to_string()
    }
}

/// Given every service in the repo, the one that owns `path`: the deepest
/// manifest directory that contains it.
pub fn service_for<'a>(path: &str, services: &'a [Service]) -> Option<&'a Service> {
    services
        .iter()
        .filter(|s| s.dir.is_empty() || path.starts_with(&format!("{}/", s.dir)))
        .max_by_key(|s| s.dir.len())
}

/// The nearest enclosing service of another service, for service-level
/// dependency edges (a crate inside a workspace).
pub fn parent_service<'a>(child: &Service, services: &'a [Service]) -> Option<&'a Service> {
    services
        .iter()
        .filter(|s| s.dir != child.dir)
        .filter(|s| s.dir.is_empty() || child.dir.starts_with(&format!("{}/", s.dir)))
        .max_by_key(|s| s.dir.len())
}

// ------------------------------------------------------------------ ownership

/// One CODEOWNERS rule.
pub struct OwnerRule {
    pub pattern: String,
    pub owners: Vec<String>,
    matcher: GlobMatcher,
}

/// Parsed CODEOWNERS. Last matching rule wins, as GitHub defines it.
#[derive(Default)]
pub struct Owners {
    rules: Vec<OwnerRule>,
    pub source: Option<String>,
}

const CODEOWNERS_LOCATIONS: &[&str] = &["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"];

impl Owners {
    /// Load whichever CODEOWNERS file the repository uses, if any.
    pub fn load(root: &Path) -> Owners {
        for loc in CODEOWNERS_LOCATIONS {
            if let Ok(text) = std::fs::read_to_string(root.join(loc)) {
                let mut owners = Owners::parse(&text);
                owners.source = Some((*loc).to_string());
                return owners;
            }
        }
        Owners::default()
    }

    pub fn parse(text: &str) -> Owners {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let Some(pattern) = parts.next() else { continue };
            let owners: Vec<String> = parts.map(|s| s.to_string()).collect();
            if owners.is_empty() {
                continue;
            }
            if let Some(matcher) = compile(pattern) {
                rules.push(OwnerRule {
                    pattern: pattern.to_string(),
                    owners,
                    matcher,
                });
            }
        }
        Owners {
            rules,
            source: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Owners of a path. Later rules win, matching GitHub's semantics.
    pub fn owners_of(&self, path: &str) -> Vec<String> {
        self.rules
            .iter()
            .rev()
            .find(|r| r.matcher.is_match(path))
            .map(|r| r.owners.clone())
            .unwrap_or_default()
    }

    /// Every rule, in file order — useful for showing why a path is owned.
    pub fn matching_rule(&self, path: &str) -> Option<&OwnerRule> {
        self.rules.iter().rev().find(|r| r.matcher.is_match(path))
    }

    /// Owner -> the patterns that grant it, for a repo-wide summary.
    pub fn by_owner(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for r in &self.rules {
            for o in &r.owners {
                out.entry(o.clone()).or_default().push(r.pattern.clone());
            }
        }
        out
    }
}

/// CODEOWNERS patterns are gitignore-ish; translate to a glob that matches
/// repo-relative paths.
fn compile(pattern: &str) -> Option<GlobMatcher> {
    let p = pattern.trim_start_matches('/');
    let candidates = if p == "*" {
        vec!["**".to_string()]
    } else if p.ends_with('/') {
        vec![format!("{}**", p)]
    } else if p.contains('*') || p.contains('?') {
        // `*.rs` should match at any depth, as it does in gitignore.
        if p.contains('/') {
            vec![p.to_string()]
        } else {
            vec![p.to_string(), format!("**/{p}")]
        }
    } else {
        // A bare path matches the file itself or everything under it.
        vec![p.to_string(), format!("{p}/**")]
    };
    let joined = format!("{{{}}}", candidates.join(","));
    Glob::new(&joined).ok().map(|g| g.compile_matcher())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, path: &str, body: &str) {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, body).unwrap();
    }

    #[test]
    fn manifests_name_their_service() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "Cargo.toml", "[package]\nname = \"golab-core\"\nversion = \"0.1.0\"\n");
        write(d.path(), "web/package.json", r#"{"name":"@acme/web","version":"1.0.0"}"#);
        write(d.path(), "svc/go.mod", "module github.com/acme/svc\n\ngo 1.22\n");
        write(d.path(), "py/pyproject.toml", "[project]\nname = \"ledger\"\n");

        assert_eq!(read_manifest(d.path(), "Cargo.toml").unwrap().name, "golab-core");
        assert_eq!(
            read_manifest(d.path(), "web/package.json").unwrap().name,
            "@acme/web"
        );
        let go = read_manifest(d.path(), "svc/go.mod").unwrap();
        assert_eq!(go.name, "svc", "the last path segment is the useful name");
        assert_eq!(go.dir, "svc");
        assert_eq!(read_manifest(d.path(), "py/pyproject.toml").unwrap().name, "ledger");
    }

    #[test]
    fn a_workspace_root_without_a_package_is_not_a_service() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/a\"]\n\n[workspace.dependencies]\nname = \"nope\"\n",
        );
        assert!(read_manifest(d.path(), "Cargo.toml").is_none());
    }

    #[test]
    fn files_belong_to_the_deepest_enclosing_service() {
        let services = vec![
            Service { name: "workspace".into(), dir: "".into(), manifest: "Cargo.toml".into(), ecosystem: "cargo".into() },
            Service { name: "core".into(), dir: "crates/core".into(), manifest: "crates/core/Cargo.toml".into(), ecosystem: "cargo".into() },
        ];
        assert_eq!(
            service_for("crates/core/src/lib.rs", &services).unwrap().name,
            "core"
        );
        assert_eq!(service_for("README.md", &services).unwrap().name, "workspace");
        assert_eq!(
            parent_service(&services[1], &services).unwrap().name,
            "workspace"
        );
        assert!(parent_service(&services[0], &services).is_none());
    }

    #[test]
    fn codeowners_rules_resolve_with_last_match_winning() {
        let owners = Owners::parse(
            "# comment\n\
             *                @acme/platform\n\
             /src/payments/   @acme/payments @alice\n\
             *.sql            @acme/data\n",
        );
        assert_eq!(owners.owners_of("README.md"), vec!["@acme/platform"]);
        assert_eq!(
            owners.owners_of("src/payments/charge.ts"),
            vec!["@acme/payments", "@alice"]
        );
        // The later `*.sql` rule wins over the earlier directory rule.
        assert_eq!(owners.owners_of("src/payments/schema.sql"), vec!["@acme/data"]);
        assert_eq!(owners.owners_of("db/001.sql"), vec!["@acme/data"]);
        assert_eq!(
            owners.matching_rule("db/001.sql").map(|r| r.pattern.as_str()),
            Some("*.sql")
        );
    }

    #[test]
    fn ownership_summarises_by_owner() {
        let owners = Owners::parse("/api/ @backend\n/web/ @frontend\n*.sql @backend\n");
        let by = owners.by_owner();
        assert_eq!(by["@backend"], vec!["/api/", "*.sql"]);
        assert_eq!(by["@frontend"], vec!["/web/"]);
    }

    #[test]
    fn an_absent_codeowners_file_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let owners = Owners::load(d.path());
        assert!(owners.is_empty());
        assert!(owners.owners_of("anything").is_empty());
    }
}
