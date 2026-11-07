//! A structured description of why a node failed, for the failing node's trace
//! entry (`error` on [`crate::DecisionGraphTrace`], see MS-FFI.md "Error trace").
//!
//! A node that knows *what* failed (the expression, the placeholder, the query) returns its error
//! wrapped in [`FailedNode`]: the wrapper displays exactly as the wrapped error, so the error text
//! (`source` in the FFI JSON) and every existing message stay byte-identical. Errors that are not
//! wrapped still get an `error` entry, described from their text ([`describe`]).
//!
//! Nothing here runs on a successful evaluation, and the expression / decision-table handlers
//! only build a [`NodeFailure`] when the evaluation is traced.

use crate::nodes::context::ValidationErrorJson;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::rc::Rc;
use zen_expression::intellisense::{IntelliSense, ReadDependency};
use zen_expression::vm::VMError;
use zen_expression::IsolateError;
use zen_types::symbol::Symbol;
use zen_types::variable::{ToVariable, Variable};

/// What kind of failure, in the words the Run panel uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureKind {
    /// The rule text cannot be read: lexer, parser or compiler error, invalid node config or
    /// query template.
    Parse,
    /// The expression ran and failed for another reason (database error, function error, …).
    Evaluation,
    /// A value the expression needs is absent or null.
    MissingValue,
    /// A value has the wrong type for what the expression does with it.
    Type,
}

/// Which part of the node failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailurePart {
    /// An expression node row (`key` names its output).
    Expression,
    /// The node's input field (`inputField` transform, decision tables and expression nodes).
    InputField,
    /// A `{{ … }}` placeholder of a database query.
    Placeholder,
    /// A database query (template or execution).
    Query,
    /// The node's configuration.
    Config,
    /// A step inside a sub-decision (`nodeId` names it).
    SubDecision,
}

/// The `error` object of the failing node's trace entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeFailure {
    pub kind: FailureKind,
    /// The underlying cause, e.g. `VM error: Opcode Subtract: Unsupported type`.
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<FailurePart>,
    /// The exact text that failed: the expression, the cell, the placeholder or the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
    /// Expression node: the output key whose expression failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Paths the expression reads that were absent or null when it failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// Sub-decision: the id of the step inside it that failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl NodeFailure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            part: None,
            expression: None,
            key: None,
            missing: Vec::new(),
            node_id: None,
        }
    }

    /// A failed expression: kind from the isolate error, `missing` from the reads of `source`
    /// that are null in `env` (`$` paths resolve against `dollar`). A type error on null operands
    /// is reported as a missing value.
    pub fn expression(
        part: FailurePart,
        source: &str,
        error: &IsolateError,
        env: &Variable,
        dollar: Option<&Variable>,
    ) -> Self {
        let missing = missing_reads(source, false, env, dollar);
        let kind = match isolate_kind(error) {
            FailureKind::Type if !missing.is_empty() => FailureKind::MissingValue,
            kind => kind,
        };
        Self {
            part: Some(part),
            expression: Some(source.to_string()),
            missing,
            ..Self::new(kind, error.to_string())
        }
    }

    pub fn with_part(mut self, part: FailurePart) -> Self {
        self.part = Some(part);
        self
    }

    pub fn with_expression(mut self, expression: impl Into<String>) -> Self {
        self.expression = Some(expression.into());
        self
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn with_missing(mut self, missing: Vec<String>) -> Self {
        self.missing = missing;
        self
    }
}

impl ToVariable for NodeFailure {
    fn to_variable(&self) -> Variable {
        serde_json::to_value(self)
            .map(Variable::from)
            .unwrap_or(Variable::Null)
    }
}

/// A node error that knows what failed. Displays as the wrapped error, and is its `source()`.
#[derive(Debug)]
pub struct FailedNode {
    pub failure: NodeFailure,
    pub error: Box<dyn Error>,
}

impl FailedNode {
    pub fn new(failure: NodeFailure, error: impl Into<Box<dyn Error>>) -> Self {
        Self {
            failure,
            error: error.into(),
        }
    }
}

impl Display for FailedNode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.error, f)
    }
}

