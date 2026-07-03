//! Crow language runtime: precise generational garbage collector + builtins.
//!
//! Compiled as a staticlib and linked into every Crow executable.
//!
//! # Object model
//!
//! Every heap object starts with a 16-byte header, and object pointers point
//! at the header:
//!
//! ```text
//!   word 0 (meta): descriptor pointer | flag bits (low 3 bits)
//!   word 1 (aux):  string -> byte length,
//!                  scalar buffer -> capacity in bytes,
//!                  ref buffer -> capacity in elements (8 bytes each),
//!                  enum -> variant tag,
//!                  struct/closure -> 0
//!   offset 16:     payload (fields / bytes / elements)
//! ```
//!
//! Descriptors (`CrowDesc`) are static data, either exported from this crate
//! (strings, buffers) or emitted by the compiler (structs, closures). They
//! tell the GC which 8-byte payload words are references.
//!
//! # GC design
//!
//! Two generations:
//! - Nursery: a contiguous block with bump allocation. A minor collection
//!   evacuates live objects into the old generation (everything that survives
//!   one minor GC is promoted) using a Cheney-style scan, then resets the
//!   bump pointer. The block is sized adaptively (512 KiB to 64 MiB) from
//!   the survival ratio observed at each minor GC; CROW_NURSERY_KB pins it.
//! - Old generation: individually allocated blocks collected by mark-sweep
//!   when the promoted volume passes a threshold.
//!
//! Precision comes from these root sources:
//! - *Stack maps*: the compiler embeds a table mapping return addresses to
//!   the frame size and SP-relative offsets of live references, registered
//!   at startup via `crow_rt_register_stackmaps`. At collection time the GC
//!   walks the native frame-pointer chain; a frame record is
//!   [saved fp, return address] on both x86-64 and arm64. When a record's
//!   return address is found in the table, the *caller* (a compiled Crow
//!   function) is suspended at that safepoint; its own frame-record address
//!   is the record's saved-fp word, its SP is that address minus the frame
//!   size recorded by Cranelift, and the live root slots are SP + offset.
//!   Deriving the caller's SP from its own record (rather than the callee's
//!   record position) keeps the walk independent of where callees place
//!   their frame records — LLVM on Linux, unlike on macOS, does not keep
//!   them at the top of the frame. The GC rewrites root slots when it moves
//!   objects; compiled code reloads spilled references after every call.
//! - `rt_roots`: addresses of runtime-internal locals that hold references
//!   across an allocation.
//! - The *remembered set*: `crow_write_ref` records old-gen fields that
//!   point into the nursery so minor GCs need not scan the old generation.
//!   These are interior pointers, which is safe because the old generation
//!   never moves.

// The `unsafe extern "C"` entry points have a single safety contract, stated
// once here rather than per function: pointer arguments must be valid Crow
// heap objects — the language has no nil, so compiled code never passes
// null. Their only callers are compiler-generated code and this runtime
// itself.
#![allow(clippy::missing_safety_doc)]

use std::cell::UnsafeCell;
use std::io::Write;

// ---------------------------------------------------------------------------
// Object model
// ---------------------------------------------------------------------------

pub const FLAG_MARK: u64 = 1;
pub const FLAG_FWD: u64 = 2;
pub const FLAG_STATIC: u64 = 4;
pub const FLAG_MASK: u64 = 7;

pub const KIND_STRUCT: u64 = 0;
pub const KIND_STRING: u64 = 1;
pub const KIND_BUF_SCALAR: u64 = 2;
pub const KIND_BUF_REF: u64 = 3;

pub const HEADER_SIZE: usize = 16;

#[repr(C)]
pub struct CrowDesc {
    pub kind: u64,
    /// Payload size in bytes (KIND_STRUCT only; other kinds derive size from aux).
    pub size: u64,
    /// Bit i set => payload word i is a reference (KIND_STRUCT only).
    pub refmap: u64,
}

#[no_mangle]
pub static crow_desc_string: CrowDesc = CrowDesc { kind: KIND_STRING, size: 0, refmap: 0 };
#[no_mangle]
pub static crow_desc_buf_scalar: CrowDesc = CrowDesc { kind: KIND_BUF_SCALAR, size: 0, refmap: 0 };
#[no_mangle]
pub static crow_desc_buf_ref: CrowDesc = CrowDesc { kind: KIND_BUF_REF, size: 0, refmap: 0 };

/// Arrays are ordinary struct-kind objects: { buf: ref, len: int, cap: int }.
/// Field offsets (16, 24, 32) are baked into compiler-emitted code.
static DESC_ARRAY: CrowDesc = CrowDesc { kind: KIND_STRUCT, size: 24, refmap: 0b001 };

const ARR_BUF: usize = HEADER_SIZE;
const ARR_LEN: usize = HEADER_SIZE + 8;
const ARR_CAP: usize = HEADER_SIZE + 16;

#[inline]
unsafe fn meta(obj: *mut u8) -> u64 {
    *(obj as *mut u64)
}

#[inline]
unsafe fn set_meta(obj: *mut u8, v: u64) {
    *(obj as *mut u64) = v;
}

#[inline]
unsafe fn aux(obj: *mut u8) -> u64 {
    *(obj as *mut u64).add(1)
}

#[inline]
unsafe fn desc_of(obj: *mut u8) -> *const CrowDesc {
    (meta(obj) & !FLAG_MASK) as *const CrowDesc
}

