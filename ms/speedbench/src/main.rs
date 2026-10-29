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
        let p = libc::malloc(bytes.len() + 1) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0;
        p as *mut c_char
    }
}

extern "C" fn loader(key: *const c_char) -> LoaderResult {
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy();
    match RULES.get().and_then(|r| r.get(key.as_ref())) {
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
    fn eval(&self, key: &CStr, ctx: &CStr, opts: Options) -> bool {
        let r = unsafe { zen_engine_evaluate(self.0, key.as_ptr(), ctx.as_ptr(), opts) };
        unsafe {
            if !r.result.is_null() {
                libc::free(r.result as *mut libc::c_void);
            }
            if !r.details.is_null() {
                libc::free(r.details as *mut libc::c_void);
            }
        }
        r.error == 0
    }
}

struct Args {
    cmd: String,
    export: String,
    only: Vec<String>,
    iters: usize,
    threads: usize,
    secs: f64,
    svg: Option<String>,
    top: usize,
    trace: bool,
    callers: Vec<String>,
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().expect("command");
    let export = it.next().expect("export file");
    let mut a = Args {
        cmd,
        export,
        only: vec![],
        iters: 3000,
        threads: 1,
        secs: 3.0,
        svg: None,
        top: 40,
        trace: false,
        callers: vec![],
    };
    while let Some(f) = it.next() {
        match f.as_str() {
            "--only" => a.only = it.next().unwrap().split(',').map(String::from).collect(),
            "--iters" => a.iters = it.next().unwrap().parse().unwrap(),
            "--threads" => a.threads = it.next().unwrap().parse().unwrap(),
            "--secs" => a.secs = it.next().unwrap().parse().unwrap(),
            "--svg" => a.svg = it.next(),
            "--top" => a.top = it.next().unwrap().parse().unwrap(),
            "--trace" => a.trace = true,
            "--callers" => a.callers = it.next().unwrap().split(',').map(String::from).collect(),
            other => panic!("unknown flag {other}"),
        }
    }
    a
}

fn main() {
    let a = parse_args();
    let raw = std::fs::read(&a.export).expect("read export");
    let v: serde_json::Value = serde_json::from_slice(&raw).expect("export json");
    let mut rules = HashMap::new();
    for (k, c) in v["rules"].as_object().unwrap() {
        rules.insert(k.clone(), serde_json::to_vec(c).unwrap());
    }
    RULES.set(rules).unwrap();
    let mut work: Vec<(CString, Vec<CString>)> = vec![];
    let mut keys: Vec<&String> = v["cases"].as_object().unwrap().keys().collect();
    keys.sort();
    for k in keys {
        if !a.only.is_empty() && !a.only.iter().any(|o| o == k) {
            continue;
        }
        let inputs = v["cases"][k]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| CString::new(serde_json::to_vec(i).unwrap()).unwrap())
            .collect();
        work.push((CString::new(k.as_str()).unwrap(), inputs));
    }
    let engine = Arc::new(Engine(unsafe { zen_engine_new_native(loader, custom) }));
    let opts = Options {
        trace: a.trace,
        max_depth: 100,
    };
    for (k, inputs) in &work {
        for i in 0..200 {
            engine.eval(k, &inputs[i % inputs.len()], opts);
        }
    }
    match a.cmd.as_str() {
        "bench" => bench(&engine, &work, &a, opts),
        "profile" => profile(&engine, &work, &a, opts),
        c => panic!("unknown command {c}"),
    }
}

