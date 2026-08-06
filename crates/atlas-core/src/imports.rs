//! Import extraction and module resolution.
//!
//! Imports are what turn a pile of per-file symbol lists into a graph. They
//! also fix the thing the Phase 0 indexer had to punt on: when two files both
//! define `charge()`, a call to `charge()` should resolve to the one the
//! calling file actually imports, not to "give up, ambiguous".
//!
//! Resolution is deliberately repo-local and best effort. An import of
//! `react` or `github.com/aws/aws-sdk-go` resolves to nothing, which is
//! correct — those are not nodes in *this* repository's graph.

use std::collections::{HashMap, HashSet};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, QueryCursor};

use crate::lang::{self, LangSpec};

/// Every source file in the workspace, indexed for the lookups resolution
/// needs: exact path, and basename for languages that import by type name.
pub struct FileIndex {
    paths: HashSet<String>,
    by_basename: HashMap<String, Vec<String>>,
    /// Directory path -> files directly inside it. Used for Go packages.
    by_dir: HashMap<String, Vec<String>>,
}

impl FileIndex {
    pub fn new<I: IntoIterator<Item = String>>(files: I) -> FileIndex {
        let mut paths = HashSet::new();
        let mut by_basename: HashMap<String, Vec<String>> = HashMap::new();
        let mut by_dir: HashMap<String, Vec<String>> = HashMap::new();
        for f in files {
            if let Some(base) = f.rsplit('/').next() {
                by_basename.entry(base.to_string()).or_default().push(f.clone());
            }
            let dir = match f.rfind('/') {
                Some(i) => f[..i].to_string(),
                None => String::new(),
            };
            by_dir.entry(dir).or_default().push(f.clone());
            paths.insert(f);
        }
        FileIndex {
            paths,
            by_basename,
            by_dir,
        }
    }

    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    fn basename(&self, name: &str) -> &[String] {
        self.by_basename.get(name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Files in the first directory whose path ends with `suffix`.
    fn dir_suffix(&self, suffix: &str, ext: &str) -> Vec<String> {
        let mut hits: Vec<&String> = self
            .by_dir
            .keys()
            .filter(|d| *d == suffix || d.ends_with(&format!("/{suffix}")))
            .collect();
        hits.sort_by_key(|d| d.len());
        match hits.first() {
            Some(dir) => self.by_dir[*dir]
                .iter()
                .filter(|f| f.ends_with(ext))
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }
}

/// A raw import specifier as written in the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawImport {
    pub specifier: String,
    pub byte: usize,
}

/// Pull every import specifier out of a file.
pub fn extract(spec: &'static LangSpec, source: &[u8]) -> Vec<RawImport> {
    let Some(query) = spec.imports_query() else {
        return Vec::new();
    };
    let mut parser = Parser::new();
    if parser.set_language(&spec.language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(source, None) else {
        return Vec::new();
    };

    let names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut module: Option<(String, usize)> = None;
        let mut callee: Option<String> = None;
        for cap in m.captures {
            let text = cap.node.utf8_text(source).unwrap_or("").to_string();
            match names[cap.index as usize] {
                "module" => module = Some((text, cap.node.start_byte())),
                "callee" => callee = Some(text),
                _ => {}
            }
        }
        // The dynamic-import pattern matches every one-string call; only
        // `require(...)` and `import(...)` are actually imports.
        if let Some(c) = &callee {
            if c != "require" && c != "import" {
                continue;
            }
        }
        if let Some((raw, byte)) = module {
            let specifier = unquote(&raw);
            if !specifier.is_empty() {
                out.push(RawImport { specifier, byte });
            }
        }
    }
    out.sort_by_key(|i| i.byte);
    out.dedup_by(|a, b| a.specifier == b.specifier);
    out
}

fn unquote(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '<' || c == '>')
        .trim()
        .to_string()
}

/// Resolve one import specifier to the repo files it refers to.
///
/// Returns empty for third-party or standard-library imports — they are real,
/// but they are not nodes in this repository.
pub fn resolve(
    lang: &str,
    from_file: &str,
    specifier: &str,
    index: &FileIndex,
) -> Vec<String> {
    let dir = parent_dir(from_file);
    match lang {
        "typescript" | "tsx" | "javascript" => resolve_js(&dir, specifier, index),
        "python" => resolve_python(from_file, specifier, index),
        "rust" => resolve_rust(from_file, specifier, index),
        "go" => resolve_go(specifier, index),
        "java" => resolve_by_type_name(specifier, "java", index),
        "csharp" => resolve_by_type_name(specifier, "cs", index),
        "cpp" => resolve_include(&dir, specifier, index),
        _ => Vec::new(),
    }
}

fn resolve_js(dir: &str, specifier: &str, index: &FileIndex) -> Vec<String> {
    // Bare specifiers are packages, except for a few workspace conventions.
    if !specifier.starts_with('.') && !specifier.starts_with('/') && !specifier.starts_with('~') {
        return Vec::new();
    }
    let base = normalize(&join(dir, specifier.trim_start_matches('~').trim_start_matches('/')));
    let exts = ["ts", "tsx", "js", "jsx", "mjs", "cjs"];
    let mut candidates = vec![base.clone()];
    // `./pay` -> pay.ts; `./pay.js` in TS often means pay.ts on disk.
    for e in exts {
        candidates.push(format!("{base}.{e}"));
        candidates.push(format!("{base}/index.{e}"));
    }
    if let Some(stem) = base.strip_suffix(".js") {
        for e in ["ts", "tsx"] {
            candidates.push(format!("{stem}.{e}"));
        }
    }
    first_existing(&candidates, index)
}

fn resolve_python(from_file: &str, specifier: &str, index: &FileIndex) -> Vec<String> {
    let (mut dir, rest) = if specifier.starts_with('.') {
        // `.mod` is a sibling, `..pkg.mod` climbs one package per extra dot.
        let dots = specifier.chars().take_while(|c| *c == '.').count();
        let mut d = parent_dir(from_file);
        for _ in 1..dots {
            d = parent_dir(&d);
        }
        (d, specifier[dots..].to_string())
    } else {
        (String::new(), specifier.to_string())
    };
    if rest.is_empty() {
        return Vec::new();
    }
    let rel = rest.replace('.', "/");

    let mut candidates = Vec::new();
    let mut push = |d: &str| {
        let base = if d.is_empty() {
            rel.clone()
        } else {
            format!("{d}/{rel}")
        };
        candidates.push(format!("{base}.py"));
        candidates.push(format!("{base}/__init__.py"));
    };
    push(&dir);
    // Absolute imports are relative to some source root; try each ancestor.
    if !specifier.starts_with('.') {
        dir = parent_dir(from_file);
        loop {
            push(&dir);
            if dir.is_empty() {
                break;
            }
            dir = parent_dir(&dir);
        }
    }
    first_existing(&candidates, index)
}

fn resolve_rust(from_file: &str, specifier: &str, index: &FileIndex) -> Vec<String> {
    let dir = parent_dir(from_file);
    // `mod foo;` — a sibling file or directory module.
    if !specifier.contains("::") {
        return first_existing(
            &[
                join(&dir, &format!("{specifier}.rs")),
                join(&dir, &format!("{specifier}/mod.rs")),
                // `src/lib.rs` declaring `mod foo;` where foo lives beside it.
                join(&stem_dir(from_file), &format!("{specifier}.rs")),
                join(&stem_dir(from_file), &format!("{specifier}/mod.rs")),
            ],
            index,
        );
    }
    let path = specifier.split("::").collect::<Vec<_>>();
    let (root, rest) = match path.first().copied() {
        Some("crate") => (crate_root(from_file), &path[1..]),
        Some("self") => (dir.clone(), &path[1..]),
        Some("super") => (parent_dir(&dir), &path[1..]),
        _ => (crate_root(from_file), &path[..]),
    };
    // Drop the trailing item name: `crate::pay::charge` is a fn in `pay`.
    let mut candidates = Vec::new();
    for take in (1..=rest.len()).rev() {
        let sub = rest[..take].join("/");
        candidates.push(join(&root, &format!("{sub}.rs")));
        candidates.push(join(&root, &format!("{sub}/mod.rs")));
    }
    first_existing(&candidates, index)
}

fn resolve_go(specifier: &str, index: &FileIndex) -> Vec<String> {
    // Go imports are absolute module paths; match the tail against a directory.
    let trimmed = specifier.trim_matches('"');
    let mut parts: Vec<&str> = trimmed.split('/').collect();
    while !parts.is_empty() {
        let suffix = parts.join("/");
        let hits = index.dir_suffix(&suffix, ".go");
        if !hits.is_empty() {
            return hits;
        }
        parts.remove(0);
    }
    Vec::new()
}

fn resolve_by_type_name(specifier: &str, ext: &str, index: &FileIndex) -> Vec<String> {
    // `com.acme.payments.Ledger` -> Ledger.java, when the repo has exactly one.
    let Some(last) = specifier.rsplit('.').next() else {
        return Vec::new();
    };
    if last == "*" || last.is_empty() {
        return Vec::new();
    }
    let hits = index.basename(&format!("{last}.{ext}"));
    if hits.len() == 1 {
        hits.to_vec()
    } else {
        Vec::new()
    }
}

fn resolve_include(dir: &str, specifier: &str, index: &FileIndex) -> Vec<String> {
    let direct = normalize(&join(dir, specifier));
    if index.contains(&direct) {
        return vec![direct];
    }
    if index.contains(specifier) {
        return vec![specifier.to_string()];
    }
    let Some(base) = specifier.rsplit('/').next() else {
        return Vec::new();
    };
    let hits = index.basename(base);
    if hits.len() == 1 {
        hits.to_vec()
    } else {
        Vec::new()
    }
}

fn first_existing(candidates: &[String], index: &FileIndex) -> Vec<String> {
    for c in candidates {
        let c = normalize(c);
        if index.contains(&c) {
            return vec![c];
        }
    }
    Vec::new()
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// The directory a Rust file's submodules live in: `src/pay.rs` owns `src/pay/`.
fn stem_dir(path: &str) -> String {
    match path.strip_suffix(".rs") {
        Some(stem) if !stem.ends_with("/mod") => stem.to_string(),
        _ => parent_dir(path),
    }
}

/// The `src/` directory of the crate containing `path`, which is what
/// `crate::` resolves against.
fn crate_root(path: &str) -> String {
    match path.find("src/") {
        Some(i) => path[..i + 3].to_string(),
        None => parent_dir(path),
    }
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Collapse `.` and `..` segments; import specifiers are full of them.
fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out.join("/")
}

/// Convenience for callers that have a path rather than a `LangSpec`.
pub fn extract_for_path(path: &str, source: &[u8]) -> Vec<RawImport> {
    match lang::for_path(path) {
        Some(spec) => extract(spec, source),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[&str]) -> FileIndex {
        FileIndex::new(files.iter().map(|s| s.to_string()))
    }

    #[test]
    fn typescript_relative_imports_resolve() {
        let idx = index(&["src/pay.ts", "src/ledger/index.ts", "src/util.tsx"]);
        assert_eq!(
            resolve("typescript", "src/app.ts", "./pay", &idx),
            vec!["src/pay.ts"]
        );
        assert_eq!(
            resolve("typescript", "src/app.ts", "./ledger", &idx),
            vec!["src/ledger/index.ts"]
        );
        assert_eq!(
            resolve("typescript", "src/deep/a.ts", "../util", &idx),
            vec!["src/util.tsx"]
        );
        // `./pay.js` in TypeScript source means pay.ts on disk.
        assert_eq!(
            resolve("typescript", "src/app.ts", "./pay.js", &idx),
            vec!["src/pay.ts"]
        );
        // Packages are not repo nodes.
        assert!(resolve("typescript", "src/app.ts", "react", &idx).is_empty());
    }

    #[test]
    fn python_absolute_and_relative_imports_resolve() {
        let idx = index(&[
            "app/models.py",
            "app/services/pay.py",
            "app/services/__init__.py",
        ]);
        assert_eq!(
            resolve("python", "app/main.py", "app.models", &idx),
            vec!["app/models.py"]
        );
        assert_eq!(
            resolve("python", "app/services/pay.py", ".", &idx),
            Vec::<String>::new(),
            "a bare dot names the package, not a module"
        );
        assert_eq!(
            resolve("python", "app/services/pay.py", ".models", &idx),
            Vec::<String>::new()
        );
        assert_eq!(
            resolve("python", "app/services/other.py", "..models", &idx),
            vec!["app/models.py"]
        );
        assert!(resolve("python", "app/main.py", "os.path", &idx).is_empty());
    }

    #[test]
    fn rust_mod_and_use_resolve() {
        let idx = index(&[
            "crates/core/src/lib.rs",
            "crates/core/src/pay.rs",
            "crates/core/src/ledger/mod.rs",
        ]);
        assert_eq!(
            resolve("rust", "crates/core/src/lib.rs", "pay", &idx),
            vec!["crates/core/src/pay.rs"]
        );
        assert_eq!(
            resolve("rust", "crates/core/src/lib.rs", "ledger", &idx),
            vec!["crates/core/src/ledger/mod.rs"]
        );
        assert_eq!(
            resolve("rust", "crates/core/src/pay.rs", "crate::ledger::Entry", &idx),
            vec!["crates/core/src/ledger/mod.rs"],
            "the trailing item name is not a module"
        );
        assert!(resolve("rust", "crates/core/src/pay.rs", "std::fmt::Debug", &idx).is_empty());
    }

    #[test]
    fn go_imports_match_a_package_directory() {
        let idx = index(&["internal/pay/pay.go", "internal/pay/fees.go", "main.go"]);
        let mut hits = resolve("go", "main.go", "github.com/acme/svc/internal/pay", &idx);
        hits.sort();
        assert_eq!(hits, vec!["internal/pay/fees.go", "internal/pay/pay.go"]);
        assert!(resolve("go", "main.go", "fmt", &idx).is_empty());
    }

    #[test]
    fn java_and_csharp_resolve_by_type_name_when_unambiguous() {
        let idx = index(&["src/Ledger.java", "src/Pay.java", "Other.cs"]);
        assert_eq!(
            resolve("java", "src/App.java", "com.acme.Ledger", &idx),
            vec!["src/Ledger.java"]
        );
        assert!(resolve("java", "src/App.java", "java.util.List", &idx).is_empty());
        assert_eq!(
            resolve("csharp", "App.cs", "Acme.Other", &idx),
            vec!["Other.cs"]
        );
    }

    #[test]
    fn cpp_includes_resolve_relative_then_by_name() {
        let idx = index(&["src/pay.h", "src/pay.cpp", "include/ledger.h"]);
        assert_eq!(
            resolve("cpp", "src/pay.cpp", "pay.h", &idx),
            vec!["src/pay.h"]
        );
        assert_eq!(
            resolve("cpp", "src/pay.cpp", "ledger.h", &idx),
            vec!["include/ledger.h"]
        );
        assert!(resolve("cpp", "src/pay.cpp", "vector", &idx).is_empty());
    }

    #[test]
    fn extraction_finds_specifiers_across_languages() {
        let cases: &[(&str, &[u8], &[&str])] = &[
            (
                "a.ts",
                b"import { charge } from './pay';\nimport React from 'react';\nconst x = require('./util');\n",
                &["./pay", "react", "./util"],
            ),
            (
                "a.py",
                b"import os\nfrom app.models import User\nfrom . import sibling\n",
                &["os", "app.models"],
            ),
            (
                "a.rs",
                b"mod pay;\nuse crate::ledger::Entry;\n",
                &["pay", "crate::ledger::Entry"],
            ),
            (
                "a.go",
                b"package main\nimport (\n  \"fmt\"\n  \"acme/pay\"\n)\n",
                &["fmt", "acme/pay"],
            ),
            ("a.java", b"import com.acme.Ledger;\n", &["com.acme.Ledger"]),
            ("a.cs", b"using Acme.Ledger;\n", &["Acme.Ledger"]),
            ("a.cpp", b"#include \"pay.h\"\n#include <vector>\n", &["pay.h", "vector"]),
        ];
        for (path, src, expected) in cases {
            let found: Vec<String> = extract_for_path(path, src)
                .into_iter()
                .map(|i| i.specifier)
                .collect();
            for want in *expected {
                assert!(found.contains(&want.to_string()), "{path}: {found:?}");
            }
        }
    }

    #[test]
    fn a_plain_call_with_a_string_argument_is_not_an_import() {
        let found = extract_for_path("a.ts", b"log('./not-an-import');\n");
        assert!(found.is_empty(), "{found:?}");
    }
}
