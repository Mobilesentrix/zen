//! Where the time goes in a Database-node call. Runs against engine-v2/testdb (MS_TESTDB_STARROCKS_URL /
//! MS_TESTDB_PG_URL) and prints a `DBSPEED` table; nothing is asserted beyond sanity.
//!
//! Phases per query: pool acquire (incl. sqlx's test-before-acquire ping), server round trip of
//! a trivial statement, prepare (persistent vs re-prepared), the point lookup itself, row decode
//! and the JSON -> Variable conversion, then the full node through the engine.

use ms_zen_database::{BindValue, BoundQuery, DatabaseConfig, DatabaseCustomNode, Databases};
use serde_json::{json, Value};
use sqlx::mysql::MySqlPoolOptions;
use sqlx::postgres::PgPoolOptions;
use sqlx::{Column, ConnectOptions, Row};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{DecisionEngine, EvaluationOptions, Variable};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set; start engine-v2/testdb"))
}

const N: usize = 300;

struct Stats {
    p50: Duration,
    p99: Duration,
    min: Duration,
}

fn stats(mut samples: Vec<Duration>) -> Stats {
    samples.sort();
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q) as usize];
    Stats {
        p50: at(0.5),
        p99: at(0.99),
        min: samples[0],
    }
}

fn row(name: &str, s: &Stats) {
    println!(
        "DBSPEED {name:<52} p50 {:>9.1?}  p99 {:>9.1?}  min {:>9.1?}",
        s.p50, s.p99, s.min
    );
}

async fn timed<F, Fut, T>(mut f: F) -> Stats
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let mut samples = Vec::with_capacity(N);
    for _ in 0..N {
        let start = Instant::now();
        let _ = f().await;
        samples.push(start.elapsed());
    }
    stats(samples)
}

/// StarRocks: ms_analytics.catalog_product (DUPLICATE KEY, 26 rows); Postgres: samplePg.orders.
const SR_POINT: &str =
    "SELECT id, code, sku, status, price FROM ms_analytics.catalog_product WHERE sku = ?";
const PG_POINT: &str =
    "SELECT increment_id, status, grand_total FROM orders WHERE increment_id = $1";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
async fn starrocks_breakdown() {
    let url = env("MS_TESTDB_STARROCKS_URL").replacen("starrocks://", "mysql://", 1);
    let opts = sqlx::mysql::MySqlConnectOptions::from_str(&url)
        .unwrap()
        .pipes_as_concat(false)
        .no_engine_substitution(false)
        .disable_statement_logging();
    println!("DBSPEED == StarRocks (testdb), {N} iterations each, warm pool ==");

    for test_before_acquire in [true, false] {
        let pool = MySqlPoolOptions::new()
            .max_connections(2)
            .test_before_acquire(test_before_acquire)
            .connect_lazy_with(opts.clone());
        drop(pool.acquire().await.unwrap());
        let s = timed(|| async { drop(pool.acquire().await.unwrap()) }).await;
        row(
            &format!("pool acquire (testBeforeAcquire={test_before_acquire})"),
            &s,
        );
        let s = timed(|| async {
            sqlx::query("SELECT 1").fetch_one(&pool).await.unwrap();
        })
        .await;
        row(
            &format!("SELECT 1 round trip (testBeforeAcquire={test_before_acquire})"),
            &s,
        );
        if test_before_acquire {
            // prepared + cached (default) vs re-prepared every time
            let s = timed(|| async {
                sqlx::query(SR_POINT)
                    .bind("107000001022")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            })
            .await;
            row("point lookup, prepared statement cached", &s);
            let s = timed(|| async {
                sqlx::query(SR_POINT)
                    .bind("107000001022")
                    .persistent(false)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            })
            .await;
            row("point lookup, re-prepared each call (persistent=false)", &s);
            let s = timed(|| async {
                sqlx::query("SELECT id, code, sku, status, price FROM ms_analytics.catalog_product WHERE sku = '107000001022'")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            })
            .await;
            row("point lookup, literal SQL (text protocol path)", &s);
            // decode + JSON conversion, measured in-process on one fetched row set
            let rows = sqlx::query(SR_POINT)
                .bind("107000001022")
                .fetch_all(&pool)
                .await
                .unwrap();
            let mut samples = Vec::with_capacity(N);
            for _ in 0..N {
                let start = Instant::now();
                let values: Vec<Value> = rows
                    .iter()
                    .map(|r| {
                        let mut object = serde_json::Map::new();
                        for (i, c) in r.columns().iter().enumerate() {
                            let v: Option<String> = r.try_get_unchecked(i).ok();
                            object.insert(
                                c.name().to_string(),
                                v.map(Value::String).unwrap_or(Value::Null),
                            );
                        }
                        Value::Object(object)
                    })
                    .collect();
                let _ = Variable::from(json!({"rows": values, "rowsCount": rows.len()}));
                samples.push(start.elapsed());
            }
            row(
                "decode rows + JSON -> Variable (in-process, 1 row)",
                &stats(samples),
            );
        }
    }

    let dbs = Arc::new(
        Databases::new(
            serde_json::from_value::<DatabaseConfig>(json!({"connections": {
                "sr": {"url": env("MS_TESTDB_STARROCKS_URL"), "maxConnections": 2}
            }}))
            .unwrap(),
        )
        .unwrap(),
    );
    let conn = dbs.resolve(None).unwrap();
    let query = || BoundQuery {
        sql: SR_POINT.into(),
        params: vec![BindValue::Text("107000001022".into())],
    };
    dbs.execute(&conn, query()).await.unwrap();
    let s = timed(|| async {
        dbs.execute(&conn, query()).await.unwrap();
    })
    .await;
    row("ms-zen-database execute (spawn on DB runtime + decode)", &s);

    let engine = engine_for(
        dbs.clone(),
        "sr",
        "SELECT id, code, sku, status, price FROM ms_analytics.catalog_product WHERE sku = {{sku}}",
    );
    let input = json!({"sku": "107000001022"});
    evaluate(&engine, input.clone()).await;
    let s = timed(|| evaluate(&engine, input.clone())).await;
    row("engine evaluate: request -> database node -> response", &s);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL / MS_TESTDB_PG_URL)"]
