//! MobileSentrix list functions: ordering, de-duplication and grouping of arrays.
//!
//! Plain functions (`sort`, `unique`, `join`, `first`, `last`) are registered as internal
//! functions. The keyed forms (`sortBy`, `minBy`, `maxBy`, `uniqueBy`, `countDistinct`,
//! `groupBy`, `keyBy`) are closure functions: the compiler evaluates the key callback once per
//! element inside the usual loop scope, collects the keys into an array and then runs
//! [`apply_key_op`] on the source array and its keys (opcode `ByKey`).
//!
//! Contracts:
//! - Ordering (`sort`, `sortBy`, `minBy`, `maxBy`): numbers (decimal), strings (ordinal,
//!   case-sensitive), booleans (false < true) and dates (by instant) are comparable within
//!   their own type; mixing two of them is an error. An array key compares element by
//!   element (multi-key ordering), a shorter prefix first. `null` keys sort last in either
//!   direction. Sorting is stable. Objects and nested arrays cannot be ordering keys.
//! - `minBy` / `maxBy` return the element, the first one on ties; elements whose key is
//!   `null` are skipped; `null` when nothing is left.
//! - Equality (`unique`, `uniqueBy`, `countDistinct`): deep value equality; `1` equals
//!   `1.0`; object key order does not matter; a date equals another date at the same
//!   instant but never a string.
//! - `groupBy` / `keyBy` name each entry by the key converted like `string()`; entries keep
//!   first-appearance order; `keyBy` keeps the first element per key.

use crate::functions::arguments::Arguments;
use crate::vm::date::DynamicVariableExt;
use crate::Variable as V;
use ahash::{HashMap, HashMapExt, HashSet, HashSetExt};
use anyhow::{anyhow, bail};
use chrono::NaiveDateTime;
use rust_decimal::Decimal;
use std::cmp::Ordering;
use std::rc::Rc;
use strum_macros::Display;
use zen_types::symbol::Symbol;
use zen_types::variable::VariableMap;

/// Keyed list operation run by the `ByKey` opcode.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Display)]
pub enum KeyOp {
    SortBy,
    MinBy,
    MaxBy,
    UniqueBy,
    CountDistinct,
    GroupBy,
    KeyBy,
}

impl KeyOp {
    fn name(&self) -> &'static str {
        match self {
            KeyOp::SortBy => "sortBy",
            KeyOp::MinBy => "minBy",
            KeyOp::MaxBy => "maxBy",
            KeyOp::UniqueBy => "uniqueBy",
            KeyOp::CountDistinct => "countDistinct",
            KeyOp::GroupBy => "groupBy",
            KeyOp::KeyBy => "keyBy",
        }
    }
}

/// `items` is the source array, `keys[i]` the key of `items[i]`. `direction` is only read by
/// `sortBy`.
pub(crate) fn apply_key_op(
    op: KeyOp,
    items: &[V],
    keys: &[V],
    direction: Option<&V>,
) -> anyhow::Result<V> {
    if items.len() != keys.len() {
        bail!(
            "{}: {} keys for {} elements",
            op.name(),
            keys.len(),
            items.len()
        );
    }

    let name = op.name();
    match op {
        KeyOp::SortBy => {
            let descending = parse_direction(name, direction)?;
            sorted(name, items, keys, descending)
        }
        KeyOp::MinBy => extreme(name, items, keys, Ordering::Less),
        KeyOp::MaxBy => extreme(name, items, keys, Ordering::Greater),
        KeyOp::UniqueBy => {
            let mut seen = HashSet::with_capacity(keys.len());
            let mut out = Vec::new();
            for (item, key) in items.iter().zip(keys) {
                if seen.insert(EqKey::of(name, key)?) {
                    out.push(item.clone());
                }
            }
            Ok(V::from_array(out))
        }
        KeyOp::CountDistinct => {
            let mut seen = HashSet::with_capacity(keys.len());
            for key in keys {
                seen.insert(EqKey::of(name, key)?);
            }
            Ok(V::Number(Decimal::from(seen.len())))
        }
        KeyOp::GroupBy => {
            let mut index: HashMap<Rc<str>, usize> = HashMap::new();
            let mut groups: Vec<(Rc<str>, Vec<V>)> = Vec::new();
            for (item, key) in items.iter().zip(keys) {
                let label = group_label(name, key)?;
                match index.get(&label) {
                    Some(&at) => groups[at].1.push(item.clone()),
                    None => {
                        index.insert(label.clone(), groups.len());
                        groups.push((label, vec![item.clone()]));
                    }
                }
            }
            let entries = groups
                .into_iter()
                .map(|(label, list)| (Symbol::from(label.as_ref()), V::from_array(list)))
                .collect();
            Ok(V::from_object(VariableMap::from_unique_entries(entries)))
        }
        KeyOp::KeyBy => {
            let mut seen: HashSet<Rc<str>> = HashSet::with_capacity(keys.len());
            let mut entries = Vec::new();
            for (item, key) in items.iter().zip(keys) {
                let label = group_label(name, key)?;
                if seen.insert(label.clone()) {
                    entries.push((Symbol::from(label.as_ref()), item.clone()));
                }
            }
            Ok(V::from_object(VariableMap::from_unique_entries(entries)))
        }
    }
}

