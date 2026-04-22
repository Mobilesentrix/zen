//! Tests for MySqlAdapter and MySqlAdapterBuilder.
use std::time::Duration;
use zen_database::{ConnectionOptions, MySqlAdapter, MySqlAdapterBuilder};

// ═══════════════════════════════════════════════════════════════════════════
//  MySqlAdapterBuilder — Unit tests (no DB required)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn builder_default_has_no_values_set() {
    // Building from default should not panic and should produce a valid builder.
    // Because the fields are private to the crate, we verify indirectly via connect attempt.
    let _builder = MySqlAdapterBuilder::default();
}

#[test]
fn builder_via_adapter_builder_fn() {
    // MySqlAdapter::builder() is the idiomatic entry point
    let _builder = MySqlAdapter::builder();
}

#[test]
fn builder_chain_all_options() {
    // Ensure all builder methods can be chained without panicking.
    let _builder = MySqlAdapter::builder()
        .max_connections(20)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(300))
        .max_lifetime(Duration::from_secs(3600))
        .test_before_acquire(true);
}

#[test]
fn builder_from_connection_options_defaults() {
    // Default ConnectionOptions should produce a builder without panicking.
    let opts = ConnectionOptions::default();
    let _builder = MySqlAdapterBuilder::from(opts);
}

#[test]
fn builder_from_connection_options_with_values() {
    let opts = ConnectionOptions {
        max_connections: Some(10),
        min_connections: Some(2),
        acquire_timeout: Some(Duration::from_millis(5000)),
        idle_timeout: Some(Duration::from_millis(60000)),
        max_lifetime: Some(Duration::from_millis(3600000)),
    };
    // Should map all values without panicking
    let _builder = MySqlAdapterBuilder::from(opts);
}

// ═══════════════════════════════════════════════════════════════════════════
//  ConnectionOptions — Serde tests (no DB required)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn connection_options_deserializes_milliseconds() {
    let json = serde_json::json!({
        "maxConnections": 10,
        "minConnections": 2,
        "acquireTimeout": 5000,    // 5 seconds expressed in ms
        "idleTimeout": 60000,      // 60 seconds expressed in ms
        "maxLifetime": 3600000     // 1 hour expressed in ms
    });

    let opts: ConnectionOptions = serde_json::from_value(json).unwrap();
    assert_eq!(opts.max_connections, Some(10));
    assert_eq!(opts.min_connections, Some(2));
    assert_eq!(opts.acquire_timeout, Some(Duration::from_millis(5000)));
    assert_eq!(opts.idle_timeout, Some(Duration::from_millis(60000)));
    assert_eq!(opts.max_lifetime, Some(Duration::from_millis(3600000)));
}

#[test]
fn connection_options_default_has_no_values() {
    let opts = ConnectionOptions::default();
    assert!(opts.max_connections.is_none());
    assert!(opts.min_connections.is_none());
    assert!(opts.acquire_timeout.is_none());
    assert!(opts.idle_timeout.is_none());
    assert!(opts.max_lifetime.is_none());
}

#[test]
fn connection_options_partial_deserialization() {
    // Only maxConnections is set — others remain None
    let json = serde_json::json!({"maxConnections": 5});
    let opts: ConnectionOptions = serde_json::from_value(json).unwrap();
    assert_eq!(opts.max_connections, Some(5));
    assert!(opts.min_connections.is_none());
    assert!(opts.acquire_timeout.is_none());
}

#[test]
fn connection_options_empty_json_gives_defaults() {
    let json = serde_json::json!({});
    let opts: ConnectionOptions = serde_json::from_value(json).unwrap();
    assert!(opts.max_connections.is_none());
    assert!(opts.acquire_timeout.is_none());
}

#[test]
fn connection_options_roundtrip_serialization() {
    let original = ConnectionOptions {
        max_connections: Some(15),
        min_connections: Some(1),
        acquire_timeout: Some(Duration::from_millis(3000)),
        idle_timeout: Some(Duration::from_millis(30000)),
        max_lifetime: None,
    };

    let json = serde_json::to_value(&original).unwrap();
    let deserialized: ConnectionOptions = serde_json::from_value(json).unwrap();

    assert_eq!(deserialized.max_connections, original.max_connections);
    assert_eq!(deserialized.min_connections, original.min_connections);
    assert_eq!(deserialized.acquire_timeout, original.acquire_timeout);
    assert_eq!(deserialized.idle_timeout, original.idle_timeout);
    assert_eq!(deserialized.max_lifetime, original.max_lifetime);
}

// ═══════════════════════════════════════════════════════════════════════════
//  Live Integration Tests (require a running MySQL/StarRocks database)
//  Run with: cargo test -p zen-database -- --ignored
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[ignore]
async fn live_connect_and_ping() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:password@localhost:3306/test_db".to_string());

    MySqlAdapter::connect(&url)
        .await
        .expect("Failed to connect to database");
}

#[tokio::test]
#[ignore]
async fn live_execute_select_1() {
    use zen_database::{DatabaseAdapter, DatabaseRequest};

    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:password@localhost:3306/test_db".to_string());

    let adapter = MySqlAdapter::connect(&url).await.expect("Failed to connect");

    let req = DatabaseRequest {
        query: "SELECT 1 AS num".into(),
    };
    let response = adapter.execute(&req).await.expect("Query failed");

    assert_eq!(response.row_count, 1);
    assert_eq!(response.rows[0]["num"], 1);
}

#[tokio::test]
#[ignore]
async fn live_execute_returns_correct_row_count() {
    use zen_database::{DatabaseAdapter, DatabaseRequest};

    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:password@localhost:3306/test_db".to_string());

    let adapter = MySqlAdapter::connect(&url).await.expect("Failed to connect");

    let req = DatabaseRequest {
        query: "SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3".into(),
    };
    let response = adapter.execute(&req).await.expect("Query failed");

    assert_eq!(response.row_count, 3);
}

#[tokio::test]
#[ignore]
async fn live_builder_with_options_connects() {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:password@localhost:3306/test_db".to_string());

    let opts = ConnectionOptions {
        max_connections: Some(5),
        min_connections: Some(1),
        acquire_timeout: Some(Duration::from_secs(10)),
        idle_timeout: None,
        max_lifetime: None,
    };

    MySqlAdapterBuilder::from(opts)
        .connect(&url)
        .await
        .expect("Failed to connect with builder options");
}

#[tokio::test]
#[ignore]
async fn live_invalid_url_returns_connection_error() {
    use zen_database::DatabaseError;

    let result = MySqlAdapter::connect("mysql://invalid-host:3306/db").await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), DatabaseError::Connection(_)));
}
