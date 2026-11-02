use ahash::HashMap;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use std::sync::Arc;
use thiserror::Error;

use crate::compiler::{Compiler, CompilerError, Opcode};
use crate::expression::{OpcodeCache, Standard, Unary};
use crate::lexer::{Lexer, LexerError};
use crate::parser::{Parser, ParserError};
use crate::scope::Scope;
use crate::variable::Variable;
use crate::vm::{VMError, VM};
use crate::{Expression, ExpressionKind};
use bumpalo::Bump;
use zen_types::symbol::Symbol;

/// Isolate is a component that encapsulates an isolated environment for executing expressions.
///
/// Rerunning the Isolate allows for efficient memory reuse through an arena allocator.
/// The arena allocator optimizes memory management by reusing memory blocks for subsequent evaluations,
/// contributing to improved performance and resource utilization in scenarios where the Isolate is reused multiple times.
#[derive(Debug)]
pub struct Isolate {
    lexer: Lexer,
    compiler: Compiler,
    vm: VM,

    bump: Bump,

    scope: Scope,
    references: HashMap<String, Variable>,
    cache: Option<Arc<OpcodeCache>>,
}

impl Isolate {
    pub fn new() -> Self {
        Self {
            lexer: Lexer::new(),
            compiler: Compiler::new(),
            vm: VM::new(),

            bump: Bump::new(),

            scope: Scope::default(),
            references: Default::default(),
            cache: None,
        }
    }

    pub fn with_environment(variable: Variable) -> Self {
        let mut isolate = Isolate::new();
        isolate.set_environment(variable);

        isolate
    }

    pub fn with_cache(mut self, cache: Option<Arc<OpcodeCache>>) -> Self {
        self.cache = cache;
        self
    }

    /// MobileSentrix: reuses this isolate as if it were
    /// `Isolate::with_environment(variable).with_cache(cache.cloned())`: the environment and
    /// locals are replaced and the reference cache emptied (the lexer, compiler, VM and arena
    /// buffers are reset before every run anyway, and keep their capacity). A cache that is the
    /// same `Arc` is kept without touching its count.
    pub fn reset(&mut self, variable: Variable, cache: Option<&Arc<OpcodeCache>>) {
        self.set_environment(variable);
        match (cache, &self.cache) {
            (Some(new), Some(current)) if Arc::ptr_eq(new, current) => {}
            (new, _) => self.cache = new.cloned(),
        }
    }

    /// Drops the environment, locals and references (the data of the last use), keeping the
    /// buffers for the next [`reset`](Self::reset).
    pub fn release(&mut self) {
        self.set_environment(Variable::Null);
    }

    pub fn set_environment(&mut self, variable: Variable) {
        self.scope.set_base(variable);
        self.references.clear();
    }