impl Error for FailedNode {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// The error as the node raised it, without a [`FailedNode`] wrapper (for downcasts).
pub fn unwrapped<'a>(error: &'a (dyn Error + 'static)) -> &'a (dyn Error + 'static) {
    match error.downcast_ref::<FailedNode>() {
        Some(failed) => failed.error.as_ref(),
        None => error,
    }
}

/// The failure of a node error: the structured one when the node attached it, otherwise one
/// described from the error itself.
pub fn describe(error: &(dyn Error + 'static)) -> NodeFailure {
    if let Some(failed) = error.downcast_ref::<FailedNode>() {
        return failed.failure.clone();
    }
    if let Some(error) = error.downcast_ref::<IsolateError>() {
        return NodeFailure::new(isolate_kind(error), error.to_string());
    }
    if error.downcast_ref::<ValidationErrorJson>().is_some() {
        return NodeFailure::new(FailureKind::Type, error.to_string());
    }
    NodeFailure::new(FailureKind::Evaluation, error.to_string())
}

/// A sub-decision failed at `node_id`: its step's failure, pointed at that step.
pub fn sub_decision(node_id: &str, source: &(dyn Error + 'static)) -> NodeFailure {
    let inner = describe(source);
    NodeFailure {
        part: Some(FailurePart::SubDecision),
        node_id: Some(inner.node_id.clone().unwrap_or_else(|| node_id.to_string())),
        ..inner
    }
}

/// The kind of an expression error.
pub fn isolate_kind(error: &IsolateError) -> FailureKind {
    match error {
        IsolateError::LexerError { .. }
        | IsolateError::ParserError { .. }
        | IsolateError::CompilerError { .. } => FailureKind::Parse,
        IsolateError::ValueCastError => FailureKind::Type,
        IsolateError::VMError { source } => match source {
            VMError::OpcodeErr { message, .. } if message == "Unsupported type" => {
                FailureKind::Type
            }
            VMError::NumberConversionError | VMError::ParseDateTimeErr { .. } => FailureKind::Type,
            _ => FailureKind::Evaluation,
        },
        IsolateError::ReferenceError | IsolateError::MissingContextReference => {
            FailureKind::Evaluation
        }
    }
}

/// Dotted paths `source` reads that are absent or null in `env` (`$…` against `dollar`), in the
/// order they appear, without duplicates. Empty when the source does not parse.
pub fn missing_reads(
    source: &str,
    unary: bool,
    env: &Variable,
    dollar: Option<&Variable>,
) -> Vec<String> {
    let mut intellisense = IntelliSense::new();
    let reads = match unary {
        true => intellisense.reads_unary(source),
        false => intellisense.reads(source),
    };
    let mut missing: Vec<String> = Vec::new();
    for read in &reads {
        // a read through an index (`lines[0].qty`) has the index elided from its path; the
        // collection itself is reported as its own read
        let ReadDependency::Direct {
            path,
            via_index: false,
            ..
        } = read
        else {
            continue;
        };
        if path.is_empty() || resolves(path, env, dollar) {
            continue;
        }
        let joined = path
            .iter()
            .map(|segment| segment.as_ref())
            .collect::<Vec<_>>()
            .join(".");
        if !missing.contains(&joined) {
            missing.push(joined);
        }
    }
    // `a` and `a.b` both missing: keep the more specific `a.b`
    let all = missing.clone();
    missing.retain(|path| {
        !all.iter()
            .any(|other| other.len() > path.len() && other.starts_with(&format!("{path}.")))
    });
    missing
}

/// Whether `path` reaches a non-null value.
fn resolves(path: &[Rc<str>], env: &Variable, dollar: Option<&Variable>) -> bool {
    let (mut current, rest) = match (path[0].as_ref(), dollar) {
        ("$", Some(dollar)) => (dollar.shallow_clone(), &path[1..]),
        ("$", None) => return true,
        _ => (env.shallow_clone(), path),
    };
    for segment in rest {
        let next = match &current {
            Variable::Object(object) => object
                .borrow()
                .get(&Symbol::from(segment.as_ref()))
                .map(Variable::shallow_clone),
            Variable::Array(array) => segment
                .parse::<usize>()
                .ok()
                .and_then(|index| array.borrow().get(index).map(Variable::shallow_clone)),
            // a value we cannot see into (dates, intervals): not reported as missing
            Variable::Dynamic(_) => return true,
            _ => None,
        };
        match next {
            Some(value) => current = value,
            None => return false,
        }
    }
    !matches!(current, Variable::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zen_expression::Isolate;

    fn failure(source: &str, env: serde_json::Value) -> NodeFailure {
        let env = Variable::from(env);
        let error = Isolate::with_environment(env.shallow_clone())
            .run_standard(source)
            .expect_err("the expression fails");
        NodeFailure::expression(FailurePart::Expression, source, &error, &env, None)
    }

    #[test]
    fn null_operands_are_a_missing_value() {
        let f = failure("missing.price - cost", json!({"sku": "1"}));
        assert_eq!(f.kind, FailureKind::MissingValue);
        assert_eq!(f.missing, vec!["missing.price", "cost"]);
        assert_eq!(f.expression.as_deref(), Some("missing.price - cost"));
        assert_eq!(f.message, "VM error: Opcode Subtract: Unsupported type");
    }

    #[test]
    fn present_null_counts_as_missing() {
        let f = failure("a.b * 2", json!({"a": {"b": null}}));
        assert_eq!(f.kind, FailureKind::MissingValue);
        assert_eq!(f.missing, vec!["a.b"]);
    }

    #[test]
    fn wrong_type_is_a_type_error() {
        let f = failure("name - 1", json!({"name": "x"}));
        assert_eq!(f.kind, FailureKind::Type);
        assert!(f.missing.is_empty());
    }

    #[test]
    fn syntax_error_is_a_parse_error() {
        let f = failure("price +* 2", json!({"price": 1}));
        assert_eq!(f.kind, FailureKind::Parse);
    }

    #[test]
    fn wrapper_displays_as_the_wrapped_error() {
        let failed = FailedNode::new(
            NodeFailure::new(FailureKind::Evaluation, "cause"),
            "Failed to evaluate expression: \"x\"".to_string(),
        );
        assert_eq!(failed.to_string(), "Failed to evaluate expression: \"x\"");
        let boxed: Box<dyn Error> = Box::new(failed);
        assert_eq!(describe(boxed.as_ref()).message, "cause");
        assert_eq!(
            unwrapped(boxed.as_ref()).to_string(),
            "Failed to evaluate expression: \"x\""
        );
    }

    #[test]
    fn serialises_only_what_is_known() {
        let f = NodeFailure::new(FailureKind::Evaluation, "boom");
        assert_eq!(
            serde_json::to_value(&f).expect("serialises"),
            json!({"kind": "evaluation", "message": "boom"})
        );
    }
}