#[inline]
fn round8(n: usize) -> usize {
    (n + 7) & !7
}

/// Payload size in bytes for an object with the given descriptor and aux word.
unsafe fn payload_size(desc: *const CrowDesc, aux: u64) -> usize {
    match (*desc).kind {
        KIND_STRUCT => (*desc).size as usize,
        KIND_STRING | KIND_BUF_SCALAR => round8(aux as usize),
        KIND_BUF_REF => aux as usize * 8,
        k => panic_rt(&format!("corrupt object descriptor (kind {k})")),
    }
}

unsafe fn total_size(obj: *mut u8) -> usize {
    HEADER_SIZE + payload_size(desc_of(obj), aux(obj))
}

/// Invoke `f` with the address of every reference field in `obj`.
unsafe fn for_each_ref_field(obj: *mut u8, mut f: impl FnMut(*mut u64)) {
    let desc = desc_of(obj);
    match (*desc).kind {
        KIND_STRUCT => {
            let mut map = (*desc).refmap;
            let mut idx = 0usize;
            while map != 0 {
                if map & 1 != 0 {
                    f((obj.add(HEADER_SIZE) as *mut u64).add(idx));
                }
                map >>= 1;
                idx += 1;
            }
        }
        KIND_BUF_REF => {
            let n = aux(obj) as usize;
            let base = obj.add(HEADER_SIZE) as *mut u64;
            for i in 0..n {
                f(base.add(i));
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// GC state
// ---------------------------------------------------------------------------

struct Gc {
    nursery_start: *mut u8,
    nursery_end: *mut u8,
    nursery_ptr: *mut u8,
    /// Addresses of runtime-internal locals holding refs across an allocation.
    rt_roots: Vec<*mut u64>,
    /// Addresses of old-gen fields that may point into the nursery.
    remset: Vec<*mut u64>,
    /// All old-generation objects (pointers to headers).
    old: Vec<*mut u8>,
    old_bytes: usize,
    major_threshold: usize,
    /// Promoted-but-not-yet-scanned objects during a minor GC.
    scan: Vec<*mut u8>,
    mark_stack: Vec<*mut u8>,
    log: bool,
    minor_count: u64,
    major_count: u64,
    promoted_bytes: u64,
    /// Frame pointer of the entry wrapper; the stack walk stops here.
    stack_base: u64,
    /// Nursery resizing based on observed survival (off when CROW_NURSERY_KB
    /// pins the size). The streaks count consecutive qualifying minor GCs.
    nursery_adaptive: bool,
    grow_streak: u32,
    shrink_streak: u32,
    /// Stack map table, sorted by absolute return address.
    sm_pcs: Vec<u64>,
    /// (frame size below the record, start, count), parallel to `sm_pcs`;
    /// (start, count) index into `sm_slots`.
    sm_ranges: Vec<(u32, u32, u32)>,
    /// SP-relative byte offsets of live references at each safepoint.
    sm_slots: Vec<u32>,
}

struct Racy<T>(UnsafeCell<T>);
unsafe impl<T> Sync for Racy<T> {}

static GC: Racy<Gc> = Racy(UnsafeCell::new(Gc {
    nursery_start: std::ptr::null_mut(),
    nursery_end: std::ptr::null_mut(),
    nursery_ptr: std::ptr::null_mut(),
    rt_roots: Vec::new(),
    remset: Vec::new(),
    old: Vec::new(),
    old_bytes: 0,
    major_threshold: 0,
    scan: Vec::new(),
    mark_stack: Vec::new(),
    log: false,
    minor_count: 0,
    major_count: 0,
    promoted_bytes: 0,
    stack_base: 0,
    nursery_adaptive: false,
    grow_streak: 0,
    shrink_streak: 0,
    sm_pcs: Vec::new(),
    sm_ranges: Vec::new(),
    sm_slots: Vec::new(),
}));

/// The language is single-threaded and the runtime never re-enters itself
/// through compiled code, so handing out one &mut per extern entry is sound.
#[allow(clippy::mut_from_ref)]
fn gc() -> &'static mut Gc {
    unsafe { &mut *GC.0.get() }
}

// The nursery is adaptive: it starts small and cache-friendly, doubles when
// minor GCs keep observing high survival (a live working set that doesn't
// fit promotes nearly everything, as tree-building workloads showed at a
// fixed 512 KiB), and halves again after a long run of near-zero survival.
// CROW_NURSERY_KB pins the size and disables adaptation.
const NURSERY_FLOOR_KB: usize = 512;
const NURSERY_CAP_KB: usize = 64 * 1024;
/// Grow after this many consecutive full minors promoting >= 1/4 of the
/// nursery; shrink after this many promoting <= 1/50.
const GROW_STREAK: u32 = 2;
const SHRINK_STREAK: u32 = 16;
const DEFAULT_MAJOR_MB: usize = 8;

/// The current frame pointer. `inline(never)` plus forced frame pointers
/// guarantee this function has its own frame record; the returned value
/// points at [saved fp, return address] and starts the walk at our caller.
#[inline(never)]
fn current_fp() -> u64 {
    let fp: u64;
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("mov {}, x29", out(reg) fp, options(nomem, nostack));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack));
    }
    fp
}

impl Gc {
    fn in_nursery(&self, p: u64) -> bool {
        p >= self.nursery_start as u64 && p < self.nursery_end as u64
    }

