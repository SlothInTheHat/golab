//! Source file -> symbols + references.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use crate::ids;
use crate::lang::{self, LangSpec};
use crate::model::{EdgeKind, SymbolKind};

#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub kind: SymbolKind,
    pub name: String,
    /// Qualified by enclosing symbols within the file (not by file path).
    pub fqn: String,
    /// Index into `ParsedFile::symbols`.
    pub parent: Option<usize>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// Hash of the symbol's full source text, nested symbols included.
    pub body_hash: String,
    /// Hash of the symbol's text with nested symbols blanked out. Editing a
    /// method changes the class's `body_hash` but not its `own_hash`, which is
    /// what lets enforcement blame the smallest symbol that actually changed.
    pub own_hash: String,
    /// Distinguishes same-named siblings (overloads, re-definitions).
    pub disambiguator: u32,
}

#[derive(Debug, Clone)]
pub struct ParsedRef {
    /// Referenced name as written at the call site.
    pub name: String,
    /// Enclosing symbol index, if the reference sits inside one.
    pub from: Option<usize>,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone)]
pub struct ParsedFile {
    pub lang: &'static str,
    pub symbols: Vec<ParsedSymbol>,
    pub refs: Vec<ParsedRef>,
    /// Hash of the whole file.
    pub content_hash: String,
    /// Hash of the file with top-level symbols blanked out: imports, module
    /// wiring and stray statements. Changing those is a change to the file
    /// itself, and needs a lease on the file.
    pub own_hash: String,
}

struct RawDef {
    kind: SymbolKind,
    name: String,
    start_byte: usize,
    end_byte: usize,
    start_line: usize,
    end_line: usize,
}

struct RawRef {
    name: String,
    kind: EdgeKind,
    start_byte: usize,
    end_byte: usize,
}

fn def_kind(capture: &str) -> Option<SymbolKind> {
    Some(match capture {
        "definition.function" => SymbolKind::Function,
        "definition.method" => SymbolKind::Method,
        "definition.class" | "definition.struct" | "definition.enum" => SymbolKind::Class,
        "definition.interface" => SymbolKind::Interface,
        "definition.trait" => SymbolKind::Trait,
        "definition.module" | "definition.namespace" => SymbolKind::Module,
        "definition.type" => SymbolKind::Type,
        "definition.constant" | "definition.variable" => SymbolKind::Constant,
        "definition.macro" => SymbolKind::Macro,
        _ => return None,
    })
}

fn ref_kind(capture: &str) -> Option<EdgeKind> {
    Some(match capture {
        "reference.call" | "reference.send" => EdgeKind::Calls,
        "reference.class" | "reference.type" | "reference.interface"
        | "reference.implementation" | "reference.module" => EdgeKind::Uses,
        _ => return None,
    })
}

thread_local! {
    static PARSERS: RefCell<HashMap<&'static str, (Parser, Query)>> = RefCell::new(HashMap::new());
}

/// Parse `source` using the grammar implied by `path`'s extension.
pub fn parse_path(path: &str, source: &[u8]) -> Result<ParsedFile> {
    let spec = lang::for_path(path).ok_or_else(|| anyhow!("no grammar for {path}"))?;
    parse_with(spec, source)
}

pub fn parse_with(spec: &'static LangSpec, source: &[u8]) -> Result<ParsedFile> {
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        if !map.contains_key(spec.name) {
            let mut parser = Parser::new();
            parser.set_language(&spec.language())?;
            let query = spec.query()?;
            map.insert(spec.name, (parser, query));
        }
        let (parser, query) = map.get_mut(spec.name).expect("just inserted");
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| anyhow!("tree-sitter failed to parse"))?;

        let mut defs: Vec<RawDef> = Vec::new();
        let mut refs: Vec<RawRef> = Vec::new();
        let mut seen: HashSet<(usize, usize, &'static str, String)> = HashSet::new();

        let capture_names = query.capture_names();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            // A match carries one definition/reference node plus its @name.
            let mut target: Option<(tree_sitter::Node, &str)> = None;
            for cap in m.captures {
                let cname = capture_names[cap.index as usize];
                if cname.starts_with("definition.") || cname.starts_with("reference.") {
                    target = Some((cap.node, cname));
                    break;
                }
            }
            let Some((node, cname)) = target else { continue };

            // Take the first @name that lives inside the captured node.
            let mut name: Option<String> = None;
            for cap in m.captures {
                if capture_names[cap.index as usize] != "name" {
                    continue;
                }
                if cap.node.start_byte() < node.start_byte() || cap.node.end_byte() > node.end_byte()
                {
                    continue;
                }
                if let Ok(text) = cap.node.utf8_text(source) {
                    if !text.is_empty() {
                        name = Some(text.to_string());
                        break;
                    }
                }
            }
            let Some(name) = name else { continue };

            if let Some(kind) = def_kind(cname) {
                let key = (node.start_byte(), node.end_byte(), kind.as_str(), name.clone());
                if !seen.insert(key) {
                    continue;
                }
                defs.push(RawDef {
                    kind,
                    name,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    start_line: node.start_position().row,
                    end_line: node.end_position().row,
                });
            } else if let Some(kind) = ref_kind(cname) {
                refs.push(RawRef {
                    name,
                    kind,
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                });
            }
        }

        Ok(build(spec.name, defs, refs, source))
    })
}

