//! How the server gets memory back after load (docs/PERF.md, "Bounded
//! memory").
//!
//! - SQLite's own allocations (page caches, prepared statements, schemas)
//!   go through mimalloc, the allocator the rest of the binary uses.
//!   Bundled SQLite otherwise calls the C library's malloc; in the release
//!   binary that is musl's, which held freed page-cache memory: 1.1 GB
//!   anonymous RSS 60 s after dashboard load on a 30-day tenant.
//! - mimalloc purges freed pages after 10 ms instead of the 1 s this
//!   build's default (v3's documented default is 10 ms). At 1 s, 30
//!   dashboard clients held 690-713 MB anonymous at peak; at 10 ms
//!   408-421 MB with the same throughput. 0 (immediate) held 323-357 MB but
//!   served ~25% fewer pages. `MIMALLOC_PURGE_DELAY` still overrides it.

use std::ffi::{c_int, c_long, c_void};

// libmimalloc-sys links these (the mimalloc crate's dependency); its Rust
// declarations of them need its `extended` feature. Named here so binaries
// that do not make mimalloc their global allocator (the lib's tests) still
// link it.
use mimalloc as _;
unsafe extern "C" {
    fn mi_malloc(size: usize) -> *mut c_void;
    fn mi_free(p: *mut c_void);
    fn mi_realloc(p: *mut c_void, size: usize) -> *mut c_void;
    fn mi_usable_size(p: *const c_void) -> usize;
    fn mi_good_size(size: usize) -> usize;
    fn mi_option_set(option: c_int, value: c_long);
    fn mi_collect(force: bool);
}

/// mi_option_purge_delay in mimalloc v3's mi_option_e.
const MI_OPTION_PURGE_DELAY: c_int = 15;
const PURGE_DELAY_MS: c_long = 10;

unsafe extern "C" fn x_malloc(n: c_int) -> *mut c_void {
    // SAFETY: plain allocation; SQLite never asks for a negative size.
    unsafe { mi_malloc(n.max(0) as usize) }
}

unsafe extern "C" fn x_free(p: *mut c_void) {
    // SAFETY: p came from x_malloc/x_realloc (or is null).
    unsafe { mi_free(p) }
}

unsafe extern "C" fn x_realloc(p: *mut c_void, n: c_int) -> *mut c_void {
    // SAFETY: p came from x_malloc/x_realloc (or is null).
    unsafe { mi_realloc(p, n.max(0) as usize) }
}

unsafe extern "C" fn x_size(p: *mut c_void) -> c_int {
    // SAFETY: p came from x_malloc/x_realloc.
    unsafe { mi_usable_size(p) as c_int }
}

unsafe extern "C" fn x_roundup(n: c_int) -> c_int {
    // SAFETY: a pure size computation.
    unsafe { mi_good_size(n.max(0) as usize) as c_int }
}

unsafe extern "C" fn x_init(_: *mut c_void) -> c_int {
    rusqlite::ffi::SQLITE_OK
}

unsafe extern "C" fn x_shutdown(_: *mut c_void) {}

/// Frees what SQLite can of a quiet connection's memory: its cached
/// prepared statements and its page cache (unpinned pages), which otherwise
/// stay as large as the busiest moment left them until it closes.
pub fn release(conn: &rusqlite::Connection) {
    conn.flush_prepared_statement_cache();
    // SAFETY: the handle is live for `conn`'s lifetime and used from this
    // thread only (connections are not shared across threads here).
    unsafe { rusqlite::ffi::sqlite3_db_release_memory(conn.handle()) };
}

/// Gives back the calling thread's free allocator pages. mimalloc keeps a
/// heap per thread, and a tenant's writer thread allocates its connections'
/// page caches in its own: freed by `release`, they stayed in that heap
/// (docs/PERF.md, "Memory per tenant"). A collect from another thread does
/// not reach them, which is why one every second from a single thread did
/// nothing (docs/PERF.md, "Memory bounded by configuration", 6).
pub fn collect_thread() {
    // SAFETY: no arguments to get wrong; acts on this thread's heap.
    unsafe { mi_collect(false) }
}

/// Call first thing in main, before any connection is opened:
/// sqlite3_config is refused once SQLite is initialized (that is reported,
/// and SQLite then keeps the C library's malloc).
pub fn configure() {
    // mimalloc reads its options at process load, before main; an explicit
    // MIMALLOC_PURGE_DELAY has already been applied and is left alone.
    if std::env::var_os("MIMALLOC_PURGE_DELAY").is_none() {
        // SAFETY: an option store; safe at any time.
        unsafe { mi_option_set(MI_OPTION_PURGE_DELAY, PURGE_DELAY_MS) };
    }
    let methods = rusqlite::ffi::sqlite3_mem_methods {
        xMalloc: Some(x_malloc),
        xFree: Some(x_free),
        xRealloc: Some(x_realloc),
        xSize: Some(x_size),
        xRoundup: Some(x_roundup),
        xInit: Some(x_init),
        xShutdown: Some(x_shutdown),
        pAppData: std::ptr::null_mut(),
    };
    // SAFETY: called before sqlite3_initialize, on the main thread; SQLite
    // copies the struct.
    let rc = unsafe { rusqlite::ffi::sqlite3_config(rusqlite::ffi::SQLITE_CONFIG_MALLOC, &methods as *const rusqlite::ffi::sqlite3_mem_methods) };
    if rc != rusqlite::ffi::SQLITE_OK {
        eprintln!("railwatch: warning: SQLite allocator not installed (sqlite3_config {rc}); SQLite uses the C library's malloc");
    }
}
