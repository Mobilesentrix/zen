//! Tests for NoopDatabaseAdapter.
use zen_database::{DatabaseAdapter, DatabaseError, DatabaseRequest, NoopDatabaseAdapter};

// ═══════════════════════════════════════════════════════════════════════════
//  NoopDatabaseAdapter
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn noop_adapter_returns_not_configured() {
    let noop = NoopDatabaseAdapter::default();
    let request = DatabaseRequest {
        query: "SELECT 1".into(),
    };

    let result = noop.execute(&request).await;
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), DatabaseError::NotConfigured));
}

#[tokio::test]
async fn noop_adapter_returns_not_configured_for_any_query() {
    let noop = NoopDatabaseAdapter::default();

    for query in &[
        "SELECT 1",
        "SELECT * FROM users",
        "INSERT INTO orders VALUES (1)",
        "",
    ] {
        let request = DatabaseRequest {
            query: (*query).into(),
        };
        let result = noop.execute(&request).await;
        assert!(
            result.is_err(),
            "Expected error for query: {}",
            query
        );
        assert!(
            matches!(result.unwrap_err(), DatabaseError::NotConfigured),
            "Expected NotConfigured for query: {}",
            query
        );
    }
}

#[tokio::test]
async fn noop_adapter_never_returns_ok() {
    let noop = NoopDatabaseAdapter::default();
    let request = DatabaseRequest {
        query: "SELECT * FROM anything".into(),
    };

    // No matter what, Noop must never succeed — it has no real connection
    let result = noop.execute(&request).await;
    assert!(result.is_ok() == false);
}
