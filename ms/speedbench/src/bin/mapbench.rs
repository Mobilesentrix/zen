//! Cost of the large (> 32 entry) object representation: ahash HashMap vs IndexMap (insertion
//! order), on what an evaluation does with a wide context: clone it, insert a few outputs, read
//! some keys, iterate it (serialize/merge), drop it.
use ahash::RandomState;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;
use zen_types::symbol::Symbol;
use zen_types::variable::Variable;

const ITERATIONS: i32 = 200_000;
const OUTPUT_KEYS: usize = 4;
/// Every how many fields a key is read.
const READ_EVERY: usize = 3;

fn field_keys(count: usize) -> Vec<Symbol> {
    (0..count)
        .map(|i| Symbol::from(format!("field_name_{i}").as_str()))
        .collect()
}

fn run<M: Clone>(
    name: &str,
    entries: usize,
    build: impl Fn(&[Symbol]) -> M,
    insert: impl Fn(&mut M, Symbol, Variable),
    contains: impl Fn(&M, &str) -> bool,
    iterated_length: impl Fn(&M) -> usize,
) -> f64 {
    let keys = field_keys(entries);
    let base = build(&keys);
    let outputs = field_keys(OUTPUT_KEYS)
        .into_iter()
        .map(|key| Symbol::from(format!("out_{}", key.as_str()).as_str()))
        .collect::<Vec<_>>();
    let start = Instant::now();
    for i in 0..ITERATIONS {
        let mut map = base.clone();
        for key in &outputs {
            insert(&mut map, key.clone(), Variable::Number(i64::from(i).into()));
        }
        let mut hits = 0;
        for key in keys.iter().step_by(READ_EVERY) {
            hits += usize::from(contains(&map, key.as_str()));
        }
        black_box(hits + iterated_length(&map));
        drop(black_box(map));
    }
    let nanos_per_iteration = start.elapsed().as_nanos() as f64 / f64::from(ITERATIONS);
    println!(
        "{name:<10} n={entries:<4} {nanos_per_iteration:8.1} ns per clone+4 inserts+{} gets+iterate+drop",
        entries.div_ceil(READ_EVERY)
    );
    nanos_per_iteration
}

fn main() {
    for entries in [34, 58, 120] {
        let hash_map = run(
            "HashMap",
            entries,
            |keys| {
                keys.iter()
                    .map(|key| (key.clone(), Variable::Bool(true)))
                    .collect::<HashMap<_, _, RandomState>>()
            },
            |map, key, value| {
                map.insert(key, value);
            },
            |map, key| map.contains_key(key),
            |map| map.iter().count(),
        );
        let index_map = run(
            "IndexMap",
            entries,
            |keys| {
                keys.iter()
                    .map(|key| (key.clone(), Variable::Bool(true)))
                    .collect::<IndexMap<_, _, RandomState>>()
            },
            |map, key, value| {
                map.insert(key, value);
            },
            |map, key| map.contains_key(key),
            |map| map.iter().count(),
        );
        println!("IndexMap / HashMap = {:.2}", index_map / hash_map);
    }
}
