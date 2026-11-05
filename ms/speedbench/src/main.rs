//! MS profiling harness. Evaluates corpus rules through the same C ABI the Go
//! binding uses (a native loader callback, so the engine wraps it in the
//! CachedLoader exactly like the Go loader; `zen_engine_evaluate` with the
//! context as a NUL-terminated JSON string; the result freed with `free`).
//!
//! Input: the JSON written by `speed export` (parity/speed): {rules: {key: content}, cases: {key: [input]}}.
//!
//!   ms-speedbench bench   EXPORT [--only a,b] [--iters N] [--threads T] [--secs S] [--trace]
//!   ms-speedbench profile EXPORT [--only a,b] [--secs S] [--svg FILE] [--top N]
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// Evaluations of every rule before measuring starts.
const WARM_UP_CALLS: usize = 200;
/// How long `bench` drives each rule from many threads.
const PARALLEL_SECONDS: f64 = 1.0;
/// Calls between two looks at the clock in `bench`.
const DEADLINE_CHECK_EVERY: usize = 64;
/// Frames above a `--callers` target that make up a call chain.
const CALLER_CHAIN_FRAMES: usize = 4;
/// Call chains printed per `--callers` target.
const CALLER_CHAINS_SHOWN: usize = 12;
const SYMBOL_HASH_SUFFIX_LENGTH: usize = 19;
const SYMBOL_NAME_MAX_CHARS: usize = 160;

#[repr(C)]
struct ZenResult {
    result: *mut c_char,
    error: u8,
    details: *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Options {
    trace: bool,
    max_depth: u8,
}

#[repr(C)]
struct LoaderResult {
    content: *mut c_char,
    error: *mut c_char,
}

#[repr(C)]
struct EngineStruct {
    _p: [u8; 0],
}

extern "C" {
    fn zen_engine_new_native(
        loader: extern "C" fn(*const c_char) -> LoaderResult,
        custom: extern "C" fn(*const c_char) -> LoaderResult,
    ) -> *mut EngineStruct;
    fn zen_engine_evaluate(
        engine: *const EngineStruct,
        key: *const c_char,
        context: *const c_char,
        options: Options,
    ) -> ZenResult;
}

// Keeps zen-ffi's symbols in the link.
#[allow(unused_imports)]
use zen_ffi as _;

static RULES: OnceLock<HashMap<String, Vec<u8>>> = OnceLock::new();

fn c_malloc_copy(bytes: &[u8]) -> *mut c_char {
    unsafe {
        let copy = libc::malloc(bytes.len() + 1).cast::<u8>();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), copy, bytes.len());
        *copy.add(bytes.len()) = 0;
        copy.cast::<c_char>()
    }
}

extern "C" fn loader(key: *const c_char) -> LoaderResult {
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    match RULES.get().and_then(|rules| rules.get(key.as_ref())) {
        Some(content) => LoaderResult {
            content: c_malloc_copy(content),
            error: std::ptr::null_mut(),
        },
        None => LoaderResult {
            content: std::ptr::null_mut(),
            error: std::ptr::null_mut(),
        },
    }
}

extern "C" fn custom(_: *const c_char) -> LoaderResult {
    LoaderResult {
        content: std::ptr::null_mut(),
        error: c_malloc_copy(b"custom nodes are not available in the profiler"),
    }
}

struct Engine(*mut EngineStruct);
unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

impl Engine {
    fn eval(&self, key: &CStr, context: &CStr, options: Options) -> bool {
        let result =
            unsafe { zen_engine_evaluate(self.0, key.as_ptr(), context.as_ptr(), options) };
        unsafe {
            if !result.result.is_null() {
                libc::free(result.result.cast::<libc::c_void>());
            }
            if !result.details.is_null() {
                libc::free(result.details.cast::<libc::c_void>());
            }
        }
        result.error == 0
    }
}

struct Args {
    command: String,
    export: String,
    only: Vec<String>,
    iterations: usize,
    threads: usize,
    seconds: f64,
    svg: Option<String>,
    top: usize,
    trace: bool,
    callers: Vec<String>,
}

fn comma_list(value: &str) -> Vec<String> {
    value.split(',').map(String::from).collect()
}