async fn postgres_breakdown() {
    let url = env("MS_TESTDB_PG_URL");
    let opts = sqlx::postgres::PgConnectOptions::from_str(&url)
        .unwrap()
        .disable_statement_logging();
    println!("DBSPEED == Postgres (testdb), {N} iterations each, warm pool ==");
    for test_before_acquire in [true, false] {
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .test_before_acquire(test_before_acquire)
            .connect_lazy_with(opts.clone());
        drop(pool.acquire().await.unwrap());
        let s = timed(|| async { drop(pool.acquire().await.unwrap()) }).await;
        row(
            &format!("pool acquire (testBeforeAcquire={test_before_acquire})"),
            &s,
        );
        let s = timed(|| async {
            sqlx::query("SELECT 1").fetch_one(&pool).await.unwrap();
        })
        .await;
        row(
            &format!("SELECT 1 round trip (testBeforeAcquire={test_before_acquire})"),
            &s,
        );
        if test_before_acquire {
            let s = timed(|| async {
                sqlx::query(PG_POINT)
                    .bind("US100000101")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            })
            .await;
            row("point lookup, prepared statement cached", &s);
            let s = timed(|| async {
                sqlx::query(PG_POINT)
                    .bind("US100000101")
                    .persistent(false)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            })
            .await;
            row("point lookup, re-prepared each call (persistent=false)", &s);
        }
    }
    let dbs = Arc::new(
        Databases::new(
            serde_json::from_value::<DatabaseConfig>(json!({"connections": {
                "pg": {"url": url, "maxConnections": 2}
            }}))
            .unwrap(),
        )
        .unwrap(),
    );
    let conn = dbs.resolve(None).unwrap();
    let query = || BoundQuery {
        sql: PG_POINT.into(),
        params: vec![BindValue::Text("US100000101".into())],
    };
    dbs.execute(&conn, query()).await.unwrap();
    let s = timed(|| async {
        dbs.execute(&conn, query()).await.unwrap();
    })
    .await;
    row("ms-zen-database execute (spawn on DB runtime + decode)", &s);
    let engine = engine_for(
        dbs.clone(),
        "pg",
        "SELECT increment_id, status, grand_total FROM orders WHERE increment_id = {{id}}",
    );
    let input = json!({"id": "US100000101"});
    evaluate(&engine, input.clone()).await;
    let s = timed(|| evaluate(&engine, input.clone())).await;
    row("engine evaluate: request -> database node -> response", &s);
}

