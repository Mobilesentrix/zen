//! Real dev corpus (read-only, outside the repo): every databaseNode query must parse, and every
//! recorded request case must bind for nodes fed directly by the request.
//! Run with MS_CORPUS_DIR=<corpus root> ... -- --ignored corpus

use ms_zen_database::{bind, Dialect, NodeLabel, QueryTemplate};
use serde_json::Value;
use std::path::{Path, PathBuf};
use zen_expression::Variable;

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let p = entry.unwrap().path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|e| e == "json") {
            out.push(p);
        }
    }
}

fn sql_of(content: &Value) -> Option<String> {
    let q = content["query"].as_str().filter(|q| !q.trim().is_empty());
    let c = (content["useCustomQuery"] == Value::Bool(true))
        .then(|| content["customQuery"].as_str())
        .flatten()
        .filter(|q| !q.trim().is_empty());
    q.or(c).map(str::to_string)
}

#[test]
#[ignore = "needs MS_CORPUS_DIR (dev corpus export)"]
fn corpus_queries_parse_and_cases_bind() {
    let root = PathBuf::from(std::env::var("MS_CORPUS_DIR").expect("MS_CORPUS_DIR"));
    let mut rules = Vec::new();
    walk(&root.join("rules"), &mut rules);

    let (mut nodes, mut parsed, mut builder_only, mut binds, mut bind_fail) = (0, 0, 0, 0, 0);
    let mut failures = Vec::new();
    for rule_path in &rules {
        let rule: Value =
            serde_json::from_str(&std::fs::read_to_string(rule_path).unwrap()).unwrap();
        let graph = if rule["content"]["nodes"].is_array() {
            &rule["content"]
        } else {
            &rule
        };
        let Some(all_nodes) = graph["nodes"].as_array() else {
            continue;
        };
        let input_ids: Vec<&str> = all_nodes
            .iter()
            .filter(|n| n["type"] == "inputNode")
            .filter_map(|n| n["id"].as_str())
            .collect();
        let rel = rule_path.strip_prefix(root.join("rules")).unwrap();
        let cases: Vec<Value> = std::fs::read_to_string(root.join("cases").join(rel))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        for node in all_nodes.iter().filter(|n| n["type"] == "databaseNode") {
            nodes += 1;
            let Some(sql) = sql_of(&node["content"]) else {
                builder_only += 1; // selectedTables: SQL is built by module-go
                continue;
            };
            let template = match QueryTemplate::parse(&sql, Dialect::MySql) {
                Ok(t) => t,
                Err(e) => {
                    // playground/ holds scratch rules; a rejection there is reported, not fatal.
                    let line = format!("{}: {e}", rel.display());
                    println!("PARSE-REJECT {line}");
                    if !rel.starts_with("playground") {
                        failures.push(line);
                    }
                    continue;
                }
            };
            parsed += 1;
            // No corpus query uses {{#if}} blocks or LIMIT/OFFSET counts, so every one keeps
            // the static bind path byte for byte.
            if template.is_dynamic() {
                failures.push(format!("{}: unexpectedly dynamic template", rel.display()));
            }
            // Every corpus query only reads, so each keeps early start, concurrent loop items and
            // per-evaluation sharing.
            if !template.is_read_only() {
                failures.push(format!(
                    "{}: not classified read-only: {sql}",
                    rel.display()
                ));
            }
            let id = node["id"].as_str().unwrap_or("");
            let fed_by_request = graph["edges"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| e["targetId"] == id && input_ids.iter().any(|i| e["sourceId"] == *i));
            if !fed_by_request {
                continue;
            }
            let label = NodeLabel {
                name: node["name"].as_str().unwrap_or(""),
                id,
            };
            for case in &cases {
                match bind(&template, &Variable::from(case["content"].clone()), label) {
                    Ok(_) => binds += 1,
                    Err(e) => {
                        bind_fail += 1;
                        if !rel.starts_with("playground") {
                            failures.push(format!("{} / {}: {e}", rel.display(), case["name"]));
                        }
                        println!("BIND-ERROR {} / {}: {e}", rel.display(), case["name"]);
                    }
                }
            }
        }
    }
    println!(
        "CORPUS rules={} db_nodes={nodes} parsed={parsed} builder_only={builder_only} case_binds_ok={binds} case_binds_err={bind_fail}",
        rules.len()
    );
    assert!(
        failures.is_empty(),
        "non-playground failures:\n{}",
        failures.join("\n")
    );
    assert!(parsed > 0);
}