fn parse_args() -> Args {
    let mut arguments = std::env::args().skip(1);
    let command = arguments.next().expect("command");
    let export = arguments.next().expect("export file");
    let mut args = Args {
        command,
        export,
        only: vec![],
        iterations: 3000,
        threads: 1,
        seconds: 3.0,
        svg: None,
        top: 40,
        trace: false,
        callers: vec![],
    };
    while let Some(flag) = arguments.next() {
        match flag.as_str() {
            "--only" => args.only = comma_list(&arguments.next().unwrap()),
            "--iters" => args.iterations = arguments.next().unwrap().parse().unwrap(),
            "--threads" => args.threads = arguments.next().unwrap().parse().unwrap(),
            "--secs" => args.seconds = arguments.next().unwrap().parse().unwrap(),
            "--svg" => args.svg = arguments.next(),
            "--top" => args.top = arguments.next().unwrap().parse().unwrap(),
            "--trace" => args.trace = true,
            "--callers" => args.callers = comma_list(&arguments.next().unwrap()),
            other => panic!("unknown flag {other}"),
        }
    }
    args
}

/// One rule key and the contexts it is evaluated with.
type RuleCases = (CString, Vec<CString>);

/// Publishes the export's rules to the loader callback and returns the selected cases.
fn load_export(args: &Args) -> Vec<RuleCases> {
    let raw = std::fs::read(&args.export).expect("read export");
    let export: serde_json::Value = serde_json::from_slice(&raw).expect("export json");
    let mut rules = HashMap::new();
    for (key, content) in export["rules"].as_object().unwrap() {
        rules.insert(key.clone(), serde_json::to_vec(content).unwrap());
    }
    RULES.set(rules).unwrap();
    let mut keys: Vec<&String> = export["cases"].as_object().unwrap().keys().collect();
    keys.sort();
    let mut work = vec![];
    for key in keys {
        if !args.only.is_empty() && !args.only.iter().any(|only| only == key) {
            continue;
        }
        let inputs = export["cases"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|input| CString::new(serde_json::to_vec(input).unwrap()).unwrap())
            .collect();
        work.push((CString::new(key.as_str()).unwrap(), inputs));
    }
    work
}

fn main() {
    let args = parse_args();
    let work = load_export(&args);
    let engine = Arc::new(Engine(unsafe { zen_engine_new_native(loader, custom) }));
    let options = Options {
        trace: args.trace,
        max_depth: 100,
    };
    for (key, inputs) in &work {
        for i in 0..WARM_UP_CALLS {
            engine.eval(key, &inputs[i % inputs.len()], options);
        }
    }
    match args.command.as_str() {
        "bench" => bench(&engine, &work, &args, options),
        "profile" => profile(&engine, &work, &args, options),
        other => panic!("unknown command {other}"),
    }
}

fn bench(engine: &Arc<Engine>, work: &[RuleCases], args: &Args, options: Options) {
    println!(
        "{:<56} {:>9} {:>9} {:>9} {:>11}",
        "rule", "p50 us", "p99 us", "mean us", "par ops/s"
    );
    let mut pooled = vec![];
    for (key, inputs) in work {
        let (mut latencies, succeeded) = measure_rule(engine, key, inputs, args, options);
        pooled.extend_from_slice(&latencies);
        let mean = latencies.iter().sum::<f64>() / latencies.len() as f64;
        latencies.sort_by(f64::total_cmp);
        let operations_per_second = if args.threads > 1 {
            parallel(engine, key, inputs, args.threads, PARALLEL_SECONDS, options)
        } else {
            0.0
        };
        println!(
            "{:<56} {:>9.1} {:>9.1} {:>9.1} {:>11.0} {}",
            key.to_string_lossy(),
            latencies[latencies.len() / 2],
            latencies[latencies.len() * 99 / 100],
            mean,
            operations_per_second,
            if succeeded == latencies.len() {
                ""
            } else {
                "(errors)"
            }
        );
    }
    pooled.sort_by(f64::total_cmp);
    println!(
        "pooled p50 {:.1} us over {} calls",
        pooled[pooled.len() / 2],
        pooled.len()
    );
}

/// Sequential latencies of one rule in microseconds, and how many calls succeeded.
fn measure_rule(
    engine: &Engine,
    key: &CStr,
    inputs: &[CString],
    args: &Args,
    options: Options,
) -> (Vec<f64>, usize) {
    let mut latencies = Vec::with_capacity(args.iterations);
    let mut succeeded = 0;
    let deadline = Instant::now() + Duration::from_secs_f64(args.seconds);
    for i in 0..args.iterations {
        let call_start = Instant::now();
        if engine.eval(key, &inputs[i % inputs.len()], options) {
            succeeded += 1;
        }
        latencies.push(call_start.elapsed().as_nanos() as f64 / 1000.0);
        if i % DEADLINE_CHECK_EVERY == DEADLINE_CHECK_EVERY - 1 && Instant::now() > deadline {
            break;
        }
    }
    (latencies, succeeded)
}

