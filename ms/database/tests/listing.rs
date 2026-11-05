//! "Product listing search" benchmark and parity check.
//!
//! The workload: a front end asks "Apple iPhone 16, black, not sold out"; the rule finds
//! the matching products, the compatible ones, the stock, and returns an ordered SKU page. Three
//! versions of the same rule live in `bench/listing-rules.json`:
//! * `naive`        one lookup per question, joined/sorted/paged in a function step;
//! * `single_today` one query without blocks or counts (no paging: StarRocks cannot bind LIMIT);
//! * `single_new`   one query with `{{#if}}` optional filters and a checked LIMIT/OFFSET.
//!
//! Datasource (read-only, configurable):
//! * `MS_LISTING_URL`    connection URL (e.g. `starrocks://root@starrocks:9030/mobilesentrix_dev`);
//! * `MS_LISTING_SCHEMA` schema holding the tables (default `bench_listing`, seeded by
//!   `bench/listing-seed.sql`);
//! * `MS_LISTING_RULES`  rules file (default `bench/listing-rules.json`): a real DEV database gets
//!   its own copy with its own table and column names.
//!
//! Every statement is checked to be a single SELECT before anything runs (never writes).
//!
//! Knobs: `MS_LISTING_TBA` (testBeforeAcquire, default true = the node's default),
//! `MS_LISTING_ROUNDS` (5), `MS_LISTING_ITER` (20), `MS_LISTING_CASES` (300),
//! `MS_LISTING_SECONDS` (10), `MS_LISTING_WORKERS` (16), `MS_LISTING_VARIANTS` (all).

use ms_zen_database::{ConnectionStats, DatabaseCustomNode, Databases};
use serde_json::{json, Value};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use zen_engine::loader::MemoryLoader;
use zen_engine::model::DecisionContent;
use zen_engine::{DecisionEngine, EvaluationOptions, Variable};

const VARIANTS: [&str; 3] = ["naive", "single_today", "single_new"];

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_num(name: &str, default: usize) -> usize {
    env_or(name, &default.to_string()).parse().unwrap()
}

fn variants() -> Vec<&'static str> {
    let wanted = env_or("MS_LISTING_VARIANTS", "naive,single_today,single_new");
    VARIANTS
        .iter()
        .copied()
        .filter(|v| wanted.split(',').any(|w| w.trim() == *v))
        .collect()
}

/// Every database step's SQL must be one read-only SELECT (the harness may point at a real
/// DEV database later): no statement separator, starts with SELECT or WITH.
fn assert_read_only(sql: &str) {
    let head = sql.trim_start().to_ascii_uppercase();
    assert!(
        (head.starts_with("SELECT") || head.starts_with("WITH")) && !sql.contains(';'),
        "refusing to run a statement that is not a single SELECT: {sql}"
    );
}

fn rules() -> Value {
    let path = std::env::var("MS_LISTING_RULES")
        .unwrap_or_else(|_| format!("{}/bench/listing-rules.json", env!("CARGO_MANIFEST_DIR")));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let schema = env_or("MS_LISTING_SCHEMA", "bench_listing");
    assert!(
        schema
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "schema must be an identifier"
    );
    serde_json::from_str(&text.replace("${S}", &schema)).unwrap()
}

fn databases() -> Arc<Databases> {
    let url = std::env::var("MS_LISTING_URL")
        .or_else(|_| std::env::var("MS_TESTDB_STARROCKS_URL"))
        .expect("MS_LISTING_URL is not set");
    let tba = env_or("MS_LISTING_TBA", "true") == "true";
    let config = json!({"connections": {"listing": {
        "url": url, "maxConnections": 16, "minConnections": 4, "connectEagerly": true,
        // the naive version moves whole tables (47 500 products); the default maxRows of
        // 10 000 would fail it
        "maxRows": 100000, "testBeforeAcquire": tba
    }}});
    Arc::new(Databases::from_json(&config.to_string()).unwrap())
}

fn engine(dbs: &Arc<Databases>, rules: &Value) -> Arc<DecisionEngine> {
    let loader = MemoryLoader::default();
    for name in VARIANTS {
        let graph = rules[name].clone();
        for node in graph["nodes"].as_array().unwrap() {
            if let Some(sql) = node["content"]["config"]["query"].as_str() {
                assert_read_only(sql);
            }
        }
        let content: DecisionContent = serde_json::from_value(graph).unwrap();
        loader.add(name, content);
    }
    Arc::new(DecisionEngine::new(
        Arc::new(loader),
        Arc::new(DatabaseCustomNode::new(dbs.clone())),
    ))
}

