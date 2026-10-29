use crate::decision_graph::early::EarlyPlan;
use crate::decision_graph::graph::DecisionGraph;
use crate::decision_graph::schema_dict;
use crate::decision_graph::walker::StableDiDecisionGraph;
use crate::loader::DynamicLoader;
use crate::nodes::decision_table::cells::CompiledCells;
use crate::nodes::decision_table::index::TableIndex;
use crate::nodes::function::v2::strip::TypeStripper;
use crate::nodes::statement::is_read_only_sql;
use crate::nodes::validator_cache::ValidatorCache;
use crate::policy::PolicyDocument;
use crate::DecisionGraphValidationError;
use ahash::{HashMap, HashMapExt, HashSet};
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::Arc;
use zen_expression::{ExpressionKind, Isolate, OpcodeCache};
use zen_types::decision::{DecisionEdge, DecisionNode, DecisionNodeKind, FunctionNodeContent};

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum DecisionContent {
    Graph(Arc<GraphContent>),
    Policy(PolicyContent),
}

impl<'de> Deserialize<'de> for DecisionContent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let is_policy = value
            .as_object()
            .is_some_and(|object| object.contains_key("blocks"));

        let content = if is_policy {
            serde_path_to_error::deserialize::<_, PolicyContent>(value).map(Self::Policy)
        } else {
            serde_path_to_error::deserialize::<_, GraphContent>(value)
                .map(|graph| Self::Graph(Arc::new(graph)))
        };

        content.map_err(serde::de::Error::custom)
    }
}

impl Default for DecisionContent {
    fn default() -> Self {
        Self::Graph(Arc::new(GraphContent::default()))
    }
}

impl DecisionContent {
    pub fn as_graph(&self) -> Option<&GraphContent> {
        match self {
            Self::Graph(g) => Some(g),
            Self::Policy(_) => None,
        }
    }

    pub fn as_policy(&self) -> Option<&PolicyContent> {
        match self {
            Self::Policy(p) => Some(p),
            Self::Graph(_) => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Graph(_) => "graph",
            Self::Policy(_) => "policy",
        }
    }

    pub fn into_graph_arc(self: Arc<Self>) -> Option<Arc<GraphContent>> {
        match self.as_ref() {
            Self::Graph(g) => Some(g.clone()),
            Self::Policy(_) => None,
        }
    }
}

impl From<GraphContent> for DecisionContent {
    fn from(value: GraphContent) -> Self {
        Self::Graph(Arc::new(value))
    }
}

impl From<Arc<GraphContent>> for DecisionContent {
    fn from(value: Arc<GraphContent>) -> Self {
        Self::Graph(value)
    }
}

impl From<PolicyContent> for DecisionContent {
    fn from(value: PolicyContent) -> Self {
        Self::Policy(value)
    }
}