/// Turn a flat capture list into a containment tree with qualified names.
fn build(
    lang: &'static str,
    mut defs: Vec<RawDef>,
    raw_refs: Vec<RawRef>,
    source: &[u8],
) -> ParsedFile {
    // Outermost-first so a simple stack yields the containment hierarchy.
    defs.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(b.end_byte.cmp(&a.end_byte))
            .then(a.kind.as_str().cmp(b.kind.as_str()))
    });

    let mut symbols: Vec<ParsedSymbol> = Vec::with_capacity(defs.len());
    let mut stack: Vec<usize> = Vec::new();
    let mut fqn_counts: HashMap<(SymbolKind, String), u32> = HashMap::new();

    for raw in defs {
        while let Some(&top) = stack.last() {
            let t = &symbols[top];
            if raw.start_byte >= t.end_byte {
                stack.pop();
            } else {
                break;
            }
        }

        // Two grammar patterns often describe the same declaration at slightly
        // different granularity (`const f = () => {}`). Keep the outer one.
        if let Some(&top) = stack.last() {
            let t = &symbols[top];
            if t.name == raw.name && t.start_byte <= raw.start_byte && raw.end_byte <= t.end_byte {
                continue;
            }
        }

        let parent = stack.last().copied();
        let fqn = match parent {
            Some(p) => format!("{}.{}", symbols[p].fqn, raw.name),
            None => raw.name.clone(),
        };
        let counter = fqn_counts.entry((raw.kind, fqn.clone())).or_insert(0);
        let disambiguator = *counter;
        *counter += 1;

        let body = source
            .get(raw.start_byte..raw.end_byte.min(source.len()))
            .unwrap_or(&[]);
        symbols.push(ParsedSymbol {
            kind: raw.kind,
            name: raw.name,
            fqn,
            parent,
            start_byte: raw.start_byte,
            end_byte: raw.end_byte,
            start_line: raw.start_line,
            end_line: raw.end_line,
            body_hash: ids::content_hash(body),
            own_hash: String::new(), // filled in below, once children are known
            disambiguator,
        });
        stack.push(symbols.len() - 1);
    }

    // Own-hash: the symbol's text with each direct child's span elided.
    let child_spans: Vec<Vec<(usize, usize)>> = {
        let mut v = vec![Vec::new(); symbols.len()];
        for s in &symbols {
            if let Some(p) = s.parent {
                v[p].push((s.start_byte, s.end_byte));
            }
        }
        v
    };
    for i in 0..symbols.len() {
        let (start, end) = (symbols[i].start_byte, symbols[i].end_byte);
        symbols[i].own_hash = elided_hash(source, start, end, &child_spans[i]);
    }
    let top_level: Vec<(usize, usize)> = symbols
        .iter()
        .filter(|s| s.parent.is_none())
        .map(|s| (s.start_byte, s.end_byte))
        .collect();

    // Attribute each reference to the innermost symbol containing it.
    let refs = raw_refs
        .into_iter()
        .map(|r| {
            let mut best: Option<usize> = None;
            for (i, s) in symbols.iter().enumerate() {
                if s.start_byte <= r.start_byte && r.end_byte <= s.end_byte {
                    match best {
                        Some(b) if symbols[b].start_byte >= s.start_byte => {}
                        _ => best = Some(i),
                    }
                }
            }
            ParsedRef {
                name: r.name,
                from: best,
                kind: r.kind,
            }
        })
        .collect();

    ParsedFile {
        lang,
        symbols,
        refs,
        content_hash: ids::content_hash(source),
        own_hash: elided_hash(source, 0, source.len(), &top_level),
    }
}