    /// Walk the frame-pointer chain and collect the addresses of all live
    /// reference slots described by the registered stack maps. Frames whose
    /// return address is not in the table (runtime/libc frames, safepoints
    /// with no live references) are skipped.
    unsafe fn compiled_root_slots(&self) -> Vec<*mut u64> {
        let mut out = Vec::new();
        if self.sm_pcs.is_empty() {
            return out;
        }
        let debug = std::env::var("CROW_GC_DEBUG").is_ok();
        let mut rec = current_fp();
        while rec != 0 && rec < self.stack_base && rec & 7 == 0 {
            let ret = *((rec + 8) as *const u64);
            if let Ok(i) = self.sm_pcs.binary_search(&ret) {
                // The *caller* is a Crow function suspended at this
                // safepoint. Its frame-record address is this record's
                // saved-fp word; its SP is that minus its frame size.
                let caller_fp = *(rec as *const u64);
                let (span, start, count) = self.sm_ranges[i];
                let sp = caller_fp - span as u64;
                for j in start..start + count {
                    let off = self.sm_slots[j as usize] as u64;
                    let addr = (sp + off) as *mut u64;
                    if debug {
                        eprintln!(
                            "[walk] rec={rec:#x} ret={ret:#x} fp={caller_fp:#x} off={off} slot={:#x} val={:#x}",
                            addr as u64, *addr
                        );
                    }
                    out.push(addr);
                }
            }
            rec = *(rec as *const u64);
        }
        out
    }

    unsafe fn alloc_old(&mut self, size: usize) -> *mut u8 {
        let layout = std::alloc::Layout::from_size_align(size, 16).unwrap();
        let p = std::alloc::alloc_zeroed(layout);
        if p.is_null() {
            panic_rt("out of memory");
        }
        self.old.push(p);
        self.old_bytes += size;
        p
    }

    /// Allocate a zeroed object. May trigger collection.
    unsafe fn alloc(&mut self, desc: *const CrowDesc, aux_word: u64) -> *mut u8 {
        let total = HEADER_SIZE + round8(payload_size(desc, aux_word));
        let total = (total + 15) & !15;
        let nursery_size = self.nursery_end as usize - self.nursery_start as usize;

        let obj = if total > nursery_size / 4 {
            // Pretenure large objects directly in the old generation.
            // Collect *before* allocating: `alloc_old` registers the block
            // for sweeping, but its header is only written below — a GC
            // in between would sweep a descriptor-less object.
            if self.old_bytes + total > self.major_threshold {
                self.minor_gc(); // runs a major collection when warranted
            }
            self.alloc_old(total)
        } else {
            if (self.nursery_end as usize - self.nursery_ptr as usize) < total {
                self.minor_gc();
            }
            let p = self.nursery_ptr;
            self.nursery_ptr = p.add(total);
            std::ptr::write_bytes(p, 0, total);
            p
        };
        set_meta(obj, desc as u64);
        *(obj as *mut u64).add(1) = aux_word;
        obj
    }

    /// If *slot points into the nursery, evacuate the object to the old
    /// generation (or reuse its forwarding pointer) and update the slot.
    unsafe fn forward(&mut self, slot: *mut u64) {
        let val = *slot;
        if val == 0 || !self.in_nursery(val) {
            return;
        }
        let obj = val as *mut u8;
        let m = meta(obj);
        if m & FLAG_FWD != 0 {
            *slot = m & !FLAG_MASK;
            return;
        }
        // Round to 16 like every other old-gen allocation, so the sweep's
        // dealloc layout matches the alloc layout. The copy of the rounded
        // size is safe: nursery slots are themselves 16-byte rounded.
        let size = (total_size(obj) + 15) & !15;
        let new = self.alloc_old(size);
        std::ptr::copy_nonoverlapping(obj, new, size);
        set_meta(obj, new as u64 | FLAG_FWD);
        self.promoted_bytes += size as u64;
        self.scan.push(new);
        *slot = new as u64;
    }

    unsafe fn minor_gc(&mut self) {
        let t0 = std::time::Instant::now();
        let used_before = self.nursery_ptr as usize - self.nursery_start as usize;
        let old_before = self.old_bytes;

        // Roots: runtime-internal roots, remembered set, compiled frames.
        for i in 0..self.rt_roots.len() {
            self.forward(self.rt_roots[i]);
        }
        for i in 0..self.remset.len() {
            self.forward(self.remset[i]);
        }
        for slot in self.compiled_root_slots() {
            self.forward(slot);
        }
        // Cheney scan of promoted objects.
        while let Some(obj) = self.scan.pop() {
            let mut fields = Vec::new();
            for_each_ref_field(obj, |a| fields.push(a));
            for a in fields {
                self.forward(a);
            }
        }

        self.nursery_ptr = self.nursery_start;
        self.remset.clear();
        self.minor_count += 1;
        if self.log {
            eprintln!(
                "[crow-gc] minor #{}: {} KiB live -> promoted {} KiB in {:?}",
                self.minor_count,
                used_before / 1024,
                (self.old_bytes - old_before) / 1024,
                t0.elapsed()
            );
        }
        if self.nursery_adaptive {
            self.adapt_nursery(used_before, self.old_bytes - old_before);
        }
        if self.old_bytes > self.major_threshold {
            self.major_gc();
        }
    }