impl From<Arc<PolicyDocument>> for PolicyContent {
    fn from(value: Arc<PolicyDocument>) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphContent {
    pub nodes: Vec<Arc<DecisionNode>>,
    pub edges: Vec<Arc<DecisionEdge>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<Arc<str>>,

    #[serde(skip)]
    pub compiled_cache: Option<Arc<OpcodeCache>>,

    #[serde(skip)]
    pub stripped_functions: Option<Arc<HashMap<Arc<str>, Arc<str>>>>,

    #[serde(skip)]
    pub resolved_schemas: Option<Arc<HashMap<Arc<str>, (Arc<serde_json::Value>, u64)>>>,

    #[serde(skip)]
    pub(crate) validator_cache: ValidatorCache,

    #[serde(skip)]
    pub(crate) dt_indexes: Option<Arc<HashMap<Arc<str>, TableIndex>>>,

    /// MobileSentrix: the node/edge graph and its validation, built once by `compile` instead of
    /// on every evaluation.
    /// MobileSentrix: decision-table cells resolved to their bytecode in `compiled_cache`.
    #[serde(skip)]
    pub(crate) dt_cells: Option<Arc<HashMap<Arc<str>, CompiledCells>>>,

    #[serde(skip)]
    pub(crate) prepared_graph: Option<Arc<PreparedGraph>>,

    /// MobileSentrix: ids of the nodes that can read `$nodes` (the outputs of the nodes before
    /// them). Only these get the `$nodes` object built; `None` (not compiled) = every node.
    #[serde(skip)]
    pub(crate) nodes_readers: Option<Arc<HashSet<Arc<str>>>>,

    /// MobileSentrix: ids of the nodes that may run at the same time as other ready nodes of
    /// this set (see [`concurrent_nodes`]). `None` (not compiled) = none.
    #[serde(skip)]
    pub(crate) concurrent_nodes: Option<Arc<HashSet<Arc<str>>>>,
}

/// Nodes that can run concurrently with each other: database steps (custom nodes of kind
/// `database`) whose query only reads ([`is_read_only_sql`]) and whose content never names
/// `$nodes`. Such a step reads only its input and its own configuration, does not touch the
/// shared QuickJS runtime, and strips `$nodes` from what it returns, so its result does not
/// depend on which sibling finished first. A step that may write runs in walk order. Function
/// and sub-decision nodes share the function runtime and stay sequential.
fn concurrent_nodes(nodes: &[Arc<DecisionNode>]) -> HashSet<Arc<str>> {
    nodes
        .iter()
        .filter(|node| match &node.kind {
            DecisionNodeKind::CustomNode { content } => {
                &*content.kind == "database"
                    && database_query(&content.config).is_some_and(is_read_only_sql)
                    && serde_json::to_string(&node.kind).is_ok_and(|text| !text.contains("$nodes"))
            }
            _ => false,
        })
        .map(|node| node.id.clone())
        .collect()
}

/// The SQL of a database step's config: `query`, or `customQuery` when `useCustomQuery` is set
/// (the database node reads the same two fields).
fn database_query(config: &serde_json::Value) -> Option<&str> {
    let text = |key: &str| {
        config
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|q| !q.trim().is_empty())
    };
    text("query").or_else(|| {
        config
            .get("useCustomQuery")
            .and_then(|v| v.as_bool())
            .filter(|on| *on)
            .and_then(|_| text("customQuery"))
    })
}

/// Nodes that can observe `$nodes`: function, custom and sub-decision nodes always (they hand
/// it on), any other node only if its content spells the `$nodes` identifier somewhere (an
/// expression, a cell, a condition or an input field reads it by that name only).
fn nodes_readers(nodes: &[Arc<DecisionNode>]) -> HashSet<Arc<str>> {
    nodes
        .iter()
        .filter(|node| match &node.kind {
            DecisionNodeKind::FunctionNode { .. }
            | DecisionNodeKind::CustomNode { .. }
            | DecisionNodeKind::DecisionNode { .. } => true,
            _ => serde_json::to_string(&node.kind).map_or(true, |text| text.contains("$nodes")),
        })
        .map(|node| node.id.clone())
        .collect()
}

/// The evaluation graph of a compiled decision (derived from `nodes` and `edges` only).
#[derive(Debug)]
pub(crate) struct PreparedGraph {
    pub(crate) graph: Result<Arc<StableDiDecisionGraph>, DecisionGraphValidationError>,
    pub(crate) validation: Result<(), DecisionGraphValidationError>,
    /// MobileSentrix: what may start before the walk reaches it (`None` = nothing), decided
    /// once per compiled rule version.
    pub(crate) early: Option<EarlyPlan>,
}

impl PreparedGraph {
    fn build(content: &GraphContent) -> Self {
        match DecisionGraph::build_graph(content) {
            Ok(graph) => Self {
                validation: DecisionGraph::validate_graph(&graph),
                early: match (
                    content.concurrent_nodes.as_deref(),
                    content.nodes_readers.as_deref(),
                ) {
                    (Some(concurrent), Some(readers)) => {
                        EarlyPlan::build(&graph, concurrent, readers)
                    }
                    _ => None,
                },
                graph: Ok(Arc::new(graph)),
            },
            Err(error) => Self {
                validation: Ok(()),
                early: None,
                graph: Err(error),
            },
        }
    }
}

