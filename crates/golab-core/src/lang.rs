//! Language registry.
//!
//! Every grammar ships a `tags.scm` query that already knows how to find
//! definitions and references in idiomatic code, so we lean on those instead
//! of hand-rolling node-kind tables per language. Where a grammar's tags query
//! misses something we care about (arrow-function consts in TS, calls in C++),
//! we append supplemental patterns using the same capture names.

use std::collections::HashMap;
use std::sync::OnceLock;

use tree_sitter::{Language, Query};

pub struct LangSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    language: fn() -> Language,
    /// Tags queries to layer, base first. TypeScript, for instance, ships only
    /// the patterns that differ from JavaScript, so both must be loaded.
    tags: &'static [&'static str],
    /// Extra patterns appended to `tags`; dropped if the grammar rejects them.
    extra: &'static str,
    /// Query capturing `@module` on every import specifier, so the indexer can
    /// draw file-to-file edges instead of guessing at names.
    imports: &'static str,
}

impl LangSpec {
    pub fn language(&self) -> Language {
        (self.language)()
    }

    /// Compiled query, preferring tags + supplements and degrading to plain
    /// tags if a supplement does not match this grammar's node types.
    pub fn query(&self) -> Result<Query, tree_sitter::QueryError> {
        let lang = self.language();
        let base = self.tags.join("\n");
        if !self.extra.is_empty() {
            let combined = format!("{base}\n{}", self.extra);
            if let Ok(q) = Query::new(&lang, &combined) {
                return Ok(q);
            }
        }
        Query::new(&lang, &base)
    }

    /// Compiled import query, or `None` if this grammar has no import syntax
    /// we understand.
    pub fn imports_query(&self) -> Option<Query> {
        if self.imports.is_empty() {
            return None;
        }
        Query::new(&self.language(), self.imports).ok()
    }

    /// File extensions to try when an import specifier omits one.
    pub fn source_extensions(&self) -> &'static [&'static str] {
        self.extensions
    }
}