async fn evaluate(
    engine: &DecisionEngine,
    variant: &str,
    input: &Value,
) -> Result<Vec<String>, String> {
    let response = ms_zen_database::with_evaluation_scope(engine.evaluate_with_opts(
        variant,
        Variable::from(input.clone()),
        EvaluationOptions {
            trace: false,
            max_depth: 5,
        },
    ))
    .await
    .map_err(|e| format!("{e:?}").chars().take(600).collect::<String>())?;
    let out: Value = serde_json::to_value(&response.result).unwrap();
    Ok(out["skus"]
        .as_array()
        .unwrap_or_else(|| panic!("{variant}: no skus in {out}"))
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect())
}

fn stats(dbs: &Databases) -> ConnectionStats {
    dbs.stats()[0].1
}

/// `single_today` has no paging: it only answers page 0 of 24.
fn supports(variant: &str, input: &Value) -> bool {
    variant != "single_today"
        || (input["page"].as_u64().unwrap_or(0) == 0
            && input["pageSize"].as_u64().unwrap_or(24) == 24
            && input.get("cyclePages").is_none())
}

/// `cyclePages: n` in a shape: call k asks for page k % n (every page a new LIMIT/OFFSET text,
/// so the prepared-statement cache is exercised the way real paging does).
fn paged(input: &Value, call: usize) -> Value {
    match input["cyclePages"].as_u64() {
        None => input.clone(),
        Some(n) => {
            let mut v = input.clone();
            v.as_object_mut().unwrap().remove("cyclePages");
            v["page"] = json!(call as u64 % n);
            v
        }
    }
}

/// Ten representative search shapes.
fn shapes() -> Vec<(&'static str, Value)> {
    vec![
        ("model only", json!({"model": "iPhone 16"})),
        (
            "brand IN (Apple, Samsung)",
            json!({"brands": ["Apple", "Samsung"]}),
        ),
        ("colour only", json!({"colour": "black"})),
        ("attribute only", json!({"quality": "OEM"})),
        (
            "model + colour + in stock",
            json!({"model": "iPhone 16", "colour": "black", "inStock": true}),
        ),
        (
            "brand + in stock",
            json!({"brands": ["Apple"], "inStock": true}),
        ),
        (
            "colour + part + in stock",
            json!({"colour": "blue", "partType": "screen", "inStock": true}),
        ),
        (
            "everything",
            json!({"model": "iPhone 16", "brands": ["Apple"], "colour": "black", "quality": "OEM", "partType": "screen", "inStock": true}),
        ),
        ("no filter (browse)", json!({})),
        (
            "in stock, page 3 of 48",
            json!({"inStock": true, "page": 3, "pageSize": 48}),
        ),
        (
            "in stock, pages 0-199 cycling",
            json!({"inStock": true, "pageSize": 24, "cyclePages": 200}),
        ),
    ]
}

/// xorshift64*: deterministic random inputs without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

