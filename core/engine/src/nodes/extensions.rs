use crate::loader::{DynamicLoader, NoopLoader};
use crate::nodes::custom::{DynamicCustomNode, NoopCustomNode};
use crate::nodes::decision_table::cells::CompiledCells;
use crate::nodes::decision_table::index::TableIndex;
use crate::nodes::function::http_handler::DynamicHttpHandler;
use crate::nodes::function::v2::function::{Function, FunctionConfig};
use crate::nodes::function::v2::module::console::ConsoleListener;
use crate::nodes::function::v2::module::http::listener::HttpListener;
use crate::nodes::function::v2::module::zen::ZenListener;
use crate::nodes::validator_cache::ValidatorCache;
use anyhow::Context;
use std::cell::OnceCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use zen_expression::{DateValue, OpcodeCache, Variable};

/// This is created on every graph evaluation
#[derive(Debug, Clone)]
pub struct NodeHandlerExtensions {
    pub(crate) function_runtime: Arc<tokio::sync::OnceCell<Function>>,
    pub(crate) validator_cache: Arc<OnceCell<ValidatorCache>>,
    pub(crate) loader: DynamicLoader,
    pub(crate) custom_node: DynamicCustomNode,
    pub(crate) http_handler: DynamicHttpHandler,
    pub(crate) compiled_cache: Option<Arc<OpcodeCache>>,
    pub(crate) stripped_functions: Option<Arc<ahash::HashMap<Arc<str>, Arc<str>>>>,
    pub(crate) dt_indexes: Option<Arc<ahash::HashMap<Arc<str>, TableIndex>>>,
    /// Decision-table cells resolved to bytecode, by node id.
    pub(crate) dt_cells: Option<Arc<ahash::HashMap<Arc<str>, CompiledCells>>>,
    /// Whether any value of this evaluation can hold an engine date (see [`DateWatch`]).
    pub(crate) dates: Arc<DateWatch>,
}

/// Tells an input node whether the walk for engine date values (`dates::DeclaredDates`) can
/// find anything. Engine dates come from two places only: the input the evaluation started
/// with (checked once, by the first input node), and the date functions of expressions, which
/// all create their values through [`DateValue::wrap`] and so move [`DateValue::created`].
/// Inputs parsed from JSON hold none, so for them the walk is skipped until a date is made.
#[derive(Debug)]
pub(crate) struct DateWatch {
    baseline: u64,
    root: AtomicU8,
}

impl DateWatch {
    const UNKNOWN: u8 = 0;
    const NONE: u8 = 1;
    const HOLDS: u8 = 2;

    pub(crate) fn new() -> Self {
        Self {
            baseline: DateValue::created(),
            root: AtomicU8::new(Self::UNKNOWN),
        }
    }

    /// `input` is the value an input node received. The first call of an evaluation sees the
    /// evaluation's own input and records whether it holds a date.
    pub(crate) fn may_hold(&self, input: &Variable) -> bool {
        if self.root.load(Ordering::Relaxed) == Self::UNKNOWN {
            let state = match DateValue::contains(input) {
                true => Self::HOLDS,
                false => Self::NONE,
            };
            self.root.store(state, Ordering::Relaxed);
        }
        self.root.load(Ordering::Relaxed) != Self::NONE || DateValue::created() != self.baseline
    }
}

impl Default for NodeHandlerExtensions {
    fn default() -> Self {
        Self {
            function_runtime: Default::default(),
            validator_cache: Default::default(),

            loader: Arc::new(NoopLoader::default()),
            custom_node: Arc::new(NoopCustomNode::default()),
            compiled_cache: None,
            stripped_functions: None,
            dt_indexes: None,
            dt_cells: None,
            http_handler: None,
            dates: Arc::new(DateWatch::new()),
        }
    }
}

impl NodeHandlerExtensions {
    pub async fn function_runtime(&self) -> anyhow::Result<&Function> {
        self.function_runtime
            .get_or_try_init(|| {
                Function::create(FunctionConfig {
                    listeners: Some(vec![
                        Box::new(ConsoleListener),
                        Box::new(HttpListener {
                            http_handler: self.http_handler.clone(),
                        }),
                        Box::new(ZenListener {
                            loader: self.loader.clone(),
                            custom_node: self.custom_node.clone(),
                            http_handler: self.http_handler.clone(),
                        }),
                    ]),
                })
            })
            .await
            .context("Failed to create function")
    }

    pub fn validator_cache(&self) -> &ValidatorCache {
        self.validator_cache
            .get_or_init(|| ValidatorCache::default())
    }

    pub fn custom_node(&self) -> &DynamicCustomNode {
        &self.custom_node
    }

    pub fn loader(&self) -> &DynamicLoader {
        &self.loader
    }

    pub fn http_handler(&self) -> &DynamicHttpHandler {
        &self.http_handler
    }
}

#[cfg(test)]
mod tests {
    use super::DateWatch;
    use serde_json::json;
    use zen_expression::{DateValue, Variable};

    #[test]
    fn json_input_holds_no_date_until_one_is_made() {
        let watch = DateWatch::new();
        let input: Variable = json!({"rows": [{"a": 1}, {"b": "2026-01-01"}]}).into();
        assert!(!watch.may_hold(&input), "plain JSON, no date made yet");
        assert!(!watch.may_hold(&input));
        let _date = DateValue::from_text("2026-01-01").unwrap();
        assert!(
            watch.may_hold(&input),
            "a date was made after the evaluation began"
        );
    }

    #[test]
    fn a_date_in_the_first_input_is_found_at_any_depth() {
        let watch = DateWatch::new();
        let date = DateValue::from_text("2026-01-01").unwrap();
        let input = Variable::from_object(
            [("rows".into(), Variable::from_array(vec![date]))]
                .into_iter()
                .collect(),
        );
        assert!(watch.may_hold(&input));
        assert!(
            watch.may_hold(&json!({"x": 1}).into()),
            "stays true for the evaluation"
        );
    }
}
