//! Fixed-size, portable crash breadcrumbs. All application diagnostics live under
//! the install-local FalkorDB/logs directory; no background logger or full dump.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    collections::hash_map::DefaultHasher,
    fmt::{self, Write as _},
    fs::{self, File, OpenOptions},
    hash::{Hash, Hasher},
    io::{Seek, SeekFrom, Write as _},
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        OnceLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::Mutex;

// Windows' fixed-depth stack capture needs no heap allocation and is invoked
// only after a panic or failed Rust heap allocation (never in the hot path).
#[cfg(windows)]
#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlCaptureStackBackTrace(
        frames_to_skip: u32,
        frames_to_capture: u32,
        backtrace: *mut *mut std::ffi::c_void,
        backtrace_hash: *mut u32,
    ) -> u16;
}

const SLOT_BYTES: usize = 512;
const SLOT_COUNT: u64 = 4096; // 2 MiB, permanently fixed size
const CRASH_BYTES: usize = 4096; // latest and previous: <= 8 KiB total

static DIAGNOSTICS: OnceLock<Diagnostics> = OnceLock::new();
static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_QUERY: Cell<u64> = const { Cell::new(0) };
    static CURRENT_STAGE: Cell<&'static str> = const { Cell::new("idle") };
}

struct Diagnostics {
    breadcrumbs: Mutex<File>,
    crash: Mutex<File>,
    next_slot: AtomicU64,
}

/// Allocate through the regular system allocator. Only failed Rust allocations
/// have an additional (best-effort, non-allocating) diagnostics operation.
/// Native/C allocation failures are not intercepted here.
pub struct ObservedAllocator;

// SAFETY: System handles allocations/deallocations; we preserve every layout
// and pointer unmodified. The failure reporter uses preallocated stack buffers.
unsafe impl GlobalAlloc for ObservedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            record_allocation_failure(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            record_allocation_failure(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(ptr, layout, new_size) };
        if next.is_null() {
            record_allocation_failure(new_size);
        }
        next
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
    }
}

struct FixedLine<const N: usize> {
    bytes: [u8; N],
    len: usize,
}
impl<const N: usize> FixedLine<N> {
    fn new() -> Self {
        Self { bytes: [0; N], len: 0 }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
impl<const N: usize> fmt::Write for FixedLine<N> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        let available = N.saturating_sub(self.len);
        let mut count = available.min(value.len());
        while !value.is_char_boundary(count) {
            count -= 1;
        }
        self.bytes[self.len..self.len + count].copy_from_slice(&value.as_bytes()[..count]);
        self.len += count;
        Ok(())
    }
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn fingerprint(value: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Preserve query structure, but never persist literal contents, numeric
/// values, or CYPHER parameter preambles. Capped to a small prefix.
fn shape_into(out: &mut FixedLine<SLOT_BYTES>, cypher: &str) {
    let trimmed = cypher.trim_start();
    // CYPHER options can carry quoted private values and misleading keywords.
    // Redact their shape completely rather than looking for MATCH inside text.
    if trimmed.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("CYPHER ")) {
        let _ = out.write_str("[CYPHER preamble and shape redacted]");
        return;
    }
    let input = cypher;
    let mut quoted = None;
    let mut escaped = false;
    let mut last_space = false;
    let mut emitted = 0usize;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if emitted >= 150 {
            break;
        }
        if let Some(quote) = quoted {
            if escaped { escaped = false; continue; }
            if ch == '\\' { escaped = true; continue; }
            if ch == quote {
                if chars.peek() == Some(&quote) {
                    // Cypher also accepts doubled quote escapes.
                    chars.next();
                } else {
                    quoted = None;
                }
            }
            continue;
        }
        if ch == '\'' || ch == '"' {
            quoted = Some(ch);
            let _ = out.write_str("'?'");
            emitted += 3;
            last_space = false;
        } else if ch.is_ascii_digit() {
            let _ = out.write_char('#');
            emitted += 1;
            last_space = false;
        } else if ch.is_ascii_whitespace() {
            if !last_space {
                let _ = out.write_char(' ');
                emitted += 1;
            }
            last_space = true;
        } else if ch.is_ascii_graphic() {
            let _ = out.write_char(ch);
            emitted += 1;
            last_space = false;
        } else {
            let _ = out.write_char('?');
            emitted += 1;
            last_space = false;
        }
    }
}

impl Diagnostics {
    fn event(&self, kind: &str, query_id: u64, graph: &str, cypher: Option<&str>, extra: u64) {
        let mut line = FixedLine::<SLOT_BYTES>::new();
        let slot = self.next_slot.fetch_add(1, Ordering::Relaxed) % SLOT_COUNT;
        let _ = write!(
            &mut line,
            "v1 t_ms={} event={} id={} graph_hash={:016x} extra={}",
            timestamp_ms(), kind, query_id, fingerprint(graph), extra
        );
        if let Some(query) = cypher {
            let _ = write!(&mut line, " query_hash={:016x} len={} shape=", fingerprint(query), query.len());
            shape_into(&mut line, query);
        }
        let _ = line.write_char('\n');

        // Nonblocking: diagnostics must never hold up graph query execution or
        // allocate on the OOM path. Ring slots overwrite instead of append.
        if let Some(mut file) = self.breadcrumbs.try_lock() {
            if file.seek(SeekFrom::Start(slot * SLOT_BYTES as u64)).is_ok() {
                let _ = file.write_all(&line.bytes);
            }
        }
    }

