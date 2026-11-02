//! The `{{#if …}}`, `{{else}}`, `{{/if}}` markers and the conditions of optional blocks.

use super::parser::is_word_char;
use super::{Position, TemplateError};
use std::sync::Arc;
use zen_expression::expression::Standard;
use zen_expression::{compile_expression, Expression};

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

#[derive(Clone, Copy)]
pub(super) enum BlockMarker<'a> {
    If(&'a str),
    Else,
    End,
}

/// `#if <guard>`, `else`, `/if` (inner whitespace allowed: `{{ #if x }}`, `{{ /if }}`).
/// `#if <guard>` and `/if` are not valid zen expressions, so reading them as markers changes
/// the meaning of no template that compiles as plain placeholders; only a bare `{{ else }}`
/// (a request field named "else") and a bare `{{#if}}` are read differently.
pub(super) fn block_marker(source: &str) -> Option<BlockMarker<'_>> {
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

/// Whether the word `separator` stands alone at `index`.
fn is_separator_at(bytes: &[u8], index: usize, separator: &str) -> bool {
    bytes.len() - index >= separator.len()
        && bytes[index..index + separator.len()].eq_ignore_ascii_case(separator.as_bytes())
        && (index == 0 || !is_word_char(bytes[index - 1]))
        && bytes
            .get(index + separator.len())
            .is_none_or(|b| !is_word_char(*b))
}

/// Splits `text` on the word `separator` (`or` / `and`) where it stands alone at nesting depth 0,
/// outside string literals.
fn split_top_level<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let bytes = text.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut start, mut index) = (0i32, 0usize, 0usize);
    let mut quote: Option<u8> = None;
    while index < bytes.len() {
        let c = bytes[index];
        if let Some(open_quote) = quote {
            if c == b'\\' {
                index += 1;
            } else if c == open_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = Some(c),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            _ if depth == 0 && is_separator_at(bytes, index, separator) => {
                parts.push(&text[start..index]);
                index += separator.len();
                start = index;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(&text[start..]);
    parts
}

/// Strips leading `not`s from a term: the remaining text, and whether they negate it.
fn strip_negations(term: &str) -> (&str, bool) {
    let mut term = term.trim();
    let mut negate = false;
    while let Some(rest) = term
        .strip_prefix("not")
        .filter(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '('))
    {
        negate = !negate;
        term = rest.trim_start();
    }
    (term, negate)
}

pub(super) fn compile_guard(condition: &str, pos: Position) -> Result<Guard, TemplateError> {
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
            let (term, negate) = strip_negations(term);
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
