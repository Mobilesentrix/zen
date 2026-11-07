//! StarRocks re-executes a cached server-side prepared statement whose plan has a correlated
//! subquery with the parameter values of its FIRST execution (3.5.20 and 4.1.4; see
//! docs/starrocks-prepared-statement-stale-params.md). A Lookup must still return the row asked
//! for. Needs a StarRocks FE at MS_TEST_STARROCKS_URL (e.g. `mysql://root@starrocks:9030`); the
//! tests create their own fixture database. See MS-BUILD.md ("StarRocks prepared statements").

use ms_zen_database::{BindValue, BoundQuery, Databases};
use serde_json::json;
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection};
use sqlx::{ConnectOptions, Connection as _, Executor, Row};
use std::str::FromStr;
use std::sync::Once;

const NEEDS_STARROCKS: &str = "needs a StarRocks FE (MS_TEST_STARROCKS_URL)";
const FIXTURE_DB: &str = "ms_zen_stale_params";

/// Two runs on each of two dates, so both the date and the id select the row.
const RUNS: [(&str, i64); 4] = [
    ("2026-11-01", 13962),
    ("2026-11-05", 14239),
    ("2026-11-01", 13964),
    ("2026-11-05", 14236),
];

/// The shape that came back wrong on admin.dev: two parameters, a correlated scalar subquery.
const SUBQUERY_SQL: &str = "SELECT r.run_id AS runId, \
     (SELECT MAX(i.settings_cutoff) FROM ms_zen_stale_params.po_plan_run_inputs i \
       WHERE i.run_date = r.run_date AND i.run_id = r.run_id) AS cutoff \
     FROM ms_zen_stale_params.po_plan_run r WHERE r.run_date = ? AND r.run_id = ?";

/// The lookup-phase anchor: two correlated subqueries, one of them over the same table.
const ANCHOR_SQL: &str = "SELECT r.run_id AS runId, r.execution_id AS executionId, \
     (SELECT MIN(x.start_time) FROM ms_zen_stale_params.po_plan_run x \
       WHERE x.execution_id = r.execution_id AND x.algo_name = 'PO_PLAN') AS runStarted, \
     (SELECT MAX(i.settings_cutoff) FROM ms_zen_stale_params.po_plan_run_inputs i \
       WHERE i.run_date = r.run_date AND i.run_id = r.run_id) AS cutoff \
     FROM ms_zen_stale_params.po_plan_run r WHERE r.run_date = ? AND r.run_id = ?";

