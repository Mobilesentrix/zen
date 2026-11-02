//! MobileSentrix: the library's allocator, and the memory it shares with its C caller.
//!
//! Rust code in libzen_ffi allocates with mimalloc. glibc malloc (upstream's allocator) keeps a
//! separate arena per thread (cgo calls arrive on many Go threads, plus the database runtime's
//! threads) and returns little of it to the OS after a burst, so a process that evaluated rules
//! with large database results stays near its peak memory long after the load ends. mimalloc
//! returns freed pages to the OS after a short delay, and is also faster for the many small
//! allocations of an evaluation.
//!
//! mimalloc purges freed memory lazily, from inside later allocator calls. After a burst the
//! threads that did the work (cgo threads, database runtime threads) sit idle, so nothing would
//! run that purge. [`start_purger`] runs one background thread that collects once a second
//! (`mi_collect(false)`: the purge of every arena range whose delay has expired, whichever thread
//! freed it, plus that thread's own heap), so idle memory returns within about a second, while
//! the evaluation path keeps mimalloc's default purge delay.
//!
//! Two further allocator defaults keep memory resident after a burst of large evaluations:
//! * mimalloc v3 madvises its arenas `MADV_HUGEPAGE` by default. With transparent huge pages it
//!   purges only whole 2 MiB ranges, and the kernel keeps a partly freed huge page charged until
//!   it splits it, so freed memory stays resident. The library builds mimalloc without THP (the
//!   crate's `no_thp` feature).
//! * glibc raises its mmap threshold (up to 32 MiB) each time a large mmapped block is freed, and
//!   the trim threshold with it. The response buffers handed to the caller (`malloc_json`, tens of
//!   megabytes) would then come from per-thread arenas that are never trimmed. [`start_purger`] fixes the
//!   threshold at 1 MiB, so every response of 1 MiB or more is its own mapping, returned to the
//!   OS by the caller's `free()` (and grown by `mremap`, without copying).
//!
//! C contract: memory this library hands to its caller is freed by the caller
//! with `free()`, and memory the caller hands over (loader / custom-node callback results) was
//! allocated with `malloc()`. So every such buffer is allocated or released here with libc's
//! `malloc` / `free`, never with the Rust global allocator.
use std::ffi::{c_char, CStr, CString};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[allow(unused_doc_comments)]
/// cbindgen:ignore
extern "C" {
    // mimalloc's C API (linked in through the mimalloc crate); not part of zen_engine.h.
    fn mi_collect(force: bool);
}

/// glibc mmap threshold: `malloc` blocks of this size or more are separate mappings.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const RESPONSE_MMAP_THRESHOLD: libc::c_int = 1 << 20;

/// Fixes glibc's mmap threshold and starts the once-a-second purge thread (idempotent; called
/// when an engine is created).
pub(crate) fn start_purger() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        // A fixed threshold (setting it turns off glibc's dynamic adjustment); see the module doc.
        // SAFETY: mallopt only changes glibc's allocation parameters.
        #[cfg(all(target_os = "linux", target_env = "gnu"))]
        unsafe {
            libc::mallopt(libc::M_MMAP_THRESHOLD, RESPONSE_MMAP_THRESHOLD);
        }
        let spawned = std::thread::Builder::new()
            .name("zen-mem-purge".into())
            .spawn(|| {
                // mi_collect is a no-op on a thread whose mimalloc heap was never initialised,
                // which happens on its first allocation: allocate once, and keep it.
                let initialised = std::hint::black_box(Box::new(0u64));
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    // SAFETY: mi_collect takes no pointers and may be called from any thread.
                    unsafe { mi_collect(false) };
                    std::hint::black_box(&initialised);
                }
            });
        // Without the thread memory is still freed, only returned to the OS later.
        drop(spawned);
    });
}

