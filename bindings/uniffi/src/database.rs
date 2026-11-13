//! The custom-node adapter of `ZenEngine::with_database`: the `database` node of
//! `ms-zen-database` in front of the foreign custom-node handler.

use crate::error::ZenError;
use ms_zen_database::{DatabaseCustomNode, DatabaseUnavailableNode, Databases, FallbackAdapter};
use std::sync::Arc;
use zen_engine::nodes::custom::DynamicCustomNode;

/// With a connection map, Lookup steps query it (pools created once, here); without one they
/// fail with "database node not available". Every other custom node goes to `fallback`.
pub(crate) fn database_adapter(
    database_config: Option<&str>,
    fallback: FallbackAdapter,
) -> Result<DynamicCustomNode, ZenError> {
    let Some(json) = database_config else {
        return Ok(Arc::new(DatabaseUnavailableNode::new(fallback)));
    };
    let databases =
        Databases::from_json(json).map_err(|e| ZenError::DatabaseConfigError(e.to_string()))?;
    Ok(Arc::new(DatabaseCustomNode::with_fallback(
        Arc::new(databases),
        fallback,
    )))
}

#[cfg(test)]
mod tests {
    use crate::engine::ZenEngine;
    use crate::error::ZenError;
    use crate::loader::{ZenDecisionLoaderCallback, ZenLoader};
    use crate::types::JsonBuffer;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Request -> Lookup step (editor form) -> Response.
    fn lookup_rule(content: Value) -> Value {
        json!({
            "nodes": [
                {"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}},
                {"id": "db", "name": "Stock", "type": "databaseNode", "position": {"x": 0, "y": 0},
                 "content": content},
                {"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}
            ],
            "edges": [
                {"id": "e1", "type": "edge", "sourceId": "in", "targetId": "db"},
                {"id": "e2", "type": "edge", "sourceId": "db", "targetId": "out"}
            ]
        })
    }

    fn buffer(value: &Value) -> JsonBuffer {
        JsonBuffer(serde_json::to_vec(value).unwrap())
    }

    fn static_loader(rule: &Value) -> ZenLoader {
        ZenLoader::Static {
            content: HashMap::from([("lookup".to_string(), buffer(rule))]),
        }
    }

    async fn evaluate(engine: &ZenEngine) -> Result<Value, ZenError> {
        let response = engine
            .evaluate(
                "lookup".to_string(),
                JsonBuffer(br#"{"sku":"A1"}"#.to_vec()),
                None,
            )
            .await?;
        Ok(response.result.try_into().unwrap())
    }

    fn custom_query() -> Value {
        lookup_rule(json!({
            "instance": "ddd3956b", "instanceName": "starrocks", "useCustomQuery": true,
            "customQuery": "SELECT qty FROM shop.stock WHERE sku = {{ sku }}"
        }))
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_database_config_a_lookup_fails_clearly() {
        let engine =
            ZenEngine::with_database(Some(static_loader(&custom_query())), None, None, false)
                .unwrap();
        let error = evaluate(&engine).await.unwrap_err().to_string();
        assert!(
            error.contains("database node not available") && error.contains("Stock"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_plain_constructor_still_refuses_editor_content() {
        let result = ZenEngine::new(Some(static_loader(&custom_query())), None);
        assert!(matches!(result, Err(ZenError::JsonDeserializationFailed)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_loader_kind_converts_lookup_steps() {
        struct Callback(Value);

        #[async_trait::async_trait]
        impl ZenDecisionLoaderCallback for Callback {
            async fn load(&self, _key: String) -> Result<Option<JsonBuffer>, ZenError> {
                Ok(Some(buffer(&self.0)))
            }
        }

        let rule = custom_query();
        let dir = std::env::temp_dir().join(format!("zen-uniffi-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lookup"), serde_json::to_vec(&rule).unwrap()).unwrap();

        let loaders = [
            static_loader(&rule),
            ZenLoader::Callback {
                callback: Arc::new(Callback(rule.clone())),
            },
            ZenLoader::Filesystem {
                path: dir.to_string_lossy().to_string(),
            },
        ];
        for loader in loaders {
            let engine = ZenEngine::with_database(Some(loader), None, None, false).unwrap();
            // Reaching the Lookup step proves the rule loaded and was converted.
            let error = evaluate(&engine).await.unwrap_err().to_string();
            assert!(error.contains("database node not available"), "{error}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unsafe_builder_sql_is_refused_when_loaded() {
        let rule = lookup_rule(json!({
            "selectedTables": [{"tableName": "orders", "columns": ["id"]}],
            "whereClauseGroup": {"logicalJoin": "AND", "children": [
                {"table": "orders", "column": "id", "operator": "= 1 OR 1=1 --", "value": "x"}]}
        }));
        let result = ZenEngine::with_database(Some(static_loader(&rule)), None, None, false);
        let error = result.err().expect("refused").to_string();
        assert!(
            error.contains("unsupported where operator") && error.contains("database node"),
            "{error}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_decision_converts_lookup_steps() {
        let engine = ZenEngine::with_database(None, None, None, false).unwrap();
        let decision = engine.create_decision(buffer(&custom_query())).unwrap();
        let error = decision
            .evaluate(JsonBuffer(br#"{"sku":"A1"}"#.to_vec()), None)
            .await
            .err()
            .expect("no database")
            .to_string();
        assert!(error.contains("database node not available"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn invalid_database_config_is_refused_without_the_url() {
        for bad in [
            "not json",
            r#"{"connections":{"x":{"url":"oracle://user:s3cret@h/db"}}}"#,
            r#"{"connections":{},"default":"missing"}"#,
        ] {
            let result = ZenEngine::with_database(None, None, Some(bad.to_string()), false);
            match result {
                Err(ZenError::DatabaseConfigError(message)) => {
                    assert!(!message.contains("s3cret"), "{message}")
                }
                Err(other) => panic!("{bad}: unexpected error {other}"),
                Ok(_) => panic!("{bad}: accepted"),
            }
        }
    }

    /// A configured but unreachable instance: the step reaches the database node, which reports
    /// the connection failure (no live database in unit tests).
    #[tokio::test(flavor = "multi_thread")]
    async fn routing_by_instance_resolves_the_editor_instance() {
        let config = json!({"connections": {"starrocks": {
            "url": "mysql://user:pw@127.0.0.1:9/shop", "aliases": ["ddd3956b"], "acquireTimeout": 500
        }}});
        let unknown = lookup_rule(json!({
            "instance": "elsewhere", "useCustomQuery": true, "customQuery": "SELECT 1"
        }));
        let engine = ZenEngine::with_database(
            Some(static_loader(&unknown)),
            None,
            Some(config.to_string()),
            true,
        )
        .unwrap();
        let error = evaluate(&engine).await.unwrap_err().to_string();
        assert!(
            error.contains("database instance \\\"elsewhere\\\" is not configured")
                || error.contains("database instance \"elsewhere\" is not configured"),
            "{error}"
        );

        let engine = ZenEngine::with_database(
            Some(static_loader(&custom_query())),
            None,
            Some(config.to_string()),
            true,
        )
        .unwrap();
        let error = evaluate(&engine).await.unwrap_err().to_string();
        assert!(!error.contains("not configured"), "{error}");
        assert!(!error.contains("pw@"), "{error}");
    }
}
