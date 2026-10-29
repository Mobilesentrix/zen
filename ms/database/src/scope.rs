//! Per-evaluation de-duplication of identical queries.
//!
//! No row data is ever cached across requests. Inside ONE evaluate
//! call, however, two identical lookups (same connection, same SQL, same bound values) share one
//! round trip. The embedder opens the scope around the call with [`with_evaluation_scope`];
//! the table lives in a tokio task-local, so it is exact even when several evaluations
//! interleave on one thread (the FFI's batch path), and it is dropped when the call returns.
//! Outside a scope every query goes to the database.

use crate::bind::{BindValue, BoundQuery};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DedupKey {
    connection: String,
    sql: Arc<str>,
    params: Vec<BindValue>,
}

impl DedupKey {
    pub(crate) fn new(connection: &str, query: &BoundQuery) -> Self {
        Self {
            connection: connection.to_string(),
            sql: query.sql.clone(),
            params: query.params.clone(),
        }
    }
}

type Table = Rc<RefCell<HashMap<DedupKey, Rc<Vec<Value>>>>>;

tokio::task_local! {
    static EVALUATION: Table;
}

/// Runs `evaluation` with a fresh de-duplication table that is discarded when it returns.
pub async fn with_evaluation_scope<F: Future>(evaluation: F) -> F::Output {
    EVALUATION
        .scope(Rc::new(RefCell::new(HashMap::new())), evaluation)
        .await
}

/// Whether the current task runs inside [`with_evaluation_scope`].
pub fn in_evaluation_scope() -> bool {
    EVALUATION.try_with(|_| ()).is_ok()
}

/// Rows already fetched for `key` in this evaluation, if any (shared, not copied).
pub(crate) fn lookup(key: &DedupKey) -> Option<Rc<Vec<Value>>> {
    EVALUATION
        .try_with(|table| table.borrow().get(key).cloned())
        .ok()
        .flatten()
}

/// Remembers `rows` for `key` until the evaluation returns (no-op outside a scope).
pub(crate) fn store(key: DedupKey, rows: Rc<Vec<Value>>) {
    let _ = EVALUATION.try_with(|table| {
        table.borrow_mut().insert(key, rows);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn get(key: &DedupKey) -> Option<Vec<Value>> {
        lookup(key).map(|rows| rows.as_ref().clone())
    }

    fn key(sql: &str, n: i64) -> DedupKey {
        DedupKey::new(
            "sr",
            &BoundQuery {
                sql: sql.into(),
                params: vec![BindValue::Int(n)],
            },
        )
    }

    #[tokio::test]
    async fn outside_a_scope_nothing_is_remembered() {
        assert!(!in_evaluation_scope());
        store(key("SELECT 1", 1), Rc::new(vec![json!({"a": 1})]));
        assert_eq!(get(&key("SELECT 1", 1)), None);
    }

    #[tokio::test]
    async fn inside_a_scope_identical_queries_hit_and_the_table_dies_with_the_scope() {
        with_evaluation_scope(async {
            assert!(in_evaluation_scope());
            assert_eq!(get(&key("SELECT 1", 1)), None);
            store(key("SELECT 1", 1), Rc::new(vec![json!({"a": 1})]));
            assert_eq!(get(&key("SELECT 1", 1)), Some(vec![json!({"a": 1})]));
            // a different bound value or SQL is a different query
            assert_eq!(get(&key("SELECT 1", 2)), None);
            assert_eq!(get(&key("SELECT 2", 1)), None);
            // and so is the same statement on another connection
            let other = DedupKey::new(
                "pg",
                &BoundQuery {
                    sql: "SELECT 1".into(),
                    params: vec![BindValue::Int(1)],
                },
            );
            assert_eq!(get(&other), None);
        })
        .await;
        // the next evaluation starts empty
        with_evaluation_scope(async {
            assert_eq!(get(&key("SELECT 1", 1)), None);
        })
        .await;
        assert!(!in_evaluation_scope());
    }

    #[tokio::test]
    async fn concurrent_scopes_on_one_thread_do_not_share() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let a = tokio::task::spawn_local(with_evaluation_scope(async {
                    store(key("SELECT 1", 1), Rc::new(vec![json!({"from": "a"})]));
                    tokio::task::yield_now().await;
                    get(&key("SELECT 1", 1))
                }));
                let b = tokio::task::spawn_local(with_evaluation_scope(async {
                    tokio::task::yield_now().await;
                    get(&key("SELECT 1", 1))
                }));
                assert_eq!(a.await.unwrap(), Some(vec![json!({"from": "a"})]));
                assert_eq!(b.await.unwrap(), None);
            })
            .await;
    }
}
