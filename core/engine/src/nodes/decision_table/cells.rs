//! MobileSentrix: a decision table's cells resolved to their compiled bytecode once, when the
//! decision is compiled, so a row is evaluated without hashing every cell's source text to find
//! its bytecode in the opcode cache on every evaluation.
//!
//! The bytecode is the very entry of the decision's `OpcodeCache` (same compile, same `Arc`).
//! A cell that is not in the cache (it failed to compile) is `None`, and the table falls back
//! to running it by source text, as upstream does, which reproduces the same error.
use ahash::HashMap;
use std::sync::Arc;
use zen_expression::compiler::Opcode;
use zen_expression::OpcodeCache;
use zen_types::decision::{DecisionTableInputField, DecisionTableOutputField};

pub(crate) type Bytecode = Arc<[Opcode]>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompiledCells {
    /// rows x inputs: the compiled cell, for non-empty cells whose source compiled.
    pub(crate) inputs: Vec<Vec<Option<Bytecode>>>,
    /// rows x outputs.
    pub(crate) outputs: Vec<Vec<Option<Bytecode>>>,
}

impl CompiledCells {
    pub(crate) fn build(
        inputs: &[DecisionTableInputField],
        outputs: &[DecisionTableOutputField],
        rules: &[HashMap<Arc<str>, Arc<str>>],
        cache: &OpcodeCache,
    ) -> Self {
        let inputs = rules
            .iter()
            .map(|rule| {
                inputs
                    .iter()
                    .map(|input| {
                        let source = rule.get(&input.id).filter(|s| !s.is_empty())?;
                        let map = match input.field {
                            Some(_) => &cache.unary,
                            None => &cache.standard,
                        };
                        map.get(source).cloned()
                    })
                    .collect()
            })
            .collect();
        let outputs = rules
            .iter()
            .map(|rule| {
                outputs
                    .iter()
                    .map(|output| {
                        let source = rule.get(&output.id).filter(|s| !s.is_empty())?;
                        cache.standard.get(source).cloned()
                    })
                    .collect()
            })
            .collect();
        Self { inputs, outputs }
    }

    #[inline]
    pub(crate) fn input(&self, row: usize, col: usize) -> Option<&Bytecode> {
        self.inputs.get(row)?.get(col)?.as_ref()
    }

    #[inline]
    pub(crate) fn output(&self, row: usize, col: usize) -> Option<&Bytecode> {
        self.outputs.get(row)?.get(col)?.as_ref()
    }
}
