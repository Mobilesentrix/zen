use crate::variable::{DynamicVariable, Variable};
use crate::vm::date::VmDate;
use std::cell::Cell;
use std::rc::Rc;

thread_local! {
    /// Engine date values created on this thread so far (see [`DateValue::created`]).
    static CREATED: Cell<u64> = const { Cell::new(0) };
}

pub struct DateValue;

impl DateValue {
    /// Wraps an engine date as a value. Every engine date value is made here, which is what
    /// lets [`DateValue::created`] tell the engine that no date can exist yet.
    pub fn wrap<D: DynamicVariable + 'static>(date: D) -> Variable {
        CREATED.with(|created| created.set(created.get().wrapping_add(1)));
        Variable::Dynamic(Rc::new(date))
    }

    /// How many engine date values this thread has created. It only ever grows: a value read
    /// at the start of an evaluation and read again later differ if a date was made in between
    /// (by that evaluation, or by another one on the same thread, which only costs a walk).
    pub fn created() -> u64 {
        CREATED.with(Cell::get)
    }

    /// True when `value` is, or holds anywhere inside it, an engine date value. Reads only.
    pub fn contains(value: &Variable) -> bool {
        match value {
            Variable::Dynamic(_) => true,
            Variable::Object(object) => object
                .borrow()
                .iter()
                .any(|(_, child)| Self::contains(child)),
            Variable::Array(array) => array.borrow().iter().any(Self::contains),
            _ => false,
        }
    }

    pub fn from_text(text: &str) -> Option<Variable> {
        VmDate::from_text(text).map(Self::wrap)
    }

    pub fn is_text(text: &str) -> bool {
        VmDate::parses(text)
    }

    pub fn source_text(value: &Variable) -> Option<Variable> {
        let text = value.dynamic::<VmDate>()?.source()?;
        Some(Variable::String(text.into()))
    }

    pub fn is(value: &Variable) -> bool {
        matches!(value, Variable::Dynamic(dynamic) if dynamic.type_name() == "date")
    }
}