/// Derived data: two prepared graphs of equal content are equal.
impl PartialEq for PreparedGraph {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct PolicyContent(pub Arc<PolicyDocument>);

impl GraphContent {
    pub fn compile(&mut self) {
        self.compile_functions();
        self.build_dt_indexes();
        if self.nodes_readers.is_none() {
            self.nodes_readers = Some(Arc::new(nodes_readers(&self.nodes)));
        }
        if self.concurrent_nodes.is_none() {
            self.concurrent_nodes = Some(Arc::new(concurrent_nodes(&self.nodes)));
        }
        // After the two sets above: the early-start plan is derived from them.
        if self.prepared_graph.is_none() {
            self.prepared_graph = Some(Arc::new(PreparedGraph::build(self)));
        }
        self.build_compiled_cache();
        self.build_dt_cells();
    }

    fn build_dt_cells(&mut self) {
        if self.dt_cells.is_some() {
            return;
        }
        let Some(cache) = self.compiled_cache.clone() else {
            return;
        };
        let cells: HashMap<Arc<str>, CompiledCells> = self
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                DecisionNodeKind::DecisionTableNode { content } => Some((
                    node.id.clone(),
                    CompiledCells::build(&content.inputs, &content.outputs, &content.rules, &cache),
                )),
                _ => None,
            })
            .collect();
        self.dt_cells = Some(Arc::new(cells));
    }

    fn build_compiled_cache(&mut self) {
        if self.compiled_cache.is_some() {
            return;
        }

        let mut sources: Vec<(Arc<str>, ExpressionKind)> = Vec::new();
        for node in &self.nodes {
            match &node.kind {
                DecisionNodeKind::ExpressionNode { content } => {
                    for expr in content.expressions.iter() {
                        if !expr.key.is_empty() && !expr.value.is_empty() {
                            sources.push((expr.value.clone(), ExpressionKind::Standard));
                        }
                    }
                    // MobileSentrix: also the node's input field (loop/transform), which runs
                    // as a standard expression on every evaluation.
                    if let Some(field) = &content.transform_attributes.input_field {
                        sources.push((field.clone(), ExpressionKind::Standard));
                    }
                }
                DecisionNodeKind::SwitchNode { content } => {
                    // MobileSentrix: switch conditions (upstream compiles them per evaluation).
                    for statement in content.statements.iter() {
                        if !statement.condition.is_empty() {
                            sources.push((statement.condition.clone(), ExpressionKind::Standard));
                        }
                    }
                }
                DecisionNodeKind::DecisionTableNode { content } => {
                    // MobileSentrix: input fields (set_reference runs each as a standard
                    // expression) and the transform input field (upstream: per evaluation).
                    for input in content.inputs.iter() {
                        if let Some(field) = input.field.as_ref().filter(|f| !f.is_empty()) {
                            sources.push((field.clone(), ExpressionKind::Standard));
                        }
                    }
                    if let Some(field) = &content.transform_attributes.input_field {
                        sources.push((field.clone(), ExpressionKind::Standard));
                    }
                    for rule in content.rules.iter() {
                        for input in content.inputs.iter() {
                            let Some(rule_value) = rule.get(&input.id) else {
                                continue;
                            };

                            let kind = if input.field.is_some() {
                                ExpressionKind::Unary
                            } else {
                                ExpressionKind::Standard
                            };

                            sources.push((rule_value.clone(), kind));
                        }

                        for output in content.outputs.iter() {
                            let Some(rule_value) = rule.get(&output.id) else {
                                continue;
                            };

                            sources.push((rule_value.clone(), ExpressionKind::Standard));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut cache: OpcodeCache = OpcodeCache::new();
        let mut isolate = Isolate::new();

        for (source, kind) in &sources {
            let map = match kind {
                ExpressionKind::Standard => &mut cache.standard,
                ExpressionKind::Unary => &mut cache.unary,
            };
            if map.contains_key(source) {
                continue;
            }

            let result = match kind {
                ExpressionKind::Standard => isolate
                    .compile_standard(source)
                    .map(|e| e.bytecode().to_vec()),
                ExpressionKind::Unary => {
                    isolate.compile_unary(source).map(|e| e.bytecode().to_vec())
                }
            };
            if let Ok(bytecode) = result {
                map.insert(source.clone(), Arc::from(bytecode));
            }
        }

        self.compiled_cache.replace(Arc::new(cache));
    }

    fn build_dt_indexes(&mut self) {
        if self.dt_indexes.is_some() {
            return;
        }

        let indexes: HashMap<Arc<str>, TableIndex> = self
            .nodes
            .iter()
            .filter_map(|node| match &node.kind {
                DecisionNodeKind::DecisionTableNode { content } => {
                    TableIndex::build(&content.inputs, &content.rules)
                        .map(|index| (node.id.clone(), index))
                }
                _ => None,
            })
            .collect();

        self.dt_indexes = Some(Arc::new(indexes));
    }

    pub async fn resolve_schemas(&mut self, loader: &DynamicLoader) -> Result<(), String> {
        if self.resolved_schemas.is_some() {
            return Ok(());
        }

        let mut referencing: Vec<(Arc<str>, Arc<serde_json::Value>)> = Vec::new();
        for node in &self.nodes {
            let schema = match &node.kind {
                DecisionNodeKind::InputNode { content } => content.schema.as_ref(),
                DecisionNodeKind::OutputNode { content } => content.schema.as_ref(),
                _ => None,
            };
            if let Some(schema) = schema {
                if schema_dict::schema_references_dictionary(schema) {
                    referencing.push((node.id.clone(), schema.clone()));
                }
            }
        }

        if referencing.is_empty() {
            self.resolved_schemas = Some(Arc::new(HashMap::new()));
            return Ok(());
        }

        let dictionaries = schema_dict::load_import_dictionaries(loader, &self.imports).await?;
        let mut resolved = HashMap::with_capacity(referencing.len());
        for (id, schema) in referencing {
            let (value, salt) = schema_dict::resolve_schema(&schema, &dictionaries)?;
            resolved.insert(id, (Arc::new(value), salt));
        }

        self.resolved_schemas = Some(Arc::new(resolved));
        Ok(())
    }

    fn compile_functions(&mut self) {
        if self.stripped_functions.is_some() {
            return;
        }
        let mut stripped: HashMap<Arc<str>, Arc<str>> = HashMap::new();
        for node in &self.nodes {
            let DecisionNodeKind::FunctionNode {
                content: FunctionNodeContent::Version2(function),
            } = &node.kind
            else {
                continue;
            };
            stripped
                .entry(function.source.clone())
                .or_insert_with(|| TypeStripper::strip(&function.source));
        }
        self.stripped_functions.replace(Arc::new(stripped));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_graph_error_mentions_field() {
        let error = serde_json::from_str::<DecisionContent>(r#"{"nodes":[],"edges":"bad"}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("edges"), "{error}");
    }

    #[test]
    fn malformed_policy_error_mentions_inner_field() {
        let json = r#"{"blocks":[{"type":"assertion","id":"b1","props":{"data":{}}}]}"#;
        let error = serde_json::from_str::<DecisionContent>(json)
            .unwrap_err()
            .to_string();
        assert!(error.contains("output"), "{error}");
    }

    #[test]
    fn compile_strips_function_sources() {
        let json = r#"{"nodes":[{"id":"f","name":"f","type":"functionNode","content":{"source":"export const handler = (input: { age: number }) => ({ total: input.age });"}}],"edges":[]}"#;
        let mut content: GraphContent = serde_json::from_str(json).unwrap();
        content.compile();
        let stripped = content.stripped_functions.as_ref().unwrap();
        assert_eq!(stripped.len(), 1);
        let value = stripped.values().next().unwrap();
        assert!(!value.contains(": number"), "{value}");
    }

    #[test]
    fn valid_graph_routes_to_graph_variant() {
        let content: DecisionContent = serde_json::from_str(r#"{"nodes":[],"edges":[]}"#).unwrap();
        assert!(content.as_graph().is_some());
    }

    #[test]
    fn valid_policy_routes_to_policy_variant() {
        let content: DecisionContent = serde_json::from_str(r#"{"blocks":[]}"#).unwrap();
        assert!(content.as_policy().is_some());
    }
}
