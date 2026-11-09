use super::*;
use serde_json::json;

fn transform(rule: Value, routing: InstanceRouting) -> Value {
    let mut rule = rule;
    transform_database_nodes(&mut rule, routing).unwrap();
    rule
}

fn one_node(content: Value) -> Value {
    json!({"nodes": [{"id": "db", "type": "databaseNode", "content": content}], "edges": []})
}

fn first_config(rule: &Value) -> &Map<String, Value> {
    let node = &rule["nodes"][0];
    assert_eq!(node["type"], "customNode");
    assert_eq!(node["content"]["kind"], "database");
    node["content"]["config"].as_object().unwrap()
}

#[test]
fn converts_editor_content_with_defaults() {
    struct Case {
        content: Value,
        query: &'static str,
        output: &'static str,
        pass: bool,
    }
    let cases = [
        Case {
            content: json!({"useCustomQuery": true, "customQuery": "SELECT * FROM o WHERE id = {{ id }}", "outputPath": "orders", "passThrough": false}),
            query: "SELECT * FROM o WHERE id = {{ id }}",
            output: "orders",
            pass: false,
        },
        Case {
            content: json!({"database": "shop", "instance": "ddd3956b", "selectedTables": [{"tableName": "orders", "columns": ["id", "total"]}]}),
            query: "SELECT `id`, `total` FROM `shop`.`orders`",
            output: "dbResult",
            pass: true,
        },
        Case {
            content: json!({"query": "SELECT 1", "outputPath": "x", "passThrough": true}),
            query: "SELECT 1",
            output: "x",
            pass: true,
        },
    ];
    for case in cases {
        let rule = transform(one_node(case.content), InstanceRouting::DefaultConnection);
        let config = first_config(&rule);
        assert_eq!(config["query"], case.query);
        assert_eq!(config["outputPath"], case.output);
        assert_eq!(config["passThrough"], case.pass);
        assert!(!config.contains_key("instance"), "{config:?}");
    }
}

#[test]
fn incomplete_content_still_becomes_a_custom_node() {
    let rule = transform(
        one_node(json!({"useCustomQuery": true, "instance": "x"})),
        InstanceRouting::DefaultConnection,
    );
    let config = first_config(&rule);
    assert!(!config.contains_key("query"));
    assert_eq!(config["editorInstance"], "x");
    assert_eq!(config["useCustomQuery"], true);
}

#[test]
fn compiled_keys_are_not_forwarded_and_others_are() {
    let rule = transform(
        one_node(json!({
            "useCustomQuery": true, "customQuery": "SELECT 1", "instance": "sr", "instanceName": "starrocks",
            "engine": "STARROCKS", "database": "shop", "resultHandling": {"none": "continue"},
            "selectedTables": [], "whereClauseGroup": {}
        })),
        InstanceRouting::DefaultConnection,
    );
    let config = first_config(&rule);
    for gone in [
        "useCustomQuery",
        "customQuery",
        "selectedTables",
        "whereClauseGroup",
        "instance",
    ] {
        assert!(!config.contains_key(gone), "{gone}: {config:?}");
    }
    assert_eq!(config["editorInstance"], "sr");
    assert_eq!(config["instanceName"], "starrocks");
    assert_eq!(config["engine"], "STARROCKS");
    assert_eq!(config["database"], "shop");
    assert_eq!(config["resultHandling"], json!({"none": "continue"}));
}

#[test]
fn routing_by_instance_keeps_the_instance_for_the_engine() {
    let content =
        json!({"useCustomQuery": true, "customQuery": "SELECT 1", "instance": "ddd3956b"});
    let routed = transform(one_node(content.clone()), InstanceRouting::ByInstance);
    let config = first_config(&routed);
    assert_eq!(config["instance"], "ddd3956b");
    assert!(!config.contains_key("editorInstance"));

    let carried = transform(
        one_node(json!({"editorInstance": "a"})),
        InstanceRouting::ByInstance,
    );
    assert_eq!(first_config(&carried)["instance"], "a");
}

#[test]
fn leaves_other_nodes_and_shapes_alone() {
    let rule = json!({"nodes": [{"id": "e", "type": "expressionNode", "content": {"expressions": []}}, 5, null]});
    assert_eq!(
        transform(rule.clone(), InstanceRouting::DefaultConnection),
        rule
    );
    for odd in [json!(null), json!([1]), json!({"nodes": {}}), json!({})] {
        assert_eq!(
            transform(odd.clone(), InstanceRouting::DefaultConnection),
            odd
        );
    }
    assert!(!has_editor_database_node(&rule));
    assert!(has_editor_database_node(&one_node(json!({}))));
}

