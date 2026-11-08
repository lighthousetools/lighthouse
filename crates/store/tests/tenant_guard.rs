//! A guard against queries that forget the tenant.
//!
//! Every statement in the library code of the workspace that reads or writes a
//! table with a `tenant_id` column must name `tenant_id`, so a row of one
//! tenant cannot be reached from another by its ID. The few statements that
//! are scoped another way are listed below with the reason.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Statements scoped without `tenant_id`, by file and a piece of their text.
const EXCEPTIONS: &[(&str, &str, &str)] = &[
    (
        "crates/auth/src/",
        "device_tokens",
        "device tokens are instance-wide and found by their hash or their user",
    ),
    (
        "crates/sync/src/store.rs",
        "WHERE {condition}",
        "the condition starts with `tenant_id = $1`",
    ),
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Tables with a `tenant_id` column, read from the migrations.
fn tenant_tables() -> BTreeSet<String> {
    let mut tables = BTreeSet::new();
    let mut current: Option<String> = None;
    let mut files: Vec<_> = std::fs::read_dir(workspace().join("crates/store/migrations"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    files.sort();
    for file in files {
        for line in std::fs::read_to_string(file).unwrap().lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("CREATE TABLE ") {
                current = rest.split_whitespace().next().map(str::to_owned);
            } else if line.starts_with(");") {
                current = None;
            } else if line.starts_with("tenant_id ")
                && let Some(table) = &current
            {
                tables.insert(table.clone());
            }
        }
    }
    tables
}

fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path.file_name().is_some_and(|name| name != "tests.rs")
        {
            files.push(path);
        }
    }
}

/// String literals of a file before its test module, with line continuations
/// joined, and the line each starts on.
fn literals(source: &str) -> Vec<(usize, String)> {
    let source = source
        .find("#[cfg(test)]")
        .map_or(source, |end| &source[..end]);
    let chars: Vec<char> = source.chars().collect();
    let mut found = Vec::new();
    let mut line = 1;
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\n' => line += 1,
            '/' if chars.get(index + 1) == Some(&'/') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
                continue;
            }
            '\'' if chars.get(index + 2) == Some(&'\'') => index += 2,
            '\'' if chars.get(index + 1) == Some(&'\\') && chars.get(index + 3) == Some(&'\'') => {
                index += 3;
            }
            '"' => {
                let start = line;
                let mut text = String::new();
                index += 1;
                while index < chars.len() && chars[index] != '"' {
                    if chars[index] == '\\' {
                        index += 1;
                        if chars.get(index) == Some(&'\n') {
                            line += 1;
                            while chars
                                .get(index + 1)
                                .is_some_and(|next| next.is_whitespace())
                            {
                                index += 1;
                                if chars[index] == '\n' {
                                    line += 1;
                                }
                            }
                        } else {
                            text.push(chars[index]);
                        }
                    } else {
                        if chars[index] == '\n' {
                            line += 1;
                        }
                        text.push(chars[index]);
                    }
                    index += 1;
                }
                found.push((start, text));
            }
            _ => {}
        }
        index += 1;
    }
    found
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn tables_used(text: &str, tables: &BTreeSet<String>) -> Vec<String> {
    let words = words(text);
    words
        .windows(2)
        .filter(|pair| ["from", "join", "update", "into"].contains(&pair[0].as_str()))
        .filter(|pair| tables.contains(&pair[1]))
        .map(|pair| pair[1].clone())
        .collect()
}

#[test]
fn every_statement_on_a_tenant_table_names_the_tenant() {
    let tables = tenant_tables();
    assert!(tables.contains("tasks") && tables.contains("runs"));
    let mut files = Vec::new();
    rust_files(&workspace().join("crates"), &mut files);
    let mut missing = Vec::new();
    let mut checked = 0;
    for file in files {
        let relative = file
            .strip_prefix(workspace())
            .unwrap()
            .display()
            .to_string();
        if relative.contains("/tests/") {
            continue;
        }
        let source = std::fs::read_to_string(&file).unwrap();
        for (line, text) in literals(&source) {
            let statement = words(&text);
            let Some(first) = statement.first() else {
                continue;
            };
            if !["select", "update", "delete", "insert", "with"].contains(&first.as_str()) {
                continue;
            }
            let used = tables_used(&text, &tables);
            if used.is_empty() {
                continue;
            }
            checked += 1;
            if text.contains("tenant_id") {
                continue;
            }
            let excepted = EXCEPTIONS.iter().any(|(path, piece, _)| {
                relative.starts_with(path)
                    && (text.contains(piece) || used.iter().any(|t| t == piece))
            });
            if !excepted {
                missing.push(format!("{relative}:{line}: {}", text.trim()));
            }
        }
    }
    assert!(checked > 200, "the guard found only {checked} statements");
    assert!(
        missing.is_empty(),
        "statements without tenant_id:\n{}",
        missing.join("\n")
    );
}

#[test]
fn the_guard_reads_literals_and_tables() {
    let source =
        "let a = \"SELECT x FROM tasks \\\n    WHERE id = $1\"; // \"FROM runs\"\nlet b = '\"';";
    let found = literals(source);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1, "SELECT x FROM tasks WHERE id = $1");
    let tables = BTreeSet::from(["tasks".to_owned()]);
    assert_eq!(tables_used(&found[0].1, &tables), ["tasks"]);
    assert!(tables_used("SELECT 1 FROM users", &tables).is_empty());
}
