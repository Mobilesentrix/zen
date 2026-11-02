//! Walks the SQL with a small lexer to find placeholders and refuse those in positions where a
//! bind parameter cannot work (inside string literals, inside quoted identifiers, after
//! `FROM`/`JOIN`/`ORDER BY`/`.` …).

use super::guard::{block_marker, compile_guard, BlockMarker};
use super::{
    render, Dialect, Guard, Piece, Placeholder, Position, QueryTemplate, SlotKind, TemplateError,
};
use std::sync::Arc;
use zen_expression::compile_expression;

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

#[derive(Debug, Clone, Copy)]
enum QuoteKind {
    String,
    Identifier,
}

/// What came just before the cursor in normal SQL context (comments/whitespace skipped).
#[derive(Debug, Clone, Default)]
struct TokenContext {
    previous: Option<Token>,
    before_previous: Option<Token>,
    /// Inside a `LIMIT` / `OFFSET` clause: a placeholder here is a [`SlotKind::Count`].
    limit_clause: bool,
}

struct OpenBlock {
    position: Position,
    guard: usize,
    /// Pieces before the block (restored when it closes).
    outer: Vec<Piece>,
    /// The `then` branch once `{{else}}` was seen.
    then: Option<Vec<Piece>>,
    /// Token context at `{{#if}}`, restored for the `{{else}}` branch.
    context_at_open: TokenContext,
}

pub(super) struct Parser<'a> {
    source: &'a str,
    bytes: &'a [u8],
    dialect: Dialect,
    cursor: usize,
    pieces: Vec<Piece>,
    placeholders: Vec<Placeholder>,
    /// Start of the SQL text not yet copied into `pieces`.
    sql_start: usize,
    context: TokenContext,
    /// Paren stack: `true` when the paren opened a FROM-as-value function.
    parens: Vec<bool>,
    literal_marker: Option<(Position, String)>,
    guards: Vec<Guard>,
    /// Open `{{#if}}` blocks, innermost last.
    blocks: Vec<OpenBlock>,
    dynamic: bool,
}

