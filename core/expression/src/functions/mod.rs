pub use crate::functions::date_method::DateMethod;
pub use crate::functions::defs::FunctionTypecheck;
pub use crate::functions::deprecated::DeprecatedFunction;
pub use crate::functions::internal::InternalFunction;
pub use crate::functions::method::{MethodKind, MethodRegistry};
pub use crate::functions::registry::FunctionRegistry;

use std::fmt::Display;
use strum_macros::{Display, EnumIter, EnumString, IntoStaticStr};

pub(crate) mod arguments;
mod date_method;
pub(crate) mod defs;
mod deprecated;
pub(crate) mod internal;
mod method;
pub(crate) mod ms_list;
pub(crate) mod registry;

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum FunctionKind {
    Internal(InternalFunction),
    Deprecated(DeprecatedFunction),
    Closure(ClosureFunction),
}

impl TryFrom<&str> for FunctionKind {
    type Error = strum::ParseError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        InternalFunction::try_from(value)
            .map(FunctionKind::Internal)
            .or_else(|_| DeprecatedFunction::try_from(value).map(FunctionKind::Deprecated))
            .or_else(|_| ClosureFunction::try_from(value).map(FunctionKind::Closure))
    }
}

impl Display for FunctionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FunctionKind::Internal(i) => write!(f, "{i}"),
            FunctionKind::Deprecated(d) => write!(f, "{d}"),
            FunctionKind::Closure(c) => write!(f, "{c}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Display, EnumString, EnumIter, IntoStaticStr, Clone, Copy)]
#[strum(serialize_all = "camelCase")]
pub enum ClosureFunction {
    All,
    None,
    Some,
    One,
    Filter,
    Map,
    FlatMap,
    Count,
    // MobileSentrix list functions (see ms_list.rs)
    SortBy,
    MinBy,
    MaxBy,
    UniqueBy,
    CountDistinct,
    GroupBy,
    KeyBy,
    Reduce,
    Scan,
}

/// Name of the accumulator inside a `reduce` / `scan` callback.
pub const ACCUMULATOR: &str = "acc";

impl ClosureFunction {
    /// The callback must return a bool (`filter`, `all`, ...); the others take any value
    /// (a key, a mapped value, the next accumulator).
    pub fn is_predicate(&self) -> bool {
        matches!(
            self,
            Self::All | Self::None | Self::Some | Self::One | Self::Filter | Self::Count
        )
    }

    /// Accepted argument counts. The array is always argument 0 and the callback argument 1;
    /// `sortBy` takes an optional direction and `reduce` / `scan` a required initial value
    /// as argument 2. Argument 2 is evaluated outside the callback.
    pub fn arity(&self) -> (usize, usize) {
        match self {
            Self::SortBy => (2, 3),
            Self::Reduce | Self::Scan => (3, 3),
            _ => (2, 2),
        }
    }

    pub fn accepts_arguments(&self, count: usize) -> bool {
        let (min, max) = self.arity();
        (min..=max).contains(&count)
    }

    /// The callback can read the accumulator `acc`.
    pub fn has_accumulator(&self) -> bool {
        matches!(self, Self::Reduce | Self::Scan)
    }
}
