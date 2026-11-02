//! Errors of the connection pools and the queries they run.

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DatabaseError {
    #[error("Invalid database configuration: {0}")]
    Config(String),
    #[error("Database connection error: {0}")]
    Connection(String),
    #[error("Query execution error: {0}")]
    Query(String),
    #[error("{0}")]
    UnknownInstance(String),
    #[error("query returned more than {limit} rows (connection \"{connection}\" maxRows); let the query filter, join, sort and LIMIT in the database so it returns only the rows the rule uses. Raising maxRows moves the whole result into the engine and can exhaust its memory")]
    TooManyRows { connection: String, limit: usize },
    #[error("loop of {items} items exceeds maxLoopItems ({limit}) of connection \"{connection}\"; query the items in one statement (for example `IN ({{{{ items }}}})`) or raise maxLoopItems")]
    TooManyLoopItems {
        connection: String,
        limit: usize,
        items: usize,
    },
    #[error("query exceeded the queryTimeout of {ms} ms on connection \"{connection}\"")]
    Timeout { connection: String, ms: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_connection_and_the_limit() {
        let e = DatabaseError::TooManyRows {
            connection: "sr".into(),
            limit: 10,
        };
        assert_eq!(
            e.to_string(),
            "query returned more than 10 rows (connection \"sr\" maxRows); let the query filter, join, sort and LIMIT in the database so it returns only the rows the rule uses. Raising maxRows moves the whole result into the engine and can exhaust its memory"
        );
        let e = DatabaseError::Timeout {
            connection: "sr".into(),
            ms: 300,
        };
        assert_eq!(
            e.to_string(),
            "query exceeded the queryTimeout of 300 ms on connection \"sr\""
        );
    }
}
