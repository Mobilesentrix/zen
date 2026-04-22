//! Tests for DatabaseRequest, DatabaseResponse, and DatabaseError types.
use serde_json::json;
use zen_database::{DatabaseError, DatabaseRequest, DatabaseResponse};

// ═══════════════════════════════════════════════════════════════════════════
//  DatabaseRequest
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn database_request_serializes_query_field() {
    let req = DatabaseRequest {
        query: "SELECT * FROM users".into(),
    };
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["query"], "SELECT * FROM users");
}

#[test]
fn database_request_deserializes_from_camel_case() {
    let json = json!({"query": "SELECT 1"});
    let req: DatabaseRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.query, "SELECT 1");
}

#[test]
fn database_request_roundtrip() {
    let original = DatabaseRequest {
        query: "SELECT id, name FROM orders WHERE id = 1".into(),
    };
    let serialized = serde_json::to_string(&original).unwrap();
    let deserialized: DatabaseRequest = serde_json::from_str(&serialized).unwrap();
    assert_eq!(original.query, deserialized.query);
}

// ═══════════════════════════════════════════════════════════════════════════
//  DatabaseResponse
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn database_response_serializes_row_count_as_camel_case() {
    let resp = DatabaseResponse {
        rows: json!([{"id": 1}]),
        row_count: 1,
    };
    let json = serde_json::to_value(&resp).unwrap();
    // Verify camelCase serialization: row_count → rowCount
    assert_eq!(json["rowCount"], 1);
    assert_eq!(json["rows"][0]["id"], 1);
    // Ensure snake_case key is NOT present
    assert!(json.get("row_count").is_none());
}

#[test]
fn database_response_deserializes_row_count_from_camel_case() {
    let json = json!({"rows": [{"id": 1}], "rowCount": 1});
    let resp: DatabaseResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.row_count, 1);
    assert_eq!(resp.rows[0]["id"], 1);
}

#[test]
fn database_response_handles_empty_rows() {
    let resp = DatabaseResponse {
        rows: json!([]),
        row_count: 0,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["rowCount"], 0);
    assert!(json["rows"].as_array().unwrap().is_empty());
}

#[test]
fn database_response_handles_multiple_rows() {
    let resp = DatabaseResponse {
        rows: json!([{"id": 1}, {"id": 2}, {"id": 3}]),
        row_count: 3,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["rowCount"], 3);
    assert_eq!(json["rows"].as_array().unwrap().len(), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
//  DatabaseError
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn error_display_connection() {
    let err = DatabaseError::Connection("host unreachable".into());
    assert_eq!(
        err.to_string(),
        "Database connection error: host unreachable"
    );
}

#[test]
fn error_display_query() {
    let err = DatabaseError::Query("syntax error".into());
    assert_eq!(err.to_string(), "Query execution error: syntax error");
}

#[test]
fn error_display_not_configured() {
    let err = DatabaseError::NotConfigured;
    assert_eq!(err.to_string(), "Database adapter not configured");
}

#[test]
fn error_display_other() {
    let inner: Box<dyn std::error::Error + Send + Sync> =
        Box::new(std::io::Error::new(std::io::ErrorKind::Other, "disk full"));
    let err = DatabaseError::Other(inner);
    assert!(err.to_string().contains("disk full"));
}

#[test]
fn error_connection_contains_message() {
    let msg = "connection timeout after 30s";
    let err = DatabaseError::Connection(msg.into());
    assert!(err.to_string().contains(msg));
}

#[test]
fn error_query_contains_message() {
    let msg = "table 'users' doesn't exist";
    let err = DatabaseError::Query(msg.into());
    assert!(err.to_string().contains(msg));
}