fn engine_for(dbs: Arc<Databases>, instance: &str, sql: &str) -> DecisionEngine {
    let content: DecisionContent = serde_json::from_value(json!({
        "nodes": [
            {"id": "in", "name": "Request", "type": "inputNode", "position": {"x": 0, "y": 0}},
            {"id": "db", "name": "db", "type": "customNode", "position": {"x": 0, "y": 0},
             "content": {"kind": "database", "config": {"instance": instance, "query": sql, "passThrough": false}}},
            {"id": "out", "name": "Response", "type": "outputNode", "position": {"x": 0, "y": 0}}
        ],
        "edges": [
            {"id": "e1", "type": "edge", "sourceId": "in", "targetId": "db"},
            {"id": "e2", "type": "edge", "sourceId": "db", "targetId": "out"}
        ]
    }))
    .unwrap();
    let loader = MemoryLoader::default();
    loader.add("d", content);
    DecisionEngine::new(Arc::new(loader), Arc::new(DatabaseCustomNode::new(dbs)))
}

async fn evaluate(engine: &DecisionEngine, input: Value) {
    engine
        .evaluate_with_opts(
            "d",
            Variable::from(input),
            EvaluationOptions {
                trace: false,
                max_depth: 5,
            },
        )
        .await
        .unwrap();
}

