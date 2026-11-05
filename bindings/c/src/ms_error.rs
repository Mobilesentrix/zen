//! Evaluation errors as the JSON `details` of a `ZenResult`, with the structured
//! part of a result-handling node error (`details`) added and, for a traced evaluation, the
//! trace the engine collected up to the failing node; see MS-FFI.md §1/§3.

use ms_zen_database::ResultHandlingError;
use serde_json::Value;
use zen_engine::{EvaluationError, EvaluationTraceKind};

/// Upstream's serialisation (`type`, `source`, `nodeId`, …) plus:
/// * `trace` — when `traced` (the caller evaluated with `trace: true`): the partial trace, every
///   node that completed plus the failing node with its input, exactly the shape a successful
///   traced response carries.
/// * `details` — for a node error raised by the database node's `resultHandling`, an object
///   with `reason`, `rowsCount`, `rows`.
pub(crate) fn evaluation_error_value(error: &EvaluationError, traced: bool) -> Value {
    let mode = if traced {
        EvaluationTraceKind::Default
    } else {
        EvaluationTraceKind::None
    };
    let mut value = error
        .serialize_with_mode(serde_json::value::Serializer, mode)
        .unwrap_or_else(|_| Value::String(error.to_string()));
    if let EvaluationError::NodeError { source, .. } = error {
        if let Some(details) = source.downcast_ref::<ResultHandlingError>() {
            if let Value::Object(map) = &mut value {
                map.insert("details".to_string(), details.details());
            }
        }
    }
    value
}
