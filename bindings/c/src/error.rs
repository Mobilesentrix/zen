use serde_json::{json, Value};
use strum::{EnumDiscriminants, FromRepr};

#[allow(dead_code)]
#[derive(EnumDiscriminants)]
#[strum_discriminants(derive(FromRepr))]
#[repr(u8)]
pub enum ZenError {
    Zero,

    InvalidArgument,
    StringNullError,
    StringUtf8Error,
    JsonSerializationFailed,
    JsonDeserializationFailed,

    IsolateError(Value),
    EvaluationError(Value),

    LoaderKeyNotFound {
        key: String,
    },
    LoaderInternalError {
        key: String,
        message: String,
    },

    TemplateEngineError {
        template: String,
        message: String,
    },

    LoaderConfigError {
        message: String,
    },

    /// Invalid database connection map (code 12).
    DatabaseConfigError {
        message: String,
    },

    /// A Rust panic caught at the C boundary (code 13); see guard.rs.
    Panic {
        message: String,
    },
}

impl ZenError {
    pub fn details(&self) -> Option<String> {
        match &self {
            ZenError::IsolateError(error) => Some(error.to_string()),
            ZenError::EvaluationError(error) => Some(error.to_string()),
            ZenError::LoaderKeyNotFound { key } => Some(json!({ "key": key }).to_string()),
            ZenError::LoaderInternalError { key, message } => {
                Some(json!({ "key": key, "message": message }).to_string())
            }
            ZenError::TemplateEngineError { template, message } => {
                Some(json!({ "template": template, "message": message }).to_string())
            }
            ZenError::DatabaseConfigError { message } => {
                Some(json!({ "message": message }).to_string())
            }
            ZenError::LoaderConfigError { message } => {
                Some(json!({ "message": message }).to_string())
            }
            ZenError::Panic { message } => Some(json!({ "message": message }).to_string()),
            _ => None,
        }
    }
}