const TS_EXTRA: &str = r#"
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function
(public_field_definition
  name: (property_identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.method
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (member_expression property: (property_identifier) @name)) @reference.call
"#;

const JS_EXTRA: &str = r#"
(variable_declarator
  name: (identifier) @name
  value: [(arrow_function) (function_expression)]) @definition.function
"#;

const CPP_EXTRA: &str = r#"
(call_expression function: (identifier) @name) @reference.call
(call_expression function: (field_expression field: (field_identifier) @name)) @reference.call
"#;

const GO_EXTRA: &str = r#"
(const_spec name: (identifier) @name) @definition.constant
"#;

const RUST_EXTRA: &str = r#"
(const_item name: (identifier) @name) @definition.constant
(static_item name: (identifier) @name) @definition.constant
(impl_item type: (type_identifier) @name) @definition.class
(impl_item type: (generic_type type: (type_identifier) @name)) @definition.class
"#;


const TS_IMPORTS: &str = r#"
(import_statement source: (string) @module)
(export_statement source: (string) @module)
(call_expression function: (identifier) @callee arguments: (arguments (string) @module))
"#;

const PY_IMPORTS: &str = r#"
(import_statement name: (dotted_name) @module)
(import_statement name: (aliased_import name: (dotted_name) @module))
(import_from_statement module_name: (dotted_name) @module)
(import_from_statement module_name: (relative_import) @module)
"#;

const RUST_IMPORTS: &str = r#"
(use_declaration argument: (_) @module)
(mod_item name: (identifier) @module)
"#;

const GO_IMPORTS: &str = r#"
(import_spec path: (interpreted_string_literal) @module)
"#;

const JAVA_IMPORTS: &str = r#"
(import_declaration (scoped_identifier) @module)
"#;

const CSHARP_IMPORTS: &str = r#"
(using_directive (qualified_name) @module)
(using_directive (identifier) @module)
"#;

const CPP_IMPORTS: &str = r#"
(preproc_include path: (string_literal) @module)
(preproc_include path: (system_lib_string) @module)
"#;

static SPECS: &[LangSpec] = &[
    LangSpec {
        name: "rust",
        extensions: &["rs"],
        language: || tree_sitter_rust::LANGUAGE.into(),
        tags: &[tree_sitter_rust::TAGS_QUERY],
        extra: RUST_EXTRA,
        imports: RUST_IMPORTS,
    },
    LangSpec {
        name: "typescript",
        extensions: &["ts", "mts", "cts"],
        language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        tags: &[
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY,
        ],
        extra: TS_EXTRA,
        imports: TS_IMPORTS,
    },
    LangSpec {
        name: "tsx",
        extensions: &["tsx"],
        language: || tree_sitter_typescript::LANGUAGE_TSX.into(),
        tags: &[
            tree_sitter_javascript::TAGS_QUERY,
            tree_sitter_typescript::TAGS_QUERY,
        ],
        extra: TS_EXTRA,
        imports: TS_IMPORTS,
    },
    LangSpec {
        name: "javascript",
        extensions: &["js", "jsx", "mjs", "cjs"],
        language: || tree_sitter_javascript::LANGUAGE.into(),
        tags: &[tree_sitter_javascript::TAGS_QUERY],
        extra: JS_EXTRA,
        imports: TS_IMPORTS,
    },
    LangSpec {
        name: "python",
        extensions: &["py", "pyi"],
        language: || tree_sitter_python::LANGUAGE.into(),
        tags: &[tree_sitter_python::TAGS_QUERY],
        extra: "",
        imports: PY_IMPORTS,
    },
    LangSpec {
        name: "go",
        extensions: &["go"],
        language: || tree_sitter_go::LANGUAGE.into(),
        tags: &[tree_sitter_go::TAGS_QUERY],
        extra: GO_EXTRA,
        imports: GO_IMPORTS,
    },
    LangSpec {
        name: "java",
        extensions: &["java"],
        language: || tree_sitter_java::LANGUAGE.into(),
        tags: &[tree_sitter_java::TAGS_QUERY],
        extra: "",
        imports: JAVA_IMPORTS,
    },
    LangSpec {
        name: "csharp",
        extensions: &["cs"],
        language: || tree_sitter_c_sharp::LANGUAGE.into(),
        tags: &[tree_sitter_c_sharp::TAGS_QUERY],
        extra: "",
        imports: CSHARP_IMPORTS,
    },
    LangSpec {
        name: "cpp",
        extensions: &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "h"],
        language: || tree_sitter_cpp::LANGUAGE.into(),
        tags: &[tree_sitter_cpp::TAGS_QUERY],
        extra: CPP_EXTRA,
        imports: CPP_IMPORTS,
    },
];

fn by_extension() -> &'static HashMap<&'static str, usize> {
    static MAP: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for (i, spec) in SPECS.iter().enumerate() {
            for ext in spec.extensions {
                m.insert(*ext, i);
            }
        }
        m
    })
}

pub fn all() -> &'static [LangSpec] {
    SPECS
}

pub fn by_name(name: &str) -> Option<&'static LangSpec> {
    SPECS.iter().find(|s| s.name == name)
}

/// Pick a grammar from a file path's extension.
pub fn for_path(path: &str) -> Option<&'static LangSpec> {
    let ext = path.rsplit('.').next()?;
    if ext == path {
        return None; // no dot at all
    }
    let ext = ext.to_ascii_lowercase();
    by_extension().get(ext.as_str()).map(|i| &SPECS[*i])
}

/// Every extension the runtime knows how to index.
pub fn known_extensions() -> Vec<&'static str> {
    let mut v: Vec<_> = by_extension().keys().copied().collect();
    v.sort_unstable();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_grammar_compiles_its_query() {
        for spec in all() {
            let q = spec.query().unwrap_or_else(|e| panic!("{}: {e}", spec.name));
            assert!(q.pattern_count() > 0, "{} has no patterns", spec.name);
        }
    }

    #[test]
    fn extension_routing() {
        assert_eq!(for_path("src/a/b.ts").unwrap().name, "typescript");
        assert_eq!(for_path("App.tsx").unwrap().name, "tsx");
        assert_eq!(for_path("main.RS").unwrap().name, "rust");
        assert!(for_path("Makefile").is_none());
    }
}
