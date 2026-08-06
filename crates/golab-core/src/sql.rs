//! Database nodes.
//!
//! Tables come from DDL in `.sql` files, and become real nodes in the graph so
//! that "who writes to `payments`?" is a query rather than a grep. References
//! come from SQL embedded in application code — which is where it always is.
//!
//! This is a scanner, not a SQL parser: it recognises the handful of shapes
//! that name a table and ignores everything else. A real parser would buy
//! precision inside expressions, which is not what the graph needs.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

/// A table declared by DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: usize,
    /// `CREATE TABLE` or `ALTER TABLE`.
    pub statement: String,
}

/// A possibly schema-qualified, possibly quoted identifier:
/// `payments`, `"public"."Payments"`, `[dbo].[Payments]`.
const IDENT: &str = r#"(?:["`\[]?[A-Za-z_]\w*["`\]]?\s*\.\s*)*["`\[]?[A-Za-z_]\w*["`\]]?"#;

fn ddl() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            r#"(?is)\b(create\s+table(?:\s+if\s+not\s+exists)?|alter\s+table|drop\s+table(?:\s+if\s+exists)?)\s+({IDENT})"#
        ))
        .expect("ddl regex")
    })
}

fn refs() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(&format!(
            r#"(?is)\b(from|join|into|update|delete\s+from|table)\s+({IDENT})"#
        ))
        .expect("ref regex")
    })
}

/// Extract table definitions from a `.sql` file.
///
/// A table is defined once but altered many times; the definition wins, and
/// migrations that only `ALTER` still register the table they touch.
pub fn tables(source: &[u8]) -> Vec<TableDef> {
    let Ok(text) = std::str::from_utf8(source) else {
        return Vec::new();
    };
    let mut out: Vec<TableDef> = Vec::new();
    for c in ddl().captures_iter(text) {
        let whole = c.get(0).expect("whole match");
        let statement = c[1].split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase();
        let name = normalize(&c[2]);
        if name.is_empty() {
            continue;
        }
        // Keep the CREATE if we have one; otherwise the first mention.
        if let Some(existing) = out.iter_mut().find(|t| t.name == name) {
            if statement.starts_with("CREATE") && !existing.statement.starts_with("CREATE") {
                existing.statement = statement;
                existing.start_byte = whole.start();
                existing.end_byte = whole.end();
                existing.line = text[..whole.start()].matches('\n').count();
            }
            continue;
        }
        out.push(TableDef {
            name,
            start_byte: whole.start(),
            end_byte: whole.end(),
            line: text[..whole.start()].matches('\n').count(),
            statement,
        });
    }
    out
}

/// Which of the `known` tables a chunk of code touches.
///
/// Matching against known table names (rather than trusting the regex alone)
/// is what keeps `from collections import x` out of the database graph.
pub fn referenced(source: &str, known: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if known.is_empty() {
        return out;
    }
    for c in refs().captures_iter(source) {
        let name = normalize(&c[2]);
        if known.contains(&name) {
            out.insert(name);
        }
    }
    out
}

/// Strip quoting and any schema qualifier, then case-fold:
/// `"public"."Payments"` -> `payments`.
fn normalize(raw: &str) -> String {
    raw.rsplit('.')
        .next()
        .unwrap_or(raw)
        .trim()
        .trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
        .to_ascii_lowercase()
}

/// Is this a file the SQL scanner should read?
pub fn is_sql_path(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".sql")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &[u8]) -> Vec<String> {
        tables(src).into_iter().map(|t| t.name).collect()
    }

    #[test]
    fn ddl_defines_tables() {
        let src = br#"
CREATE TABLE payments (
  id UUID PRIMARY KEY,
  amount INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS ledger_entries (id UUID);

ALTER TABLE payments ADD COLUMN currency TEXT;
"#;
        assert_eq!(names(src), vec!["payments", "ledger_entries"]);
        assert_eq!(tables(src)[0].statement, "CREATE TABLE");
    }

    #[test]
    fn a_migration_that_only_alters_still_registers_the_table() {
        let src = b"ALTER TABLE audit_log ADD COLUMN actor TEXT;\n";
        let t = tables(src);
        assert_eq!(t[0].name, "audit_log");
        assert_eq!(t[0].statement, "ALTER TABLE");
    }

    #[test]
    fn schema_qualifiers_and_quoting_are_normalised() {
        let src = br#"CREATE TABLE "public"."Payments" (id INT);"#;
        assert_eq!(names(src), vec!["payments"]);
    }

    #[test]
    fn code_references_are_matched_against_known_tables_only() {
        let known: BTreeSet<String> = ["payments", "ledger_entries"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let code = r#"
        const rows = await db.query("SELECT * FROM payments WHERE id = $1", [id]);
        await db.query("INSERT INTO ledger_entries (id) VALUES ($1)", [id]);
        "#;
        let hit = referenced(code, &known);
        assert!(hit.contains("payments"));
        assert!(hit.contains("ledger_entries"));

        // A Python import is not a database read, even though it says "from".
        assert!(referenced("from collections import OrderedDict", &known).is_empty());
        // Nor is an unrelated table.
        assert!(referenced("SELECT * FROM sessions", &known).is_empty());
    }

    #[test]
    fn joins_and_updates_count_as_references() {
        let known: BTreeSet<String> = ["payments", "users"].iter().map(|s| s.to_string()).collect();
        let hit = referenced(
            "UPDATE payments SET x = 1; SELECT * FROM a JOIN users ON a.id = users.id",
            &known,
        );
        assert_eq!(hit.len(), 2);
    }
}