fn url() -> String {
    std::env::var("MS_TEST_STARROCKS_URL")
        .unwrap_or_else(|_| panic!("MS_TEST_STARROCKS_URL is not set; {NEEDS_STARROCKS}"))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn connect() -> MySqlConnection {
    MySqlConnectOptions::from_str(&url().replacen("starrocks://", "mysql://", 1))
        .unwrap()
        .pipes_as_concat(false)
        .no_engine_substitution(false)
        .connect()
        .await
        .unwrap()
}

/// (Re)creates the fixture once per test binary (the tests run in parallel).
fn ensure_fixture() {
    static FIXTURE: Once = Once::new();
    FIXTURE.call_once(|| runtime().block_on(fixture()));
}

/// Partitioned DUPLICATE KEY tables shaped like ms_analytics.po_plan_run*.
async fn fixture() {
    let mut connection = connect().await;
    let statements = [
        format!("CREATE DATABASE IF NOT EXISTS {FIXTURE_DB}"),
        format!("DROP TABLE IF EXISTS {FIXTURE_DB}.po_plan_run"),
        format!("DROP TABLE IF EXISTS {FIXTURE_DB}.po_plan_run_inputs"),
        format!(
            "CREATE TABLE {FIXTURE_DB}.po_plan_run (run_date DATE NOT NULL, run_id BIGINT NOT NULL, \
             execution_id VARCHAR(64), algo_name VARCHAR(32), start_time DATETIME) \
             DUPLICATE KEY(run_date, run_id) PARTITION BY date_trunc('day', run_date) \
             DISTRIBUTED BY HASH(run_id) BUCKETS 2 PROPERTIES (\"replication_num\" = \"1\")"
        ),
        format!(
            "CREATE TABLE {FIXTURE_DB}.po_plan_run_inputs (run_date DATE NOT NULL, run_id BIGINT NOT NULL, \
             settings_cutoff DATETIME) \
             DUPLICATE KEY(run_date, run_id) PARTITION BY date_trunc('day', run_date) \
             DISTRIBUTED BY HASH(run_id) BUCKETS 2 PROPERTIES (\"replication_num\" = \"1\")"
        ),
        format!(
            "INSERT INTO {FIXTURE_DB}.po_plan_run VALUES \
             ('2026-11-01', 13962, 'e1', 'PO_PLAN', '2026-11-01 01:00:00'), \
             ('2026-11-01', 13964, 'e1', 'PO_PLAN', '2026-11-01 01:05:00'), \
             ('2026-11-05', 14236, 'e2', 'PO_PLAN', '2026-11-05 01:00:00'), \
             ('2026-11-05', 14239, 'e2', 'PO_PLAN', '2026-11-05 01:05:00')"
        ),
        format!(
            "INSERT INTO {FIXTURE_DB}.po_plan_run_inputs VALUES \
             ('2026-11-01', 13962, '2026-10-31 00:00:00'), ('2026-11-01', 13964, '2026-10-31 00:00:00'), \
             ('2026-11-05', 14236, '2026-11-04 00:00:00'), ('2026-11-05', 14239, '2026-11-04 00:00:00')"
        ),
    ];
    for statement in &statements {
        connection.execute(statement.as_str()).await.unwrap();
    }
    connection.close().await.unwrap();
}

/// One pooled connection, so every query after the first reuses the same server session.
fn one_connection() -> Databases {
    Databases::from_json(
        &json!({ "connections": { "sr": { "url": url(), "maxConnections": 1 } } }).to_string(),
    )
    .unwrap()
}

/// Every (date, id) in [`RUNS`], five times round, through the crate on one connection; the
/// ids asked for whose answer was another run (or not exactly one row).
fn wrong_answers(databases: &Databases, sql: &str) -> Vec<String> {
    let connection = databases.resolve(Some("sr")).unwrap();
    let rt = runtime();
    let mut wrong = Vec::new();
    for _ in 0..5 {
        for (date, run_id) in RUNS {
            let query = BoundQuery {
                sql: sql.into(),
                params: vec![BindValue::Text(date.to_string()), BindValue::Int(run_id)],
            };
            let rows = rt.block_on(databases.execute(&connection, query)).unwrap();
            let got: Vec<_> = rows.iter().map(|row| row["runId"].clone()).collect();
            if got != vec![json!(run_id)] {
                wrong.push(format!("asked {run_id} got {got:?}"));
            }
        }
    }
    wrong
}

#[test]
#[ignore = "needs a StarRocks FE (MS_TEST_STARROCKS_URL)"]
fn lookup_with_correlated_subquery_returns_the_row_asked_for() {
    ensure_fixture();
    let databases = one_connection();
    for sql in [SUBQUERY_SQL, ANCHOR_SQL] {
        let wrong = wrong_answers(&databases, sql);
        assert!(
            wrong.is_empty(),
            "{} of 20 wrong: {wrong:?}\n{sql}",
            wrong.len()
        );
    }
}

/// Upstream canary: proves the server bug the crate works around. When this starts failing the
/// StarRocks in use re-binds parameters correctly; see the doc before re-enabling statement reuse.
#[test]
#[ignore = "needs a StarRocks FE (MS_TEST_STARROCKS_URL)"]
fn upstream_starrocks_reexecuted_statement_keeps_first_parameters() {
    ensure_fixture();
    runtime().block_on(async {
        let mut connection = connect().await;
        let mut answers = Vec::new();
        for (date, run_id) in RUNS {
            // persistent (the sqlx default): prepared once, then COM_STMT_EXECUTE on the same id
            let row = sqlx::query(SUBQUERY_SQL)
                .bind(date)
                .bind(run_id)
                .fetch_one(&mut connection)
                .await
                .unwrap();
            answers.push(row.get::<i64, _>("runId"));
        }
        let version: String = sqlx::query_scalar("SELECT current_version()")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(
            answers,
            vec![13962; 4],
            "StarRocks {version} answered a re-executed prepared statement correctly; the bug in \
             docs/starrocks-prepared-statement-stale-params.md may be fixed upstream"
        );
    });
}

/// What re-preparing costs: per-query latency on one warm connection, statement kept and
/// re-executed (the sqlx default, wrong on StarRocks) vs prepared + executed + closed every
/// time (what the crate does on StarRocks). Prints `PREPARE_COST` lines; asserts nothing.
#[test]
#[ignore = "needs a StarRocks FE (MS_TEST_STARROCKS_URL)"]
fn prepare_cost_per_query() {
    const N: usize = 300;
    ensure_fixture();
    runtime().block_on(async {
        let mut connection = connect().await;
        let point = "SELECT r.run_id AS runId FROM ms_zen_stale_params.po_plan_run r \
                     WHERE r.run_date = ? AND r.run_id = ?";
        for (name, sql) in [("point lookup", point), ("correlated subquery", SUBQUERY_SQL)] {
            for persistent in [true, false] {
                let mut samples = Vec::with_capacity(N);
                for i in 0..N {
                    let (date, run_id) = RUNS[i % RUNS.len()];
                    let started = std::time::Instant::now();
                    sqlx::query(sql)
                        .bind(date)
                        .bind(run_id)
                        .persistent(persistent)
                        .fetch_all(&mut connection)
                        .await
                        .unwrap();
                    samples.push(started.elapsed());
                }
                samples.sort();
                let at = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
                println!(
                    "PREPARE_COST {name:<20} persistent={persistent:<5} p50 {:>8.2?} p90 {:>8.2?} min {:>8.2?}",
                    at(0.5),
                    at(0.9),
                    samples[0]
                );
            }
        }
    });
}
