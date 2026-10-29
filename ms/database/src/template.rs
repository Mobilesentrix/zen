//! `{{ expr }}` placeholder parser for database-node SQL.
//!
//! A placeholder stands for one *value*. Each placeholder is compiled once into a zen
//! expression; at evaluation time its result is bound as a query parameter (never spliced
//! into the SQL text). The parser walks the SQL with a small lexer so it can reject
//! placeholders in positions where a bind parameter cannot work (inside string literals,
//! inside quoted identifiers, after `FROM`/`JOIN`/`ORDER BY`/`.` …) and queries that mix
//! literal bind markers with placeholders.

use std::fmt;
use std::sync::Arc;
use zen_expression::expression::Standard;
use zen_expression::{compile_expression, Expression};

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
    /// accept a bound parameter (StarRocks: "using parameter(?) as limit or offset not
    /// supported"). Only the digits of a checked non-negative integer are written.
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

/// One truthiness test of a guard: a zen expression, optionally negated by a leading `not`.
#[derive(Debug, Clone)]
pub(crate) struct Term {
    pub(crate) negate: bool,
    pub(crate) expression: Expression<Standard>,
}

/// The condition of a `{{#if …}}` block: terms joined by `or` (outer) and `and` (inner) at the
/// guard's top level, each tested for truthiness (missing, null, false, 0, blank text, an empty
/// list or object are false). Inside parentheses, `and` / `or` are zen's own boolean operators.
#[derive(Debug, Clone)]
pub struct Guard {
    pub source: Arc<str>,
    pub position: Position,
    pub(crate) any_of: Vec<Vec<Term>>,
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
    /// Pre-rendered SQL used when every placeholder resolves to a scalar (the common case),
    /// so no per-call string building is needed. Only used for a template that is not
    /// [`dynamic`](Self::is_dynamic).
    pub(crate) scalar_sql: Arc<str>,
    /// Has `{{#if}}` blocks or `LIMIT` / `OFFSET` counts: the SQL text is chosen per call.
    pub(crate) dynamic: bool,
    /// The statement only reads (`zen_engine::nodes::statement::is_read_only_sql`).
    pub(crate) read_only: bool,
}

