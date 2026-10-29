//! MobileSentrix: the evaluation context parsed straight into a `Variable`.
//!
//! Upstream parses the context JSON into a `serde_json::Value` and then converts that tree into
//! a `Variable` (`Variable::try_from_value`): two full trees per call. This builds the `Variable`
//! in one pass with the same result, entry for entry:
//! - object keys come out in `serde_json::Map` order: without serde_json's `preserve_order`
//!   feature (not enabled in this workspace) that map is a `BTreeMap<String, _>`, so keys are
//!   sorted by their bytes and a repeated key keeps its last value;
//! - numbers convert exactly like `Variable::try_from_value` (arbitrary precision: the literal
//!   text through `Decimal::from_str_exact`, then `from_scientific`, then `f64`), and a number
//!   no `Decimal` can hold is an error, as there ([`OutOfRange::Error`]); upstream's decision API
//!   converts with `Variable::from(Value)` instead, which turns such a number into `null`
//!   ([`OutOfRange::Null`]).
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::de::{DeserializeSeed, Deserializer, Error, MapAccess, SeqAccess, Visitor};
use std::fmt::Formatter;
use std::str::FromStr;
use zen_engine::Variable;
use zen_types::symbol::Symbol;
use zen_types::variable::VariableMap;

/// serde_json's private marker key for an arbitrary-precision number.
const NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// What a number outside `Decimal`'s range becomes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutOfRange {
    Error,
    Null,
}

pub(crate) fn parse_context(
    bytes: &[u8],
    range: OutOfRange,
) -> Result<Variable, serde_json::Error> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let parsed = ContextSeed(range).deserialize(&mut de)?;
    de.end()?;
    // An out-of-range number is an error only if it survives: `try_from_value` converts the
    // finished Value tree, where a repeated key had already dropped its earlier values.
    match parsed.out_of_range {
        true => Err(serde_json::Error::custom("number out of range")),
        false => Ok(parsed.value),
    }
}

/// A parsed value, and whether it holds a number no `Decimal` can represent
/// ([`OutOfRange::Error`] only; such a number is `null` in the tree until then).
struct Parsed {
    value: Variable,
    out_of_range: bool,
}

impl Parsed {
    fn ok(value: Variable) -> Self {
        Self {
            value,
            out_of_range: false,
        }
    }
}

fn decimal_from_text(text: &str) -> Option<Decimal> {
    Decimal::from_str_exact(text)
        .or_else(|_| Decimal::from_scientific(text))
        .ok()
        .or_else(|| text.parse::<f64>().ok().and_then(Decimal::from_f64))
}

fn number(decimal: Option<Decimal>, range: OutOfRange) -> Parsed {
    match decimal {
        Some(d) => Parsed::ok(Variable::Number(d)),
        None => Parsed {
            value: Variable::Null,
            out_of_range: range == OutOfRange::Error,
        },
    }
}

#[derive(Clone, Copy)]
struct ContextSeed(OutOfRange);

impl<'de> DeserializeSeed<'de> for ContextSeed {
    type Value = Parsed;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Parsed, D::Error> {
        deserializer.deserialize_any(ContextVisitor(self.0))
    }
}

/// A map key: serde_json's number marker, or a name (hipstr keeps short names inline, so a
/// borrowed key costs no allocation).
enum Key {
    NumberToken,
    Name(Symbol),
}

impl<'de> serde::Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = Key;
            fn expecting(&self, f: &mut Formatter) -> std::fmt::Result {
                f.write_str("a string key")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<Key, E> {
                Ok(match v == NUMBER_TOKEN {
                    true => Key::NumberToken,
                    false => Key::Name(Symbol::from(v)),
                })
            }
        }
        deserializer.deserialize_str(KeyVisitor)
    }
}

/// The text of an arbitrary-precision number, converted without copying it. Like serde_json's
/// own Value, text that is not a JSON number (only possible when a document spells the marker
/// key itself) is an error.
struct NumberText(OutOfRange);

impl<'de> DeserializeSeed<'de> for NumberText {
    type Value = Parsed;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Parsed, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for NumberText {
    type Value = Parsed;

    fn expecting(&self, f: &mut Formatter) -> std::fmt::Result {
        f.write_str("a number")
    }

    fn visit_str<E: Error>(self, text: &str) -> Result<Parsed, E> {
        serde_json::Number::from_str(text).map_err(E::custom)?;
        Ok(number(decimal_from_text(text), self.0))
    }
}

struct ContextVisitor(OutOfRange);

impl<'de> Visitor<'de> for ContextVisitor {
    type Value = Parsed;

