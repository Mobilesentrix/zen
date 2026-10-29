//! Cost of the large (> 32 entry) object representation: upstream's ahash HashMap vs
//! this fork's IndexMap (insertion order), on what an evaluation does with a wide context:
//! clone it, insert a few outputs, read some keys, iterate it (serialize/merge), drop it.
use ahash::RandomState;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::hint::black_box;
use std::time::Instant;
use zen_types::symbol::Symbol;
use zen_types::variable::Variable;

fn keys(n: usize) -> Vec<Symbol> {
    (0..n)
        .map(|i| Symbol::from(format!("field_name_{i}").as_str()))
        .collect()
}

fn run<M: Clone>(
    name: &str,
    n: usize,
    build: impl Fn(&[Symbol]) -> M,
    insert: impl Fn(&mut M, Symbol, Variable),
    get: impl Fn(&M, &str) -> bool,
    iter_len: impl Fn(&M) -> usize,
) -> f64 {
    let ks = keys(n);
    let base = build(&ks);
    let outs = keys(4)
        .into_iter()
        .map(|k| Symbol::from(format!("out_{}", k.as_str()).as_str()))
        .collect::<Vec<_>>();
    let iters = 200_000;
    let t = Instant::now();
    for i in 0..iters {
        let mut m = base.clone();
        for k in &outs {
            insert(&mut m, k.clone(), Variable::Number((i as i64).into()));
        }
        let mut hits = 0;
        for k in ks.iter().step_by(3) {
            hits += get(&m, k.as_str()) as usize;
        }
        black_box(hits + iter_len(&m));
        drop(black_box(m));
    }
    let ns = t.elapsed().as_nanos() as f64 / iters as f64;
    println!(
        "{name:<10} n={n:<4} {ns:8.1} ns per clone+4 inserts+{} gets+iterate+drop",
        n.div_ceil(3)
    );
    ns
}

fn main() {
    for n in [34, 58, 120] {
        let h = run(
            "HashMap",
            n,
            |ks| {
                ks.iter()
                    .map(|k| (k.clone(), Variable::Bool(true)))
                    .collect::<HashMap<_, _, RandomState>>()
            },
            |m, k, v| {
                m.insert(k, v);
            },
            |m, k| m.contains_key(k),
            |m| m.iter().count(),
        );
        let x = run(
            "IndexMap",
            n,
            |ks| {
                ks.iter()
                    .map(|k| (k.clone(), Variable::Bool(true)))
                    .collect::<IndexMap<_, _, RandomState>>()
            },
            |m, k, v| {
                m.insert(k, v);
            },
            |m, k| m.contains_key(k),
            |m| m.iter().count(),
        );
        println!("IndexMap / HashMap = {:.2}", x / h);
    }
}