    pub fn set_cache(&mut self, cache: Arc<OpcodeCache>) {
        self.cache = Some(cache);
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn set_local(&mut self, name: Symbol, value: Variable) {
        self.scope.set_local(name, value);
    }

    pub fn insert_dollar(&mut self, path: &str, value: Variable) {
        let dollar = match self.scope.local(&Variable::dollar_key()) {
            Some(existing @ Variable::Object(_)) => existing.shallow_clone(),
            _ => {
                let created = Variable::empty_object();
                self.scope
                    .set_local(Variable::dollar_key(), created.clone());
                created
            }
        };

        let _ = dollar.dot_insert(path, value);
    }

    pub fn set_reference(&mut self, reference: &str) -> Result<(), IsolateError> {
        let reference_value = match self.references.get(reference) {
            Some(value) => value.clone(),
            None => {
                let result = self.run_standard(reference)?;
                self.references
                    .insert(reference.to_string(), result.clone());
                result
            }
        };

        self.set_reference_value(reference_value)
    }

    pub fn set_reference_value(&mut self, value: Variable) -> Result<(), IsolateError> {
        self.scope.set_local(Variable::dollar_key(), value);

        Ok(())
    }

    pub fn get_reference(&self, reference: &str) -> Option<Variable> {
        self.references.get(reference).cloned()
    }

    fn run_internal(&mut self, source: &str, kind: ExpressionKind) -> Result<(), IsolateError> {
        self.bump.reset();
        let bump = &self.bump;

        let tokens = self.lexer.tokenize(bump, source)?;

        let base_parser = Parser::try_new(&tokens, bump)?;
        let parser_result = match kind {
            ExpressionKind::Unary => base_parser.unary().parse(),
            ExpressionKind::Standard => base_parser.standard().parse(),
        };

        parser_result.error()?;

        self.compiler.compile(parser_result.root)?;

        Ok(())
    }

    pub fn compile_standard(&mut self, source: &str) -> Result<Expression<Standard>, IsolateError> {
        self.run_internal(source, ExpressionKind::Standard)?;
        let bytecode = self.compiler.get_bytecode().to_vec();

        Ok(Expression::new_standard(Arc::from(bytecode)))
    }

    pub fn run_standard(&mut self, source: &str) -> Result<Variable, IsolateError> {
        // MobileSentrix: run the cached bytecode in place (disjoint field borrows), so the
        // Arc's shared counter is not updated.
        if let Some(codes) = self.cache.as_ref().and_then(|c| c.standard.get(source)) {
            return Ok(self.vm.run(codes, &self.scope)?);
        }

        self.run_internal(source, ExpressionKind::Standard)?;

        let bytecode = self.compiler.get_bytecode();
        let result = self.vm.run(bytecode, &self.scope)?;

        Ok(result)
    }
    pub fn run_compiled(&mut self, source: &[Opcode]) -> Result<Variable, IsolateError> {
        let result = self.vm.run(source, &self.scope)?;

        Ok(result)
    }

    pub fn compile_unary(&mut self, source: &str) -> Result<Expression<Unary>, IsolateError> {
        self.run_internal(source, ExpressionKind::Unary)?;
        let bytecode = self.compiler.get_bytecode().to_vec();

        Ok(Expression::new_unary(Arc::from(bytecode)))
    }

    pub fn run_unary(&mut self, source: &str) -> Result<bool, IsolateError> {
        if let Some(codes) = self.cache.as_ref().and_then(|c| c.unary.get(source)) {
            let result = self.vm.run(codes, &self.scope)?;
            return result.as_bool().ok_or_else(|| IsolateError::ValueCastError);
        }

        self.run_internal(source, ExpressionKind::Unary)?;

        let bytecode = self.compiler.get_bytecode();
        let result = self.vm.run(bytecode, &self.scope)?;

        result.as_bool().ok_or_else(|| IsolateError::ValueCastError)
    }

    pub fn run_unary_compiled(&mut self, code: &[Opcode]) -> Result<bool, IsolateError> {
        let result = self.vm.run(code, &self.scope)?;

        result.as_bool().ok_or(IsolateError::ValueCastError)
    }
}

/// Errors which happen within isolate or during evaluation
#[derive(Debug, Error)]
pub enum IsolateError {
    #[error("Lexer error: {source}")]
    LexerError { source: LexerError },

    #[error("Parser error: {source}")]
    ParserError { source: ParserError },

    #[error("Compiler error: {source}")]
    CompilerError { source: CompilerError },

    #[error("VM error: {source}")]
    VMError { source: VMError },

    #[error("Value cast error")]
    ValueCastError,

    #[error("Failed to compute reference")]
    ReferenceError,

    #[error("Missing context reference")]
    MissingContextReference,
}

impl Serialize for IsolateError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(None)?;

        match &self {
            IsolateError::ReferenceError => {
                map.serialize_entry("type", "referenceError")?;
            }
            IsolateError::MissingContextReference => {
                map.serialize_entry("type", "missingContextReference")?;
            }
            IsolateError::ValueCastError => {
                map.serialize_entry("type", "valueCastError")?;
            }
            IsolateError::LexerError { source } => {
                map.serialize_entry("type", "lexerError")?;
                map.serialize_entry("source", source.to_string().as_str())?;
            }
            IsolateError::ParserError { source } => {
                map.serialize_entry("type", "parserError")?;
                map.serialize_entry("source", source.to_string().as_str())?;
            }
            IsolateError::CompilerError { source } => {
                map.serialize_entry("type", "compilerError")?;
                map.serialize_entry("source", source.to_string().as_str())?;
            }
            IsolateError::VMError { source } => {
                map.serialize_entry("type", "vmError")?;
                map.serialize_entry("source", source.to_string().as_str())?;
            }
        }

        map.end()
    }
}

impl From<LexerError> for IsolateError {
    fn from(source: LexerError) -> Self {
        IsolateError::LexerError { source }
    }
}

impl From<ParserError> for IsolateError {
    fn from(source: ParserError) -> Self {
        IsolateError::ParserError { source }
    }
}

impl From<VMError> for IsolateError {
    fn from(source: VMError) -> Self {
        IsolateError::VMError { source }
    }
}

impl From<CompilerError> for IsolateError {
    fn from(source: CompilerError) -> Self {
        IsolateError::CompilerError { source }
    }
}