    fn crash(&self, reason: &str, query_id: u64, size: usize, location: &str) {
        let mut line = FixedLine::<CRASH_BYTES>::new();
        let _ = write!(
            &mut line,
            "v1 t_ms={} reason={} query_id={} requested_bytes={} location={}\n\
             See query-breadcrumbs.bin for recent query IDs and redacted shapes.\n",
            timestamp_ms(), reason, query_id, size, location
        );
        #[cfg(windows)]
        {
            let mut frames = [std::ptr::null_mut::<std::ffi::c_void>(); 20];
            let count = unsafe {
                RtlCaptureStackBackTrace(2, frames.len() as u32, frames.as_mut_ptr(), std::ptr::null_mut())
            } as usize;
            let _ = line.write_str("native_stack_ips=");
            for frame in frames.iter().take(count) {
                let _ = write!(&mut line, " {:p}", *frame);
            }
            let _ = line.write_char('\n');
        }
        if let Some(mut file) = self.crash.try_lock() {
            if file.seek(SeekFrom::Start(0)).is_ok() {
                // Never grow beyond 4 KiB; do not fsync on the query path.
                let _ = file.write_all(line.as_bytes());
                let _ = file.set_len(line.len as u64);
                let _ = file.flush();
            }
        }
    }
}

pub fn install(logs_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(logs_dir)
        .map_err(|err| format!("create diagnostics directory: {err}"))?;
    let dir = logs_dir.canonicalize()
        .map_err(|err| format!("resolve diagnostics directory: {err}"))?;
    // Portable root confinement is checked by server.rs before this function.
    for name in ["query-breadcrumbs.bin", "crash-latest.txt", "crash-previous.txt"] {
        let p = dir.join(name);
        if let Ok(metadata) = fs::symlink_metadata(&p) {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!("refusing non-regular diagnostics file {}", p.display()));
            }
        }
    }
    let breadcrumbs = OpenOptions::new().read(true).write(true).create(true)
        .open(dir.join("query-breadcrumbs.bin"))
        .map_err(|err| format!("open query breadcrumbs: {err}"))?;
    breadcrumbs.set_len(SLOT_BYTES as u64 * SLOT_COUNT)
        .map_err(|err| format!("size query breadcrumbs: {err}"))?;

    // Keep exactly the most recent two small crash reports; previous remains
    // readable across restarts until another actual crash is recorded.
    let latest = dir.join("crash-latest.txt");
    let previous = dir.join("crash-previous.txt");
    if fs::metadata(&latest).is_ok_and(|m| m.len() > 0) {
        // Enforce the bound even if a pre-existing report came from an older build.
        OpenOptions::new().write(true).open(&latest)
            .and_then(|file| file.set_len(CRASH_BYTES as u64))
            .map_err(|err| format!("bound existing crash report: {err}"))?;
        if previous.exists() {
            fs::remove_file(&previous).map_err(|err| format!("rotate crash report: {err}"))?;
        }
        fs::rename(&latest, &previous)
            .map_err(|err| format!("rotate latest crash report: {err}"))?;
    }
    if previous.exists() {
        OpenOptions::new().write(true).open(&previous)
            .and_then(|file| {
                let len = file.metadata()?.len();
                if len > CRASH_BYTES as u64 { file.set_len(CRASH_BYTES as u64)?; }
                Ok(())
            }).map_err(|err| format!("bound previous crash report: {err}"))?;
    }
    let crash = OpenOptions::new().write(true).create(true).truncate(true)
        .open(&latest).map_err(|err| format!("open crash report: {err}"))?;

    DIAGNOSTICS.set(Diagnostics {
        breadcrumbs: Mutex::new(breadcrumbs),
        crash: Mutex::new(crash),
        next_slot: AtomicU64::new(0),
    }).map_err(|_| "diagnostics already initialized".to_string())?;

    std::panic::set_hook(Box::new(|info| {
        let qid = CURRENT_QUERY.try_with(Cell::get).unwrap_or(0);
        let location = info.location();
        let mut loc = FixedLine::<256>::new();
        if let Some(location) = location {
            let _ = write!(&mut loc, "{}:{}:{}", location.file(), location.line(), location.column());
        } else {
            let _ = loc.write_str("unknown");
        }
        if let Some(diag) = DIAGNOSTICS.get() {
            diag.event("PANIC", qid, "", None, 0);
            diag.crash("rust_panic", qid, 0, std::str::from_utf8(loc.as_bytes()).unwrap_or("unknown"));
        }
        eprintln!("FalkorDB panicked (query_id={qid}); see FalkorDB/logs/crash-latest.txt");
    }));
    if let Some(diag) = DIAGNOSTICS.get() {
        diag.event("STARTUP", 0, "", None, std::process::id() as u64);
    }
    Ok(())
}