impl QueryTemplate {
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
    let mut n = 0usize;
    for piece in pieces {
        match piece {
            Piece::Sql(s) => out.push_str(s),
            // static rendering (non-dynamic templates have no blocks): first branch
            Piece::Block { then, .. } => out.push_str(&render(dialect, then, width)),
            Piece::Slot(slot) => {
                for i in 0..width(*slot) {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    n += 1;
                    match dialect {
                        Dialect::MySql => out.push('?'),
                        Dialect::Postgres => {
                            out.push('$');
                            out.push_str(&n.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Functions whose argument list uses `FROM` as a value separator
/// (`EXTRACT(YEAR FROM x)`, `SUBSTRING(s FROM 2)`, `TRIM(LEADING 'x' FROM s)`…).
const FROM_VALUE_FUNCTIONS: &[&str] = &[
    "EXTRACT",
    "SUBSTRING",
    "SUBSTR",
    "TRIM",
    "POSITION",
    "OVERLAY",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Punct(u8),
}

struct Parser<'a> {
    src: &'a str,
    bytes: &'a [u8],
    dialect: Dialect,
    i: usize,
    pieces: Vec<Piece>,
    placeholders: Vec<Placeholder>,
    sql_start: usize,
    /// Last two significant tokens in normal SQL context (comments/whitespace skipped).
    prev: Option<Token>,
    prev2: Option<Token>,
    /// Paren stack: `true` when the paren opened a FROM-as-value function.
    parens: Vec<bool>,
    literal_marker: Option<(Position, String)>,
    guards: Vec<Guard>,
    /// Open `{{#if}}` blocks, innermost last.
    blocks: Vec<OpenBlock>,
    /// Inside a `LIMIT` / `OFFSET` clause: a placeholder here is a [`SlotKind::Count`].
    limit_clause: bool,
    dynamic: bool,
}

struct OpenBlock {
    position: Position,
    guard: usize,
    /// Pieces before the block (restored when it closes).
    outer: Vec<Piece>,
    /// The `then` branch once `{{else}}` was seen.
    then: Option<Vec<Piece>>,
    /// Token context at `{{#if}}`, restored for the `{{else}}` branch.
    tokens: (Option<Token>, Option<Token>, bool),
}

impl<'a> Parser<'a> {
    fn new(src: &'a str, dialect: Dialect) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            dialect,
            i: 0,
            pieces: Vec::new(),
            placeholders: Vec::new(),
            sql_start: 0,
            prev: None,
            prev2: None,
            parens: Vec::new(),
            literal_marker: None,
            guards: Vec::new(),
            blocks: Vec::new(),
            limit_clause: false,
            dynamic: false,
        }
    }

    fn pos(&self, at: usize) -> Position {
        let before = &self.src[..at];
        let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
        let line_start = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let column = self.src[line_start..at].chars().count() + 1;
        Position { line, column }
    }

    fn peek(&self, off: usize) -> Option<u8> {
        self.bytes.get(self.i + off).copied()
    }

    fn push_token(&mut self, token: Token) {
        match &token {
            Token::Word(w) => self.limit_clause = w == "LIMIT" || w == "OFFSET",
            Token::Punct(b'(') | Token::Punct(b')') => self.limit_clause = false,
            Token::Punct(_) => {}
        }
        self.prev2 = self.prev.take();
        self.prev = Some(token);
    }

    fn run(mut self) -> Result<QueryTemplate, TemplateError> {
        while self.i < self.bytes.len() {
            let c = self.bytes[self.i];
            match c {
                b'{' if self.peek(1) == Some(b'{') => self.placeholder()?,
                b'\'' => {
                    let escapes =
                        self.dialect == Dialect::MySql || self.is_postgres_escape_string();
                    self.quoted(b'\'', escapes, QuoteKind::String)?;
                }
                b'"' => match self.dialect {
                    Dialect::MySql => self.quoted(b'"', true, QuoteKind::String)?,
                    Dialect::Postgres => self.quoted(b'"', false, QuoteKind::Identifier)?,
                },
                b'`' if self.dialect == Dialect::MySql => {
                    self.quoted(b'`', false, QuoteKind::Identifier)?
                }
                b'-' if self.peek(1) == Some(b'-') && self.is_line_comment_dash() => {
                    self.line_comment()
                }
                b'#' if self.dialect == Dialect::MySql => self.line_comment(),
                b'/' if self.peek(1) == Some(b'*') => self.block_comment(),
                b'$' if self.dialect == Dialect::Postgres => self.dollar()?,
                b'?' if self.dialect == Dialect::MySql => {
                    if self.literal_marker.is_none() {
                        self.literal_marker = Some((self.pos(self.i), "?".to_string()));
                    }
                    self.push_token(Token::Punct(b'?'));
                    self.i += 1;
                }
                b'(' => {
                    let from_fn = matches!(&self.prev, Some(Token::Word(w))
                        if FROM_VALUE_FUNCTIONS.contains(&w.as_str()));
                    self.parens.push(from_fn);
                    self.push_token(Token::Punct(b'('));
                    self.i += 1;
                }
                b')' => {
                    self.parens.pop();
                    self.push_token(Token::Punct(b')'));
                    self.i += 1;
                }
                c if c.is_ascii_whitespace() => self.i += 1,
                c if is_word_start(c) => {
                    let start = self.i;
                    while self.i < self.bytes.len() && is_word_char(self.bytes[self.i]) {
                        self.i += 1;
                    }
                    let word = self.src[start..self.i].to_ascii_uppercase();
                    self.push_token(Token::Word(word));
                }
                c if c.is_ascii() => {
                    self.push_token(Token::Punct(c));
                    self.i += 1;
                }
                _ => {
                    // Non-ASCII outside quotes: advance one UTF-8 char.
                    let ch = self.src[self.i..]
                        .chars()
                        .next()
                        .map(char::len_utf8)
                        .unwrap_or(1);
                    self.push_token(Token::Punct(b'~'));
                    self.i += ch;
                }
            }
        }

        if let Some(open) = self.blocks.last() {
            return Err(TemplateError::UnclosedBlock(open.position));
        }
        if !self.placeholders.is_empty() || !self.guards.is_empty() {
            if let Some((pos, marker)) = self.literal_marker.take() {
                return Err(TemplateError::MixedMarkers { pos, marker });
            }
        }

        if self.sql_start < self.bytes.len() {
            self.pieces
                .push(Piece::Sql(self.src[self.sql_start..].to_string()));
        }

        let scalar_sql: Arc<str> = render(self.dialect, &self.pieces, &|_| 1).into();
        Ok(QueryTemplate {
            dialect: self.dialect,
            pieces: self.pieces,
            placeholders: self.placeholders,
            guards: self.guards,
            scalar_sql,
            dynamic: self.dynamic,
            read_only: zen_engine::nodes::statement::is_read_only_sql(self.src),
        })
    }

    /// `E'…'` / `e'…'` in Postgres: the `E` must be a standalone prefix.
    fn is_postgres_escape_string(&self) -> bool {
        if self.dialect != Dialect::Postgres || self.i == 0 {
            return false;
        }
        let p = self.bytes[self.i - 1];
        if p != b'E' && p != b'e' {
            return false;
        }
        self.i < 2 || !is_word_char(self.bytes[self.i - 2])
    }

    /// MySQL requires whitespace (or EOL) after `--` for it to start a comment.
    fn is_line_comment_dash(&self) -> bool {
        match self.dialect {
            Dialect::Postgres => true,
            Dialect::MySql => match self.peek(2) {
                None => true,
                Some(c) => c.is_ascii_whitespace(),
            },
        }
    }

    fn line_comment(&mut self) {
        while self.i < self.bytes.len() && self.bytes[self.i] != b'\n' {
            self.i += 1;
        }
    }

    fn block_comment(&mut self) {
        let nests = self.dialect == Dialect::Postgres;
        let mut depth = 0usize;
        while self.i < self.bytes.len() {
            if self.bytes[self.i] == b'/' && self.peek(1) == Some(b'*') {
                if depth == 0 || nests {
                    depth += 1;
                }
                self.i += 2;
            } else if self.bytes[self.i] == b'*' && self.peek(1) == Some(b'/') {
                depth -= 1;
                self.i += 2;
                if depth == 0 {
                    return;
                }
            } else {
                self.i += 1;
            }
        }
    }

    fn quoted(&mut self, quote: u8, backslash: bool, kind: QuoteKind) -> Result<(), TemplateError> {
        self.i += 1;
        while self.i < self.bytes.len() {
            let c = self.bytes[self.i];
            if backslash && c == b'\\' {
                self.i += 2;
                continue;
            }
            if c == quote {
                if self.peek(1) == Some(quote) {
                    self.i += 2;
                    continue;
                }
                self.i += 1;
                break;
            }
            if c == b'{' && self.peek(1) == Some(b'{') {
                return Err(self.quoted_error(quote, kind));
            }
            self.i += 1;
        }
        self.push_token(match kind {
            QuoteKind::String => Token::Punct(b'\''),
            QuoteKind::Identifier => Token::Word("\"IDENT\"".to_string()),
        });
        Ok(())
    }

    fn quoted_error(&self, quote: u8, kind: QuoteKind) -> TemplateError {
        let pos = self.pos(self.i);
        match kind {
            QuoteKind::Identifier => TemplateError::InsideIdentifier {
                pos,
                quote: quote as char,
            },
            QuoteKind::String => {
                let (kind, quotes) = match quote {
                    b'"' => ("double-quoted string", "double quotes"),
                    b'$' => ("dollar-quoted string", "dollar quotes"),
                    _ => ("quoted string", "quotes"),
                };
                TemplateError::InsideString { pos, kind, quotes }
            }
        }
    }

    /// Postgres `$`: positional parameter `$1`, dollar-quoted string `$tag$…$tag$`, or part
    /// of an identifier.
    fn dollar(&mut self) -> Result<(), TemplateError> {
        if self.i > 0 && is_word_char(self.bytes[self.i - 1]) {
            self.i += 1;
            return Ok(());
        }
        let start = self.i;
        if matches!(self.peek(1), Some(c) if c.is_ascii_digit()) {
            if self.literal_marker.is_none() {
                let mut end = self.i + 1;
                while end < self.bytes.len() && self.bytes[end].is_ascii_digit() {
                    end += 1;
                }
                self.literal_marker = Some((self.pos(start), self.src[start..end].to_string()));
            }
            self.i += 1;
            while self.i < self.bytes.len() && self.bytes[self.i].is_ascii_digit() {
                self.i += 1;
            }
            self.push_token(Token::Punct(b'?'));
            return Ok(());
        }
        // tag: [A-Za-z_][A-Za-z0-9_]* then `$`
        let mut j = self.i + 1;
        if j < self.bytes.len() && (self.bytes[j].is_ascii_alphabetic() || self.bytes[j] == b'_') {
            while j < self.bytes.len()
                && (self.bytes[j].is_ascii_alphanumeric() || self.bytes[j] == b'_')
            {
                j += 1;
            }
        }
        if j < self.bytes.len() && self.bytes[j] == b'$' {
            let tag = &self.src[start..=j];
            let body_start = j + 1;
            let close = self.src[body_start..].find(tag).map(|p| body_start + p);
            let body_end = close.unwrap_or(self.bytes.len());
            if let Some(p) = self.src[body_start..body_end].find("{{") {
                self.i = body_start + p;
                return Err(self.quoted_error(b'$', QuoteKind::String));
            }
            self.i = close.map(|c| c + tag.len()).unwrap_or(self.bytes.len());
            self.push_token(Token::Punct(b'\''));
            return Ok(());
        }
        self.push_token(Token::Punct(b'$'));
        self.i += 1;
        Ok(())
    }

    fn identifier_keyword(&self) -> Option<String> {
        match &self.prev {
            Some(Token::Punct(b'.')) => Some(".".to_string()),
            Some(Token::Word(w)) => match w.as_str() {
                "FROM" => {
                    let in_value_fn = self.parens.last().copied().unwrap_or(false);
                    let distinct = matches!(&self.prev2, Some(Token::Word(p)) if p == "DISTINCT");
                    (!in_value_fn && !distinct).then(|| "FROM".to_string())
                }
                "JOIN" | "INTO" | "UPDATE" | "TABLE" | "AS" => Some(w.clone()),
                "BY" => match &self.prev2 {
                    Some(Token::Word(p)) if p == "ORDER" || p == "GROUP" || p == "PARTITION" => {
                        Some(format!("{p} BY"))
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    fn placeholder(&mut self) -> Result<(), TemplateError> {
        let open = self.i;
        let pos = self.pos(open);
        let body_start = open + 2;
        let mut j = body_start;
        let mut close = None;
        while j < self.bytes.len() {
            let c = self.bytes[j];
            match c {
                b'\'' | b'"' | b'`' => {
                    // skip expression string literal
                    j += 1;
                    while j < self.bytes.len() && self.bytes[j] != c {
                        if self.bytes[j] == b'\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                    j += 1;
                }
                b'{' if self.bytes.get(j + 1) == Some(&b'{') => {
                    return Err(TemplateError::Nested(self.pos(j)));
                }
                b'}' if self.bytes.get(j + 1) == Some(&b'}') => {
                    close = Some(j);
                    break;
                }
                _ => j += 1,
            }
        }
        let Some(close) = close else {
            return Err(TemplateError::Unterminated(pos));
        };
        let source = self.src[body_start..close].trim();
        if source.is_empty() {
            return Err(TemplateError::Empty(pos));
        }
        if let Some(marker) = block_marker(source) {
            self.flush_sql(open);
            self.i = close + 2;
            self.sql_start = self.i;
            return self.block(marker, pos);
        }
        let kind = if self.limit_clause {
            SlotKind::Count
        } else {
            SlotKind::Value
        };
        if let Some(keyword) = self.identifier_keyword() {
            return Err(TemplateError::IdentifierPosition { pos, keyword });
        }
        let expression =
            compile_expression(source).map_err(|e| TemplateError::InvalidExpression {
                pos,
                expression: source.to_string(),
                message: e.to_string(),
            })?;

        self.flush_sql(open);
        self.dynamic |= kind == SlotKind::Count;
        self.pieces.push(Piece::Slot(self.placeholders.len()));
        self.placeholders.push(Placeholder {
            source: Arc::from(source),
            expression,
            position: pos,
            kind,
        });
        self.i = close + 2;
        self.sql_start = self.i;
        self.push_token(Token::Punct(b'?'));
        Ok(())
    }
}

impl Parser<'_> {
    fn flush_sql(&mut self, upto: usize) {
        if self.sql_start < upto {
            self.pieces
                .push(Piece::Sql(self.src[self.sql_start..upto].to_string()));
        }
    }

    /// `{{#if guard}}`, `{{else}}`, `{{/if}}`. Markers emit no SQL and no token: identifier
    /// checks see through them, so `FROM {{#if x}}{{ t }}{{/if}}` is still refused.
    fn block(&mut self, marker: BlockMarker<'_>, pos: Position) -> Result<(), TemplateError> {
        match marker {
            BlockMarker::If(condition) => {
                let guard = compile_guard(condition, pos)?;
                self.dynamic = true;
                self.guards.push(guard);
                let tokens = (self.prev.clone(), self.prev2.clone(), self.limit_clause);
                self.blocks.push(OpenBlock {
                    position: pos,
                    guard: self.guards.len() - 1,
                    outer: std::mem::take(&mut self.pieces),
                    then: None,
                    tokens,
                });
            }
            BlockMarker::Else => {
                let open = self.blocks.last_mut().ok_or(TemplateError::UnopenedBlock {
                    pos,
                    marker: "else",
                })?;
                if open.then.is_some() {
                    return Err(TemplateError::DuplicateElse(pos));
                }
                open.then = Some(std::mem::take(&mut self.pieces));
                let (prev, prev2, limit) = open.tokens.clone();
                self.prev = prev;
                self.prev2 = prev2;
                self.limit_clause = limit;
            }
            BlockMarker::End => {
                let open = self
                    .blocks
                    .pop()
                    .ok_or(TemplateError::UnopenedBlock { pos, marker: "/if" })?;
                let last = std::mem::replace(&mut self.pieces, open.outer);
                let (then, otherwise) = match open.then {
                    Some(then) => (then, last),
                    None => (last, Vec::new()),
                };
                self.pieces.push(Piece::Block {
                    guard: open.guard,
                    then,
                    otherwise,
                });
            }
        }
        Ok(())
    }
}

enum BlockMarker<'a> {
    If(&'a str),
    Else,
    End,
}

/// `#if <guard>`, `else`, `/if` (inner whitespace allowed: `{{ #if x }}`, `{{ /if }}`).
/// `#if <guard>` and `/if` are not valid zen expressions, so reading them as markers changes
/// the meaning of no template that compiles as plain placeholders; only a bare `{{ else }}`
/// (a request field named "else") and a bare `{{#if}}` are read differently.
fn block_marker(source: &str) -> Option<BlockMarker<'_>> {
    if let Some(rest) = source.strip_prefix('#') {
        let condition = rest.trim_start().strip_prefix("if")?;
        return match condition.chars().next() {
            None => Some(BlockMarker::If("")),
            Some(c) if c.is_whitespace() || c == '(' => Some(BlockMarker::If(condition.trim())),
            _ => None,
        };
    }
    if let Some(rest) = source.strip_prefix('/') {
        return (rest.trim() == "if").then_some(BlockMarker::End);
    }
    (source == "else").then_some(BlockMarker::Else)
}

/// Splits `text` on the word `sep` (`or` / `and`) where it stands alone at nesting depth 0,
/// outside string literals.
fn split_top_level<'a>(text: &'a str, sep: &str) -> Vec<&'a str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut start, mut i) = (0i32, 0usize, 0usize);
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == b'\\' {
                i += 1;
            } else if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0
                && bytes.len() - i >= sep.len()
                && bytes[i..i + sep.len()].eq_ignore_ascii_case(sep.as_bytes())
                && (i == 0 || !is_word_char(bytes[i - 1]))
                && bytes.get(i + sep.len()).is_none_or(|b| !is_word_char(*b)) =>
            {
                parts.push(&text[start..i]);
                i += sep.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&text[start..]);
    parts
}

fn compile_guard(condition: &str, pos: Position) -> Result<Guard, TemplateError> {
    if condition.is_empty() {
        return Err(TemplateError::EmptyCondition(pos));
    }
    let invalid = |expression: &str, message: String| TemplateError::InvalidExpression {
        pos,
        expression: expression.to_string(),
        message,
    };
    let mut any_of = Vec::new();
    for alternative in split_top_level(condition, "or") {
        let mut all_of = Vec::new();
        for term in split_top_level(alternative, "and") {
            let mut term = term.trim();
            let mut negate = false;
            while let Some(rest) = term
                .strip_prefix("not")
                .filter(|r| r.starts_with(|c: char| c.is_whitespace() || c == '('))
            {
                negate = !negate;
                term = rest.trim_start();
            }
            if term.is_empty() {
                return Err(invalid(condition, "a condition term is empty".to_string()));
            }
            let expression = compile_expression(term).map_err(|e| invalid(term, e.to_string()))?;
            all_of.push(Term { negate, expression });
        }
        any_of.push(all_of);
    }
    Ok(Guard {
        source: Arc::from(condition),
        position: pos,
        any_of,
    })
}

#[derive(Debug, Clone, Copy)]
enum QuoteKind {
    String,
    Identifier,
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}