    /// Resize the nursery based on this minor GC's survival ratio. Called
    /// right after evacuation — the nursery is empty and nothing points into
    /// it, so swapping the block is safe.
    unsafe fn adapt_nursery(&mut self, allocated: usize, promoted: usize) {
        let size = self.nursery_end as usize - self.nursery_start as usize;
        // A collection of a half-empty nursery was forced (gc_collect, old-gen
        // pressure) and says nothing about the steady-state survival ratio.
        if allocated < size / 2 {
            return;
        }
        if promoted * 4 >= allocated && size < NURSERY_CAP_KB * 1024 {
            // The live working set doesn't fit: promotion is wasted copying
            // that a larger nursery avoids entirely.
            self.grow_streak += 1;
            self.shrink_streak = 0;
            if self.grow_streak >= GROW_STREAK {
                self.set_nursery_size(size * 2);
                self.grow_streak = 0;
            }
        } else if promoted * 50 <= allocated && size > NURSERY_FLOOR_KB * 1024 {
            // Almost everything dies young; a smaller nursery frees memory
            // and stays hotter in cache.
            self.shrink_streak += 1;
            self.grow_streak = 0;
            if self.shrink_streak >= SHRINK_STREAK {
                self.set_nursery_size(size / 2);
                self.shrink_streak = 0;
            }
        } else {
            self.grow_streak = 0;
            self.shrink_streak = 0;
        }
    }

    /// Replace the (empty) nursery block with one of `new_size` bytes.
    unsafe fn set_nursery_size(&mut self, new_size: usize) {
        let new_size = new_size.clamp(NURSERY_FLOOR_KB * 1024, NURSERY_CAP_KB * 1024);
        let old_size = self.nursery_end as usize - self.nursery_start as usize;
        if new_size == old_size {
            return;
        }
        debug_assert_eq!(self.nursery_ptr, self.nursery_start, "nursery must be empty");
        let old_layout = std::alloc::Layout::from_size_align(old_size, 16).unwrap();
        std::alloc::dealloc(self.nursery_start, old_layout);
        let layout = std::alloc::Layout::from_size_align(new_size, 16).unwrap();
        self.nursery_start = std::alloc::alloc(layout);
        if self.nursery_start.is_null() {
            panic_rt("out of memory");
        }
        self.nursery_end = self.nursery_start.add(new_size);
        self.nursery_ptr = self.nursery_start;
        if self.log {
            eprintln!("[crow-gc] nursery resized to {} KiB", new_size / 1024);
        }
    }

    unsafe fn mark_value(&mut self, val: u64) {
        if val == 0 {
            return;
        }
        let obj = val as *mut u8;
        let m = meta(obj);
        if m & (FLAG_STATIC | FLAG_MARK) != 0 {
            return;
        }
        set_meta(obj, m | FLAG_MARK);
        self.mark_stack.push(obj);
    }

