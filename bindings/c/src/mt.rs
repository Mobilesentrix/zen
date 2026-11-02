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

/// The runtime every C-API evaluation is driven on (`block_on`). Evaluations run in parallel on
/// their calling threads; its one worker drives timers and I/O and runs what they spawn.
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