impl<'a> Parser<'a> {
    pub(super) fn new(source: &'a str, dialect: Dialect) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            dialect,
            cursor: 0,
            pieces: Vec::new(),
            placeholders: Vec::new(),
            sql_start: 0,
            context: TokenContext::default(),
            parens: Vec::new(),
            literal_marker: None,
            guards: Vec::new(),
            blocks: Vec::new(),
            dynamic: false,
        }
    }

    pub(super) fn run(mut self) -> Result<QueryTemplate, TemplateError> {
        while self.cursor < self.bytes.len() {
            self.scan_next()?;
        }
        self.finish()
    }

    fn scan_next(&mut self) -> Result<(), TemplateError> {
        let c = self.bytes[self.cursor];
        match c {
            b'{' if self.peek(1) == Some(b'{') => self.placeholder()?,
            b'\'' => {
                let escapes = self.dialect == Dialect::MySql || self.is_postgres_escape_string();
                self.quoted(b'\'', escapes, QuoteKind::String)?;
            }
            b'"' => match self.dialect {
                Dialect::MySql => self.quoted(b'"', true, QuoteKind::String)?,
                Dialect::Postgres => self.quoted(b'"', false, QuoteKind::Identifier)?,
            },
            b'`' if self.dialect == Dialect::MySql => {
                self.quoted(b'`', false, QuoteKind::Identifier)?;
            }
            b'-' if self.peek(1) == Some(b'-') && self.is_line_comment_dash() => {
                self.line_comment();
            }
            b'#' if self.dialect == Dialect::MySql => self.line_comment(),
            b'/' if self.peek(1) == Some(b'*') => self.block_comment(),
            b'$' if self.dialect == Dialect::Postgres => self.dollar()?,
            b'?' if self.dialect == Dialect::MySql => self.literal_question_mark(),
            b'(' => self.open_paren(),
            b')' => {
                self.parens.pop();
                self.push_token(Token::Punct(b')'));
                self.cursor += 1;
            }
            c if c.is_ascii_whitespace() => self.cursor += 1,
            c if is_word_start(c) => self.word(),
            c if c.is_ascii() => {
                self.push_token(Token::Punct(c));
                self.cursor += 1;
            }
            _ => self.non_ascii(),
        }
        Ok(())
    }

    fn finish(mut self) -> Result<QueryTemplate, TemplateError> {
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
                .push(Piece::Sql(self.source[self.sql_start..].to_string()));
        }
        let scalar_sql: Arc<str> = render(self.dialect, &self.pieces, &|_| 1).into();
        Ok(QueryTemplate {
            dialect: self.dialect,
            pieces: self.pieces,
            placeholders: self.placeholders,
            guards: self.guards,
            scalar_sql,
            dynamic: self.dynamic,
            read_only: zen_engine::nodes::statement::is_read_only_sql(self.source),
        })
    }

    fn position_at(&self, offset: usize) -> Position {
        let before = &self.source[..offset];
        let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
        let column = self.source[line_start..offset].chars().count() + 1;
        Position { line, column }
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.cursor + offset).copied()
    }

    fn push_token(&mut self, token: Token) {
        match &token {
            Token::Word(word) => self.context.limit_clause = word == "LIMIT" || word == "OFFSET",
            Token::Punct(b'(' | b')') => self.context.limit_clause = false,
            Token::Punct(_) => {}
        }
        self.context.before_previous = self.context.previous.take();
        self.context.previous = Some(token);
    }

    fn literal_question_mark(&mut self) {
        if self.literal_marker.is_none() {
            self.literal_marker = Some((self.position_at(self.cursor), "?".to_string()));
        }
        self.push_token(Token::Punct(b'?'));
        self.cursor += 1;
    }

    fn open_paren(&mut self) {
        let opens_from_function = matches!(&self.context.previous, Some(Token::Word(word))
            if FROM_VALUE_FUNCTIONS.contains(&word.as_str()));
        self.parens.push(opens_from_function);
        self.push_token(Token::Punct(b'('));
        self.cursor += 1;
    }

    fn word(&mut self) {
        let start = self.cursor;
        while self.cursor < self.bytes.len() && is_word_char(self.bytes[self.cursor]) {
            self.cursor += 1;
        }
        let word = self.source[start..self.cursor].to_ascii_uppercase();
        self.push_token(Token::Word(word));
    }

    /// Non-ASCII outside quotes: advance one UTF-8 char.
    fn non_ascii(&mut self) {
        let width = self.source[self.cursor..]
            .chars()
            .next()
            .map_or(1, char::len_utf8);
        self.push_token(Token::Punct(b'~'));
        self.cursor += width;
    }

    /// `E'…'` / `e'…'` in Postgres: the `E` must be a standalone prefix.
    fn is_postgres_escape_string(&self) -> bool {
        if self.dialect != Dialect::Postgres || self.cursor == 0 {
            return false;
        }
        let previous = self.bytes[self.cursor - 1];
        if previous != b'E' && previous != b'e' {
            return false;
        }
        self.cursor < 2 || !is_word_char(self.bytes[self.cursor - 2])
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
        while self.cursor < self.bytes.len() && self.bytes[self.cursor] != b'\n' {
            self.cursor += 1;
        }
    }

    fn block_comment(&mut self) {
        let nests = self.dialect == Dialect::Postgres;
        let mut depth = 0usize;
        while self.cursor < self.bytes.len() {
            if self.bytes[self.cursor] == b'/' && self.peek(1) == Some(b'*') {
                if depth == 0 || nests {
                    depth += 1;
                }
                self.cursor += 2;
            } else if self.bytes[self.cursor] == b'*' && self.peek(1) == Some(b'/') {
                depth -= 1;
                self.cursor += 2;
                if depth == 0 {
                    return;
                }
            } else {
                self.cursor += 1;
            }
        }
    }

    fn quoted(&mut self, quote: u8, backslash: bool, kind: QuoteKind) -> Result<(), TemplateError> {
        self.cursor += 1;
        while self.cursor < self.bytes.len() {
            let c = self.bytes[self.cursor];
            if backslash && c == b'\\' {
                self.cursor += 2;
                continue;
            }
            if c == quote {
                if self.peek(1) == Some(quote) {
                    self.cursor += 2;
                    continue;
                }
                self.cursor += 1;
                break;
            }
            if c == b'{' && self.peek(1) == Some(b'{') {
                return Err(self.quoted_error(quote, kind));
            }
            self.cursor += 1;
        }
        self.push_token(match kind {
            QuoteKind::String => Token::Punct(b'\''),
            QuoteKind::Identifier => Token::Word("\"IDENT\"".to_string()),
        });
        Ok(())
    }

    fn quoted_error(&self, quote: u8, kind: QuoteKind) -> TemplateError {
        let pos = self.position_at(self.cursor);
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
        if self.cursor > 0 && is_word_char(self.bytes[self.cursor - 1]) {
            self.cursor += 1;
            return Ok(());
        }
        if matches!(self.peek(1), Some(c) if c.is_ascii_digit()) {
            self.positional_parameter();
            return Ok(());
        }
        let tag_end = self.dollar_tag_end();
        if self.bytes.get(tag_end) == Some(&b'$') {
            return self.dollar_quoted(tag_end);
        }
        self.push_token(Token::Punct(b'$'));
        self.cursor += 1;
        Ok(())
    }

    fn positional_parameter(&mut self) {
        let start = self.cursor;
        let mut end = start + 1;
        while end < self.bytes.len() && self.bytes[end].is_ascii_digit() {
            end += 1;
        }
        if self.literal_marker.is_none() {
            self.literal_marker =
                Some((self.position_at(start), self.source[start..end].to_string()));
        }
        self.cursor = end;
        self.push_token(Token::Punct(b'?'));
    }

    /// The offset after an optional tag (`[A-Za-z_][A-Za-z0-9_]*`) following the `$` at the cursor.
    fn dollar_tag_end(&self) -> usize {
        let mut end = self.cursor + 1;
        if end < self.bytes.len()
            && (self.bytes[end].is_ascii_alphabetic() || self.bytes[end] == b'_')
        {
            while end < self.bytes.len()
                && (self.bytes[end].is_ascii_alphanumeric() || self.bytes[end] == b'_')
            {
                end += 1;
            }
        }
        end
    }

    /// A dollar-quoted string whose opening tag ends with the `$` at `closing_dollar`.
    fn dollar_quoted(&mut self, closing_dollar: usize) -> Result<(), TemplateError> {
        let tag = &self.source[self.cursor..=closing_dollar];
        let body_start = closing_dollar + 1;
        let close = self.source[body_start..]
            .find(tag)
            .map(|offset| body_start + offset);
        let body_end = close.unwrap_or(self.bytes.len());
        if let Some(offset) = self.source[body_start..body_end].find("{{") {
            self.cursor = body_start + offset;
            return Err(self.quoted_error(b'$', QuoteKind::String));
        }
        self.cursor = close.map_or(self.bytes.len(), |close| close + tag.len());
        self.push_token(Token::Punct(b'\''));
        Ok(())
    }

    fn identifier_keyword(&self) -> Option<String> {
        match &self.context.previous {
            Some(Token::Punct(b'.')) => Some(".".to_string()),
            Some(Token::Word(word)) => match word.as_str() {
                "FROM" => {
                    let in_value_function = self.parens.last().copied().unwrap_or(false);
                    let distinct = matches!(&self.context.before_previous,
                        Some(Token::Word(before)) if before == "DISTINCT");
                    (!in_value_function && !distinct).then(|| "FROM".to_string())
                }
                "JOIN" | "INTO" | "UPDATE" | "TABLE" | "AS" => Some(word.clone()),
                "BY" => match &self.context.before_previous {
                    Some(Token::Word(before))
                        if before == "ORDER" || before == "GROUP" || before == "PARTITION" =>
                    {
                        Some(format!("{before} BY"))
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        }
    }

    /// The offset of the `}}` closing the placeholder whose body starts at `body_start`, skipping
    /// the expression's string literals.
    fn find_placeholder_close(&self, body_start: usize) -> Result<Option<usize>, TemplateError> {
        let mut offset = body_start;
        while offset < self.bytes.len() {
            let c = self.bytes[offset];
            match c {
                b'\'' | b'"' | b'`' => {
                    offset += 1;
                    while offset < self.bytes.len() && self.bytes[offset] != c {
                        if self.bytes[offset] == b'\\' {
                            offset += 1;
                        }
                        offset += 1;
                    }
                    offset += 1;
                }
                b'{' if self.bytes.get(offset + 1) == Some(&b'{') => {
                    return Err(TemplateError::Nested(self.position_at(offset)));
                }
                b'}' if self.bytes.get(offset + 1) == Some(&b'}') => return Ok(Some(offset)),
                _ => offset += 1,
            }
        }
        Ok(None)
    }

    fn placeholder(&mut self) -> Result<(), TemplateError> {
        let open = self.cursor;
        let pos = self.position_at(open);
        let body_start = open + 2;
        let Some(close) = self.find_placeholder_close(body_start)? else {
            return Err(TemplateError::Unterminated(pos));
        };
        let source = self.source[body_start..close].trim();
        if source.is_empty() {
            return Err(TemplateError::Empty(pos));
        }
        if let Some(marker) = block_marker(source) {
            self.flush_sql(open);
            self.cursor = close + 2;
            self.sql_start = self.cursor;
            return self.block(marker, pos);
        }
        let kind = if self.context.limit_clause {
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
        self.cursor = close + 2;
        self.sql_start = self.cursor;
        self.push_token(Token::Punct(b'?'));
        Ok(())
    }

    fn flush_sql(&mut self, upto: usize) {
        if self.sql_start < upto {
            self.pieces
                .push(Piece::Sql(self.source[self.sql_start..upto].to_string()));
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
                self.blocks.push(OpenBlock {
                    position: pos,
                    guard: self.guards.len() - 1,
                    outer: std::mem::take(&mut self.pieces),
                    then: None,
                    context_at_open: self.context.clone(),
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
                self.context = open.context_at_open.clone();
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

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

pub(super) fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}