pub struct QueryBreadcrumb {
    id: u64,
    previous_id: u64,
    previous_stage: &'static str,
    started: Instant,
    completed: bool,
}

/// Store a precise engine phase in thread-local memory without doing any I/O.
/// On an allocation failure, the phase is emitted with the failing allocation.
pub fn mark_stage(stage: &'static str) {
    let _ = CURRENT_STAGE.try_with(|slot| slot.set(stage));
}
pub fn start_query(graph: &str, cypher: &str, operation: &str) -> QueryBreadcrumb {
    let id = NEXT_QUERY.fetch_add(1, Ordering::Relaxed);
    let previous_id = CURRENT_QUERY.with(|id_slot| id_slot.replace(id));
    let previous_stage = CURRENT_STAGE.with(|slot| slot.replace("planning"));
    if let Some(diag) = DIAGNOSTICS.get() {
        diag.event(operation, id, graph, Some(cypher), previous_id);
    }
    QueryBreadcrumb { id, previous_id, previous_stage, started: Instant::now(), completed: false }
}
impl QueryBreadcrumb {
    pub fn complete(&mut self, ok: bool) {
        self.completed = true;
        if let Some(diag) = DIAGNOSTICS.get() {
            diag.event(if ok { "DONE" } else { "ERROR" }, self.id, "", None,
                self.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    }
}
impl Drop for QueryBreadcrumb {
    fn drop(&mut self) {
        if !self.completed {
            if let Some(diag) = DIAGNOSTICS.get() {
                diag.event("EARLY_EXIT", self.id, "", None,
                    self.started.elapsed().as_millis().min(u64::MAX as u128) as u64);
            }
        }
        let _ = CURRENT_QUERY.try_with(|slot| slot.set(self.previous_id));
        let _ = CURRENT_STAGE.try_with(|slot| slot.set(self.previous_stage));
    }
}

fn record_allocation_failure(size: usize) {
    let id = CURRENT_QUERY.try_with(Cell::get).unwrap_or(0);
    let stage = CURRENT_STAGE.try_with(Cell::get).unwrap_or("unknown");
    if let Some(diag) = DIAGNOSTICS.get() {
        diag.event("ALLOC_FAIL", id, "", None, size as u64);
        diag.crash("rust_allocation_failed", id, size, stage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_query_shape_excludes_literals_and_numbers() {
        let mut line = FixedLine::<SLOT_BYTES>::new();
        shape_into(&mut line, "MATCH (n {secret:'private', id:12345}) RETURN n");
        let out = std::str::from_utf8(line.as_bytes()).unwrap();
        assert!(!out.contains("private"));
        assert!(!out.contains("12345"));
        assert!(out.contains("MATCH"));
    }
    #[test]
    fn nested_query_scope_restores_parent() {
        let mut outer = start_query("test", "RETURN 1", "READ");
        let outer_id = CURRENT_QUERY.with(Cell::get);
        assert!(outer_id > 0);
        {
            let mut inner = start_query("test", "RETURN 2", "READ");
            assert_ne!(CURRENT_QUERY.with(Cell::get), outer_id);
            mark_stage("capture_read");
            inner.complete(true);
        }
        assert_eq!(CURRENT_QUERY.with(Cell::get), outer_id);
        outer.complete(true);
        drop(outer);
        assert_eq!(CURRENT_QUERY.with(Cell::get), 0);
        assert_eq!(CURRENT_STAGE.with(Cell::get), "idle");
    }

    #[test]
    fn redacts_cypher_params_and_doubled_quotes() {
        let mut line = FixedLine::<SLOT_BYTES>::new();
        shape_into(&mut line, "CYPHER secret='token123' MATCH (n {value:'private''hidden'}) RETURN n");
        let out = std::str::from_utf8(line.as_bytes()).unwrap();
        assert!(out.contains("preamble and shape redacted"));
        assert!(!out.contains("token123"));
        assert!(!out.contains("private"));
        assert!(!out.contains("hidden"));
    }

    #[test]
    fn quoted_cypher_option_cannot_smuggle_query_keywords() {
        let mut line = FixedLine::<SLOT_BYTES>::new();
        shape_into(&mut line, "CYPHER credential='pfx MATCH sensitive_suffix' MATCH (n) RETURN n");
        let text = std::str::from_utf8(line.as_bytes()).unwrap();
        assert!(!text.contains("credential"));
        assert!(!text.contains("sensitive_suffix"));
        assert!(text.contains("redacted"));
    }

    #[test]
    fn line_does_not_split_unicode_or_exceed_bound() {
        let mut line = FixedLine::<5>::new();
        write!(&mut line, "ééé").unwrap();
        assert!(line.len <= 5);
        assert!(std::str::from_utf8(line.as_bytes()).is_ok());
    }
}
