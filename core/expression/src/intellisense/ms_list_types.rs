//! Type checking of the MobileSentrix list functions (see `functions/ms_list.rs`).

use super::{TypeInfo, TypesProvider};
use crate::functions::internal::InternalFunction;
use crate::functions::registry::FunctionRegistry;
use crate::functions::{ClosureFunction, FunctionKind};
use crate::parser::Node;
use crate::variable::VariableType;
use std::ops::Deref;
use std::rc::Rc;

impl TypesProvider {
    /// Result type of the MobileSentrix keyed / folding closure functions (see
    /// `functions/ms_list.rs`); checks the key and the trailing argument.
    pub(super) fn ms_list_closure_type(
        &mut self,
        c: ClosureFunction,
        type_list: &[VariableType],
        arguments: &[&Node],
    ) -> TypeInfo {
        let element = type_list[0]
            .iterator()
            .map(|t| t.deref().clone())
            .unwrap_or(VariableType::Any);
        let key = type_list[1].unwrap_nullable().0;

        let orders = matches!(
            c,
            ClosureFunction::SortBy | ClosureFunction::MinBy | ClosureFunction::MaxBy
        );
        let names = matches!(c, ClosureFunction::GroupBy | ClosureFunction::KeyBy);
        if (orders && matches!(key, VariableType::Object(_) | VariableType::Interval))
            || (names
                && matches!(
                    key,
                    VariableType::Object(_) | VariableType::Array(_) | VariableType::Interval
                ))
        {
            let allowed = match orders {
                true => "a number, string, bool, date or an array of them",
                false => "a string, number, bool or date",
            };
            self.set_error(
                arguments[1],
                format!("Key must be {allowed}, but its type is `{}`.", type_list[1]),
            );
        }

        let direction = match (c, arguments.get(2)) {
            (ClosureFunction::SortBy, Some(node)) if !matches!(node, Node::Error { .. }) => {
                type_list.get(2).map(|t| (*node, t))
            }
            _ => None,
        };
        if let Some((node, direction)) = direction {
            if !direction.satisfies(&VariableType::String) {
                self.set_error(
                    node,
                    format!("Direction must be 'asc' or 'desc', but its type is `{direction}`."),
                );
            }
        }

        match c {
            ClosureFunction::SortBy | ClosureFunction::UniqueBy => {
                TypeInfo::from(type_list[0].clone())
            }
            ClosureFunction::MinBy | ClosureFunction::MaxBy => {
                TypeInfo::from(element.merge(&VariableType::Null))
            }
            ClosureFunction::CountDistinct => TypeInfo::from(VariableType::Number),
            ClosureFunction::GroupBy | ClosureFunction::KeyBy => {
                TypeInfo::from(VariableType::empty_object())
            }
            ClosureFunction::Reduce => TypeInfo::from(type_list[2].merge(&type_list[1])),
            ClosureFunction::Scan => {
                TypeInfo::from(VariableType::Array(Rc::new(type_list[1].clone())))
            }
            _ => TypeInfo::from(VariableType::Any),
        }
    }

    /// `sort` / `unique` keep the array type; `first` / `last` return a nullable element.
    pub(super) fn ms_list_typecheck(
        &mut self,
        function: InternalFunction,
        kind: &FunctionKind,
        type_list: &[VariableType],
        arguments: &[&Node],
    ) -> TypeInfo {
        let Some(def) = FunctionRegistry::get_definition(kind) else {
            return TypeInfo::from(VariableType::Any);
        };

        let typecheck = def.check_types(type_list);
        for (i, arg_error) in typecheck.arguments {
            let code = Self::mismatch(def.param_type_str(i), &type_list[i]);
            self.set_coded_error(arguments[i], arg_error, code);
        }
        if typecheck.general.is_some() || !type_list[0].is_array() {
            return TypeInfo {
                kind: typecheck.return_type,
                error: typecheck.general,
            };
        }

        match function {
            InternalFunction::First | InternalFunction::Last => {
                let element = type_list[0]
                    .iterator()
                    .map(|t| t.deref().clone())
                    .unwrap_or(VariableType::Any);
                TypeInfo::from(element.merge(&VariableType::Null))
            }
            _ => TypeInfo::from(type_list[0].clone()),
        }
    }
}