/// `sort(array)` / `sort(array, 'desc')`: the elements are their own keys.
pub fn sort(args: Arguments) -> anyhow::Result<V> {
    let a = args.array(0)?;
    let arr = a.borrow();
    let descending = parse_direction("sort", args.ovar(1))?;
    sorted("sort", &arr, &arr, descending)
}

/// `unique(array)`: first occurrence of each distinct value, in input order.
pub fn unique(args: Arguments) -> anyhow::Result<V> {
    let a = args.array(0)?;
    let arr = a.borrow();
    let mut seen = HashSet::with_capacity(arr.len());
    let mut out = Vec::new();
    for item in arr.iter() {
        if seen.insert(EqKey::of("unique", item)?) {
            out.push(item.clone());
        }
    }
    Ok(V::from_array(out))
}

/// `join(array, separator)`: strings as they are, numbers as `string()` prints them.
pub fn join(args: Arguments) -> anyhow::Result<V> {
    let a = args.array(0)?;
    let separator = args.str(1)?;
    let arr = a.borrow();
    let mut out = String::new();
    for (i, item) in arr.iter().enumerate() {
        if i > 0 {
            out.push_str(separator);
        }
        match item {
            V::String(s) => out.push_str(s.as_str()),
            V::Number(n) => out.push_str(&n.to_string()),
            other => bail!(
                "join: element {i} is {}; only strings and numbers can be joined",
                other.type_name()
            ),
        }
    }
    Ok(V::String(out.as_str().into()))
}

/// `first(array)`: the first element, `null` for an empty array.
pub fn first(args: Arguments) -> anyhow::Result<V> {
    let a = args.array(0)?;
    let arr = a.borrow();
    Ok(arr.first().cloned().unwrap_or(V::Null))
}

/// `last(array)`: the last element, `null` for an empty array.
pub fn last(args: Arguments) -> anyhow::Result<V> {
    let a = args.array(0)?;
    let arr = a.borrow();
    Ok(arr.last().cloned().unwrap_or(V::Null))
}

fn parse_direction(name: &str, direction: Option<&V>) -> anyhow::Result<bool> {
    let Some(direction) = direction else {
        return Ok(false);
    };
    match direction.as_str() {
        Some("asc") => Ok(false),
        Some("desc") => Ok(true),
        _ => Err(anyhow!(
            "{name}: direction must be 'asc' or 'desc', got {direction}"
        )),
    }
}

fn sorted(name: &str, items: &[V], keys: &[V], descending: bool) -> anyhow::Result<V> {
    check_sort_keys(name, keys)?;
    let mut order: Vec<usize> = (0..items.len()).collect();
    // `sort_by` is stable: equal keys keep their input order.
    order.sort_by(|&a, &b| compare_keys(&keys[a], &keys[b], descending));
    Ok(V::from_array(
        order.into_iter().map(|i| items[i].clone()).collect(),
    ))
}

/// First element whose key is strictly `wanted` (Less for min, Greater for max) against the
/// best so far; null keys are skipped.
fn extreme(name: &str, items: &[V], keys: &[V], wanted: Ordering) -> anyhow::Result<V> {
    check_sort_keys(name, keys)?;
    let mut best: Option<usize> = None;
    for (i, key) in keys.iter().enumerate() {
        if matches!(key, V::Null) {
            continue;
        }
        match best {
            Some(b) if compare_keys(key, &keys[b], false) != wanted => {}
            _ => best = Some(i),
        }
    }
    Ok(best.map(|i| items[i].clone()).unwrap_or(V::Null))
}

/// Checks that every key can be ordered against every other key: one scalar type for scalar
/// keys; for array keys, one scalar type per position. Nulls go anywhere.
fn check_sort_keys(name: &str, keys: &[V]) -> anyhow::Result<()> {
    let mut scalar: Option<&'static str> = None;
    let mut positions: Vec<Option<&'static str>> = Vec::new();
    let mut has_list = false;
    for key in keys {
        match key {
            V::Array(parts) => {
                has_list = true;
                let parts = parts.borrow();
                if positions.len() < parts.len() {
                    positions.resize(parts.len(), None);
                }
                for (i, part) in parts.iter().enumerate() {
                    if matches!(part, V::Array(_)) {
                        bail!("{name}: nested arrays cannot be sort keys");
                    }
                    unify(name, &mut positions[i], scalar_kind(name, part)?, Some(i))?;
                }
            }
            other => unify(name, &mut scalar, scalar_kind(name, other)?, None)?,
        }
    }

    if let (true, Some(kind)) = (has_list, scalar) {
        bail!("{name}: keys mix arrays and {kind} values");
    }
    Ok(())
}

