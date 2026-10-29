//! MobileSentrix: whether a database step's SQL only reads.
//!
//! Database steps that only read may start ahead of the walk, run their loop items
//! concurrently and share identical queries inside one evaluation. A statement that may write
//! (or lock) must run exactly where and as often as the walk runs it, so the classification is
//! conservative: a statement is read-only only when
//! * its first keyword (after leading parentheses) is `SELECT` or `WITH`,
//! * no word in it is a data-changing or locking keyword (`INSERT`, `UPDATE`, `DELETE`, `MERGE`,
//!   `INTO`, `CREATE`, `CALL`, `LOCK`, … — this also catches data-modifying CTEs and
//!   `SELECT … FOR UPDATE` / `SELECT … INTO`),
//! * it is one statement (no `;` except trailing ones).
//!
//! Words are matched in the raw text, string literals and comments included, so a keyword in a
//! string makes a read look like a write (it then simply runs in walk order); text cannot make a
//! write look like a read. `INSERT(` / `REPLACE(` are MySQL string functions, a word right after
//! `.` or inside backticks is an identifier, and `{{ … }}` placeholders are skipped (they are
//! bound values, never SQL).

const WRITE_WORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "UPSERT", "REPLACE", "INTO", "CREATE", "DROP", "ALTER",
    "TRUNCATE", "RENAME", "GRANT", "REVOKE", "CALL", "EXEC", "EXECUTE", "DO", "COPY", "LOAD",
    "LOCK", "UNLOCK", "HANDLER", "VACUUM", "ANALYZE", "REFRESH", "NOTIFY", "LISTEN",
];

/// Words that are string functions when a `(` follows them (MySQL `INSERT(str, …)`,
/// `REPLACE(str, from, to)`).
const FUNCTION_WORDS: &[&str] = &["INSERT", "REPLACE"];

/// Whether `sql` (a database step's query template) only reads; see the module doc.
pub fn is_read_only_sql(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    let mut first: Option<&str> = None;
    let mut prev_significant: u8 = b' ';
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'{' && bytes.get(i + 1) == Some(&b'{') {
            match sql[i + 2..].find("}}") {
                Some(end) => i += 2 + end + 2,
                None => return false,
            }
            prev_significant = b'?';
            continue;
        }
        if c == b'`' {
            match sql[i + 1..].find('`') {
                Some(end) => i += 1 + end + 1,
                None => return false,
            }
            prev_significant = b'`';
            continue;
        }
        if c == b';' {
            if !sql[i + 1..].trim().chars().all(|c| c == ';') {
                return false;
            }
            break;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
            {
                i += 1;
            }
            let word = &sql[start..i];
            if prev_significant == b'.' {
                prev_significant = b'a';
                continue;
            }
            if first.is_none() {
                first = Some(word);
                if !(word.eq_ignore_ascii_case("SELECT") || word.eq_ignore_ascii_case("WITH")) {
                    return false;
                }
            }
            if let Some(w) = WRITE_WORDS.iter().find(|w| word.eq_ignore_ascii_case(w)) {
                let call = sql[i..].trim_start().starts_with('(');
                if !(call && FUNCTION_WORDS.contains(w)) {
                    return false;
                }
            }
            prev_significant = b'a';
            continue;
        }
        if c.is_ascii_digit() {
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.')
            {
                i += 1;
            }
            prev_significant = b'0';
            continue;
        }
        if first.is_none() && !(c.is_ascii_whitespace() || c == b'(') {
            // a comment, a string or anything else before the first keyword
            return false;
        }
        if !c.is_ascii_whitespace() {
            prev_significant = c;
        }
        i += 1;
    }
    first.is_some()
}

#[cfg(test)]
mod tests {
    use super::is_read_only_sql as ro;

    #[test]
    fn reads() {
        assert!(ro("SELECT sku FROM products WHERE id = {{ id }}"));
        assert!(ro("  select 1"));
        assert!(ro("(SELECT a FROM t) UNION (SELECT a FROM u)"));
        assert!(ro("WITH x AS (SELECT 1 AS a) SELECT a FROM x;"));
        assert!(ro(
            "SELECT REPLACE(name, 'a', 'b'), INSERT(s, 1, 2, 'x') FROM t"
        ));
        assert!(ro("SELECT t.lock, t.`update`, `delete` FROM t"));
        assert!(ro(
            "SELECT * FROM t WHERE status = {{ status ?? 'insert' }}"
        ));
        assert!(ro("SELECT 1.5e3, x_1 FROM t;;"));
    }

    #[test]
    fn writes_and_unclear_statements() {
        assert!(!ro("INSERT INTO log (a) VALUES ({{ a }})"));
        assert!(!ro("update t set a = 1"));
        assert!(!ro("DELETE FROM t"));
        assert!(!ro("REPLACE INTO t VALUES (1)"));
        assert!(!ro("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"));
        assert!(!ro(
            "WITH d AS (INSERT INTO t VALUES (1) RETURNING *) SELECT * FROM d"
        ));
        assert!(!ro("SELECT * FROM t FOR UPDATE"));
        assert!(!ro("SELECT a INTO OUTFILE '/tmp/x' FROM t"));
        assert!(!ro("SELECT 1; DELETE FROM t"));
        assert!(!ro("CALL proc()"));
        assert!(!ro("SET @a = 1"));
        assert!(!ro("-- comment\nSELECT 1"));
        assert!(!ro("SELECT 'it''s an update' FROM t"));
        assert!(!ro(""));
        assert!(!ro("SELECT {{ unterminated"));
    }
}