fn bench(engine: &Arc<Engine>, work: &[(CString, Vec<CString>)], a: &Args, opts: Options) {
    println!(
        "{:<56} {:>9} {:>9} {:>9} {:>11}",
        "rule", "p50 us", "p99 us", "mean us", "par ops/s"
    );
    let mut pooled = vec![];
    for (k, inputs) in work {
        let mut s = Vec::with_capacity(a.iters);
        let mut ok = 0;
        let deadline = Instant::now() + Duration::from_secs_f64(a.secs);
        for i in 0..a.iters {
            let t = Instant::now();
            if engine.eval(k, &inputs[i % inputs.len()], opts) {
                ok += 1;
            }
            s.push(t.elapsed().as_nanos() as f64 / 1000.0);
            if i % 64 == 63 && Instant::now() > deadline {
                break;
            }
        }
        pooled.extend_from_slice(&s);
        let mean = s.iter().sum::<f64>() / s.len() as f64;
        s.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let par = if a.threads > 1 {
            parallel(engine, k, inputs, a.threads, 1.0, opts)
        } else {
            0.0
        };
        println!(
            "{:<56} {:>9.1} {:>9.1} {:>9.1} {:>11.0} {}",
            k.to_string_lossy(),
            s[s.len() / 2],
            s[s.len() * 99 / 100],
            mean,
            par,
            if ok == s.len() { "" } else { "(errors)" }
        );
    }
    pooled.sort_by(|x, y| x.partial_cmp(y).unwrap());
    println!(
        "pooled p50 {:.1} us over {} calls",
        pooled[pooled.len() / 2],
        pooled.len()
    );
}

fn parallel(
    engine: &Arc<Engine>,
    k: &CString,
    inputs: &[CString],
    threads: usize,
    secs: f64,
    opts: Options,
) -> f64 {
    let stop = Arc::new(AtomicBool::new(false));
    let ops = Arc::new(AtomicU64::new(0));
    let start = Instant::now();
    std::thread::scope(|sc| {
        for t in 0..threads {
            let (stop, ops, engine) = (stop.clone(), ops.clone(), engine.clone());
            sc.spawn(move || {
                let mut i = t;
                let mut n = 0;
                while !stop.load(Ordering::Relaxed) {
                    engine.eval(k, &inputs[i % inputs.len()], opts);
                    i += 1;
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / start.elapsed().as_secs_f64()
}

fn profile(engine: &Arc<Engine>, work: &[(CString, Vec<CString>)], a: &Args, opts: Options) {
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(4999)
        .build()
        .unwrap();
    let per = Duration::from_secs_f64(a.secs / work.len() as f64);
    for (k, inputs) in work {
        let end = Instant::now() + per;
        let mut i = 0;
        while Instant::now() < end {
            engine.eval(k, &inputs[i % inputs.len()], opts);
            i += 1;
        }
    }
    let report = guard.report().build().unwrap();
    if let Some(svg) = &a.svg {
        let f = std::fs::File::create(svg).unwrap();
        report.flamegraph(f).unwrap();
    }
    let mut selfc: HashMap<String, isize> = HashMap::new();
    let mut incl: HashMap<String, isize> = HashMap::new();
    let mut total = 0isize;
    let mut callers: HashMap<String, HashMap<String, isize>> = HashMap::new();
    for (frames, count) in &report.data {
        total += count;
        let names: Vec<String> = frames
            .frames
            .iter()
            .flat_map(|f| f.iter().map(|s| short(&s.name())))
            .collect();
        for target in &a.callers {
            // the chain of the 4 frames above the first frame named `target`
            if let Some(i) = names.iter().position(|n| n == target) {
                let chain = names[i + 1..]
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" < ");
                *callers
                    .entry(target.clone())
                    .or_default()
                    .entry(chain)
                    .or_default() += count;
            }
        }
        if let Some(leaf) = names.first() {
            *selfc.entry(leaf.clone()).or_default() += count;
        }
        let uniq: HashSet<&String> = names.iter().collect();
        for n in uniq {
            *incl.entry(n.clone()).or_default() += count;
        }
    }
    for (target, chains) in &callers {
        let mut v: Vec<_> = chains.iter().collect();
        v.sort_by(|x, y| y.1.cmp(x.1));
        println!("== CALLERS of {target}");
        for (chain, c) in v.into_iter().take(12) {
            println!("{:6.2}%  {}", *c as f64 * 100.0 / total as f64, chain);
        }
    }
    for (title, map) in [("SELF", &selfc), ("INCLUSIVE", &incl)] {
        let mut v: Vec<_> = map.iter().collect();
        v.sort_by(|x, y| y.1.cmp(x.1));
        println!("== {title} (samples {total})");
        for (n, c) in v.into_iter().take(a.top) {
            println!("{:6.2}%  {}", *c as f64 * 100.0 / total as f64, n);
        }
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
        Some(i) if out.len() - i == 19 => out[..i].to_string(),
        _ => out,
    };
    out.chars().take(160).collect()
}