/// Hash `source[start..end]` with `spans` replaced by a fixed marker.
fn elided_hash(source: &[u8], start: usize, end: usize, spans: &[(usize, usize)]) -> String {
    const HOLE: &[u8] = b"\x00<nested>\x00";
    let end = end.min(source.len());
    if start >= end {
        return ids::content_hash(b"");
    }
    let mut spans: Vec<(usize, usize)> = spans
        .iter()
        .copied()
        .filter(|(s, e)| *s >= start && *e <= end && s < e)
        .collect();
    spans.sort_unstable();

    let mut buf: Vec<u8> = Vec::with_capacity(end - start);
    let mut cursor = start;
    for (s, e) in spans {
        if s < cursor {
            continue; // overlapping span already elided
        }
        buf.extend_from_slice(&source[cursor..s]);
        buf.extend_from_slice(HOLE);
        cursor = e;
    }
    buf.extend_from_slice(&source[cursor..end]);
    ids::content_hash(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(pf: &ParsedFile) -> Vec<String> {
        let mut v: Vec<_> = pf
            .symbols
            .iter()
            .map(|s| format!("{}:{}", s.kind.as_str(), s.fqn))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn typescript_class_methods_and_arrow_consts() {
        let src = br#"
export class PaymentService {
  async processPayment(id: string) {
    return this.charge(id);
  }
  charge(id: string) { return 1; }
}

export const refund = async (id: string) => {
  return processPayment(id);
};

function processPayment(id: string) { return 0; }
"#;
        let pf = parse_path("src/pay.ts", src).unwrap();
        let n = names(&pf);
        assert!(n.contains(&"class:PaymentService".to_string()), "{n:?}");
        assert!(
            n.contains(&"method:PaymentService.processPayment".to_string()),
            "{n:?}"
        );
        assert!(n.contains(&"function:refund".to_string()), "{n:?}");
        assert!(n.contains(&"function:processPayment".to_string()), "{n:?}");

        // `refund` calls processPayment -> that edge must be attributed to refund.
        let refund_idx = pf.symbols.iter().position(|s| s.fqn == "refund").unwrap();
        assert!(pf
            .refs
            .iter()
            .any(|r| r.name == "processPayment" && r.from == Some(refund_idx)));
    }

    #[test]
    fn python_nested_scopes() {
        let src = br#"
class Ledger:
    def post(self, entry):
        return self.validate(entry)

    def validate(self, entry):
        return True

def post(entry):
    return None
"#;
        let pf = parse_path("ledger.py", src).unwrap();
        let n = names(&pf);
        assert!(n.contains(&"class:Ledger".to_string()), "{n:?}");
        assert!(n.contains(&"function:Ledger.post".to_string()), "{n:?}");
        assert!(n.contains(&"function:post".to_string()), "{n:?}");
    }

    /// Rust methods must be scoped to their `impl` type, otherwise
    /// `Wallet::balance` and a free `balance()` are indistinguishable.
    #[test]
    fn rust_impl_blocks_are_scopes() {
        let src = br#"
pub struct Wallet;

impl Wallet {
    pub fn balance(&self) -> u64 { self.raw() }
    fn raw(&self) -> u64 { 0 }
}

impl Ledger<T> {
    pub fn balance(&self) -> u64 { 1 }
}

pub fn balance() -> u64 { 0 }
"#;
        let pf = parse_path("wallet.rs", src).unwrap();
        let n = names(&pf);
        assert!(n.contains(&"function:Wallet.balance".to_string()), "{n:?}");
        assert!(n.contains(&"function:Wallet.raw".to_string()), "{n:?}");
        assert!(n.contains(&"function:Ledger.balance".to_string()), "{n:?}");
        assert!(n.contains(&"function:balance".to_string()), "{n:?}");
    }

    #[test]
    fn same_name_siblings_get_distinct_disambiguators() {
        let src = br#"
def handler():
    pass

def handler():
    pass
"#;
        let pf = parse_path("dup.py", src).unwrap();
        let handlers: Vec<_> = pf.symbols.iter().filter(|s| s.name == "handler").collect();
        assert_eq!(handlers.len(), 2);
        assert_ne!(handlers[0].disambiguator, handlers[1].disambiguator);
    }

    /// Each supported grammar must find both a container and a callable in a
    /// canonical snippet; otherwise leasing that language is a lie.
    #[test]
    fn every_language_extracts_symbols() {
        let cases: &[(&str, &[u8], &[&str])] = &[
            (
                "a.rs",
                b"struct Wallet;\nimpl Wallet { fn balance(&self) -> u64 { 0 } }\n",
                &["Wallet", "balance"],
            ),
            (
                "a.ts",
                b"export class Svc { pay(x: number) { return x; } }\nexport function top() {}\n",
                &["Svc", "pay", "top"],
            ),
            (
                "a.tsx",
                b"export class Svc { pay(x: number) { return x; } }\nexport function top() {}\n",
                &["Svc", "pay", "top"],
            ),
            (
                "a.js",
                b"class Svc { pay(x) { return x; } }\nfunction top() {}\n",
                &["Svc", "pay", "top"],
            ),
            (
                "a.py",
                b"class Svc:\n    def pay(self, x):\n        return x\n\ndef top():\n    pass\n",
                &["Svc", "pay", "top"],
            ),
            (
                "a.go",
                b"package main\ntype Svc struct{}\nfunc (s Svc) Pay() int { return 0 }\nfunc Top() {}\n",
                &["Svc", "Pay", "Top"],
            ),
            (
                "a.java",
                b"class Svc { int pay(int x) { return x; } }\n",
                &["Svc", "pay"],
            ),
            (
                "a.cs",
                b"class Svc { int Pay(int x) { return x; } }\n",
                &["Svc", "Pay"],
            ),
            (
                "a.cpp",
                b"class Svc { public: int pay(int x); };\nint top() { return 0; }\n",
                &["Svc", "pay", "top"],
            ),
        ];

        for (path, src, expected) in cases {
            let pf = parse_path(path, src).unwrap_or_else(|e| panic!("{path}: {e}"));
            let found: Vec<&str> = pf.symbols.iter().map(|s| s.name.as_str()).collect();
            for want in *expected {
                assert!(found.contains(want), "{path}: missing {want} in {found:?}");
            }
        }
    }

    #[test]
    fn body_hash_tracks_content() {
        let a = parse_path("a.py", b"def f():\n    return 1\n").unwrap();
        let b = parse_path("a.py", b"def f():\n    return 2\n").unwrap();
        assert_ne!(a.symbols[0].body_hash, b.symbols[0].body_hash);
        assert_eq!(a.symbols[0].fqn, b.symbols[0].fqn);
    }

    #[test]
    fn own_hash_ignores_nested_symbols() {
        let before = parse_path(
            "a.py",
            b"class C:\n    def m(self):\n        return 1\n",
        )
        .unwrap();
        let after = parse_path(
            "a.py",
            b"class C:\n    def m(self):\n        return 2\n",
        )
        .unwrap();

        let c_before = before.symbols.iter().find(|s| s.name == "C").unwrap();
        let c_after = after.symbols.iter().find(|s| s.name == "C").unwrap();
        assert_ne!(c_before.body_hash, c_after.body_hash, "body includes the method");
        assert_eq!(
            c_before.own_hash, c_after.own_hash,
            "editing a method must not count as editing its class"
        );

        let m_before = before.symbols.iter().find(|s| s.name == "m").unwrap();
        let m_after = after.symbols.iter().find(|s| s.name == "m").unwrap();
        assert_ne!(m_before.own_hash, m_after.own_hash);
    }

    #[test]
    fn file_own_hash_tracks_top_level_only() {
        let a = parse_path("a.py", b"import os\n\ndef f():\n    return 1\n").unwrap();
        let b = parse_path("a.py", b"import os\n\ndef f():\n    return 2\n").unwrap();
        let c = parse_path("a.py", b"import sys\n\ndef f():\n    return 1\n").unwrap();
        assert_eq!(a.own_hash, b.own_hash, "a function body is not the file");
        assert_ne!(a.own_hash, c.own_hash, "changed imports are a file-level edit");
    }
}