fn parallel(
    engine: &Arc<Engine>,
    key: &CStr,
    inputs: &[CString],
    threads: usize,
    seconds: f64,
    options: Options,
) -> f64 {
    let stop = Arc::new(AtomicBool::new(false));
    let operations = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    std::thread::scope(|scope| {
        for thread_index in 0..threads {
            let (stop, operations, engine) = (stop.clone(), operations.clone(), engine.clone());
            scope.spawn(move || {
                let mut input_index = thread_index;
                let mut completed = 0;
                while !stop.load(Ordering::Relaxed) {
                    engine.eval(key, &inputs[input_index % inputs.len()], options);
                    input_index += 1;
                    completed += 1;
                }
                operations.fetch_add(completed, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(seconds));
        stop.store(true, Ordering::Relaxed);
    });
    operations.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

fn profile(engine: &Arc<Engine>, work: &[RuleCases], args: &Args, options: Options) {
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(4999)
        .build()
        .unwrap();
    let per_rule = Duration::from_secs_f64(args.seconds / work.len() as f64);
    for (key, inputs) in work {
        let end = Instant::now() + per_rule;
        let mut i = 0;
        while Instant::now() < end {
            engine.eval(key, &inputs[i % inputs.len()], options);
            i += 1;
        }
    }
    let report = guard.report().build().unwrap();
    if let Some(svg) = &args.svg {
        let file = std::fs::File::create(svg).unwrap();
        report.flamegraph(file).unwrap();
    }
    SampleCounts::from_report(&report, &args.callers).print(args.top);
}

/// Profile samples aggregated by symbol.
#[derive(Default)]
struct SampleCounts {
    total: isize,
    self_samples: HashMap<String, isize>,
    inclusive_samples: HashMap<String, isize>,
    /// By `--callers` target: samples per call chain above it.
    callers: HashMap<String, HashMap<String, isize>>,
}

impl SampleCounts {
    fn from_report(report: &pprof::Report, caller_targets: &[String]) -> Self {
        let mut counts = Self::default();
        for (frames, count) in &report.data {
            counts.total += count;
            let names: Vec<String> = frames
                .frames
                .iter()
                .flat_map(|frame| frame.iter().map(|symbol| short(&symbol.name())))
                .collect();
            counts.add_caller_chains(&names, caller_targets, *count);
            if let Some(leaf) = names.first() {
                *counts.self_samples.entry(leaf.clone()).or_default() += count;
            }
            let unique: HashSet<&String> = names.iter().collect();
            for name in unique {
                *counts.inclusive_samples.entry(name.clone()).or_default() += count;
            }
        }
        counts
    }

    /// Records the frames above the first frame named after each target.
    fn add_caller_chains(&mut self, names: &[String], targets: &[String], count: isize) {
        for target in targets {
            if let Some(position) = names.iter().position(|name| name == target) {
                let chain = names[position + 1..]
                    .iter()
                    .take(CALLER_CHAIN_FRAMES)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" < ");
                *self
                    .callers
                    .entry(target.clone())
                    .or_default()
                    .entry(chain)
                    .or_default() += count;
            }
        }
    }

    fn print(&self, top: usize) {
        for (target, chains) in &self.callers {
            println!("== CALLERS of {target}");
            print_top(chains, CALLER_CHAINS_SHOWN, self.total);
        }
        for (title, samples) in [
            ("SELF", &self.self_samples),
            ("INCLUSIVE", &self.inclusive_samples),
        ] {
            println!("== {title} (samples {})", self.total);
            print_top(samples, top, self.total);
        }
    }
}

fn print_top(samples: &HashMap<String, isize>, limit: usize, total: isize) {
    let mut sorted: Vec<_> = samples.iter().collect();
    sorted.sort_by(|left, right| right.1.cmp(left.1));
    for (name, count) in sorted.into_iter().take(limit) {
        println!("{:6.2}%  {}", *count as f64 * 100.0 / total as f64, name);
    }
}

/// Drops the generic arguments and hash of a demangled Rust symbol.
fn short(name: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for ch in name.chars() {
        match ch {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    let out = match out.rfind("::h") {
        Some(i) if out.len() - i == SYMBOL_HASH_SUFFIX_LENGTH => out[..i].to_string(),
        _ => out,
    };
    out.chars().take(SYMBOL_NAME_MAX_CHARS).collect()
}
