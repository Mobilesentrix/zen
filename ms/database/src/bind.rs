//! Evaluates placeholders against the node input and turns the results into bind values.

use crate::template::{Dialect, Guard, Piece, Placeholder, QueryTemplate, SlotKind};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::Arc;
use zen_engine::nodes::failure::{isolate_kind, FailureKind};
use zen_expression::vm::VM;
use zen_expression::Variable;

/// Wire protocols cap a statement at 65 535 bind parameters (u16 count).
pub const MAX_BIND_PARAMS: usize = u16::MAX as usize;

/// A value bound to one `?` / `$n` slot. `Send` so it can cross into the DB runtime.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BindValue {
    Bool(bool),
    Int(i64),
    Decimal(Decimal),
    Text(String),
}

/// SQL + ordered bind values for one execution.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundQuery {
    pub sql: Arc<str>,
    pub params: Vec<BindValue>,
}

/// Identifies the node in error messages.
#[derive(Debug, Clone, Copy)]
pub struct NodeLabel<'a> {
    pub name: &'a str,
    pub id: &'a str,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BindError {
    #[error("Database query placeholder \"{{{{ {expression} }}}}\" in node \"{node_name}\" ({node_id}) {problem}")]
    Placeholder {
        expression: String,
        node_name: String,
        node_id: String,
        problem: String,
        /// MobileSentrix error trace: what kind of failure this is (not part of the message).
        kind: FailureKind,
    },
    #[error("query expands to {count} bind parameters, more than the {MAX_BIND_PARAMS} the {protocol} protocol allows; narrow the query or batch it")]
    TooManyParams {
        count: usize,
        protocol: &'static str,
    },
}

enum Resolved {
    Scalar(BindValue),
    List(Vec<BindValue>),
}

/// Evaluates every placeholder against `context` and produces the final SQL + params.
pub fn bind(
    template: &QueryTemplate,
    context: &Variable,
    node: NodeLabel<'_>,
) -> Result<BoundQuery, BindError> {
    if template.is_dynamic() {
        return bind_dynamic(template, context, node);
    }
    let mut vm = VM::new();
    let mut resolved = Vec::with_capacity(template.placeholders().len());
    let mut any_list = false;
    for placeholder in template.placeholders() {
        let value = placeholder
            .expression
            .evaluate_with(context.shallow_clone(), &mut vm)
            .map_err(|e| {
                let kind = isolate_kind(&e);
                problem(placeholder, node, kind, format!("failed to evaluate: {e}"))
            })?;
        let r = resolve(&value, placeholder, node, context)?;
        any_list |= matches!(r, Resolved::List(_));
        resolved.push(r);
    }

    let count: usize = resolved
        .iter()
        .map(|r| match r {
            Resolved::Scalar(_) => 1,
            Resolved::List(v) => v.len(),
        })
        .sum();
    check_param_count(template, count)?;

    let sql = if any_list {
        let widths: Vec<usize> = resolved
            .iter()
            .map(|r| match r {
                Resolved::Scalar(_) => 1,
                Resolved::List(v) => v.len(),
            })
            .collect();
        Arc::from(template.render(&widths))
    } else {
        template.scalar_sql().clone()
    };

    let mut params = Vec::with_capacity(count);
    for r in resolved {
        match r {
            Resolved::Scalar(v) => params.push(v),
            Resolved::List(v) => params.extend(v),
        }
    }
    Ok(BoundQuery { sql, params })
}

fn check_param_count(template: &QueryTemplate, count: usize) -> Result<(), BindError> {
    if count > MAX_BIND_PARAMS {
        return Err(BindError::TooManyParams {
            count,
            protocol: match template.dialect() {
                Dialect::MySql => "MySQL",
                Dialect::Postgres => "PostgreSQL",
            },
        });
    }
    Ok(())
}

/// Templates with `{{#if}}` blocks or LIMIT/OFFSET counts: walks the pieces, keeps the branch
/// each guard picks, and evaluates only the placeholders of kept branches (a placeholder in a
/// dropped filter may well be null: that is why the filter was dropped). Values are bound
/// exactly as on the static path; the only text that varies is the template's own SQL and the
/// digits of a checked count.
fn bind_dynamic(
    template: &QueryTemplate,
    context: &Variable,
    node: NodeLabel<'_>,
) -> Result<BoundQuery, BindError> {
    let mut walk = Walk {
        template,
        context,
        node,
        vm: VM::new(),
        sql: String::with_capacity(template.scalar_sql().len() + 16),
        params: Vec::new(),
    };
    walk.pieces(&template.pieces)?;
    check_param_count(template, walk.params.len())?;
    Ok(BoundQuery {
        sql: Arc::from(walk.sql),
        params: walk.params,
    })
}

struct Walk<'a, 'n> {
    template: &'a QueryTemplate,
    context: &'a Variable,
    node: NodeLabel<'n>,
    vm: VM,
    sql: String,
    params: Vec<BindValue>,
}