    fn expecting(&self, f: &mut Formatter) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: Error>(self, v: bool) -> Result<Parsed, E> {
        Ok(Parsed::ok(Variable::Bool(v)))
    }

    // Without arbitrary_precision serde_json reports numbers through these; with it (as built
    // here) they arrive as the NUMBER_TOKEN map below. Kept equal to `try_from_value`.
    fn visit_i64<E: Error>(self, v: i64) -> Result<Parsed, E> {
        Ok(Parsed::ok(Variable::Number(Decimal::from(v))))
    }

    fn visit_u64<E: Error>(self, v: u64) -> Result<Parsed, E> {
        Ok(Parsed::ok(Variable::Number(Decimal::from(v))))
    }

    fn visit_f64<E: Error>(self, v: f64) -> Result<Parsed, E> {
        Ok(number(Decimal::from_f64(v), self.0))
    }

    fn visit_str<E: Error>(self, v: &str) -> Result<Parsed, E> {
        Ok(Parsed::ok(Variable::String(v.into())))
    }

    fn visit_unit<E: Error>(self) -> Result<Parsed, E> {
        Ok(Parsed::ok(Variable::Null))
    }

    fn visit_none<E: Error>(self) -> Result<Parsed, E> {
        Ok(Parsed::ok(Variable::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Parsed, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        let mut out_of_range = false;
        while let Some(item) = seq.next_element_seed(ContextSeed(self.0))? {
            out_of_range |= item.out_of_range;
            items.push(item.value);
        }
        Ok(Parsed {
            value: Variable::from_array(items),
            out_of_range,
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Parsed, A::Error> {
        // serde_json gives no size hint for objects; start with room for a typical one.
        let mut entries: Vec<(Symbol, Parsed)> = Vec::with_capacity(map.size_hint().unwrap_or(16));
        while let Some(key) = map.next_key::<Key>()? {
            let key = match key {
                Key::NumberToken if entries.is_empty() => {
                    return map.next_value_seed(NumberText(self.0));
                }
                Key::NumberToken => Symbol::from(NUMBER_TOKEN),
                Key::Name(name) => name,
            };
            let value = map.next_value_seed(ContextSeed(self.0))?;
            entries.push((key, value));
        }
        // BTreeMap order: sorted by key bytes; a repeated key keeps its last value. The sort is
        // stable, so of equal keys the one last in the input is last in its run and kept.
        // Already strictly ascending (common: clients often send sorted keys) means sorted and
        // without repeats, so there is nothing to do.
        let ascending = entries
            .windows(2)
            .all(|w| w[0].0.as_bytes() < w[1].0.as_bytes());
        if !ascending {
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            entries.dedup_by(|later, earlier| {
                if later.0 == earlier.0 {
                    std::mem::swap(later, earlier);
                    true
                } else {
                    false
                }
            });
        }
        let out_of_range = entries.iter().any(|(_, p)| p.out_of_range);
        let entries = entries.into_iter().map(|(k, p)| (k, p.value)).collect();
        Ok(Parsed {
            value: Variable::from_object(VariableMap::from_unique_entries(entries)),
            out_of_range,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The serialized form, entry order included, except that objects over 32 entries (hash
    /// maps, whose order is per instance) are written in sorted order.
    fn canon(v: &Variable) -> String {
        match v {
            Variable::Array(a) => {
                let items: Vec<String> = a.borrow().iter().map(canon).collect();
                format!("[{}]", items.join(","))
            }
            Variable::Object(o) => {
                let o = o.borrow();
                let mut items: Vec<String> = o
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{}:{}",
                            serde_json::to_string(k.as_str()).unwrap(),
                            canon(v)
                        )
                    })
                    .collect();
                if items.len() > 32 {
                    items.sort();
                }
                format!("{{{}}}", items.join(","))
            }
            other => serde_json::to_string(other).unwrap(),
        }
    }

    fn old_engine(bytes: &[u8]) -> Option<String> {
        let value = serde_json::from_slice::<Value>(bytes).ok()?;
        let var = Variable::try_from_value(value).ok()?;
        Some(canon(&var))
    }

    fn old_decision(bytes: &[u8]) -> Option<String> {
        let text = std::str::from_utf8(bytes).ok()?;
        let value = serde_json::from_str::<Value>(text).ok()?;
        Some(canon(&Variable::from(value)))
    }

    fn new(bytes: &[u8], range: OutOfRange) -> Option<String> {
        parse_context(bytes, range).ok().map(|v| canon(&v))
    }

    fn check(json: &str) {
        let b = json.as_bytes();
        assert_eq!(
            new(b, OutOfRange::Error),
            old_engine(b),
            "engine path: {json}"
        );
        assert_eq!(
            new(b, OutOfRange::Null),
            old_decision(b),
            "decision path: {json}"
        );
    }

    /// Every JSON string escape: tab, quote, backslash, slash, a BMP code point and a surrogate
    /// pair (written with escapes so the source stays ASCII).
    const ESCAPED: &str = concat!(
        r#"{"esc":"t"#,
        "\\t",
        r#"q"#,
        "\\\"",
        r#"b"#,
        "\\\\",
        r#"s"#,
        "\\/",
        r#"u"#,
        "\\u00e9",
        r#"p"#,
        "\\ud83d\\ude00",
        r#"n"#,
        "\\n",
        r#""}"#
    );

    #[test]
    fn same_tree_as_value_conversion() {
        for json in [
            r#"{}"#,
            r#"{"b":1,"a":2,"c":{"z":true,"y":null,"x":[3,2,{"k":"v","j":1.50}]}}"#,
            r#"{"a":1,"a":2,"b":3,"a":4}"#,
            r#"{"é":1,"e":2,"E":3,"_":4,"10":5,"9":6,"":7}"#,
            r#"{"n":[0,-0,1e3,1E-3,12.3400,-7.5e+2,123456789012345678901234567890,0.0000000000000000000000000001]}"#,
            r#"{"big":1e40}"#,
            r#"{"tiny":1e-40}"#,
            r#"{"s":"plain text, unicode é😀"}"#,
            r#"{"$serde_json::private::Number":"x"}"#,
            r#"{"a":{"$serde_json::private::Number":"1"}}"#,
            r#"[1,{"b":2,"a":1}]"#,
            r#"  {"w" : [ ] , "v" : { } }  "#,
            r#"{"a":1} trailing"#,
            r#"{"a":"#,
            r#"null"#,
            ESCAPED,
        ] {
            check(json);
        }
    }

    #[test]
    fn same_tree_for_generated_documents() {
        // Deterministic random JSON (LCG): shuffled and repeated keys, nested values, numbers
        // of every shape.
        let mut seed: u64 = 0x5eed;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        fn gen(next: &mut dyn FnMut(u64) -> u64, depth: u32, out: &mut String) {
            let keys = [
                "a",
                "b",
                "aa",
                "B",
                "z",
                "_x",
                "10",
                "2",
                "é",
                "key with space",
                "$nodes",
            ];
            match if depth > 3 { next(5) } else { next(7) } {
                0 => out.push_str("null"),
                1 => out.push_str(if next(2) == 0 { "true" } else { "false" }),
                2 => {
                    let nums = [
                        "0",
                        "-1",
                        "3.14159",
                        "1e5",
                        "2.5E-3",
                        "99999999999999999999999999999",
                        "0.10",
                        "-0.0",
                        "7e40",
                        "1e-30",
                    ];
                    out.push_str(nums[next(nums.len() as u64) as usize]);
                }
                3 => out.push_str(&format!("\"s{}\\u00e9\"", next(100))),
                4 => out.push_str("\"plain\""),
                5 => {
                    out.push('[');
                    for i in 0..next(4) {
                        if i > 0 {
                            out.push(',');
                        }
                        gen(next, depth + 1, out);
                    }
                    out.push(']');
                }
                _ => {
                    out.push('{');
                    for i in 0..next(40) {
                        if i > 0 {
                            out.push(',');
                        }
                        let key = keys[next(keys.len() as u64) as usize];
                        out.push_str(&format!("\"{key}{}\":", next(20)));
                        gen(next, depth + 1, out);
                    }
                    out.push('}');
                }
            }
        }
        for _ in 0..3000 {
            let mut doc = String::from("{\"root\":");
            gen(&mut next, 0, &mut doc);
            doc.push('}');
            check(&doc);
        }
    }
}