    /// Mark-sweep over the old generation. The nursery must be empty
    /// (a minor GC always runs first).
    unsafe fn major_gc(&mut self) {
        let t0 = std::time::Instant::now();
        let before = self.old_bytes;

        for i in 0..self.rt_roots.len() {
            self.mark_value(*self.rt_roots[i]);
        }
        for slot in self.compiled_root_slots() {
            self.mark_value(*slot);
        }
        while let Some(obj) = self.mark_stack.pop() {
            let mut vals = Vec::new();
            for_each_ref_field(obj, |a| vals.push(*a));
            for v in vals {
                self.mark_value(v);
            }
        }

        let mut live_bytes = 0usize;
        let old = std::mem::take(&mut self.old);
        self.old = old
            .into_iter()
            .filter(|&obj| {
                let m = meta(obj);
                if m & FLAG_MARK != 0 {
                    set_meta(obj, m & !FLAG_MARK);
                    live_bytes += (total_size(obj) + 15) & !15;
                    true
                } else {
                    let size = (total_size(obj) + 15) & !15;
                    let layout = std::alloc::Layout::from_size_align(size, 16).unwrap();
                    std::alloc::dealloc(obj, layout);
                    false
                }
            })
            .collect();
        self.old_bytes = live_bytes;
        self.major_threshold = (live_bytes * 2).max(DEFAULT_MAJOR_MB * 1024 * 1024);
        self.major_count += 1;
        if self.log {
            eprintln!(
                "[crow-gc] major #{}: {} KiB -> {} KiB live in {:?}",
                self.major_count,
                before / 1024,
                live_bytes / 1024,
                t0.elapsed()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime entry points (the compiler ABI)
// ---------------------------------------------------------------------------

/// Stack-overflow guard. Every compiled function prologue compares SP
/// against this limit and calls `crow_panic_stack` when below it. Zero (its
/// value before `crow_rt_init` runs) disables the check.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static mut crow_stack_limit: u64 = 0;

/// Headroom above the true stack bottom when the guard fires: enough for the
/// deepest runtime path (an allocation triggering a full collection with
/// stack walking) plus printing the panic itself.
const STACK_SLACK: u64 = 256 * 1024;

/// The lowest address of the current thread's stack, queried from the OS.
fn os_stack_bottom() -> Option<u64> {
    #[cfg(target_os = "macos")]
    unsafe {
        let this = libc::pthread_self();
        let top = libc::pthread_get_stackaddr_np(this) as u64;
        let size = libc::pthread_get_stacksize_np(this) as u64;
        Some(top - size)
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut attr: libc::pthread_attr_t = std::mem::zeroed();
        if libc::pthread_getattr_np(libc::pthread_self(), &mut attr) != 0 {
            return None;
        }
        let mut addr: *mut libc::c_void = std::ptr::null_mut();
        let mut size: libc::size_t = 0;
        let rc = libc::pthread_attr_getstack(&attr, &mut addr, &mut size);
        libc::pthread_attr_destroy(&mut attr);
        if rc != 0 {
            return None;
        }
        Some(addr as u64)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Compute the guard limit: the OS stack bottom plus slack, optionally
/// raised by CROW_STACK_KB (which caps the usable stack below `stack_base`,
/// mirroring CROW_NURSERY_KB; mainly for tests).
fn stack_limit_for(stack_base: u64) -> u64 {
    let floor = match os_stack_bottom() {
        Some(bottom) => bottom + STACK_SLACK,
        // Query failed: assume the platform-default 8 MiB below main's
        // first frame. If the real stack is smaller the guard simply
        // never fires (the pre-guard behavior).
        None => stack_base.saturating_sub(8 * 1024 * 1024) + STACK_SLACK,
    };
    let capped = std::env::var("CROW_STACK_KB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kb| stack_base.saturating_sub(kb.max(64) * 1024));
    match capped {
        Some(c) => c.max(floor),
        None => floor,
    }
}

#[no_mangle]
pub extern "C" fn crow_rt_init(stack_base: u64) {
    let g = gc();
    g.stack_base = stack_base;
    unsafe { crow_stack_limit = stack_limit_for(stack_base) };
    // A pinned size (CROW_NURSERY_KB) disables adaptive resizing.
    let pinned = std::env::var("CROW_NURSERY_KB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    g.nursery_adaptive = pinned.is_none();
    let kb = pinned.unwrap_or(NURSERY_FLOOR_KB).max(16);
    let size = kb * 1024;
    let layout = std::alloc::Layout::from_size_align(size, 16).unwrap();
    unsafe {
        g.nursery_start = std::alloc::alloc(layout);
        assert!(!g.nursery_start.is_null(), "failed to allocate nursery");
        g.nursery_end = g.nursery_start.add(size);
        g.nursery_ptr = g.nursery_start;
    }
    g.major_threshold = DEFAULT_MAJOR_MB * 1024 * 1024;
    g.log = std::env::var("CROW_GC_LOG").is_ok_and(|v| v == "1");
}

#[no_mangle]
pub extern "C" fn crow_rt_exit(code: i64) -> ! {
    let _ = std::io::stdout().flush();
    let g = gc();
    if g.log {
        eprintln!(
            "[crow-gc] exit: {} minor / {} major collections, {} KiB promoted, {} KiB old live",
            g.minor_count,
            g.major_count,
            g.promoted_bytes / 1024,
            g.old_bytes / 1024
        );
    }
    std::process::exit(code as i32)
}

/// Parse and index the compiler-emitted stack map table. Layout (u64 words):
///
/// ```text
/// [0]                n = number of safepoint entries
/// [1 + 5i ..]        n entries: { function address (relocated),
///                                 return-address offset within the function,
///                                 frame size below the frame record,
///                                 start index into the slot array,
///                                 slot count }
/// [1 + 5n + j]       slot array: SP-relative byte offsets, one per word
/// ```
#[no_mangle]
pub unsafe extern "C" fn crow_rt_register_stackmaps(table: *const u64) {
    let g = gc();
    unsafe {
        let n = *table as usize;
        let mut entries = Vec::with_capacity(n);
        let mut max_slot = 0usize;
        for i in 0..n {
            let e = table.add(1 + 5 * i);
            let pc = (*e).wrapping_add(*e.add(1));
            let span = *e.add(2) as u32;
            let start = *e.add(3) as u32;
            let count = *e.add(4) as u32;
            entries.push((pc, span, start, count));
            max_slot = max_slot.max((start + count) as usize);
        }
        entries.sort_unstable_by_key(|e| e.0);
        let slots_base = table.add(1 + 5 * n);
        g.sm_slots = (0..max_slot).map(|j| *slots_base.add(j) as u32).collect();
        g.sm_pcs = entries.iter().map(|e| e.0).collect();
        g.sm_ranges = entries.iter().map(|e| (e.1, e.2, e.3)).collect();
    }
}

#[no_mangle]
pub unsafe extern "C" fn crow_alloc(desc: *const CrowDesc, aux_word: u64) -> *mut u8 {
    unsafe { gc().alloc(desc, aux_word) }
}

/// Store `val` into `*field` (a reference field of `holder`), recording the
/// old->young edge in the remembered set when needed.
#[no_mangle]
pub unsafe extern "C" fn crow_write_ref(holder: *mut u8, field: *mut u64, val: u64) {
    let g = gc();
    unsafe {
        *field = val;
        if val != 0 && g.in_nursery(val) && !g.in_nursery(holder as u64) {
            g.remset.push(field);
        }
    }
}

#[no_mangle]
pub extern "C" fn crow_gc_collect(full: u64) {
    let g = gc();
    unsafe {
        g.minor_gc();
        if full != 0 {
            g.major_gc();
        }
    }
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

unsafe fn str_bytes<'a>(s: *mut u8) -> &'a [u8] {
    std::slice::from_raw_parts(s.add(HEADER_SIZE), aux(s) as usize)
}

unsafe fn alloc_string(bytes: &[u8]) -> *mut u8 {
    let obj = gc().alloc(&crow_desc_string, bytes.len() as u64);
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), obj.add(HEADER_SIZE), bytes.len());
    obj
}

#[no_mangle]
pub unsafe extern "C" fn crow_str_concat(a: *mut u8, b: *mut u8) -> *mut u8 {
    let g = gc();
    unsafe {
        let mut ra = a as u64;
        let mut rb = b as u64;
        g.rt_roots.push(&mut ra);
        g.rt_roots.push(&mut rb);
        let len = aux(a) as usize + aux(b) as usize;
        let obj = g.alloc(&crow_desc_string, len as u64);
        let (a, b) = (ra as *mut u8, rb as *mut u8);
        std::ptr::copy_nonoverlapping(a.add(HEADER_SIZE), obj.add(HEADER_SIZE), aux(a) as usize);
        std::ptr::copy_nonoverlapping(
            b.add(HEADER_SIZE),
            obj.add(HEADER_SIZE + aux(a) as usize),
            aux(b) as usize,
        );
        g.rt_roots.pop();
        g.rt_roots.pop();
        obj
    }
}

#[no_mangle]
pub unsafe extern "C" fn crow_str_eq(a: *mut u8, b: *mut u8) -> u64 {
    unsafe { (str_bytes(a) == str_bytes(b)) as u64 }
}

#[no_mangle]
pub extern "C" fn crow_itos(v: i64) -> *mut u8 {
    unsafe { alloc_string(v.to_string().as_bytes()) }
}

#[no_mangle]
pub extern "C" fn crow_utos(v: u64) -> *mut u8 {
    unsafe { alloc_string(v.to_string().as_bytes()) }
}

#[no_mangle]
pub extern "C" fn crow_ftos(v: f64) -> *mut u8 {
    unsafe { alloc_string(format_float(v).as_bytes()) }
}

fn format_float(v: f64) -> String {
    if v == v.trunc() && v.is_finite() && v.abs() < 1e15 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// Strict decimal integer parse: an optional leading `-` followed by one or
/// more digits, matching the whole string. Anything else panics.
fn parse_int(txt: &str, line: u64) -> i64 {
    let digits = txt.strip_prefix('-').unwrap_or(txt);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        panic_rt(&format!("stoi: invalid integer \"{txt}\" at line {line}"));
    }
    match txt.parse::<i64>() {
        Ok(v) => v,
        Err(_) => panic_rt(&format!("stoi: integer \"{txt}\" out of range at line {line}")),
    }
}

/// Strict float parse: decimal or scientific notation with an optional
/// leading `-`; no leading `+`, no `inf`/`NaN` words, and the value must be
/// finite. Anything else panics.
fn parse_float(txt: &str, line: u64) -> f64 {
    let ok = !txt.is_empty()
        && txt.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
        && !txt.starts_with('+');
    let v = if ok { txt.parse::<f64>().ok() } else { None };
    match v {
        Some(v) if v.is_finite() => v,
        Some(_) => panic_rt(&format!("stof: float \"{txt}\" out of range at line {line}")),
        None => panic_rt(&format!("stof: invalid float \"{txt}\" at line {line}")),
    }
}

#[no_mangle]
pub unsafe extern "C" fn crow_stoi(s: *mut u8, line: u64) -> i64 {
    let txt = unsafe { std::str::from_utf8(str_bytes(s)).unwrap_or("") };
    parse_int(txt, line)
}

#[no_mangle]
pub unsafe extern "C" fn crow_stof(s: *mut u8, line: u64) -> f64 {
    let txt = unsafe { std::str::from_utf8(str_bytes(s)).unwrap_or("") };
    parse_float(txt, line)
}

/// `s.to_bytes()`: copy a string's bytes into a fresh `[u8]`.
#[no_mangle]
pub unsafe extern "C" fn crow_stob(s: *mut u8) -> *mut u8 {
    let g = gc();
    unsafe {
        let len = aux(s) as usize;
        let mut root = s as u64;
        g.rt_roots.push(&mut root);
        let arr = crow_array_new(1, 0, len as u64);
        g.rt_roots.pop();
        let s = root as *mut u8;
        let buf = *(arr.add(ARR_BUF) as *mut u64) as *mut u8;
        std::ptr::copy_nonoverlapping(s.add(HEADER_SIZE), buf.add(HEADER_SIZE), len);
        *(arr.add(ARR_LEN) as *mut i64) = len as i64;
        arr
    }
}

/// `bs.to_string()`: build a string from a `[u8]`. The bytes must be valid UTF-8
/// (panics otherwise), so every observable string stays valid UTF-8.
#[no_mangle]
pub unsafe extern "C" fn crow_btos(arr: *mut u8, line: u64) -> *mut u8 {
    let g = gc();
    unsafe {
        let len = *(arr.add(ARR_LEN) as *mut i64) as usize;
        let buf = *(arr.add(ARR_BUF) as *mut u64) as *mut u8;
        let bytes = std::slice::from_raw_parts(buf.add(HEADER_SIZE), len);
        if std::str::from_utf8(bytes).is_err() {
            panic_rt(&format!("btos: invalid UTF-8 at line {line}"));
        }
        let mut root = arr as u64;
        g.rt_roots.push(&mut root);
        let obj = g.alloc(&crow_desc_string, len as u64);
        g.rt_roots.pop();
        let arr = root as *mut u8;
        let buf = *(arr.add(ARR_BUF) as *mut u64) as *mut u8;
        std::ptr::copy_nonoverlapping(buf.add(HEADER_SIZE), obj.add(HEADER_SIZE), len);
        obj
    }
}

// ---------------------------------------------------------------------------
// Arrays: { buf: ref @16, len: int @24, cap: int @32 }
//
// `len` and `cap` count elements. Scalar buffers pack elements at their
// natural size (1/2/4/8 bytes, passed in by compiled code as `elem_size`);
// reference buffers always use 8-byte elements.
// ---------------------------------------------------------------------------

unsafe fn buf_desc(elem_is_ref: u64) -> *const CrowDesc {
    if elem_is_ref != 0 {
        &crow_desc_buf_ref
    } else {
        &crow_desc_buf_scalar
    }
}

/// Buffer aux word for a capacity: bytes for scalar buffers, elements for
/// reference buffers.
fn buf_aux(elem_size: u64, elem_is_ref: u64, cap: u64) -> u64 {
    if elem_is_ref != 0 {
        cap
    } else {
        cap * elem_size
    }
}

#[inline]
unsafe fn elem_read(base: *mut u8, elem_size: u64, idx: usize) -> u64 {
    let p = base.add(idx * elem_size as usize);
    match elem_size {
        1 => *p as u64,
        2 => *(p as *mut u16) as u64,
        4 => *(p as *mut u32) as u64,
        _ => *(p as *mut u64),
    }
}

#[inline]
unsafe fn elem_write(base: *mut u8, elem_size: u64, idx: usize, val: u64) {
    let p = base.add(idx * elem_size as usize);
    match elem_size {
        1 => *p = val as u8,
        2 => *(p as *mut u16) = val as u16,
        4 => *(p as *mut u32) = val as u32,
        _ => *(p as *mut u64) = val,
    }
}

#[no_mangle]
pub extern "C" fn crow_array_new(elem_size: u64, elem_is_ref: u64, cap: u64) -> *mut u8 {
    let g = gc();
    unsafe {
        let cap = cap.max(4);
        let mut arr = g.alloc(&DESC_ARRAY, 0) as u64;
        g.rt_roots.push(&mut arr);
        let buf = g.alloc(buf_desc(elem_is_ref), buf_aux(elem_size, elem_is_ref, cap));
        g.rt_roots.pop();
        let arr = arr as *mut u8;
        crow_write_ref(arr, (arr.add(ARR_BUF)) as *mut u64, buf as u64);
        *(arr.add(ARR_LEN) as *mut i64) = 0;
        *(arr.add(ARR_CAP) as *mut i64) = cap as i64;
        arr
    }
}

#[no_mangle]
pub unsafe extern "C" fn crow_array_push(arr: *mut u8, val: u64, elem_size: u64, elem_is_ref: u64) {
    let g = gc();
    unsafe {
        let len = *(arr.add(ARR_LEN) as *mut i64);
        let cap = *(arr.add(ARR_CAP) as *mut i64);
        let mut arr_root = arr as u64;
        let mut val_root = val;
        if len == cap {
            g.rt_roots.push(&mut arr_root);
            if elem_is_ref != 0 {
                g.rt_roots.push(&mut val_root);
            }
            let new_cap = (cap * 2).max(4) as u64;
            let new_buf = g.alloc(buf_desc(elem_is_ref), buf_aux(elem_size, elem_is_ref, new_cap));
            if elem_is_ref != 0 {
                g.rt_roots.pop();
            }
            g.rt_roots.pop();
            let arr = arr_root as *mut u8;
            let old_buf = *(arr.add(ARR_BUF) as *mut u64) as *mut u8;
            let old_elems = old_buf.add(HEADER_SIZE);
            let new_elems = new_buf.add(HEADER_SIZE);
            if elem_is_ref != 0 {
                // Per-element barriered copies: the new buffer may have been
                // pretenured into the old generation.
                for i in 0..len as usize {
                    crow_write_ref(
                        new_buf,
                        (new_elems as *mut u64).add(i),
                        *(old_elems as *mut u64).add(i),
                    );
                }
            } else {
                std::ptr::copy_nonoverlapping(
                    old_elems,
                    new_elems,
                    len as usize * elem_size as usize,
                );
            }
            crow_write_ref(arr, arr.add(ARR_BUF) as *mut u64, new_buf as u64);
            *(arr.add(ARR_CAP) as *mut i64) = new_cap as i64;
        }
        let arr = arr_root as *mut u8;
        let buf = *(arr.add(ARR_BUF) as *mut u64) as *mut u8;
        if elem_is_ref != 0 {
            let slot = (buf.add(HEADER_SIZE) as *mut u64).add(len as usize);
            crow_write_ref(buf, slot, val_root);
        } else {
            elem_write(buf.add(HEADER_SIZE), elem_size, len as usize, val_root);
        }
        *(arr.add(ARR_LEN) as *mut i64) = len + 1;
    }
}

/// Returns the raw element, zero-extended for narrow scalars; compiled code
/// re-extends signed kinds.
#[no_mangle]
pub unsafe extern "C" fn crow_array_pop(arr: *mut u8, elem_size: u64) -> u64 {
    unsafe {
        let len = *(arr.add(ARR_LEN) as *mut i64);
        if len == 0 {
            panic_rt("pop on empty array");
        }
        let buf = *(arr.add(ARR_BUF) as *mut u64) as *mut u8;
        let val = elem_read(buf.add(HEADER_SIZE), elem_size, len as usize - 1);
        *(arr.add(ARR_LEN) as *mut i64) = len - 1;
        val
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

fn out(s: &[u8]) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(s);
}

#[no_mangle]
pub extern "C" fn crow_print_int(v: i64) {
    out(v.to_string().as_bytes());
}

#[no_mangle]
pub extern "C" fn crow_print_uint(v: u64) {
    out(v.to_string().as_bytes());
}

#[no_mangle]
pub extern "C" fn crow_print_float(v: f64) {
    out(format_float(v).as_bytes());
}

#[no_mangle]
pub extern "C" fn crow_print_bool(v: u64) {
    out(if v != 0 { b"true" } else { b"false" });
}

#[no_mangle]
pub unsafe extern "C" fn crow_print_str(s: *mut u8) {
    unsafe { out(str_bytes(s)) };
}

#[no_mangle]
pub extern "C" fn crow_print_newline() {
    out(b"\n");
    let _ = std::io::stdout().flush();
}

// ---------------------------------------------------------------------------
// Panics
// ---------------------------------------------------------------------------

fn panic_rt(msg: &str) -> ! {
    let _ = std::io::stdout().flush();
    eprintln!("crow: runtime error: {msg}");
    std::process::exit(101)
}

#[no_mangle]
pub extern "C" fn crow_panic_bounds(idx: i64, len: i64, line: u64) -> ! {
    panic_rt(&format!("index {idx} out of bounds (len {len}) at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_panic_div(line: u64) -> ! {
    panic_rt(&format!("division by zero at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_panic_overflow(line: u64) -> ! {
    panic_rt(&format!("integer overflow at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_panic_shift(line: u64) -> ! {
    panic_rt(&format!("invalid shift amount at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_panic_cast(line: u64) -> ! {
    panic_rt(&format!("cast out of range at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_panic_unwrap(line: u64) -> ! {
    panic_rt(&format!("unwrap of None at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_assert_fail(line: u64) -> ! {
    panic_rt(&format!("assertion failed at line {line}"));
}

#[no_mangle]
pub extern "C" fn crow_panic_stack(line: u64) -> ! {
    panic_rt(&format!("stack overflow at line {line}"));
}

// ---------------------------------------------------------------------------
// Unit tests for the pure helpers. The GC itself is global, stack-scanning
// state and is exercised end-to-end by the crowc integration suite.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_float_edges() {
        // Integer-valued finite floats below 1e15 get a ".0" suffix.
        assert_eq!(format_float(0.0), "0.0");
        assert_eq!(format_float(-0.0), "-0.0");
        assert_eq!(format_float(1.0), "1.0");
        assert_eq!(format_float(-42.0), "-42.0");
        assert_eq!(format_float(999_999_999_999_999.0), "999999999999999.0");
        // At and past the threshold the value prints like an integer.
        assert_eq!(format_float(1e15), "1000000000000000");
        assert_eq!(format_float(-1e15), "-1000000000000000");
        // Everything else is Rust's shortest round-trip form.
        assert_eq!(format_float(2.5), "2.5");
        assert_eq!(format_float(0.1), "0.1");
        assert_eq!(format_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_float(1.5e-8), "0.000000015");
        assert_eq!(format_float(f64::NAN), "NaN");
        assert_eq!(format_float(f64::INFINITY), "inf");
        assert_eq!(format_float(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn buf_aux_units() {
        // Scalar buffers count bytes; reference buffers count elements.
        assert_eq!(buf_aux(1, 0, 10), 10);
        assert_eq!(buf_aux(4, 0, 10), 40);
        assert_eq!(buf_aux(8, 0, 10), 80);
        assert_eq!(buf_aux(8, 1, 10), 10);
        assert_eq!(buf_aux(1, 1, 10), 10);
    }

    #[test]
    fn elem_read_write_size_dispatch() {
        let mut buf = [0u8; 32];
        let base = buf.as_mut_ptr();
        unsafe {
            // Each width writes exactly its own lane and nothing around it.
            for (size, idx, val) in [(1u64, 3usize, 0xABu64), (2, 3, 0xBEEF), (4, 3, 0xDEADBEEF), (8, 3, u64::MAX)] {
                buf.fill(0);
                elem_write(base, size, idx, val);
                assert_eq!(elem_read(base, size, idx), val, "size {size}");
                assert_eq!(elem_read(base, size, idx - 1), 0, "size {size} lane below");
                if (idx + 1) * size as usize + size as usize <= buf.len() {
                    assert_eq!(elem_read(base, size, idx + 1), 0, "size {size} lane above");
                }
            }
            // Narrow writes truncate: only the low bytes land.
            buf.fill(0);
            elem_write(base, 1, 0, 0x1FF);
            assert_eq!(elem_read(base, 1, 0), 0xFF);
            assert_eq!(elem_read(base, 1, 1), 0);
            elem_write(base, 2, 0, 0xFFFF_1234);
            assert_eq!(elem_read(base, 2, 0), 0x1234);
        }
    }
}
