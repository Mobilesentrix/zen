use std::sync::OnceLock;
use std::thread::available_parallelism;

use tokio::runtime;
use tokio::runtime::Runtime;
use tokio_util::task::LocalPoolHandle;

pub(crate) fn worker_pool() -> LocalPoolHandle {
    static LOCAL_POOL: OnceLock<LocalPoolHandle> = OnceLock::new();
    LOCAL_POOL
        .get_or_init(|| LocalPoolHandle::new(available_parallelism().map(Into::into).unwrap_or(1)))
        .clone()
}

/// The runtime every C-API evaluation is driven on (`block_on`).
///
/// MobileSentrix: a multi-thread runtime with one worker. On a current-thread runtime only one
/// caller at a time can hold the scheduler core inside `block_on`, so concurrent evaluations
/// (cgo calls from many Go threads) would queue for it. On a multi-thread runtime `block_on`
/// polls the future on the calling thread itself, so evaluations run in parallel, while the
/// worker drives the shared timer and I/O drivers (function-node sleeps, HTTP) and runs
/// anything an evaluation spawns.
/// Returned by reference: an `Arc` clone per call would make every evaluation bump one shared
/// counter, a cache line all threads would contend for.
pub(crate) fn tokio_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("zen-ffi-runtime")
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime")
    })
}
