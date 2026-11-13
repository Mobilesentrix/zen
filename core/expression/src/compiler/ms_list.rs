//! Bytecode for the MobileSentrix list closure functions (see `functions/ms_list.rs`).
//!
//! Keyed functions (`sortBy`, `minBy`, `maxBy`, `uniqueBy`, `countDistinct`, `groupBy`,
//! `keyBy`) run the key callback in the usual loop scope, collect the keys into an array and
//! finish with `ByKey`, which reads the source array from the still-open scope.
//!
//! `reduce` / `scan` keep the accumulator in the loop scope: `SetAccumulator` stores the
//! initial value and every callback result, `Accumulator` reads it (`acc` in the callback).
//!
//! Trailing arguments (sortBy direction, reduce / scan initial value) are compiled before
//! `Begin`, so `#` inside them refers to an enclosing closure, as it reads in the source.

use super::CompilerInner;
use crate::compiler::error::{CompilerError, CompilerResult};
use crate::compiler::Opcode;
use crate::functions::ms_list::KeyOp;
use crate::functions::{ClosureFunction, FunctionKind};
use crate::parser::Node;

impl<'arena> CompilerInner<'arena, '_> {
    pub(super) fn compile_ms_list_closure(
        &mut self,
        kind: &FunctionKind,
        function: ClosureFunction,
        arguments: &[&'arena Node<'arena>],
    ) -> CompilerResult<usize> {
        let op = match function {
            ClosureFunction::SortBy => KeyOp::SortBy,
            ClosureFunction::MinBy => KeyOp::MinBy,
            ClosureFunction::MaxBy => KeyOp::MaxBy,
            ClosureFunction::UniqueBy => KeyOp::UniqueBy,
            ClosureFunction::CountDistinct => KeyOp::CountDistinct,
            ClosureFunction::GroupBy => KeyOp::GroupBy,
            ClosureFunction::KeyBy => KeyOp::KeyBy,
            ClosureFunction::Reduce => return self.compile_fold(kind, arguments, false),
            ClosureFunction::Scan => return self.compile_fold(kind, arguments, true),
            ClosureFunction::All
            | ClosureFunction::None
            | ClosureFunction::Some
            | ClosureFunction::One
            | ClosureFunction::Filter
            | ClosureFunction::Map
            | ClosureFunction::FlatMap
            | ClosureFunction::Count => {
                return Err(CompilerError::UnknownFunction {
                    name: kind.to_string(),
                })
            }
        };

        if op == KeyOp::SortBy {
            match arguments.get(2) {
                Some(_) => self.compile_argument(kind, arguments, 2)?,
                None => self.emit(Opcode::PushNull),
            };
        }

        self.compile_argument(kind, arguments, 0)?;
        self.emit(Opcode::Begin);
        self.emit_loop(|c| {
            c.compile_argument(kind, arguments, 1)?;
            Ok(())
        })?;
        self.emit(Opcode::GetLen);
        self.emit(Opcode::Array);
        self.emit(Opcode::ByKey(op));
        Ok(self.emit(Opcode::End))
    }

    fn compile_fold(
        &mut self,
        kind: &FunctionKind,
        arguments: &[&'arena Node<'arena>],
        scan: bool,
    ) -> CompilerResult<usize> {
        self.compile_argument(kind, arguments, 2)?;
        self.compile_argument(kind, arguments, 0)?;
        self.emit(Opcode::Begin);
        self.emit(Opcode::SetAccumulator);
        self.emit_loop(|c| {
            c.next_closure_accumulates = true;
            let result = c.compile_argument(kind, arguments, 1);
            c.next_closure_accumulates = false;
            result?;
            c.emit(Opcode::SetAccumulator);
            if scan {
                c.emit(Opcode::Accumulator(0));
            }
            Ok(())
        })?;

        if scan {
            self.emit(Opcode::GetLen);
            self.emit(Opcode::End);
            Ok(self.emit(Opcode::Array))
        } else {
            self.emit(Opcode::Accumulator(0));
            Ok(self.emit(Opcode::End))
        }
    }
}