fn random_input(rng: &mut Rng) -> Value {
    let models = [
        "iPhone 16",
        "iPhone 15 Pro",
        "iPad 12",
        "Galaxy S153",
        "Galaxy A10",
        "Pixel 7",
        "Moto G12",
        "OnePlus 9",
        "Redmi Note 40",
        "No Such Phone",
        "iPhone 3 Plus",
    ];
    let brands = [
        "Apple", "Samsung", "Google", "Motorola", "OnePlus", "Xiaomi", "Nokia",
    ];
    let colours = [
        "black", "white", "blue", "red", "gold", "silver", "green", "purple", "pink",
    ];
    let qualities = ["OEM", "Premium", "Aftermarket", "Refurbished"];
    let parts = [
        "screen",
        "battery",
        "back glass",
        "camera",
        "charging port",
        "housing",
        "speaker",
        "flex",
    ];
    let mut input = serde_json::Map::new();
    if rng.chance(35) {
        input.insert("model".into(), json!(rng.pick(&models)));
    }
    if rng.chance(30) {
        let n = 1 + rng.below(3);
        let list: Vec<&str> = (0..n).map(|_| rng.pick(&brands)).collect();
        input.insert("brands".into(), json!(list));
    }
    if rng.chance(40) {
        input.insert("colour".into(), json!(rng.pick(&colours)));
    }
    if rng.chance(25) {
        input.insert("quality".into(), json!(rng.pick(&qualities)));
    }
    if rng.chance(25) {
        input.insert("partType".into(), json!(rng.pick(&parts)));
    }
    if rng.chance(50) {
        input.insert("inStock".into(), json!(rng.chance(80)));
    }
    if rng.chance(30) {
        input.insert("page".into(), json!(rng.below(4)));
        input.insert(
            "pageSize".into(),
            json!([10, 24, 48][rng.below(3) as usize]),
        );
    }
    // explicit nulls and empties behave like "not asked"
    if rng.chance(10) {
        input.insert("colour".into(), Value::Null);
    }
    if rng.chance(10) {
        input.insert("brands".into(), json!([]));
    }
    Value::Object(input)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Property: every version returns the same SKUs in the same order for random inputs.
#[test]
#[ignore = "needs MS_LISTING_URL and the bench_listing schema"]
fn listing_versions_agree() {
    let dbs = databases();
    let engine = engine(&dbs, &rules());
    let rt = runtime();
    let variants = variants();
    let cases = env_num("MS_LISTING_CASES", 300);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut compared, mut nonempty) = (0, 0);
    for (name, input) in shapes()
        .into_iter()
        .map(|(n, i)| (n.to_string(), i))
        .chain((0..cases).map(|k| (format!("random #{k}"), random_input(&mut rng))))
    {
        let reference = rt.block_on(evaluate(&engine, "naive", &input)).unwrap();
        for variant in variants.iter().filter(|v| **v != "naive") {
            if !supports(variant, &input) {
                continue;
            }
            let got = rt
                .block_on(evaluate(&engine, variant, &input))
                .unwrap_or_else(|e| panic!("{variant} failed on {name} {input}: {e}"));
            assert_eq!(got, reference, "{variant} differs on {name} {input}");
            compared += 1;
        }
        nonempty += usize::from(!reference.is_empty());
    }
    println!(
        "LISTING parity: {compared} comparisons, {nonempty} of {} inputs with results, all equal",
        cases + 10
    );
}

struct Summary {
    p50: Duration,
    p99: Duration,
}