/// Orderable type of a scalar key; `None` for null.
fn scalar_kind(name: &str, key: &V) -> anyhow::Result<Option<&'static str>> {
    Ok(match key {
        V::Null => None,
        V::Bool(_) => Some("bool"),
        V::Number(_) => Some("number"),
        V::String(_) => Some("string"),
        V::Dynamic(d) if d.as_date().is_some() => Some("date"),
        V::Dynamic(d) => bail!("{name}: a {} value cannot be a sort key", d.type_name()),
        V::Object(_) => bail!("{name}: objects cannot be sort keys"),
        V::Array(_) => Some("array"),
    })
}

/// Total order on keys accepted by `check_sort_keys`; nulls last in either direction.
fn compare_keys(a: &V, b: &V, descending: bool) -> Ordering {
    let directed = |o: Ordering| if descending { o.reverse() } else { o };
    match (a, b) {
        (V::Null, V::Null) => Ordering::Equal,
        (V::Null, _) => Ordering::Greater,
        (_, V::Null) => Ordering::Less,
        (V::Bool(x), V::Bool(y)) => directed(x.cmp(y)),
        (V::Number(x), V::Number(y)) => directed(x.cmp(y)),
        (V::String(x), V::String(y)) => directed(x.as_str().cmp(y.as_str())),
        (V::Dynamic(x), V::Dynamic(y)) => match (x.as_date(), y.as_date()) {
            (Some(x), Some(y)) => directed(x.cmp(y)),
            _ => Ordering::Equal,
        },
        (V::Array(x), V::Array(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            for (p, q) in x.iter().zip(y.iter()) {
                let o = compare_keys(p, q, descending);
                if o != Ordering::Equal {
                    return o;
                }
            }
            directed(x.len().cmp(&y.len()))
        }
        // Different kinds were rejected by `check_sort_keys`.
        _ => Ordering::Equal,
    }
}

fn unify(
    name: &str,
    seen: &mut Option<&'static str>,
    kind: Option<&'static str>,
    position: Option<usize>,
) -> anyhow::Result<()> {
    let Some(kind) = kind else {
        return Ok(());
    };
    match seen {
        None => *seen = Some(kind),
        Some(existing) if *existing == kind => {}
        Some(existing) => match position {
            Some(i) => bail!("{name}: key part {i} mixes {existing} and {kind} values"),
            None => bail!("{name}: cannot compare {existing} keys with {kind} keys"),
        },
    }
    Ok(())
}

/// A hashable, deep-equality view of a value.
#[derive(PartialEq, Eq, Hash)]
enum EqKey {
    Null,
    Bool(bool),
    Number(Decimal),
    String(Rc<str>),
    Date(Option<NaiveDateTime>),
    List(Vec<EqKey>),
    Object(Vec<(Rc<str>, EqKey)>),
}

impl EqKey {
    fn of(name: &str, value: &V) -> anyhow::Result<EqKey> {
        Ok(match value {
            V::Null => EqKey::Null,
            V::Bool(b) => EqKey::Bool(*b),
            // Normalised so that 1, 1.0 and -0 hash and compare like the numbers they are.
            V::Number(n) => match n.is_zero() {
                true => EqKey::Number(Decimal::ZERO),
                false => EqKey::Number(n.normalize()),
            },
            V::String(s) => EqKey::String(Rc::from(s.as_str())),
            V::Dynamic(d) => match d.as_date() {
                Some(date) => EqKey::Date(date.0.map(|t| t.naive_utc())),
                None => bail!("{name}: a {} value cannot be compared", d.type_name()),
            },
            V::Array(a) => EqKey::List(
                a.borrow()
                    .iter()
                    .map(|v| EqKey::of(name, v))
                    .collect::<anyhow::Result<Vec<_>>>()?,
            ),
            V::Object(o) => {
                let mut fields = o
                    .borrow()
                    .iter()
                    .map(|(k, v)| Ok((Rc::<str>::from(k.as_str()), EqKey::of(name, v)?)))
                    .collect::<anyhow::Result<Vec<(Rc<str>, EqKey)>>>()?;
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                EqKey::Object(fields)
            }
        })
    }
}

/// Entry name for `groupBy` / `keyBy`: the key as `string()` converts it.
fn group_label(name: &str, key: &V) -> anyhow::Result<Rc<str>> {
    Ok(match key {
        V::Null => Rc::from("null"),
        V::Bool(b) => Rc::from(b.to_string()),
        V::Number(n) => Rc::from(n.to_string()),
        V::String(s) => Rc::from(s.as_str()),
        V::Dynamic(d) if d.as_date().is_some() => Rc::from(d.to_string()),
        other => bail!(
            "{name}: key must be a string, number, bool, date or null, got {}",
            other.type_name()
        ),
    })
}
