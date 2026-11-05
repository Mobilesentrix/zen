//! `{{ expr }}` placeholder templates for database-node SQL.
//!
//! A placeholder stands for one *value*. Each placeholder is compiled once into a zen
//! expression; at evaluation time its result is bound as a query parameter (never spliced
//! into the SQL text). The parser rejects placeholders in positions where a bind parameter
//! cannot work and queries that mix literal bind markers with placeholders.

mod guard;
mod parser;

use parser::Parser;
use std::fmt;
use std::sync::Arc;
use zen_expression::expression::Standard;
use zen_expression::Expression;

pub use guard::Guard;
pub(crate) use guard::Term;

/// SQL dialect, decides bind-marker syntax and quoting rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dialect {
    /// MySQL wire protocol (MySQL, MariaDB, StarRocks): `?` markers, backtick identifiers,
    /// backslash escapes and `#` comments.
    MySql,
    /// PostgreSQL: `$n` markers, `"` identifiers, dollar-quoted strings, `E''` escapes.
    Postgres,
}

/// 1-based source position of a placeholder or token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, column {}", self.line, self.column)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    #[error("unterminated placeholder at {0}: `{{{{` has no closing `}}}}`")]
    Unterminated(Position),
    #[error("empty placeholder `{{{{ }}}}` at {0}")]
    Empty(Position),
    #[error("nested `{{{{` inside the placeholder at {0}")]
    Nested(Position),
    #[error("placeholder at {pos} is inside a {kind}; remove the surrounding {quotes}, a placeholder already stands for a complete value (for a prefix/suffix match use `LIKE {{{{ '%' + term + '%' }}}}`)")]
    InsideString {
        pos: Position,
        kind: &'static str,
        quotes: &'static str,
    },
    #[error("placeholder at {pos} is inside a quoted identifier {quote}…{quote}; placeholders bind values, they cannot name a table or column")]
    InsideIdentifier { pos: Position, quote: char },
    #[error("placeholder at {pos} follows `{keyword}`, an identifier position; placeholders bind values, they cannot name a table or column (MySQL accepts `ORDER BY ?` as a constant and silently ignores it)")]
    IdentifierPosition { pos: Position, keyword: String },
    #[error("query mixes a literal `{marker}` at {pos} with {{{{ }}}} placeholders; every bind slot must come from a placeholder or the values line up against the wrong slots")]
    MixedMarkers { pos: Position, marker: String },
    #[error("placeholder `{{{{ {expression} }}}}` at {pos} is not a valid expression: {message}")]
    InvalidExpression {
        pos: Position,
        expression: String,
        message: String,
    },
    #[error("`{{{{{marker}}}}}` at {pos} has no matching `{{{{#if …}}}}` before it")]
    UnopenedBlock { pos: Position, marker: &'static str },
    #[error("`{{{{#if …}}}}` at {0} is never closed with `{{{{/if}}}}`")]
    UnclosedBlock(Position),
    #[error("second `{{{{else}}}}` at {0} in one `{{{{#if}}}}` block")]
    DuplicateElse(Position),
    #[error("`{{{{#if}}}}` at {0} has no condition; write e.g. `{{{{#if colour}}}}`")]
    EmptyCondition(Position),
}

/// What a placeholder turns into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// A bound parameter (`?` / `$n`), the only form a request value ever takes in the SQL.
    Value,
    /// A whole number written into the SQL text after `LIMIT` / `OFFSET`, where servers do not
    /// accept a bound parameter. Only the digits of a checked non-negative integer are written.
    Count,
}

/// One compiled placeholder.
#[derive(Debug, Clone)]
pub struct Placeholder {
    /// Expression source, trimmed (what the user wrote between the braces).
    pub source: Arc<str>,
    pub expression: Expression<Standard>,
    pub position: Position,
    pub kind: SlotKind,
}

#[derive(Debug, Clone)]
pub(crate) enum Piece {
    Sql(String),
    Slot(usize),
    Block {
        guard: usize,
        then: Vec<Piece>,
        otherwise: Vec<Piece>,
    },
}

/// A parsed, validated query template. Cheap to share (`Arc`), immutable.
#[derive(Debug, Clone)]
pub struct QueryTemplate {
    pub(crate) dialect: Dialect,
    pub(crate) pieces: Vec<Piece>,
    pub(crate) placeholders: Vec<Placeholder>,
    pub(crate) guards: Vec<Guard>,
    /// Pre-rendered SQL used when every placeholder resolves to a scalar, so no per-call string
    /// building is needed. Only used for a template that is not [`dynamic`](Self::is_dynamic).
    pub(crate) scalar_sql: Arc<str>,
    /// Has `{{#if}}` blocks or `LIMIT` / `OFFSET` counts: the SQL text is chosen per call.
    pub(crate) dynamic: bool,
    /// The statement only reads (`zen_engine::nodes::statement::is_read_only_sql`).
    pub(crate) read_only: bool,
}

impl QueryTemplate {
    /// # Errors
    /// The template is malformed or places a placeholder where a bind parameter cannot work.
    pub fn parse(sql: &str, dialect: Dialect) -> Result<Self, TemplateError> {
        Parser::new(sql, dialect).run()
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn placeholders(&self) -> &[Placeholder] {
        &self.placeholders
    }

    pub fn guards(&self) -> &[Guard] {
        &self.guards
    }

    /// Whether the SQL text depends on the request (optional blocks or LIMIT/OFFSET counts).
    /// Values are bound parameters either way.
    pub fn is_dynamic(&self) -> bool {
        self.dynamic
    }

    /// Whether the statement only reads. Only read-only statements share identical queries
    /// inside an evaluation, run loop items concurrently and start ahead of the walk; any
    /// other statement runs exactly as often and in the order the rule asks.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// SQL with exactly one bind marker per placeholder.
    pub fn scalar_sql(&self) -> &Arc<str> {
        &self.scalar_sql
    }

    /// Renders SQL where placeholder `i` expands to `widths[i]` bind markers.
    pub fn render(&self, widths: &[usize]) -> String {
        render(self.dialect, &self.pieces, &|slot| widths[slot])
    }
}

fn render(dialect: Dialect, pieces: &[Piece], width: &dyn Fn(usize) -> usize) -> String {
    let mut out = String::new();
    let mut marker_number = 0usize;
    for piece in pieces {
        match piece {
            Piece::Sql(text) => out.push_str(text),
            // static rendering (non-dynamic templates have no blocks): first branch
            Piece::Block { then, .. } => out.push_str(&render(dialect, then, width)),
            Piece::Slot(slot) => {
                for i in 0..width(*slot) {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    marker_number += 1;
                    match dialect {
                        Dialect::MySql => out.push('?'),
                        Dialect::Postgres => {
                            out.push('$');
                            out.push_str(&marker_number.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}