fn summary(mut samples: Vec<Duration>) -> Summary {
    samples.sort();
    let at = |q: f64| samples[((samples.len() as f64 - 1.0) * q).round() as usize];
    Summary {
        p50: at(0.5),
        p99: at(0.99),
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Sequential latency per shape and version, interleaved rounds, plus the per-call profile:
/// round trips, rows received, time waiting for a connection, time in the database, and the
/// rest (engine: binding, decode, JSON, function step).
#[test]
#[ignore = "needs MS_LISTING_URL and the bench_listing schema"]
fn listing_latency() {
    let dbs = databases();
    let engine = engine(&dbs, &rules());
    let rt = runtime();
    let rounds = env_num("MS_LISTING_ROUNDS", 5);
    let iterations = env_num("MS_LISTING_ITER", 20);
    let variants = variants();
    println!(
        "LISTING latency: {rounds} interleaved rounds x {iterations} calls, testBeforeAcquire={}",
        env_or("MS_LISTING_TBA", "true")
    );
    println!("LISTING | shape | version | p50 ms | p99 ms | trips | rows | acquire ms | db ms | engine ms | skus |");
    for (shape, input) in shapes() {
        let active: Vec<&str> = variants
            .iter()
            .copied()
            .filter(|v| supports(v, &input))
            .collect();
        for v in &active {
            for _ in 0..3 {
                rt.block_on(evaluate(&engine, v, &input)).unwrap();
            }
        }
        let mut samples: Vec<Vec<Duration>> = vec![Vec::new(); active.len()];
        let mut profile = vec![ConnectionStats::default(); active.len()];
        let mut skus = vec![0usize; active.len()];
        for round in 0..rounds {
            for k in 0..active.len() {
                // alternate the order every round so neither version always goes first
                let k = if round % 2 == 0 {
                    k
                } else {
                    active.len() - 1 - k
                };
                let before = stats(&dbs);
                for call in 0..iterations {
                    let input = paged(&input, round * iterations + call);
                    let start = Instant::now();
                    let out = rt.block_on(evaluate(&engine, active[k], &input)).unwrap();
                    samples[k].push(start.elapsed());
                    skus[k] = out.len();
                }
                let d = stats(&dbs).since(&before);
                let p = &mut profile[k];
                p.queries += d.queries;
                p.rows += d.rows;
                p.acquire += d.acquire;
                p.execute += d.execute;
            }
        }
        for (k, v) in active.iter().enumerate() {
            let n = (rounds * iterations) as u32;
            let total: Duration = samples[k].iter().sum();
            let s = summary(samples[k].clone());
            let p = &profile[k];
            let engine_time = total.saturating_sub(p.acquire + p.execute) / n;
            println!(
                "LISTING | {shape} | {v} | {:.2} | {:.2} | {:.1} | {} | {:.3} | {:.2} | {:.2} | {} |",
                ms(s.p50),
                ms(s.p99),
                p.queries as f64 / n as f64,
                p.rows / n as u64,
                ms(p.acquire / n),
                ms(p.execute / n),
                ms(engine_time),
                skus[k]
            );
        }
    }
}

/// Load: open-loop at a fixed rate (latency measured from the scheduled start, so queueing is
/// counted) and closed-loop with `MS_LISTING_WORKERS` workers (max throughput). Workers are OS
/// threads with their own current-thread runtime sharing one engine and one pool (many callers,
/// one engine).
#[test]
#[ignore = "needs MS_LISTING_URL and the bench_listing schema"]
fn listing_load() {
    let dbs = databases();
    let engine = engine(&dbs, &rules());
    let seconds = env_num("MS_LISTING_SECONDS", 10) as u64;
    let workers = env_num("MS_LISTING_WORKERS", 16);
    let rate = env_num("MS_LISTING_RATE", 100) as u64;
    let picked = env_or(
        "MS_LISTING_LOAD_SHAPES",
        // `|`-separated: shape names contain commas
        "model + colour + in stock|brand IN (Apple, Samsung)|no filter (browse)",
    );
    println!(
        "LISTING load: {seconds} s each, {rate} req/s open loop, {workers} workers closed loop"
    );
    println!("LISTING-LOAD | shape | version | mode | done | errors | req/s | p50 ms | p99 ms |");
    for (shape, input) in shapes()
        .into_iter()
        .filter(|(n, _)| picked.split('|').any(|p| p.trim() == *n))
    {
        for v in variants().into_iter().filter(|v| supports(v, &input)) {
            for mode in ["open", "closed"] {
                let (tx, rx) = mpsc::channel::<Option<Instant>>();
                let rx = Arc::new(std::sync::Mutex::new(rx));
                let (done_tx, done_rx) = mpsc::channel::<Result<Duration, ()>>();
                let deadline = Instant::now() + Duration::from_secs(seconds);
                let threads: Vec<_> = (0..workers)
                    .map(|_| {
                        let (engine, input, rx, done_tx) =
                            (engine.clone(), input.clone(), rx.clone(), done_tx.clone());
                        let closed = mode == "closed";
                        std::thread::spawn(move || {
                            let rt = runtime();
                            loop {
                                let scheduled = if closed {
                                    if Instant::now() >= deadline {
                                        break;
                                    }
                                    Instant::now()
                                } else {
                                    match rx.lock().unwrap().recv() {
                                        Ok(Some(t)) => t,
                                        _ => break,
                                    }
                                };
                                let r = rt.block_on(evaluate(&engine, v, &input));
                                let _ =
                                    done_tx.send(r.map(|_| scheduled.elapsed()).map_err(|_| ()));
                            }
                        })
                    })
                    .collect();
                drop(done_tx);
                let started = Instant::now();
                if mode == "open" {
                    let gap = Duration::from_nanos(1_000_000_000 / rate);
                    let mut next = started;
                    while next < deadline {
                        let now = Instant::now();
                        if next > now {
                            std::thread::sleep(next - now);
                        }
                        tx.send(Some(next)).unwrap();
                        next += gap;
                    }
                }
                drop(tx);
                for t in threads {
                    t.join().unwrap();
                }
                let elapsed = started.elapsed();
                let (mut ok, mut errors) = (Vec::new(), 0);
                for r in done_rx.iter() {
                    match r {
                        Ok(d) => ok.push(d),
                        Err(()) => errors += 1,
                    }
                }
                let s = summary(ok.clone());
                println!(
                    "LISTING-LOAD | {shape} | {v} | {mode} | {} | {errors} | {:.0} | {:.2} | {:.2} |",
                    ok.len(),
                    ok.len() as f64 / elapsed.as_secs_f64(),
                    ms(s.p50),
                    ms(s.p99)
                );
            }
        }
    }
}