/// `bytes` plus a trailing NUL in `malloc` memory, for the caller to `free()`.
/// `bytes` must not contain a NUL (the caller reads up to the first one).
pub(crate) fn malloc_c_string(bytes: &[u8]) -> *mut c_char {
    // SAFETY: the buffer is len + 1 bytes; we write len bytes and the NUL, and hand it out.
    unsafe {
        let p = libc::malloc(bytes.len() + 1) as *mut u8;
        if p.is_null() {
            std::alloc::handle_alloc_error(
                std::alloc::Layout::array::<u8>(bytes.len() + 1).unwrap(),
            );
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        *p.add(bytes.len()) = 0;
        p as *mut c_char
    }
}

/// `value` as JSON text, NUL-terminated, in `malloc` memory for the caller to `free()`.
///
/// The text is written straight into a growing `malloc` buffer. Serialising into a `String` and
/// then copying it into `malloc` memory would hold up to three times the response size at once
/// (the string's old and doubled buffers while it grows, then the copy), on top of the
/// evaluation's trace. Here growth is `realloc`, which glibc serves for large buffers with `mremap` (the pages move,
/// nothing is copied), so the peak is the response itself. Same bytes as `serde_json::to_string`.
pub(crate) fn malloc_json<T: serde::Serialize + ?Sized>(
    value: &T,
) -> serde_json::Result<*mut c_char> {
    let mut buf = MallocBuf::with_capacity(4096);
    serde_json::to_writer(&mut buf, value)?;
    Ok(buf.into_c_string())
}

/// Bytes in `malloc` memory that grow by `realloc` (see [`malloc_json`]).
struct MallocBuf {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl MallocBuf {
    fn with_capacity(cap: usize) -> Self {
        // SAFETY: plain malloc; a null result is reported as an allocation failure.
        let ptr = unsafe { libc::malloc(cap) } as *mut u8;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(std::alloc::Layout::array::<u8>(cap).unwrap());
        }
        Self { ptr, len: 0, cap }
    }

    #[inline]
    fn reserve(&mut self, extra: usize) {
        let need = self
            .len
            .checked_add(extra)
            .expect("response size overflows usize");
        if need <= self.cap {
            return;
        }
        let cap = need.max(self.cap.saturating_mul(2));
        // SAFETY: ptr came from malloc/realloc; on failure the old block stays valid and owned.
        let ptr = unsafe { libc::realloc(self.ptr as *mut libc::c_void, cap) } as *mut u8;
        if ptr.is_null() {
            std::alloc::handle_alloc_error(std::alloc::Layout::array::<u8>(cap).unwrap());
        }
        self.ptr = ptr;
        self.cap = cap;
    }

    /// The bytes plus a trailing NUL, handed to the caller (who frees them with `free()`).
    fn into_c_string(mut self) -> *mut c_char {
        self.reserve(1);
        // SAFETY: reserve(1) left room for the NUL at len.
        unsafe { *self.ptr.add(self.len) = 0 };
        let ptr = self.ptr;
        std::mem::forget(self);
        ptr as *mut c_char
    }
}

impl std::io::Write for MallocBuf {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.reserve(bytes.len());
        // SAFETY: reserve made room for bytes.len() more bytes after len; the regions are distinct.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len())
        };
        self.len += bytes.len();
        Ok(bytes.len())
    }

    #[inline]
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.write(bytes).map(|_| ())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for MallocBuf {
    fn drop(&mut self) {
        // SAFETY: only reached when the buffer was not handed out (a serialisation error).
        unsafe { libc::free(self.ptr as *mut libc::c_void) };
    }
}

/// A `CString` moved to `malloc` memory, for the caller to `free()`.
pub(crate) fn malloc_cstring(s: CString) -> *mut c_char {
    malloc_c_string(s.as_bytes())
}

/// `value` in `malloc` memory, for the caller to `free()`.
pub(crate) fn malloc_value<T: Copy>(value: T) -> *mut T {
    // SAFETY: malloc returns memory aligned for any fundamental type; T is Copy (no drop).
    unsafe {
        let p = libc::malloc(std::mem::size_of::<T>()) as *mut T;
        if p.is_null() {
            std::alloc::handle_alloc_error(std::alloc::Layout::new::<T>());
        }
        p.write(value);
        p
    }
}

/// Takes ownership of a NUL-terminated string the caller allocated with `malloc`: copies it
/// into Rust memory and frees the original.
///
/// # Safety
/// `p` is non-null, NUL-terminated and was allocated with `malloc`; it is not used afterwards.
pub(crate) unsafe fn take_malloc_c_string(p: *mut c_char) -> CString {
    let owned = CStr::from_ptr(p).to_owned();
    libc::free(p as *mut libc::c_void);
    owned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_round_trip_through_malloc() {
        let p = malloc_c_string(b"hello");
        let back = unsafe { take_malloc_c_string(p) };
        assert_eq!(back.as_bytes(), b"hello");

        let p = malloc_cstring(CString::new("x").unwrap());
        assert_eq!(unsafe { CStr::from_ptr(p) }.to_bytes(), b"x");
        unsafe { libc::free(p as *mut libc::c_void) };

        let v = malloc_value(7i32);
        assert_eq!(unsafe { *v }, 7);
        unsafe { libc::free(v as *mut libc::c_void) };
    }

    /// malloc_json writes exactly what serde_json::to_string writes, across buffer growth
    /// (the response is far larger than the first 4 KiB) and for escapes and unicode.
    #[test]
    fn malloc_json_is_to_string() {
        let big: Vec<serde_json::Value> = (0..20_000)
            .map(|i| serde_json::json!({"id": i, "name": format!("n\"{i}\\\u{0}é"), "x": [i as f64 / 3.0, null, true]}))
            .collect();
        for value in [
            serde_json::json!(big),
            serde_json::json!({}),
            serde_json::json!(""),
        ] {
            let p = malloc_json(&value).unwrap();
            let got = unsafe { CStr::from_ptr(p) }.to_bytes().to_vec();
            unsafe { libc::free(p as *mut libc::c_void) };
            assert_eq!(got, serde_json::to_string(&value).unwrap().into_bytes());
        }
    }
}
