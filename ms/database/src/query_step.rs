//! Binding and running one database step's query, once or per loop item.

use crate::bind::{bind, BindError, BoundQuery, NodeLabel};
use crate::connection::Connection;
use crate::databases::Databases;
use crate::scope::{self, DedupKey, SharedRows};
use crate::template::QueryTemplate;
use futures_util::stream::{self, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use zen_engine::nodes::failure::{
    missing_reads, FailedNode, FailureKind, FailurePart, NodeFailure,
};
use zen_expression::Variable;

/// Queries one loop-mode node keeps in flight at once (the connection's pool still caps the
/// total across evaluations).
const LOOP_CONCURRENCY: usize = 8;

/// The rows of one query, or why they could not be fetched.
type QueryResult = Result<SharedRows, Box<FailedNode>>;

/// Everything that stays the same across the queries of one node run.
#[derive(Clone, Copy)]
pub(crate) struct QueryStep<'a> {
    pub(crate) databases: &'a Databases,
    pub(crate) sql: &'a str,
    pub(crate) template: &'a QueryTemplate,
    pub(crate) label: NodeLabel<'a>,
    pub(crate) connection: &'a Arc<Connection>,
}

impl QueryStep<'_> {
    /// Binds and runs the query once; the rows as decoded JSON objects.
    pub(crate) async fn query_once(
        &self,
        context: &Variable,
    ) -> Result<Vec<Value>, Box<FailedNode>> {
        let bound =
            bind(self.template, context, self.label).map_err(|e| bind_failure(&e, context))?;
        self.execute_bound(bound).await.map(unshare)
    }

    /// Loop mode: one query per item. The result per item is in item order and ends at the first
    /// failure in that order; the caller stops there.
    pub(crate) async fn query_items(&self, items: &[Variable]) -> Vec<QueryResult> {
        if self.template.is_read_only() {
            self.query_items_concurrently(items).await
        } else {
            self.query_items_in_order(items).await
        }
    }

    /// One item after another, stopping at the first failure.
    async fn query_items_in_order(&self, items: &[Variable]) -> Vec<QueryResult> {
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            let result = match bind(self.template, item, self.label) {
                Ok(bound) => self.execute_bound(bound).await,
                Err(e) => Err(bind_failure(&e, item)),
            };
            let failed = result.is_err();
            results.push(result);
            if failed {
                break;
            }
        }
        results
    }

    /// Up to [`LOOP_CONCURRENCY`] queries at once, identical queries sent once. The error
    /// reported is the one a one-at-a-time loop would report: items after a failed bind are never
    /// queried, and a query whose item comes after a failure is only wasted read work.
    async fn query_items_concurrently(&self, items: &[Variable]) -> Vec<QueryResult> {
        let bound = self.bind_until_failure(items);
        let (unique, answers) = self.assign_queries(bound);
        let rows: Vec<QueryResult> =
            stream::iter(unique.into_iter().map(|query| self.execute_bound(query)))
                .buffered(LOOP_CONCURRENCY)
                .collect()
                .await;
        hand_out_rows(answers, rows)
    }

    /// Binds items in order; the list ends at the first item that cannot be bound.
    fn bind_until_failure(&self, items: &[Variable]) -> Vec<Result<BoundQuery, Box<FailedNode>>> {
        let mut bound = Vec::with_capacity(items.len());
        for item in items {
            match bind(self.template, item, self.label) {
                Ok(query) => bound.push(Ok(query)),
                Err(e) => {
                    bound.push(Err(bind_failure(&e, item)));
                    break;
                }
            }
        }
        bound
    }

    /// The queries to send, and per item the index of the query that answers it.
    fn assign_queries(
        &self,
        bound: Vec<Result<BoundQuery, Box<FailedNode>>>,
    ) -> (Vec<BoundQuery>, Vec<Result<usize, Box<FailedNode>>>) {
        let mut unique: Vec<BoundQuery> = Vec::new();
        let mut answers = Vec::with_capacity(bound.len());
        let mut seen: HashMap<DedupKey, usize> = HashMap::new();
        for item in bound {
            answers.push(item.map(|query| {
                if self.connection.dedupe() {
                    *seen
                        .entry(DedupKey::new(self.connection.name(), &query))
                        .or_insert_with(|| {
                            unique.push(query);
                            unique.len() - 1
                        })
                } else {
                    unique.push(query);
                    unique.len() - 1
                }
            }));
        }
        (unique, answers)
    }

    /// Runs a bound query. Inside one evaluation scope a read-only query identical to one
    /// already run on the same connection is answered from those rows (never from a previous
    /// call); any other statement always goes to the database.
    async fn execute_bound(&self, bound: BoundQuery) -> QueryResult {
        let key = (self.connection.dedupe() && self.template.is_read_only())
            .then(|| DedupKey::new(self.connection.name(), &bound));
        if let Some(rows) = key.as_ref().and_then(scope::lookup) {
            return Ok(rows);
        }
        let rows = self
            .databases
            .execute(self.connection, bound)
            .await
            .map_err(|e| {
                let failure = NodeFailure::new(FailureKind::Evaluation, e.to_string())
                    .with_part(FailurePart::Query)
                    .with_expression(self.sql);
                Box::new(FailedNode::new(
                    failure,
                    format!("Database query execution failed: {e}"),
                ))
            })?;
        let rows = Rc::new(rows);
        if let Some(key) = key {
            scope::store(key, rows.clone());
        }
        Ok(rows)
    }
}

/// Hands each query's rows to the items it answers, in item order. The list ends at the first
/// failure: the caller stops there, so nothing after it is needed.
fn hand_out_rows(
    answers: Vec<Result<usize, Box<FailedNode>>>,
    rows: Vec<QueryResult>,
) -> Vec<QueryResult> {
    let mut rows: Vec<Result<SharedRows, Option<Box<FailedNode>>>> =
        rows.into_iter().map(|row| row.map_err(Some)).collect();
    let mut results = Vec::with_capacity(answers.len());
    for answer in answers {
        let result = match answer {
            Err(failure) => Err(failure),
            Ok(query_index) => match &mut rows[query_index] {
                Ok(shared) => Ok(shared.clone()),
                Err(failure) => match failure.take() {
                    Some(failure) => Err(failure),
                    // an earlier item with this query failed; the list already ended
                    None => break,
                },
            },
        };
        let failed = result.is_err();
        results.push(result);
        if failed {
            break;
        }
    }
    results
}

/// The rows for one consumer: moved out when nobody else holds them, copied otherwise.
pub(crate) fn unshare(rows: SharedRows) -> Vec<Value> {
    Rc::try_unwrap(rows).unwrap_or_else(|shared| shared.as_ref().clone())
}

/// A placeholder that could not be bound: the placeholder, its kind and, for a null, the paths
/// it reads that are absent or null in the input.
fn bind_failure(error: &BindError, context: &Variable) -> Box<FailedNode> {
    let message = error.to_string();
    let failure = match error {
        BindError::Placeholder {
            expression, kind, ..
        } => {
            let missing = match kind {
                FailureKind::MissingValue => missing_reads(expression, false, context, None),
                _ => Vec::new(),
            };
            NodeFailure::new(*kind, message.clone())
                .with_part(FailurePart::Placeholder)
                .with_expression(expression.as_str())
                .with_missing(missing)
        }
        BindError::TooManyParams { .. } => {
            NodeFailure::new(FailureKind::Evaluation, message.clone()).with_part(FailurePart::Query)
        }
    };
    Box::new(FailedNode::new(failure, message))
}