impl Walk<'_, '_> {
    fn pieces(&mut self, pieces: &[Piece]) -> Result<(), BindError> {
        for piece in pieces {
            match piece {
                Piece::Sql(text) => self.sql.push_str(text),
                Piece::Slot(slot) => self.slot(&self.template.placeholders()[*slot])?,
                Piece::Block {
                    guard,
                    then,
                    otherwise,
                } => {
                    let keep = self.guard(&self.template.guards()[*guard])?;
                    self.pieces(if keep { then } else { otherwise })?;
                }
            }
        }
        Ok(())
    }

    fn marker(&mut self) {
        match self.template.dialect() {
            Dialect::MySql => self.sql.push('?'),
            Dialect::Postgres => {
                self.sql.push('$');
                self.sql.push_str(&(self.params.len() + 1).to_string());
            }
        }
    }

    fn evaluate(&mut self, placeholder: &Placeholder) -> Result<Variable, BindError> {
        placeholder
            .expression
            .evaluate_with(self.context.shallow_clone(), &mut self.vm)
            .map_err(|e| {
                let kind = isolate_kind(&e);
                problem(
                    placeholder,
                    self.node,
                    kind,
                    format!("failed to evaluate: {e}"),
                )
            })
    }

    fn slot(&mut self, placeholder: &Placeholder) -> Result<(), BindError> {
        let value = self.evaluate(placeholder)?;
        match placeholder.kind {
            SlotKind::Count => {
                let n = count(&value)
                    .map_err(|(kind, why)| problem(placeholder, self.node, kind, why))?;
                self.sql.push_str(&n.to_string());
            }
            SlotKind::Value => match resolve(&value, placeholder, self.node, self.context)? {
                Resolved::Scalar(v) => {
                    self.marker();
                    self.params.push(v);
                }
                Resolved::List(values) => {
                    for (i, v) in values.into_iter().enumerate() {
                        if i > 0 {
                            self.sql.push_str(", ");
                        }
                        self.marker();
                        self.params.push(v);
                    }
                }
            },
        }
        Ok(())
    }