/// Short-circuit comparison: one product by SKU through the real node path,
/// (a) the DUPLICATE KEY table, short-circuit off; (b) a PRIMARY KEY copy (column store)
/// with `enable_short_circuit=true` on the connection; (c) the PRIMARY KEY + row-store copy
/// (`storage_type = column_with_row`, needs FE `enable_experimental_rowstore` on 3.5) with
/// short-circuit; plus (a) again with `testBeforeAcquire: false`. Tables `catalog_product_pk`
/// / `catalog_product_pkrow` are created ad hoc on the local testdb.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL) with the catalog_product_pk* copies"]
async fn starrocks_short_circuit_study() {
    let n: usize = std::env::var("DBSPEED_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(N);
    let url = env("MS_TESTDB_STARROCKS_URL");
    let connection = |session: Value, test_before_acquire: bool| {
        Arc::new(
            Databases::new(
                serde_json::from_value::<DatabaseConfig>(json!({"connections": {"sr": {
                    "url": url, "maxConnections": 2, "minConnections": 1,
                    "sessionVariables": session, "testBeforeAcquire": test_before_acquire
                }}}))
                .unwrap(),
            )
            .unwrap(),
        )
    };
    let cases = [
        (
            "(a) today: DUPLICATE KEY table, short-circuit off",
            "catalog_product",
            json!({}),
            true,
        ),
        (
            "(a') same, testBeforeAcquire=false",
            "catalog_product",
            json!({}),
            false,
        ),
        (
            "(b) PRIMARY KEY(sku) copy + enable_short_circuit",
            "catalog_product_pk",
            json!({"enable_short_circuit": "true"}),
            true,
        ),
        (
            "(c) PK + column_with_row copy + enable_short_circuit",
            "catalog_product_pkrow",
            json!({"enable_short_circuit": "true"}),
            true,
        ),
        (
            "(c') same, testBeforeAcquire=false",
            "catalog_product_pkrow",
            json!({"enable_short_circuit": "true"}),
            false,
        ),
    ];
    println!("DBSPEED == StarRocks short-circuit study, {n} iterations each, warm, one product by SKU ==");
    let mut reference: Option<Value> = None;
    for (label, table, session, test_before_acquire) in cases {
        let dbs = connection(session, test_before_acquire);
        let sql = format!(
            "SELECT id, code, sku, status, price FROM ms_analytics.{table} WHERE sku = {{{{sku}}}}"
        );
        let engine = engine_for(dbs.clone(), "sr", &sql);
        let input = json!({"sku": "107000001022"});
        let first = {
            let start = Instant::now();
            evaluate(&engine, input.clone()).await;
            start.elapsed()
        };
        let mut samples = Vec::with_capacity(n);
        let mut last = Value::Null;
        for _ in 0..n {
            let start = Instant::now();
            let r = engine
                .evaluate_with_opts(
                    "d",
                    Variable::from(input.clone()),
                    EvaluationOptions {
                        trace: false,
                        max_depth: 5,
                    },
                )
                .await
                .unwrap();
            samples.push(start.elapsed());
            last = serde_json::to_value(&r).unwrap()["result"]["dbResult"].clone();
        }
        // identical rows across all variants (the copies hold the same product)
        match &reference {
            None => reference = Some(last.clone()),
            Some(expected) => {
                assert_eq!(&last, expected, "{label}: rows differ from today's table")
            }
        }
        let s = stats(samples);
        println!(
            "DBSPEED {label:<58} p50 {:>9.1?}  p99 {:>9.1?}  min {:>9.1?}  first {:>9.1?}",
            s.p50, s.p99, s.min, first
        );
    }
}

/// Attribution for the wide corpus query (api/stock-feed/products, 12 KB SQL, 24 placeholders,
/// 56 rows): binary protocol with bound parameters (what the node does) vs the same statement
/// with literal values over the text protocol (test only — the node never splices values),
/// and the node path on top. Fixtures: ms-test/fixtures/wide{,-literal}.sql (untracked).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs engine-v2/testdb (MS_TESTDB_STARROCKS_URL) and ms-test/fixtures/wide*.sql"]
async fn starrocks_wide_query_protocols() {
    let n: usize = std::env::var("DBSPEED_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../ms-test/fixtures/wide.sql"
    ))
    .expect("wide.sql");
    let literal = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../ms-test/fixtures/wide-literal.sql"
    ))
    .expect("wide-literal.sql");
    let url = env("MS_TESTDB_STARROCKS_URL").replacen("starrocks://", "mysql://", 1);
    let opts = sqlx::mysql::MySqlConnectOptions::from_str(&url)
        .unwrap()
        .pipes_as_concat(false)
        .no_engine_substitution(false)
        .disable_statement_logging();
    let pool = MySqlPoolOptions::new()
        .max_connections(2)
        .test_before_acquire(false)
        .connect_lazy_with(opts);
    println!("DBSPEED == StarRocks wide query (api/stock-feed/products), {n} iterations, warm ==");
    // text protocol, literal values
    let rows = sqlx::raw_sql(literal.as_str())
        .fetch_all(&pool)
        .await
        .unwrap();
    println!("DBSPEED rows returned: {}", rows.len());
    let s = timed(|| async {
        sqlx::raw_sql(literal.as_str())
            .fetch_all(&pool)
            .await
            .unwrap();
    })
    .await;
    row("text protocol, literal SQL, raw sqlx rows (no decode)", &s);
    // binary protocol, bound params (same shape the node sends), raw rows
    let bound_sql = question_marks(&sql);
    let params: Vec<&str> = regex_like_params(&sql);
    let s = timed(|| async {
        let mut q = sqlx::query(&bound_sql);
        for p in &params {
            q = match *p {
                "site" => q.bind("eu"),
                _ => q.bind(0i64),
            };
        }
        q.fetch_all(&pool).await.unwrap();
    })
    .await;
    row(
        "binary protocol, bound params, raw sqlx rows (no decode)",
        &s,
    );
    // the node path: bind + execute on the DB runtime + streaming decode to JSON
    let dbs = Arc::new(
        Databases::new(
            serde_json::from_value::<DatabaseConfig>(json!({"connections": {"sr": {"url": env("MS_TESTDB_STARROCKS_URL"), "maxConnections": 2, "testBeforeAcquire": false}}}))
                .unwrap(),
        )
        .unwrap(),
    );
    let engine = engine_for(dbs.clone(), "sr", &sql);
    let input = json!({"site": "eu", "after_id": 0});
    evaluate(&engine, input.clone()).await;
    let s = timed(|| evaluate(&engine, input.clone())).await;
    row(
        "engine evaluate (node path: bind + binary + decode + JSON)",
        &s,
    );
}

/// Every `{{ … }}` replaced by `?` (test-only rewrite; the node's parser does this properly).
fn question_marks(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        out.push('?');
        let after = &rest[start + 2..];
        let end = after.find("}}").unwrap();
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Placeholder names in order of appearance (the wide query only uses `site` and `after_id`).
fn regex_like_params(sql: &str) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut rest = sql;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after.find("}}").unwrap();
        let name = after[..end].trim();
        out.push(if name == "site" { "site" } else { "after_id" });
        rest = &after[end + 2..];
    }
    out
}