#[test]
fn unsafe_sql_is_refused_naming_the_node() {
    let mut rule = json!({"nodes": [{"id": "db1", "type": "databaseNode", "content": {
        "selectedTables": [{"tableName": "orders", "columns": ["id"]}],
        "whereClauseGroup": {"logicalJoin": "AND", "children": [
            {"table": "orders", "column": "id", "operator": "= 1 OR 1=1 --", "value": "x"}]}}}]});
    let error = transform_database_nodes(&mut rule, InstanceRouting::DefaultConnection)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("\"db1\"") && error.contains("operator"),
        "{error}"
    );
}

#[test]
fn multi_table_where_is_escaped() {
    let rule = transform(
        one_node(json!({
            "database": "shop",
            "selectedTables": [{"tableName": "orders", "columns": ["id"]}, {"tableName": "items", "columns": ["sku"]}],
            "whereClauseGroup": {"logicalJoin": "AND", "children": [
                {"table": "orders", "column": "note", "operator": "=", "value": "\\' OR 1=1 -- "},
                {"table": "items", "column": "sku", "operator": "IN", "value": ["a'b"]}]}
        })),
        InstanceRouting::DefaultConnection,
    );
    let want = "SELECT to_json(named_struct('orders', (SELECT array_agg(named_struct('id', t.`id`)) FROM `shop`.`orders` t WHERE t.`note` = '\\\\'' OR 1=1 -- '), \
                'items', (SELECT array_agg(named_struct('sku', t.`sku`)) FROM `shop`.`items` t WHERE t.`sku` IN ('a''b')))) AS result";
    assert_eq!(first_config(&rule)["query"], want);
}

#[test]
fn request_values_compile_to_if_blocks() {
    let single = transform(
        one_node(json!({
            "database": "shop",
            "selectedTables": [{"tableName": "products", "columns": ["sku"]}],
            "whereClauseGroup": {"logicalJoin": "AND", "children": [
                {"table": "products", "column": "status", "operator": "=", "value": "active"},
                {"table": "products", "column": "colour", "operator": "=", "valueSource": "request", "requestField": "colour"}]}
        })),
        InstanceRouting::DefaultConnection,
    );
    assert_eq!(
        first_config(&single)["query"],
        "SELECT `sku` FROM `shop`.`products` WHERE (`status` = 'active' {{#if colour}} AND `colour` = {{ colour }} {{/if}})"
    );

    let several = transform(
        one_node(json!({
            "database": "shop",
            "selectedTables": [{"tableName": "products", "columns": ["sku"]}, {"tableName": "stock", "columns": ["qty"]}],
            "whereClauseGroup": {"logicalJoin": "AND", "children": [
                {"table": "products", "column": "colour", "operator": "=", "valueSource": "request", "requestField": "colour"},
                {"table": "stock", "column": "qty", "operator": ">", "value": 0}]}
        })),
        InstanceRouting::DefaultConnection,
    );
    assert_eq!(
        first_config(&several)["query"],
        "SELECT to_json(named_struct('products', (SELECT array_agg(named_struct('sku', t.`sku`)) FROM `shop`.`products` t WHERE (1 = 1 {{#if colour}} AND t.`colour` = {{ colour }} {{/if}})), \
         'stock', (SELECT array_agg(named_struct('qty', t.`qty`)) FROM `shop`.`stock` t WHERE t.`qty` > 0))) AS result"
    );
}

#[test]
fn prepared_content_is_a_decision_the_engine_reads() {
    let json = br#"{"nodes": [
        {"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}},
        {"id": "db", "name": "Lookup", "type": "databaseNode", "position": {"x": 0, "y": 0},
         "content": {"useCustomQuery": true, "customQuery": "SELECT 1"}},
        {"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}],
      "edges": []}"#;
    let content = prepare_decision_content(json, InstanceRouting::DefaultConnection).unwrap();
    assert!(content.as_graph().is_some());

    assert!(matches!(
        prepare_decision_content(b"{", InstanceRouting::DefaultConnection),
        Err(PrepareError::InvalidJson(_))
    ));
    assert!(matches!(
        prepare_decision_content(
            br#"{"nodes": [{"type": "nope"}]}"#,
            InstanceRouting::DefaultConnection
        ),
        Err(PrepareError::InvalidContent(_))
    ));
}
