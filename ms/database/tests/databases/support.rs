//! Helpers shared by the integration tests.

use ms_zen_database::{DatabaseConfig, DatabaseCustomNode, Databases};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::nodes::custom::{CustomNodeAdapter, CustomNodeRequest};
use zen_engine::nodes::{NodeResponse, NodeResult};
use zen_engine::{DecisionEngine, EvaluationOptions, Variable};

pub(crate) const NEEDS_DB: &str = "needs ms-test containers (MS_TEST_MYSQL_URL / MS_TEST_PG_URL)";

pub(crate) fn env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set; start ms-test/docker-compose.yml"))
}

pub(crate) fn databases(extra: Value) -> Arc<Databases> {
    let mut cfg = json!({
        "connections": {
            "mysql-instance": { "url": env("MS_TEST_MYSQL_URL"), "aliases": ["starrocks"], "maxConnections": 8 },
            "pg-instance": { "url": env("MS_TEST_PG_URL"), "maxConnections": 8 }
        },
        "default": "mysql-instance"
    });
    if let (Some(c), Some(e)) = (cfg.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            c.insert(k.clone(), v.clone());
        }
    }
    let config: DatabaseConfig = serde_json::from_value(cfg).unwrap();
    Arc::new(Databases::new(config).unwrap())
}

/// Fallback adapter standing in for the Go custom-node callback.
#[derive(Debug)]
struct EchoFallback;

impl CustomNodeAdapter for EchoFallback {
    fn handle(&self, request: CustomNodeRequest) -> Pin<Box<dyn Future<Output = NodeResult> + '_>> {
        Box::pin(async move {
            Ok(NodeResponse {
                output: Variable::from(json!({ "fallbackKind": request.node.kind.as_ref() })),
                trace_data: None,
            })
        })
    }
}

pub(crate) fn db_node(id: &str, config: Value) -> Value {
    json!({ "id": id, "name": id, "type": "customNode", "position": {"x": 0, "y": 0},
            "content": { "kind": "database", "config": config } })
}

pub(crate) fn graph(nodes: Vec<Value>) -> DecisionContent {
    let mut all = vec![
        json!({"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}}),
    ];
    all.extend(nodes);
    all.push(json!({"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}));
    let edges: Vec<Value> = all
        .windows(2)
        .enumerate()
        .map(|(i, w)| json!({"id": format!("e{i}"), "type": "edge", "sourceId": w[0]["id"], "targetId": w[1]["id"]}))
        .collect();
    serde_json::from_value(json!({ "nodes": all, "edges": edges })).unwrap()
}

pub(crate) fn engine_with(
    dbs: Arc<Databases>,
    decisions: Vec<(&str, DecisionContent)>,
) -> DecisionEngine {
    let loader = MemoryLoader::default();
    for (k, d) in decisions {
        loader.add(k, d);
    }
    let adapter = DatabaseCustomNode::with_fallback(dbs, Box::new(EchoFallback));
    DecisionEngine::new(Arc::new(loader), Arc::new(adapter))
}

/// A current-thread runtime driven by block_on: the pools must work from an executor that
/// only runs inside the caller's block_on.
pub(crate) fn eval(engine: &DecisionEngine, key: &str, input: Value) -> Result<Value, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        engine
            .evaluate_with_opts(
                key,
                Variable::from(input),
                EvaluationOptions {
                    trace: true,
                    max_depth: 5,
                },
            )
            .await
            .map(|r| serde_json::to_value(&r).unwrap())
            .map_err(|e| serde_json::to_value(&e).unwrap().to_string())
    })
}

pub(crate) fn databases_from(cfg: Value) -> Arc<Databases> {
    Arc::new(Databases::new(serde_json::from_value(cfg).unwrap()).unwrap())
}