    fn guard(&mut self, guard: &Guard) -> Result<bool, BindError> {
        for all_of in &guard.any_of {
            let mut all = true;
            for term in all_of {
                let value = term
                    .expression
                    .evaluate_with(self.context.shallow_clone(), &mut self.vm)
                    .map_err(|e| BindError::Placeholder {
                        kind: isolate_kind(&e),
                        expression: format!("#if {}", guard.source),
                        node_name: self.node.name.to_string(),
                        node_id: self.node.id.to_string(),
                        problem: format!("condition failed to evaluate: {e}"),
                    })?;
                if truthy(&value) == term.negate {
                    all = false;
                    break;
                }
            }
            if all {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Guard truthiness, tuned for search requests where "not asked" arrives in many forms: a
/// missing key, null, false, 0, blank text, an empty list or object are false.
pub(crate) fn truthy(value: &Variable) -> bool {
    match value {
        Variable::Null => false,
        Variable::Bool(b) => *b,
        Variable::Number(n) => !n.is_zero(),
        Variable::String(s) => !s.trim().is_empty(),
        Variable::Array(items) => !items.borrow().is_empty(),
        Variable::Object(map) => !map.borrow().is_empty(),
        Variable::Dynamic(_) => true,
    }
}

/// The largest LIMIT / OFFSET a placeholder may write (StarRocks and MySQL take 64-bit counts;
/// anything near this is a bug in the request, not a page size).
pub const MAX_COUNT: u64 = 1_000_000_000;

/// A LIMIT / OFFSET value: a whole number 0..=MAX_COUNT, given as a number or as digits.
fn count(value: &Variable) -> Result<u64, (FailureKind, String)> {
    let n = match value {
        Variable::Number(n) if n.fract().is_zero() && !n.is_sign_negative() => n.to_u64(),
        Variable::String(s) => s.trim().parse::<u64>().ok(),
        Variable::Null => return Err((
            FailureKind::MissingValue,
            "resolved to null; a LIMIT / OFFSET needs a whole number (write `?? 24` for a default)"
                .to_string(),
        )),
        _ => None,
    };
    match n {
        Some(n) if n <= MAX_COUNT => Ok(n),
        _ => Err((
            FailureKind::Type,
            format!(
                "resolved to {}, but a LIMIT / OFFSET takes a whole number from 0 to {MAX_COUNT}",
                describe(value)
            ),
        )),
    }
}

fn describe(value: &Variable) -> String {
    match value {
        Variable::Number(n) => n.to_string(),
        Variable::String(s) => format!("the text \"{}\"", s.chars().take(40).collect::<String>()),
        other => format!("a {}", other.type_name()),
    }
}

fn problem(
    placeholder: &Placeholder,
    node: NodeLabel<'_>,
    kind: FailureKind,
    problem: String,
) -> BindError {
    BindError::Placeholder {
        kind,
        expression: placeholder.source.to_string(),
        node_name: node.name.to_string(),
        node_id: node.id.to_string(),
        problem,
    }
}

fn scalar(value: &Variable) -> Result<BindValue, &'static str> {
    match value {
        Variable::Bool(b) => Ok(BindValue::Bool(*b)),
        Variable::Number(n) => Ok(number(*n)),
        Variable::String(s) => Ok(BindValue::Text(s.to_string())),
        Variable::Null => Err("null"),
        Variable::Array(_) => Err("array"),
        Variable::Object(_) => Err("object"),
        Variable::Dynamic(d) => Err(d.type_name()),
    }
}

fn number(n: Decimal) -> BindValue {
    if n.fract().is_zero() {
        if let Some(i) = n.to_i64() {
            return BindValue::Int(i);
        }
    }
    BindValue::Decimal(n.normalize())
}

fn unbindable(type_name: &str) -> String {
    let mut msg = format!(
        "resolved to a {type_name}, which has no bindable SQL form; convert it in the expression first"
    );
    if type_name.contains("date") {
        msg.push_str(
            " (for a date, `.format('%Y-%m-%d %H:%M:%S')` matches what this node reads back)",
        );
    }
    msg
}

fn resolve(
    value: &Variable,
    placeholder: &Placeholder,
    node: NodeLabel<'_>,
    context: &Variable,
) -> Result<Resolved, BindError> {
    match value {
        Variable::Null => Err(problem(
            placeholder,
            node,
            FailureKind::MissingValue,
            format!(
                "resolved to null. A missing key also evaluates to null, so check the spelling and the shape of this node's input; write `?? <default>` in the placeholder if null is intended{}",
                missing_key_hint(&placeholder.source, context)
            ),
        )),
        Variable::Array(items) => {
            let items = items.borrow();
            if items.is_empty() {
                return Err(problem(
                    placeholder,
                    node,
                    FailureKind::Type,
                    "resolved to an empty array, which has no SQL form — `IN ()` is a syntax error. Guard it upstream, e.g. `{{ len(ids) == 0 ? [-1] : ids }}`".to_string(),
                ));
            }
            let mut out = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                match scalar(item) {
                    Ok(v) => out.push(v),
                    Err("null") => {
                        return Err(problem(
                            placeholder,
                            node,
                            FailureKind::MissingValue,
                            format!("resolved to an array whose element {index} is null"),
                        ))
                    }
                    Err("array") => return Err(problem(
                        placeholder,
                        node,
                        FailureKind::Type,
                        format!("resolved to an array whose element {index} is itself an array; nested arrays are not expanded (tuple syntax such as `IN ((1,2),(3,4))` is unsupported)"),
                    )),
                    Err(t) => {
                        return Err(problem(
                            placeholder,
                            node,
                            FailureKind::Type,
                            format!("resolved to an array whose element {index} is a {t}, which has no bindable SQL form"),
                        ))
                    }
                }
            }
            Ok(Resolved::List(out))
        }
        other => scalar(other)
            .map(Resolved::Scalar)
            .map_err(|t| problem(placeholder, node, FailureKind::Type, unbindable(t))),
    }
}

/// For a plain dotted path that resolved to null, say which part exists and what keys it has.
fn missing_key_hint(source: &str, context: &Variable) -> String {
    let is_path = !source.is_empty()
        && source.split('.').all(|part| {
            let mut chars = part.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        });
    if !is_path {
        return String::new();
    }
    let parts: Vec<&str> = source.split('.').collect();
    let mut current = context.shallow_clone();
    let mut existing = String::new();
    for part in &parts {
        let next = match &current {
            Variable::Object(obj) => obj
                .borrow()
                .iter()
                .find(|(k, _)| k.as_str() == *part)
                .map(|(_, v)| v.shallow_clone()),
            _ => None,
        };
        match next {
            Some(v) if !matches!(v, Variable::Null) => {
                if !existing.is_empty() {
                    existing.push('.');
                }
                existing.push_str(part);
                current = v;
            }
            Some(_) => {
                let full = if existing.is_empty() {
                    part.to_string()
                } else {
                    format!("{existing}.{part}")
                };
                return format!(" (\"{full}\" is present but null)");
            }
            None => break,
        }
    }
    let Variable::Object(obj) = &current else {
        return String::new();
    };
    let obj = obj.borrow();
    let mut keys: Vec<String> = obj
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| k != "$nodes")
        .collect();
    keys.sort();
    const SHOWN: usize = 10;
    let more = keys.len().saturating_sub(SHOWN);
    keys.truncate(SHOWN);
    let subject = if existing.is_empty() {
        "the node input".to_string()
    } else {
        format!("\"{existing}\"")
    };
    let mut hint = format!(" ({subject} exists and has keys: {}", keys.join(", "));
    if more > 0 {
        hint.push_str(&format!(" (+{more} more)"));
    }
    hint.push(')');
    hint
}
