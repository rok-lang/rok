//! Garbage collector: mark-region (Immix-style blocks and
//! lines), generational, with opportunistic evacuation and concurrent
//! marking ([`concurrent`]). Precise in the heap and roots; ambiguous
//! words pin what they reach.
//!
//! Objects below [`LOS_THRESHOLD`] live in 32 KiB blocks of 128-byte
//! lines. Each thread bump-allocates from its TLAB; compiled
//! code inlines the bump and calls [`alloc`] only on refill,
//! which takes the world lock and zeroes the lines it hands out. Large
//! objects are one allocation each, linked through a hidden
//! [`Prefix`].
//!
//! Every `gc_word` holds the object's size (its new address
//! while evacuating) and block objects have a nonzero `type_id`, so a
//! block is parsable: non-object memory is zero. Per-block object-start
//! bitmaps are filled lazily, so an unreachable young block is freed
//! without being read.
//!
//! Generations use sticky mark bits. A minor collection
//! marks from the roots and the remembered set and sweeps only lines
//! handed out since the last collection. The write barrier tests
//! `UNLOGGED` on an old object; the slow path ([`write_barrier_slow`])
//! records it. An old LOS object is `CARDED` instead: the barrier dirties
//! a 512-byte card and a minor collection scans only dirty cards. A full
//! collection runs when old bytes exceed `max(4 MiB, 2 x live)`; past
//! [`CONCURRENT_MIN`] it marks concurrently (no evacuation then).
//!
//! Evacuation (full stop-the-world collections only): when a quarter of
//! used lines are free, blocks at most half occupied are candidates
//! ([`select_candidates`]); survivors move into holes elsewhere and one
//! pass rewrites every precise reference ([`fix_references`]). An object
//! reached by a root or an ambiguous word, pinned, or `NOMOVE` stays put:
//! compiled code and runtime calls derive raw pointers from rooted values.
//! Free pages beyond the next cycle's budget go back to the OS. Block
//! objects are remembered whole; sweep and evacuation run in pauses.
//!
//! Roots: compiled code links a frame
//! `{ prev, count, slots[count] }` into `rok_tls_shadow_stack_top` before
//! the first safepoint of each path and unlinks it on every return of a
//! path that linked it; each slot is the address of a word holding a GC
//! reference (0 = none).
//!
//! Children: [`register_type`] maps `type_id` to a `ScanTable`
//! ([`for_each_ref`]): `STRONG_REF` / `INTERIOR_REF` words, `ANY_SLOT`
//! (a reference iff the tag has [`ANY_BOXED`]), `SUM_PAYLOAD` (the tag
//! selects the variant, [`visit_sum`]), `AMBIGUOUS` and `WEAK_REF`
//! ([`clear_dead_weak`]). `TABLE_ARRAY` describes one element repeated
//! over a buffer. Type id 2 is all ambiguous words, 3 a weak cell
//! ([`weak_new`]). A word outside every object is ignored; an ambiguous
//! one only retains and pins the object it hits.
//!
//! Debug knobs (read once): `ROKR_GC_STRESS=N` (a collection every `N`
//! runtime allocations, every 8th full), `ROKR_GC_EVAC=always`,
//! `ROKR_GC_CONCURRENT=always|off`, `ROKR_GC_STATS=1`, `ROKR_GC_LOG=1`
//! ([`Pause`]), `ROKR_HEAP_DUMP=on_oom` ([`heap_dump`]). Other values warn
//! ([`rokr_common::env_knob`]).

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::fmt::Write as _;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

use rokr_common::object_header::{
    FLAG_CARDED, FLAG_FORWARDED, FLAG_LARGE, FLAG_MARK, FLAG_NOEVAC, FLAG_NOMOVE, FLAG_PINNED,
    FLAG_SATB, FLAG_SCANNED, FLAG_STICKY, FLAG_UNLOGGED, FLAG_WIPE, HEADER_ALIGN, HEADER_SIZE,
    ObjectHeader,
};
use rokr_common::{env_choice, env_flag, env_num, fatal, sys};

mod concurrent;
pub mod world;

use rokr_common::gc_abi::{
    AMBIGUOUS, ANY_SLOT, INTERIOR_REF, KIND_MASK, MIN_OBJECT_SIZE, PLAIN_TYPE_ID, STRONG_REF,
    SUM_DESC_SHIFT, SUM_PAYLOAD, USER_TYPE_BASE, WEAK_REF, WEAK_TYPE_ID,
};
pub use rokr_common::gc_abi::{ANY_BOXED, SCAN_WORDS_TYPE_ID};

/// The `ScanTable` of a weak cell ([`WEAK_TYPE_ID`]): one `WEAK_REF`
/// word after the header.
static WEAK_TABLE: [u32; 4] = [24, 1 | (1 << 16), 16, WEAK_REF];

/// Heap size below which no collection runs.
const MIN_HEAP: usize = 4 << 20;
const TABLE_ARRAY: u16 = rokr_common::gc_abi::TABLE_ARRAY as u16;

/// Immix block / line geometry.
pub const BLOCK_SIZE: usize = 32 << 10;
pub const LINE_SIZE: usize = 128;
const LINES: usize = BLOCK_SIZE / LINE_SIZE;
const GRANULES: usize = BLOCK_SIZE / 8;
/// Objects this large (header included) go to the LOS.
pub const LOS_THRESHOLD: usize = rokr_common::gc_abi::LOS_THRESHOLD as usize;
/// Card size of an old LOS object (`FLAG_CARDED`): a reference store
/// dirties the card(s) of its slot and a minor collection scans only
/// dirty cards, not the whole object.
pub const CARD_SIZE: usize = 1 << rokr_common::gc_abi::CARD_SHIFT;
/// Address space reserved for blocks (committed one block at a time):
/// one reservation, the first of these sizes the OS grants. 64-bit: the
/// block heap is capped at 64 GiB. 32-bit (a heap of about
/// 3.5 GB): at most 2 GiB, the rest of the address space left to LOS
/// objects, `malloc` and the program. wasm32 reserves nothing: see
/// [`grow_blocks`].
#[cfg(target_pointer_width = "64")]
const RESERVE: usize = 64 << 30;
#[cfg(all(target_pointer_width = "32", not(target_family = "wasm")))]
const RESERVE: usize = 2 << 30;
#[cfg(not(target_family = "wasm"))]
const RESERVE_MIN: usize = 64 << 20;

/// Line state of a wasm32 block that `malloc` took (see [`grow_blocks`]):
/// never handed out, swept or moved.
#[cfg(target_family = "wasm")]
const FOREIGN: u8 = 4;

/// Line states.
const FREE: u8 = 0;
/// Holds (part of) an object that survived the last collection.
const LIVE: u8 = 1;
/// Handed out (TLAB or medium object) since the last collection.
const TAKEN: u8 = 2;
/// Handed out while a concurrent mark runs: its objects are younger than
/// the mark, which neither reads nor sweeps them (allocate-black, line
/// granular); `TAKEN` once the mark is done.
const BLACK: u8 = 3;

/// The state of line `l` of the block whose metadata is at `m`. Atomic:
/// a concurrent mark reads lines that refills write.
fn line(m: *const BlockMeta, l: usize) -> u8 {
    // SAFETY: `m` is a block's metadata, `l < LINES`.
    unsafe { core::sync::atomic::AtomicU8::from_ptr((&raw const (*m).lines[l]).cast_mut()) }
        .load(Ordering::Relaxed)
}

fn set_line(m: *mut BlockMeta, l: usize, state: u8) {
    // SAFETY: as in `line`.
    unsafe { core::sync::atomic::AtomicU8::from_ptr(&raw mut (*m).lines[l]) }
        .store(state, Ordering::Relaxed);
}

/// Granules (start bits) per line.
const LINE_GRANULES: usize = LINE_SIZE / 8;

/// Side metadata of one block.
#[allow(clippy::struct_excessive_bools)] // independent per-block states
struct BlockMeta {
    lines: [u8; LINES],
    /// Object starts (one bit per 8-byte granule): `TAKEN` lines are
    /// parsed in by every collection, dead objects cleared by its sweep.
    starts: [u64; GRANULES / 64],
    /// Holds objects or was handed out since the last sweep (else it
    /// is all zero and all `FREE`).
    used: bool,
    /// This collection parsed the `TAKEN` lines (on the first lookup
    /// that landed in one): a block nobody looked into this time holds
    /// no survivor there, so its `TAKEN` lines just become free.
    parsed: bool,
    /// An evacuation candidate of the running full collection.
    evac: bool,
    /// Free, and its pages were given back to the OS (they fault in
    /// zeroed when the block is used again).
    released: bool,
}

/// Hidden bookkeeping right before each LOS object header.
#[repr(C)]
struct Prefix {
    /// Header of the next LOS object.
    next: *mut u8,
    /// Total object size, header included.
    size: usize,
    /// Bytes from the allocation base to the header.
    offset: usize,
    /// Card table (one byte per [`CARD_SIZE`] of the object, after its
    /// end in the same allocation). Last field: compiled code's
    /// barrier loads it from the word before the header.
    cards: *mut u8,
}

#[allow(clippy::struct_excessive_bools)] // independent states
struct Heap {
    /// Bytes of objects alive after the last collection plus everything
    /// handed out since (TLABs count in full).
    live_bytes: usize,
    next_gc: usize,
    collections: u64,
    /// Bytes alive after the last collection (all old).
    old_bytes: usize,
    /// The next automatic collection is full once
    /// `old_bytes` reaches this.
    full_at: usize,
    minors: u64,
    /// Total / longest pause (`ROKR_GC_STATS`).
    gc_ns: u64,
    max_ns: u64,
    /// Collector-thread CPU time in pauses (robust on a loaded machine).
    gc_cpu_ns: u64,
    /// Pauses of full collections (both pauses of a concurrent one):
    /// count, total, longest.
    full_pauses: u64,
    full_ns: u64,
    full_max_ns: u64,
    /// Their collector-thread CPU time: total, longest.
    full_cpu_ns: u64,
    full_cpu_max_ns: u64,
    /// `ROKR_GC_*` read (first slow-path allocation or collection).
    configured: bool,
    /// `ROKR_GC_LOG`: a line per collection to stderr.
    log: bool,
    /// `ROKR_GC_STRESS`: collect every `stress` runtime allocations (0 =
    /// off); `allocs` counts them.
    stress: u64,
    allocs: u64,
    /// Block `i` is at `base + i * BLOCK_SIZE`; 0 until the first one.
    /// `meta` is reserved for every block of the reservation, so it never
    /// moves.
    base: usize,
    nblocks: usize,
    meta: *mut BlockMeta,
    /// Bytes reserved at `base`.
    reserved: usize,
    /// wasm32: the end of the linear memory the heap grew for its blocks.
    #[cfg(target_family = "wasm")]
    grown: usize,
    /// Next line (global index `block * LINES + line`) the small / medium
    /// refill looks at; both restart at 0 after every sweep.
    small_cursor: usize,
    medium_cursor: usize,
    /// LOS objects, linked through their prefixes (newest first).
    los: *mut u8,
    los_count: usize,
    /// `ROKR_GC_EVAC=always`: every full collection evacuates every block
    /// it may (tests); otherwise only when fragmented.
    evac_always: bool,
    /// `ROKR_GC_CONCURRENT`: whether full collections mark concurrently.
    concurrent: Concurrent,
    /// A concurrent mark runs: refills hand out `BLACK` lines.
    marking: bool,
    /// The LOS list head when the running concurrent mark started (the
    /// objects in front of it are younger than the mark).
    los_mark_head: *mut u8,
    /// `live_bytes` when the running concurrent mark started.
    mark_start_live: usize,
    /// A concurrent mark found the heap fragmented: the next full
    /// collection stops the world and evacuates.
    stw_next: bool,
    /// `ROKR_GC_STATS`: concurrent marks, and those the allocation budget
    /// finished before the marker did.
    cycles: u64,
    forced: u64,
    /// Copy allocator of an evacuation: bump `copy_cursor..copy_limit`
    /// (free lines of a non-candidate block); `copy_line` is the next
    /// global line index it looks at.
    copy_cursor: usize,
    copy_limit: usize,
    copy_line: usize,
    /// 0: holes of used blocks, 1: free blocks, 2: fresh blocks.
    copy_phase: u8,
    /// Fresh blocks the copy allocator may still commit (unbounded for
    /// `ROKR_GC_EVAC=always`).
    fresh_blocks: usize,
    /// `ROKR_GC_STATS`: evacuating collections, bytes moved.
    evacuations: u64,
    moved_bytes: u64,
}

/// `ROKR_GC_CONCURRENT`: `auto` marks concurrently once the old heap
/// reaches [`CONCURRENT_MIN`]; `always` / `off` are for tests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Concurrent {
    Auto,
    Always,
    Off,
}

/// Old bytes from which `ROKR_GC_CONCURRENT=auto` marks concurrently.
const CONCURRENT_MIN: usize = 32 << 20;

struct Global<T>(UnsafeCell<T>);
// SAFETY: `HEAP` is only touched with the world lock held, `MARK` with
// the mark lock held (see their accessors).
unsafe impl<T> Sync for Global<T> {}

static HEAP: Global<Heap> = Global(UnsafeCell::new(Heap {
    live_bytes: 0,
    next_gc: MIN_HEAP,
    collections: 0,
    old_bytes: 0,
    full_at: MIN_HEAP,
    minors: 0,
    gc_ns: 0,
    max_ns: 0,
    gc_cpu_ns: 0,
    full_pauses: 0,
    full_ns: 0,
    full_max_ns: 0,
    full_cpu_ns: 0,
    full_cpu_max_ns: 0,
    configured: false,
    log: false,
    stress: 0,
    allocs: 0,
    base: 0,
    nblocks: 0,
    reserved: 0,
    #[cfg(target_family = "wasm")]
    grown: 0,
    meta: ptr::null_mut(),
    small_cursor: 0,
    medium_cursor: 0,
    los: ptr::null_mut(),
    los_count: 0,
    evac_always: false,
    concurrent: Concurrent::Auto,
    marking: false,
    los_mark_head: ptr::null_mut(),
    mark_start_live: 0,
    stw_next: false,
    cycles: 0,
    forced: 0,
    copy_cursor: 0,
    copy_limit: 0,
    copy_line: 0,
    copy_phase: 0,
    fresh_blocks: 0,
    evacuations: 0,
    moved_bytes: 0,
}));

fn heap() -> &'static mut Heap {
    // SAFETY: callers hold the world lock; no caller keeps the reference
    // across another `heap()` call.
    unsafe { &mut *HEAP.0.get() }
}

/// Marking state: the collector's while the world is stopped, the
/// marker thread's while a concurrent mark runs. Guarded by
/// [`concurrent::MARK_LOCK`]. Marking never calls [`heap`]: mutators
/// may hold it meanwhile.
struct Mark {
    /// Mark stack (grown with `realloc`), `len` entries.
    stack: *mut *mut u8,
    stack_cap: usize,
    len: usize,
    /// The blocks lookups land in: `nblocks` from `base` as of this
    /// collection's start (a concurrent mark ignores later ones).
    base: usize,
    nblocks: usize,
    meta: *mut BlockMeta,
    reserved: usize,
    /// Sorted LOS addresses as of this collection's start.
    index: *mut usize,
    index_cap: usize,
    los_count: usize,
    /// A full collection (a minor one marks young objects only).
    full: bool,
    /// A concurrent mark: lines may turn `BLACK` meanwhile, scanned
    /// objects get `SCANNED`.
    concurrent: bool,
    /// This (full) collection may evacuate: the mark records `pinned`.
    evacuating: bool,
    /// Block objects a root or an ambiguous word reaches (repeats
    /// allowed): an evacuation leaves them in place.
    pinned: *mut usize,
    pinned_len: usize,
    pinned_cap: usize,
    /// Addresses of the `WEAK_REF` words of the scanned objects.
    weak: *mut usize,
    weak_len: usize,
    weak_cap: usize,
    /// Objects marked by this collection (`ROKR_GC_LOG`).
    marked: usize,
}

/// Marker thread CPU time (`ROKR_GC_STATS`).
static MARKER_CPU_NS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

static MARK: Global<Mark> = Global(UnsafeCell::new(Mark {
    stack: ptr::null_mut(),
    stack_cap: 0,
    len: 0,
    base: 0,
    nblocks: 0,
    reserved: 0,
    meta: ptr::null_mut(),
    index: ptr::null_mut(),
    index_cap: 0,
    los_count: 0,
    full: false,
    concurrent: false,
    evacuating: false,
    pinned: ptr::null_mut(),
    pinned_len: 0,
    pinned_cap: 0,
    weak: ptr::null_mut(),
    weak_len: 0,
    weak_cap: 0,
    marked: 0,
}));

fn mark() -> &'static mut Mark {
    // SAFETY: callers hold the mark lock; no caller keeps the reference
    // across another `mark()` call.
    unsafe { &mut *MARK.0.get() }
}

const fn prefix(obj: *mut u8) -> *mut Prefix {
    obj.wrapping_sub(size_of::<Prefix>()).cast()
}

const fn header(obj: *mut u8) -> *mut ObjectHeader {
    obj.cast()
}

/// Size of the object at `obj` (its `gc_word`; not of an evacuated
/// one, see [`moved_size`]).
fn obj_size(obj: *mut u8) -> usize {
    // SAFETY: callers pass object headers (or zeroed block memory, which
    // reads as 0).
    unsafe { (*header(obj)).gc_word as usize }
}

/// Bytes after the header of the live object `obj`.
///
/// # Safety
/// `obj` must be the start of a live GC object.
pub unsafe fn payload_bytes(obj: *mut u8) -> usize {
    obj_size(obj) - HEADER_SIZE
}

/// [`obj_size`] that also reads an evacuated object's (its copy's).
fn moved_size(obj: *mut u8) -> usize {
    // SAFETY: an object header; a forwarded one's `gc_word` is its copy.
    unsafe {
        let h = &*header(obj);
        if h.gc_flags.load(Ordering::Relaxed) & FLAG_FORWARDED != 0 {
            return obj_size(h.gc_word as *mut u8);
        }
        h.gc_word as usize
    }
}

/// Write a fresh header with `size` in `gc_word` into zeroed memory.
fn init_header(obj: *mut u8, type_id: u32, size: usize, flags: u32) {
    // SAFETY: `obj` is 8-aligned memory of at least `size` bytes we own.
    unsafe {
        ObjectHeader::init(obj, type_id);
        (*header(obj)).gc_word = size as u64;
        (*header(obj)).gc_flags.store(flags, Ordering::Relaxed);
    }
}

/// Allocate a zeroed GC object of `size` bytes (header included) and
/// initialize its header (`rok_gc_alloc_slow`). May collect first.
/// Never null.
pub fn alloc(size: usize, align: usize, type_id: u32) -> *mut u8 {
    if size < HEADER_SIZE {
        fatal(b"rok_gc_alloc_slow: size smaller than the object header");
    }
    if !align.is_power_of_two() {
        fatal(b"rok runtime: allocation align is not a power of two");
    }
    // Compiled code asks for `usize::MAX` when `len * size` overflows.
    if size > isize::MAX as usize {
        fatal(b"rok runtime: allocation too large");
    }
    let size = size.max(rokr_common::gc_abi::MIN_OBJECT_SIZE as usize);
    let size8 = size.next_multiple_of(8);
    // Type id 0 (invalid) would read as free memory in a
    // block.
    if size8 >= LOS_THRESHOLD || align > HEADER_ALIGN || type_id == 0 {
        return alloc_large(size, align, type_id, 0);
    }
    let [cursor, limit] = rokr_common::tls::tlab();
    // SAFETY: this thread's own TLAB slots; `cursor..limit` is zeroed
    // block memory nobody else allocates from.
    unsafe {
        let c = cursor.read();
        if c != 0 && size8 <= limit.read() - c {
            cursor.write(c + size8);
            let obj = c as *mut u8;
            init_header(obj, type_id, size8, 0);
            return obj;
        }
    }
    world::lock();
    before_alloc(if size8 > LINE_SIZE { size8 } else { LINE_SIZE });
    // `obj..lines_end`: the lines just taken, zeroed below (free lines
    // keep their dead objects' bytes until reused).
    let (obj, lines_end) = if size8 > LINE_SIZE {
        // A medium object that does not fit the TLAB: its own lines, so
        // the TLAB's tail stays usable.
        let lines = size8.div_ceil(LINE_SIZE);
        let obj = take_run(lines, true);
        heap().live_bytes += size8;
        (obj, obj as usize + lines * LINE_SIZE)
    } else {
        let start = take_run(1, false);
        let line_end = (start as usize + 1).next_multiple_of(LINE_SIZE);
        let (end, lines_end) = if heap().stress == 0 {
            let end = end_of_run(start as usize, line_end);
            (end, end)
        } else {
            // Stress: a TLAB of exactly this object, so the next
            // allocation comes back here too.
            (start as usize + size8, line_end)
        };
        heap().live_bytes += end - start as usize;
        // SAFETY: this thread's own TLAB slots.
        unsafe {
            cursor.write(start as usize + size8);
            limit.write(end);
        }
        (start, lines_end)
    };
    world::unlock();
    // SAFETY: the lines are `TAKEN` for us; outside the lock, which no
    // collection can need before this thread's next safepoint.
    unsafe { ptr::write_bytes(obj, 0, lines_end - obj as usize) };
    init_header(obj, type_id, size8, 0);
    obj
}

/// With the lock held, before handing out `want` more bytes: collect
/// if the heap outgrew its budget or `ROKR_GC_STRESS` says so.
fn before_alloc(want: usize) {
    let h = heap();
    configure(h);
    h.allocs += 1;
    let stressed = h.stress != 0 && h.allocs % h.stress == 0;
    // A concurrent mark gets twice the young budget before the
    // allocation finishes it in a pause.
    let budget = if h.marking {
        2 * h.next_gc - h.mark_start_live.min(h.next_gc)
    } else {
        h.next_gc
    };
    if stressed || h.live_bytes.saturating_add(want) > budget {
        let full = h.old_bytes >= h.full_at || (stressed && (h.allocs / h.stress) % 8 == 0);
        collect_locked(full, true);
    }
}

/// Read the `ROKR_GC_*` knobs (old spelling `ROKR_GC_*`) once, before the
/// first collection.
fn configure(h: &mut Heap) {
    if h.configured {
        return;
    }
    h.configured = true;
    h.stress = env_num(c"ROKR_GC_STRESS", false).unwrap_or(0);
    h.evac_always = env_choice(c"ROKR_GC_EVAC", "always").is_some();
    h.concurrent = match env_choice(c"ROKR_GC_CONCURRENT", "auto|always|off") {
        Some("always") => Concurrent::Always,
        Some("off") => Concurrent::Off,
        _ => Concurrent::Auto,
    };
    // One thread: no marker thread to run beside the mutator.
    if cfg!(rok_solo) {
        h.concurrent = Concurrent::Off;
    }
    if env_flag(c"ROKR_GC_STATS") {
        sys::atexit(print_stats);
    }
    h.log = env_flag(c"ROKR_GC_LOG");
    if env_choice(c"ROKR_HEAP_DUMP", "on_oom").is_some() {
        rokr_common::set_oom_hook(heap_dump);
    }
}

/// `core::fmt` to stderr.
struct Stderr;

impl core::fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // SAFETY: `s` is valid for its own length.
        unsafe { rokr_common::write_stderr(s.as_ptr(), s.len()) };
        Ok(())
    }
}

extern "C" fn print_stats() {
    world::lock_at_exit();
    let h = heap();
    let _ = writeln!(
        Stderr,
        "rok gc: {} collections ({} minor), {}.{:03} ms total pause, max {}.{:03} ms, {}.{:03} ms cpu, {} KiB live",
        h.collections,
        h.minors,
        h.gc_ns / 1_000_000,
        h.gc_ns / 1000 % 1000,
        h.max_ns / 1_000_000,
        h.max_ns / 1000 % 1000,
        h.gc_cpu_ns / 1_000_000,
        h.gc_cpu_ns / 1000 % 1000,
        h.old_bytes >> 10
    );
    // SAFETY: `nblocks` initialized metas.
    let metas = unsafe { core::slice::from_raw_parts(h.meta, h.nblocks) };
    let released = metas.iter().filter(|m| m.released).count();
    let _ = writeln!(
        Stderr,
        "rok gc: {} blocks ({} KiB resident), {} evacuations moved {} KiB",
        h.nblocks,
        ((h.nblocks - released) * BLOCK_SIZE) >> 10,
        h.evacuations,
        h.moved_bytes >> 10
    );
    let ms = |ns: u64| (ns / 1_000_000, ns / 1000 % 1000);
    let ((ft, ftu), (fm, fmu)) = (ms(h.full_ns), ms(h.full_max_ns));
    let ((ct, ctu), (cm, cmu)) = (ms(h.full_cpu_ns), ms(h.full_cpu_max_ns));
    let (mc, mcu) = ms(MARKER_CPU_NS.load(Ordering::Relaxed));
    let _ = writeln!(
        Stderr,
        "rok gc: {} full pauses, {ft}.{ftu:03} ms total, max {fm}.{fmu:03} ms ({ct}.{ctu:03} / {cm}.{cmu:03} ms cpu); {} concurrent marks ({} finished by allocation), {mc}.{mcu:03} ms marker cpu",
        h.full_pauses, h.cycles, h.forced
    );
    world::unlock();
}

/// Types [`heap_dump`] tells apart; the rest count as one.
const DUMP_TYPES: usize = 512;
/// Types and large objects [`heap_dump`] lists.
const DUMP_LINES: usize = 32;

/// [`heap_dump`]'s tables (it must not allocate).
struct Dump {
    /// `(type_id, objects, bytes)` per type; the last entry (type id 0)
    /// counts the types that did not fit.
    types: [(u32, u64, u64); DUMP_TYPES + 1],
    /// The largest LOS objects, `(bytes, type_id)`.
    large: [(usize, u32); DUMP_LINES],
}

static DUMP: Global<Dump> = Global(UnsafeCell::new(Dump {
    types: [(0, 0, 0); DUMP_TYPES + 1],
    large: [(0, 0); DUMP_LINES],
}));

/// `ROKR_HEAP_DUMP=on_oom`: the object count and bytes of each type (most
/// bytes first) and the largest LOS objects, to
/// stderr. Runs on the failing thread without taking the world lock (it
/// may hold it): other threads' newest objects may be missed.
fn heap_dump() {
    // SAFETY: called once (`oom` takes the hook); best effort over the
    // heap as it stands.
    let Dump { types, large } = unsafe { &mut *DUMP.0.get() };
    let mut count = |type_id: u32, size: usize| {
        let mut i = type_id as usize % DUMP_TYPES;
        for _ in 0..DUMP_TYPES {
            if types[i].0 == type_id || types[i].1 == 0 {
                break;
            }
            i = (i + 1) % DUMP_TYPES;
        }
        if types[i].1 != 0 && types[i].0 != type_id {
            i = DUMP_TYPES; // type id 0: the rest
        } else {
            types[i].0 = type_id;
        }
        types[i].1 += 1;
        types[i].2 += size as u64;
    };
    let h = heap();
    let (mut objects, mut used) = (0usize, 0usize);
    for b in 0..h.nblocks {
        // SAFETY: `b < nblocks`.
        let m = unsafe { &*h.meta.add(b) };
        if !m.used {
            continue;
        }
        used += 1;
        let block = h.base + b * BLOCK_SIZE;
        // Survivors by their start bits; what was handed out since, by
        // parsing (a header being written may read as 0: skipped).
        for_each_start(&m.lines, &m.starts, true, |off| {
            if line(m, off / LINE_SIZE) == LIVE {
                let o = (block + off) as *mut u8;
                // SAFETY: a survivor's header.
                count(unsafe { (*header(o)).type_id }, obj_size(o));
                objects += 1;
            }
        });
        let mut off = 0;
        while off + HEADER_SIZE <= BLOCK_SIZE {
            let state = line(m, off / LINE_SIZE);
            if state != TAKEN && state != BLACK {
                off = (off / LINE_SIZE + 1) * LINE_SIZE;
                continue;
            }
            let o = (block + off) as *mut u8;
            // SAFETY: inside a handed-out line: zeroes or headers.
            let (type_id, size) = unsafe { ((*header(o)).type_id, obj_size(o)) };
            if type_id == 0 || size < HEADER_SIZE || size % 8 != 0 || off + size > BLOCK_SIZE {
                off += 8;
                continue;
            }
            count(type_id, size);
            objects += 1;
            off += size;
        }
    }
    let (mut los, mut los_bytes) = (0usize, 0usize);
    let mut o = h.los;
    while !o.is_null() {
        // SAFETY: walks the LOS list.
        let (size, type_id, next) =
            unsafe { ((*prefix(o)).size, (*header(o)).type_id, (*prefix(o)).next) };
        count(type_id, size);
        los += 1;
        los_bytes += size;
        if let Some(i) = large.iter().position(|&(s, _)| s < size) {
            large.copy_within(i..DUMP_LINES - 1, i + 1);
            large[i] = (size, type_id);
        }
        o = next;
    }
    types.sort_unstable_by(|a, b| b.2.cmp(&a.2));
    let _ = writeln!(
        Stderr,
        "rok heap dump: {used} blocks ({} KiB), {objects} objects in them; {los} large objects ({} KiB)",
        (used * BLOCK_SIZE) >> 10,
        los_bytes >> 10
    );
    for &(type_id, n, bytes) in types.iter().take(DUMP_LINES).filter(|t| t.1 != 0) {
        if type_id == 0 {
            let _ = writeln!(
                Stderr,
                "rok heap dump: other types: {n} objects, {bytes} bytes"
            );
        } else {
            let _ = writeln!(
                Stderr,
                "rok heap dump: type {type_id:#x}: {n} objects, {bytes} bytes"
            );
        }
    }
    for &(size, type_id) in large.iter().filter(|l| l.0 != 0) {
        let _ = writeln!(
            Stderr,
            "rok heap dump: large object of type {type_id:#x}: {size} bytes"
        );
    }
}

fn now_ns() -> u64 {
    sys::monotonic_ns() as u64
}

/// With the lock held: `lines` free lines (exactly, if `exact`; else
/// the first line of a free run, whose remaining free lines
/// [`end_of_run`] claims) in one block, marked `TAKEN`. Commits a new
/// block when no used one has room.
fn take_run(lines: usize, exact: bool) -> *mut u8 {
    let h = heap();
    let cursor = if exact {
        h.medium_cursor
    } else {
        h.small_cursor
    };
    let state = if h.marking { BLACK } else { TAKEN };
    let mut at = cursor;
    // Raw metadata pointers and atomic line writes: a concurrent mark
    // reads these lines.
    while at < h.nblocks * LINES {
        let (blk, l) = (at / LINES, at % LINES);
        // SAFETY: `blk < nblocks`.
        let meta = unsafe { h.meta.add(blk) };
        if line(meta, l) != FREE {
            at += 1;
            continue;
        }
        let mut run = 1;
        while l + run < LINES && run < lines && line(meta, l + run) == FREE {
            run += 1;
        }
        if run < lines {
            at += run;
            continue;
        }
        for i in l..l + run {
            set_line(meta, i, state);
        }
        // SAFETY: fields the mark does not read.
        unsafe {
            (*meta).used = true;
            (*meta).released = false;
        }
        if exact {
            h.medium_cursor = at + run;
        } else {
            h.small_cursor = at + run;
        }
        return (h.base + at * LINE_SIZE) as *mut u8;
    }
    let b = new_block();
    let h = heap();
    // SAFETY: `b` was just committed (no mark looks at it).
    let m = unsafe { &mut *h.meta.add(b) };
    m.lines[..lines].fill(state);
    m.used = true;
    m.released = false;
    if exact {
        h.medium_cursor = b * LINES + lines;
    }
    (h.base + b * BLOCK_SIZE) as *mut u8
}

/// With the lock held: extend a small run that starts at `start` (its
/// first line already handed out, ending at `end`) over the following
/// free lines of its block; the run's end address.
fn end_of_run(start: usize, mut end: usize) -> usize {
    let h = heap();
    let state = if h.marking { BLACK } else { TAKEN };
    let b = (start - h.base) / BLOCK_SIZE;
    // SAFETY: `start` is in block `b < nblocks`.
    let m = unsafe { h.meta.add(b) };
    let mut l = (end - h.base) / LINE_SIZE - b * LINES;
    while l < LINES && line(m, l) == FREE {
        set_line(m, l, state);
        l += 1;
        end += LINE_SIZE;
    }
    h.small_cursor = h.small_cursor.max(b * LINES + l);
    end
}

/// With the lock held: commit one more (zeroed) block; its index.
fn new_block() -> usize {
    let h = heap();
    if h.base == 0 {
        reserve_blocks(h);
    }
    #[cfg(target_family = "wasm")]
    grow_blocks(h);
    if h.nblocks == h.reserved / BLOCK_SIZE {
        rokr_common::oom();
    }
    let b = h.nblocks;
    // Commit in 64 KiB steps (a multiple of every page size): block
    // `b`'s metadata pages, unless block `b - 1`'s commit covered them.
    let step = 64 << 10;
    let (start, end) = (b * size_of::<BlockMeta>(), (b + 1) * size_of::<BlockMeta>());
    let from = if b == 0 {
        0
    } else {
        start.next_multiple_of(step)
    };
    let to = end.next_multiple_of(step);
    // SAFETY: block `b` and its metadata lie inside our reservations.
    let ok = unsafe {
        sys::commit((h.base + b * BLOCK_SIZE) as *mut u8, BLOCK_SIZE)
            && (from >= to || sys::commit(h.meta.cast::<u8>().add(from), to - from))
    };
    if !ok {
        rokr_common::oom();
    }
    // SAFETY: `b` is below the reserved count.
    unsafe {
        h.meta.add(b).write(BlockMeta {
            lines: [FREE; LINES],
            starts: [0; GRANULES / 64],
            used: false,
            parsed: false,
            evac: false,
            released: false,
        });
    }
    h.nblocks += 1;
    b
}

/// The block reservation: the first of the sizes from [`RESERVE`] down
/// the OS grants. Metadata for every block of it is reserved once and
/// committed as blocks are: a concurrent mark reads it while refills
/// commit blocks, so it never moves.
#[cfg(not(target_family = "wasm"))]
fn reserve_blocks(h: &mut Heap) {
    let mut len = RESERVE;
    let p = loop {
        let p = sys::reserve(len + BLOCK_SIZE);
        if !p.is_null() {
            break p;
        }
        len /= 2;
        if len < RESERVE_MIN {
            rokr_common::oom();
        }
    };
    h.base = (p as usize).next_multiple_of(BLOCK_SIZE);
    h.reserved = len;
    h.meta = sys::reserve(len / BLOCK_SIZE * size_of::<BlockMeta>()).cast();
    if h.meta.is_null() {
        rokr_common::oom();
    }
}

/// wasm32 memory cannot be reserved without being grown: the blocks
/// span the address space above the end of linear memory, grown by
/// [`grow_blocks`] as they are needed. Their metadata is grown first
/// (about 2.4% of the blocks' size), for the rest of the 4 GiB address
/// space or, under a lower engine limit, the first of its halves it
/// grants.
#[cfg(target_family = "wasm")]
fn reserve_blocks(h: &mut Heap) {
    let end = core::arch::wasm32::memory_size(0) * WASM_PAGE;
    let mut len = usize::MAX - end;
    loop {
        h.meta = sys::reserve(len / BLOCK_SIZE * size_of::<BlockMeta>()).cast();
        if !h.meta.is_null() {
            break;
        }
        len /= 2;
        if len < 64 << 20 {
            rokr_common::oom();
        }
    }
    h.base = core::arch::wasm32::memory_size(0) * WASM_PAGE;
    h.grown = h.base;
    // Whole blocks: `base + reserved` stays below 4 GiB.
    h.reserved = len.min(usize::MAX - h.base) / BLOCK_SIZE * BLOCK_SIZE;
}

#[cfg(target_family = "wasm")]
const WASM_PAGE: usize = 64 << 10;

/// wasm32, before block `nblocks` is used: grow linear memory for it
/// (1 MiB at a time) unless an earlier growth covers it. `malloc` grows
/// the same memory: the blocks over what it took since are skipped,
/// [`FOREIGN`].
#[cfg(target_family = "wasm")]
fn grow_blocks(h: &mut Heap) {
    const STEP: usize = 1 << 20;
    if h.base + (h.nblocks + 1) * BLOCK_SIZE <= h.grown {
        return;
    }
    let old = core::arch::wasm32::memory_grow(0, STEP / WASM_PAGE);
    if old == usize::MAX {
        rokr_common::oom();
    }
    let from = old * WASM_PAGE;
    while h.base + h.nblocks * BLOCK_SIZE < from && h.nblocks < h.reserved / BLOCK_SIZE {
        // SAFETY: metadata of a block of the reservation (grown up front).
        unsafe {
            h.meta.add(h.nblocks).write(BlockMeta {
                lines: [FOREIGN; LINES],
                starts: [0; GRANULES / 64],
                used: false,
                parsed: false,
                evac: false,
                released: true,
            });
        }
        h.nblocks += 1;
    }
    h.grown = from + STEP;
}

/// LOS allocations from this size on (prefix and card table included)
/// get pages of their own, zero-filled by the OS and unmapped when the
/// object dies; smaller ones (over-aligned small objects, objects just
/// over [`LOS_THRESHOLD`]) come from the C heap.
const LOS_MAP_MIN: usize = 64 << 10;

/// Bytes of a LOS object's allocation.
const fn los_total(p: &Prefix) -> usize {
    p.offset + p.size + p.size.div_ceil(CARD_SIZE)
}

/// A large (or over-aligned) object: its own zeroed allocation, header
/// flags `LARGE | flags`. The payload (after the header) is
/// `align`-aligned.
fn alloc_large(size: usize, align: usize, type_id: u32, flags: u32) -> *mut u8 {
    let align = align.max(HEADER_ALIGN);
    let offset = (size_of::<Prefix>() + HEADER_SIZE).next_multiple_of(align) - HEADER_SIZE;
    let Some(total) = size
        .checked_add(offset)
        .and_then(|t| t.checked_add(size.div_ceil(CARD_SIZE)))
    else {
        fatal(b"rok runtime: allocation size overflow");
    };
    let base = if total >= LOS_MAP_MIN {
        sys::map(total, align)
    } else {
        let base = sys::alloc_aligned(total, align);
        if !base.is_null() {
            // SAFETY: a fresh block of `total` bytes.
            unsafe { ptr::write_bytes(base, 0, total) };
        }
        base
    };
    if base.is_null() {
        rokr_common::oom();
    }
    world::lock();
    // The new block is not on the list yet, so a collection here cannot
    // free it.
    before_alloc(size);
    let h = heap();
    // SAFETY: `base..base+total` is ours; `obj`'s payload is
    // `align`-aligned and the prefix fits in the `offset` bytes before it.
    let obj = unsafe {
        let obj = base.cast::<u8>().add(offset);
        prefix(obj).write(Prefix {
            next: h.los,
            size,
            offset,
            cards: obj.add(size),
        });
        init_header(obj, type_id, size, FLAG_LARGE | flags);
        h.los = obj;
        h.los_count += 1;
        h.live_bytes += size;
        obj
    };
    world::unlock();
    obj
}

/// A secret buffer of `len` zeroed bytes (`rok_gc_secret_new`).
///
/// A LOS object with no references, so it never moves and is never
/// copied, marked `WIPE`, so the sweep that frees it zeroes it first
/// ([`wipe`]).
pub fn alloc_secret(len: usize) -> *mut u8 {
    let Some(size) = len
        .checked_add(HEADER_SIZE + 7)
        .filter(|&s| isize::try_from(s).is_ok())
    else {
        fatal(b"rok runtime: allocation too large");
    };
    let size = (size & !7).max(MIN_OBJECT_SIZE as usize);
    alloc_large(size, HEADER_ALIGN, PLAIN_TYPE_ID, FLAG_WIPE)
}

/// Zero the payload of the LOS object `obj` with volatile stores, which
/// the optimizer keeps even when nothing reads the memory again (the
/// release right after the sweep's wipe).
///
/// # Safety
/// `obj` must be a live LOS object (or one the sweep is freeing).
#[allow(clippy::cast_ptr_alignment)] // the payload is 8-aligned
pub unsafe fn wipe(obj: *mut u8) {
    // SAFETY: a LOS object's size (header included) is a multiple of 8
    // and its payload is 8-aligned.
    unsafe {
        let words = ((*prefix(obj)).size - HEADER_SIZE) / 8;
        let p = obj.add(HEADER_SIZE).cast::<u64>();
        for i in 0..words {
            p.add(i).write_volatile(0);
        }
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// Register the `ScanTable` for `type_id`. Ids below the user range
/// are not used by the compiler yet and are ignored.
pub fn register_type(type_id: u32, table: *const u8) {
    let Some(i) = type_id.checked_sub(USER_TYPE_BASE) else {
        return;
    };
    let i = i as usize;
    world::lock();
    let mut t = TABLES.load(Ordering::Acquire);
    // SAFETY: `t` is null or a `[len, tables..]` array from below; a
    // grown array replaces it whole and the old one is never freed (the
    // marker and barriers read it without the lock).
    unsafe {
        let len = if t.is_null() { 0 } else { *t };
        if i >= len {
            let cap = (i + 1).next_power_of_two().max(64);
            let p = libc::calloc(cap + 1, size_of::<usize>()).cast::<usize>();
            if p.is_null() {
                rokr_common::oom();
            }
            *p = cap;
            if len != 0 {
                ptr::copy_nonoverlapping(t.add(1), p.add(1), len);
            }
            t = p;
        }
        core::sync::atomic::AtomicUsize::from_ptr(t.add(1 + i))
            .store(table as usize, Ordering::Release);
        TABLES.store(t, Ordering::Release);
    }
    world::unlock();
}

/// Registered `ScanTable`s: `[len, table of USER_TYPE_BASE + i..]`.
static TABLES: core::sync::atomic::AtomicPtr<usize> =
    core::sync::atomic::AtomicPtr::new(ptr::null_mut());

fn scan_table(type_id: u32) -> *const u8 {
    let t = TABLES.load(Ordering::Acquire);
    match type_id.checked_sub(USER_TYPE_BASE) {
        // SAFETY: a bounds-checked entry of the live array.
        Some(i) if !t.is_null() && (i as usize) < unsafe { *t } => unsafe {
            core::sync::atomic::AtomicUsize::from_ptr(t.add(1 + i as usize)).load(Ordering::Acquire)
                as *const u8
        },
        _ if type_id == WEAK_TYPE_ID => WEAK_TABLE.as_ptr().cast(),
        _ => ptr::null(),
    }
}

/// Set the start bits of every object in the `TAKEN` lines of the block
/// at `block` by parsing them: an object has a nonzero `type_id` (0 is
/// reserved) and its size in `gc_word`; everything else is
/// zero. A run of `TAKEN` lines was all free when handed out, so no
/// object crosses its ends.
fn parse_taken(block: usize, m: &mut BlockMeta) {
    let mut l = 0;
    while l < LINES {
        if m.lines[l] != TAKEN {
            l += 1;
            continue;
        }
        let mut end = l + 1;
        while end < LINES && m.lines[end] == TAKEN {
            end += 1;
        }
        let mut off = l * LINE_SIZE;
        while off < end * LINE_SIZE {
            let obj = (block + off) as *mut u8;
            // SAFETY: `off` is inside the run: an object header or zeroes.
            if unsafe { (*header(obj)).type_id } == 0 {
                off += 8;
            } else {
                m.starts[off / 8 / 64] |= 1 << (off / 8 % 64);
                off += obj_size(obj);
            }
        }
        l = end;
    }
}

/// `f(offset)` for every object start in the lines of `lines` that are
/// `TAKEN` (all lines if `all`), from the start bitmap `starts`.
fn for_each_start(
    lines: &[u8; LINES],
    starts: &[u64; GRANULES / 64],
    all: bool,
    mut f: impl FnMut(usize),
) {
    const PER_WORD: usize = 64 / LINE_GRANULES;
    for (w, &word) in starts.iter().enumerate() {
        let mut bits = word;
        if !all {
            for k in 0..PER_WORD {
                if lines[w * PER_WORD + k] != TAKEN {
                    bits &= !(((1u64 << LINE_GRANULES) - 1) << (k * LINE_GRANULES));
                }
            }
        }
        while bits != 0 {
            f((w * 64 + bits.trailing_zeros() as usize) * 8);
            bits &= bits - 1;
        }
    }
}

/// Before marking: the sorted LOS index and the block heap view lookups
/// use ([`Mark`]); new block lines are parsed lazily by [`object_in`].
/// With the world stopped and the mark lock held.
fn build_index() {
    let (h, m) = (heap(), mark());
    (m.base, m.nblocks, m.meta, m.reserved) = (h.base, h.nblocks, h.meta, h.reserved);
    m.los_count = h.los_count;
    if h.los_count == 0 {
        return;
    }
    if m.index_cap < h.los_count {
        let cap = h.los_count.next_power_of_two();
        // SAFETY: realloc of our own buffer (or null).
        let p = unsafe { libc::realloc(m.index.cast(), cap * size_of::<usize>()) };
        if p.is_null() {
            rokr_common::oom();
        }
        m.index = p.cast();
        m.index_cap = cap;
    }
    // SAFETY: the list holds exactly `los_count` objects; `index` has
    // room.
    unsafe {
        let mut o = h.los;
        for i in 0..h.los_count {
            *m.index.add(i) = o as usize;
            o = (*prefix(o)).next;
        }
        core::slice::from_raw_parts_mut(m.index, h.los_count).sort_unstable();
    }
}

/// Pinned objects (`rok_gc_pin`, `FLAG_PINNED`): roots until unpinned.
/// A list, so a collection does not visit every header looking for the
/// flag.
// Spin lock and linear unpin: pins are few (deque setup, executor
// workers, FFI buffers). An object pinned n times is listed n times.
struct Pins {
    lock: core::sync::atomic::AtomicBool,
    list: UnsafeCell<(*mut *mut u8, usize, usize)>,
}

// SAFETY: `list` is only touched under `lock` (or with the world
// stopped, when no thread is inside `pin` / `unpin`).
unsafe impl Sync for Pins {}

static PINS: Pins = Pins {
    lock: core::sync::atomic::AtomicBool::new(false),
    list: UnsafeCell::new((ptr::null_mut(), 0, 0)),
};

fn with_pins<R>(f: impl FnOnce(&mut (*mut *mut u8, usize, usize)) -> R) -> R {
    while PINS
        .lock
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    // SAFETY: we hold the lock.
    let r = f(unsafe { &mut *PINS.list.get() });
    PINS.lock.store(false, Ordering::Release);
    r
}

/// Keep `obj` (and what it references) alive and in place until as
/// many [`unpin`]s. Not a safepoint.
///
/// # Safety
/// `obj` must be a live GC object.
pub unsafe fn pin(obj: *mut u8) {
    with_pins(|(objs, len, cap)| {
        if *len == *cap {
            *cap = (*cap * 2).max(16);
            // SAFETY: realloc of our own list (or null).
            *objs = unsafe { libc::realloc(objs.cast(), *cap * size_of::<*mut u8>()) }.cast();
            if objs.is_null() {
                rokr_common::oom();
            }
        }
        // SAFETY: `len < cap`.
        unsafe { *objs.add(*len) = obj };
        *len += 1;
        // SAFETY: caller contract.
        unsafe {
            (*header(obj))
                .gc_flags
                .fetch_or(FLAG_PINNED, Ordering::Relaxed)
        };
    });
}

/// Undo one [`pin`]: the object may move again after the last.
///
/// # Safety
/// `obj` must be a live GC object.
pub unsafe fn unpin(obj: *mut u8) {
    with_pins(|(objs, len, _)| {
        // SAFETY: the first `len` entries are initialized.
        let list = unsafe { core::slice::from_raw_parts_mut(*objs, *len) };
        let Some(i) = list.iter().position(|&o| o == obj) else {
            return;
        };
        list.swap(i, *len - 1);
        *len -= 1;
        if !list[..*len].contains(&obj) {
            // SAFETY: caller contract.
            unsafe {
                (*header(obj))
                    .gc_flags
                    .fetch_and(!FLAG_PINNED, Ordering::Relaxed)
            };
        }
    });
}

/// A new weak cell referring to `obj`.
///
/// The collection that frees `obj` sets the cell's word to 0, one that
/// moves it updates it. The cell is never written again, so (young when
/// written) it needs no barrier. May collect: the caller roots `obj`.
#[allow(clippy::cast_ptr_alignment)] // GC objects are 8-aligned
pub fn weak_new(obj: *mut u8) -> *mut u8 {
    let cell = alloc(24, 8, WEAK_TYPE_ID);
    // SAFETY: a fresh 24-byte object.
    unsafe { cell.add(HEADER_SIZE).cast::<usize>().write(obj as usize) };
    cell
}

/// The referent of the weak cell `cell`, null once it was freed.
///
/// # Safety
/// `cell` must be a live cell from [`weak_new`].
pub unsafe fn weak_get(cell: *mut u8) -> *mut u8 {
    // SAFETY: caller contract.
    let w = load_word(unsafe { cell.add(HEADER_SIZE) });
    // No safepoint between the load and the log: a running mark sees it.
    if w != 0 && concurrent::MARKING.load(Ordering::Relaxed) {
        concurrent::shade(w);
    }
    w as *mut u8
}

/// Never move `obj` again (its address became a number: a parking key).
/// It still dies when unreachable.
///
/// # Safety
/// `obj` must be a live GC object.
pub unsafe fn no_move(obj: *mut u8) {
    // SAFETY: caller contract.
    unsafe {
        (*header(obj))
            .gc_flags
            .fetch_or(FLAG_NOMOVE, Ordering::Relaxed)
    };
}

/// The object whose header address is `word` or whose memory contains
/// it (a `str` points at its payload bytes, or into them after slicing).
/// `young`: block objects outside `TAKEN` lines (old ones) are `None`,
/// decided from the line table without touching the object. Free and
/// `BLACK` lines hold none (a live object covers only its own lines;
/// a concurrent mark never looks at what was allocated meanwhile).
/// `MOVED`: objects may be evacuated already. `!exact` (a precise
/// reference: inside its object, or one past its end): the nearest block
/// object start at or before `word`, its header left unread.
fn object_in<const MOVED: bool>(word: usize, young: bool, exact: bool) -> Option<*mut u8> {
    let size_of = |o: *mut u8| if MOVED { moved_size(o) } else { obj_size(o) };
    let m = mark();
    if word >= m.base && word < m.base + m.nblocks * BLOCK_SIZE && !foreign(m, word) {
        let blk = (word - m.base) / BLOCK_SIZE;
        // SAFETY: `blk < nblocks`.
        let meta = unsafe { m.meta.add(blk) };
        let block = m.base + blk * BLOCK_SIZE;
        let state = line(meta, (word - block) / LINE_SIZE);
        if state == FREE || state == BLACK || (young && state != TAKEN) {
            return None;
        }
        // SAFETY: only lookups write start bits, and only while the
        // world is stopped: `TAKEN` lines exist only then (a concurrent
        // mark starts right after a sweep and makes `BLACK` ones).
        let starts = unsafe {
            if state == TAKEN && !(*meta).parsed {
                parse_taken(block, &mut *meta);
                (*meta).parsed = true;
            }
            &(*meta).starts
        };
        let granule = (word - block) / 8;
        // The nearest object start at or before `granule`: objects are
        // smaller than `LOS_THRESHOLD`, so only that many granules back.
        let lowest = granule.saturating_sub(LOS_THRESHOLD / 8);
        let mut i = granule / 64;
        let mut bits = starts[i] & (u64::MAX >> (63 - granule % 64));
        loop {
            if bits != 0 {
                let start = block + (i * 64 + 63 - bits.leading_zeros() as usize) * 8;
                if !exact {
                    return Some(start as *mut u8);
                }
                return (word < start + size_of(start as *mut u8)).then_some(start as *mut u8);
            }
            if i == 0 || (i - 1) * 64 + 63 < lowest {
                return None;
            }
            i -= 1;
            bits = starts[i];
        }
    }
    // No LOS object, or a block committed after the view (wasm32:
    // `malloc`'s memory lies among the blocks).
    if m.los_count == 0
        || (!cfg!(target_family = "wasm")
            && m.base != 0
            && word >= m.base
            && word < m.base + m.reserved)
    {
        return None;
    }
    // SAFETY: `build_index` filled `los_count` entries.
    let index = unsafe { core::slice::from_raw_parts(m.index, m.los_count) };
    let i = match index.binary_search(&word) {
        Ok(i) => i,
        Err(0) => return None,
        Err(i) => i - 1,
    };
    let start = index[i];
    (word < start + size_of(start as *mut u8)).then_some(start as *mut u8)
}

/// Whether `word`, in one of the view's blocks, is in a [`FOREIGN`] one
/// (`malloc`'s, holding LOS objects).
#[cfg(target_family = "wasm")]
fn foreign(m: &Mark, word: usize) -> bool {
    // SAFETY: the block is below the view's `nblocks`.
    line(unsafe { m.meta.add((word - m.base) / BLOCK_SIZE) }, 0) == FOREIGN
}

#[cfg(not(target_family = "wasm"))]
const fn foreign(_m: &Mark, _word: usize) -> bool {
    false
}

/// The word at `p` (8-aligned): an atomic load, as a concurrent mark
/// reads what mutators store.
fn load_word(p: *const u8) -> usize {
    // SAFETY: callers pass an aligned word inside a live object.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe { core::sync::atomic::AtomicUsize::from_ptr(p.cast::<usize>().cast_mut()) }
        .load(Ordering::Relaxed)
}

/// The `u32` at `p` (4-aligned), like [`load_word`].
fn load_u32(p: *const u8) -> u32 {
    // SAFETY: as in `load_word`.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe { core::sync::atomic::AtomicU32::from_ptr(p.cast::<u32>().cast_mut()) }
        .load(Ordering::Relaxed)
}

/// Queue the object `word` points into, if any (and young, unless the
/// mark is full). `pin`: `word` is a root or ambiguous, so the object
/// must not move (and the lookup is exact); else it is a precise
/// reference.
fn push(word: usize, pin: bool) {
    let Some(obj) = object_in::<false>(word, !mark().full, pin) else {
        return;
    };
    let m = mark();
    if pin && m.evacuating && (obj as usize) < m.base + m.nblocks * BLOCK_SIZE {
        append(
            &mut m.pinned,
            &mut m.pinned_len,
            &mut m.pinned_cap,
            obj as usize,
        );
    }
    // Marked or not is decided by `drain`, once the header is in cache.
    push_obj(obj);
}

/// Append `v` to the `realloc`-grown list `list[..len]`.
fn append(list: &mut *mut usize, len: &mut usize, cap: &mut usize, v: usize) {
    if *len == *cap {
        *cap = (*cap * 2).max(1024);
        // SAFETY: realloc of our own buffer (or null).
        let p = unsafe { libc::realloc(list.cast(), *cap * size_of::<usize>()) };
        if p.is_null() {
            rokr_common::oom();
        }
        *list = p.cast();
    }
    // SAFETY: `len < cap`.
    unsafe { *list.add(*len) = v };
    *len += 1;
}

/// The word at offset `off` of the scanned object `o`, of kind `kind`
/// ([`for_each_ref`]): queue its referent, or record a weak word.
fn visit_word(o: *mut u8, off: usize, kind: u32) {
    // SAFETY: `off` is a word inside the live object `o`.
    let at = unsafe { o.add(off) };
    if kind == WEAK_REF {
        let m = mark();
        append(&mut m.weak, &mut m.weak_len, &mut m.weak_cap, at as usize);
    } else {
        push(load_word(at), kind == AMBIGUOUS);
    }
}

/// After marking, before the sweep: clear every recorded weak word whose
/// referent this collection frees (unmarked; a minor collection frees
/// young objects only). One it does not look at (allocated during a
/// concurrent mark) stays.
fn clear_dead_weak(full: bool) {
    let m = mark();
    if m.weak_len == 0 {
        return;
    }
    // SAFETY: `weak_len` recorded words inside marked (live) objects.
    let words = unsafe { core::slice::from_raw_parts(m.weak, m.weak_len) };
    for &at in words {
        let w = load_word(at as *const u8);
        if w == 0 {
            continue;
        }
        let Some(obj) = object_in::<false>(w, false, true) else {
            continue;
        };
        // SAFETY: an object header.
        let f = unsafe { (*header(obj)).gc_flags.load(Ordering::Relaxed) };
        if f & FLAG_MARK == 0 && (full || f & FLAG_STICKY == 0) {
            // SAFETY: as above; the world is stopped.
            unsafe { (at as *mut usize).write(0) };
        }
    }
    mark().weak_len = 0;
}

/// Start loading the cache line at `p` (a hint: never faults).
#[inline]
#[allow(clippy::missing_const_for_fn)] // const only where it is a no-op
fn prefetch(p: *const u8) {
    // SAFETY (each): a prefetch does not access memory architecturally.
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, readonly, preserves_flags));
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch(p.cast(), core::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "x86")]
    unsafe {
        core::arch::x86::_mm_prefetch(p.cast(), core::arch::x86::_MM_HINT_T0);
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64", target_arch = "x86")))]
    let _ = p;
}

/// `addr` lies in an evacuation candidate block.
fn in_candidate(addr: usize) -> bool {
    let h = heap();
    // SAFETY: bounds-checked block index.
    addr >= h.base
        && addr < h.base + h.nblocks * BLOCK_SIZE
        && unsafe { (*h.meta.add((addr - h.base) / BLOCK_SIZE)).evac }
}

fn push_obj(obj: *mut u8) {
    let m = mark();
    if m.len == m.stack_cap {
        let cap = (m.stack_cap * 2).max(1024);
        // SAFETY: realloc of our own stack (or null).
        let p = unsafe { libc::realloc(m.stack.cast(), cap * size_of::<*mut u8>()) };
        if p.is_null() {
            rokr_common::oom();
        }
        m.stack = p.cast();
        m.stack_cap = cap;
    }
    // SAFETY: `len < stack_cap`.
    unsafe { *m.stack.add(m.len) = obj };
    m.len += 1;
}

/// Objects popped from the mark stack and prefetched before they are
/// scanned: the header of each is loading while the ones ahead of it
/// are scanned.
const PREFETCH: usize = 8;

/// Mark the queued objects until none is left (`batch`: at most that
/// many). A minor collection never enters an old object: whatever young
/// object it references is in the remembered set's reach.
fn drain(mut batch: usize) {
    let mut ring = [ptr::null_mut::<u8>(); PREFETCH];
    let (mut head, mut count) = (0, 0);
    while batch > 0 {
        let m = mark();
        while count < PREFETCH && m.len > 0 {
            m.len -= 1;
            // SAFETY: `push` only queues addresses of live GC objects.
            let o = unsafe { *m.stack.add(m.len) };
            prefetch(o);
            ring[(head + count) % PREFETCH] = o;
            count += 1;
        }
        if count == 0 {
            return;
        }
        let o = ring[head];
        head = (head + 1) % PREFETCH;
        count -= 1;
        batch -= 1;
        let (full, concurrent) = (m.full, m.concurrent);
        // SAFETY: as above.
        let flags = unsafe { &(*header(o)).gc_flags };
        if (!full && flags.load(Ordering::Relaxed) & FLAG_STICKY != 0)
            || flags.load(Ordering::Relaxed) & FLAG_MARK != 0
            || flags.fetch_or(FLAG_MARK, Ordering::Relaxed) & FLAG_MARK != 0
        {
            continue;
        }
        m.marked += 1;
        scan_children(o);
        if concurrent {
            // The first store into `o` from now on need not log what it
            // held (`write_barrier_slow`).
            flags.fetch_or(FLAG_SCANNED, Ordering::Release);
        }
    }
    // Out of budget: what was popped goes back.
    for k in 0..count {
        push_obj(ring[(head + k) % PREFETCH]);
    }
}

/// Queue the referents of `o`'s reference words.
// The mark loop's hot path: a plain (non-array) table read inline, the
// general `for_each_ref` otherwise.
#[allow(clippy::cast_ptr_alignment)] // tables are 4-aligned, payloads 8
fn scan_children(o: *mut u8) {
    // SAFETY: `o` is a live GC object; its ScanTable describes it.
    unsafe {
        let type_id = (*header(o)).type_id;
        if type_id == SCAN_WORDS_TYPE_ID {
            scan_range(o, 0, usize::MAX);
            return;
        }
        let t = scan_table(type_id);
        if t.is_null() {
            return;
        }
        let n = t.add(6).cast::<u16>().read_unaligned() as usize;
        if t.add(4).cast::<u16>().read_unaligned() & TABLE_ARRAY != 0 {
            // A buffer of plain references (`List<str>`, a list of
            // handles): one word per entry and element.
            let entries = core::slice::from_raw_parts(t.add(8).cast::<[u32; 2]>(), n);
            if !entries
                .iter()
                .all(|&[_, kind]| matches!(kind & KIND_MASK, STRONG_REF | INTERIOR_REF))
            {
                scan_range(o, 0, usize::MAX);
                return;
            }
            let stride = t.cast::<u32>().read_unaligned() as usize;
            if n == 0 || stride == 0 {
                return;
            }
            let end = o.add(obj_size(o));
            let mut e = o.add(HEADER_SIZE);
            while e.add(stride) <= end {
                for &[eo, _] in entries {
                    push(load_word(e.add(eo as usize)), false);
                }
                e = e.add(stride);
            }
            return;
        }
        for e in 0..n {
            let entry = t.add(8 + 8 * e).cast::<u32>();
            let mut off = entry.read_unaligned() as usize;
            let kind = entry.add(1).read_unaligned();
            let precise = match kind & KIND_MASK {
                STRONG_REF | INTERIOR_REF => true,
                ANY_SLOT => {
                    if load_u32(o.add(off)) & ANY_BOXED == 0 {
                        continue;
                    }
                    off += size_of::<usize>();
                    true
                }
                SUM_PAYLOAD => {
                    visit_sum(o, t, kind, off, &mut |off, k| visit_word(o, off, k));
                    continue;
                }
                AMBIGUOUS => false,
                WEAK_REF => {
                    visit_word(o, off, WEAK_REF);
                    continue;
                }
                _ => continue,
            };
            push(load_word(o.add(off)), !precise);
        }
    }
}

/// Queue the referents of `o`'s reference words at byte offsets
/// `lo..hi` (from the header).
fn scan_range(o: *mut u8, lo: usize, hi: usize) {
    for_each_ref(o, lo, hi, |off, kind| visit_word(o, off, kind));
}

/// `f(offset, kind)` for every word of the live object `o` at byte
/// offsets `lo..hi` (from the header) that may hold a reference, per
/// its `ScanTable`. `kind`: `STRONG_REF` (a reference or an
/// address outside the heap), `AMBIGUOUS` (anything) or `WEAK_REF` (a
/// reference or 0, not to be followed). An `ANY_SLOT` yields its payload
/// word only when the tag says it is a box. An object whose type was
/// never registered has none.
#[allow(clippy::cast_ptr_alignment)] // tables are 4-aligned, payloads 8
fn for_each_ref(o: *mut u8, lo: usize, hi: usize, mut f: impl FnMut(usize, u32)) {
    // SAFETY: `o` is a live GC object; its ScanTable describes it.
    unsafe {
        let hd = &*header(o);
        if hd.type_id == SCAN_WORDS_TYPE_ID {
            // Pointer-size words: 4 bytes on 32-bit targets.
            const W: usize = size_of::<usize>();
            let hi = hi.min(obj_size(o)) & !(W - 1);
            let mut off = lo.max(HEADER_SIZE).next_multiple_of(W);
            while off < hi {
                f(off, AMBIGUOUS);
                off += W;
            }
            return;
        }
        let t = scan_table(hd.type_id);
        if t.is_null() {
            return;
        }
        let version = t.add(4).cast::<u16>().read_unaligned();
        let n = t.add(6).cast::<u16>().read_unaligned() as usize;
        if n == 0 {
            return;
        }
        let entries = core::slice::from_raw_parts(t.add(8).cast::<[u32; 2]>(), n);
        if version & TABLE_ARRAY == 0 {
            if lo == 0 && hi == usize::MAX {
                visit_entries(o, t, entries, 0, &mut f);
            } else {
                visit_entries(o, t, entries, 0, &mut |off, p| {
                    if (lo..hi).contains(&off) {
                        f(off, p);
                    }
                });
            }
            return;
        }
        // An array table repeats its entries every `stride` bytes after
        // the header: visit the elements that overlap `lo..hi` (an `any`
        // counts by its payload word).
        let stride = t.cast::<u32>().read_unaligned() as usize;
        let count = (obj_size(o) - HEADER_SIZE) / stride;
        if lo == 0 && hi == usize::MAX {
            for i in 0..count {
                visit_entries(o, t, entries, HEADER_SIZE + i * stride, &mut f);
            }
            return;
        }
        let first = lo.saturating_sub(HEADER_SIZE + size_of::<usize>()) / stride;
        let last = hi.saturating_sub(HEADER_SIZE).div_ceil(stride).min(count);
        for i in first..last {
            visit_entries(o, t, entries, HEADER_SIZE + i * stride, &mut |off, p| {
                if (lo..hi).contains(&off) {
                    f(off, p);
                }
            });
        }
    }
}

/// Whether every reference slot of GC object `obj` is null (a
/// coroutine frame that holds nothing to copy).
///
/// # Safety
/// `obj` must be a live GC object.
pub unsafe fn refs_all_null(obj: *mut u8) -> bool {
    let mut none = true;
    for_each_ref(obj, 0, usize::MAX, |off, _| {
        // SAFETY: a reference slot inside the live `obj`.
        if none && load_word(unsafe { obj.add(off) }) != 0 {
            none = false;
        }
    });
    none
}

/// [`for_each_ref`] over the `entries` of table `t` at byte offset `at`.
#[inline]
#[allow(clippy::cast_ptr_alignment)] // the tag is 4-aligned
fn visit_entries(
    o: *mut u8,
    t: *const u8,
    entries: &[[u32; 2]],
    at: usize,
    f: &mut impl FnMut(usize, u32),
) {
    for &[eo, kind] in entries {
        let off = at + eo as usize;
        match kind & KIND_MASK {
            STRONG_REF | INTERIOR_REF => f(off, STRONG_REF),
            // SAFETY: the tag word of an `any` inside the live `o`.
            ANY_SLOT if load_u32(unsafe { o.add(off) }) & ANY_BOXED != 0 => {
                f(off + size_of::<usize>(), STRONG_REF);
            }
            SUM_PAYLOAD => visit_sum(o, t, kind, off, f),
            k @ (AMBIGUOUS | WEAK_REF) => f(off, k),
            _ => {}
        }
    }
}

/// The `SUM_PAYLOAD` entry `kind` of table `t` whose tag is at byte
/// offset `at` of `o`: visit the entries of the variant the tag selects.
#[allow(clippy::cast_ptr_alignment)] // tables and tags are 4-aligned
fn visit_sum(o: *mut u8, t: *const u8, kind: u32, at: usize, f: &mut impl FnMut(usize, u32)) {
    // SAFETY: the tag of a sum inside the live `o`; the descriptor and
    // its variants' entries lie inside the table (`gc_abi`).
    unsafe {
        let tag = load_u32(o.add(at));
        let t32 = t.cast::<u32>();
        let desc = t32.add((kind >> SUM_DESC_SHIFT) as usize);
        for v in 0..desc.read() as usize {
            let head = desc.add(1 + 3 * v);
            if head.read() == tag {
                let entries = core::slice::from_raw_parts(
                    t32.add(head.add(2).read() as usize).cast::<[u32; 2]>(),
                    head.add(1).read() as usize,
                );
                visit_entries(o, t, entries, at, f);
                return;
            }
        }
    }
}

/// Full stop-the-world collection (`rok_gc_collect`); a running
/// concurrent mark is finished first.
pub fn collect() {
    world::lock();
    configure(heap());
    if heap().marking {
        concurrent::finish(false);
    }
    collect_locked(true, false);
    world::unlock();
}

/// A minor collection (`rok_gc_collect_young`, tests).
pub fn collect_young() {
    world::lock();
    configure(heap());
    if heap().marking {
        concurrent::finish(false);
    }
    collect_locked(false, false);
    world::unlock();
}

/// With the world lock held: stop every other mutator, collect, resume.
/// While a concurrent mark runs, finish it instead (no minor collection
/// runs meanwhile). A full collection that `may_concur` marks
/// concurrently if `ROKR_GC_CONCURRENT` says so: this pause is then a
/// minor collection that starts the mark.
fn collect_locked(full: bool, may_concur: bool) {
    if heap().marking {
        concurrent::finish(true);
        return;
    }
    let concur = full && may_concur && wants_concurrent();
    let p = Pause::begin();
    if concur {
        mark_sweep(false);
        concurrent::start();
    } else {
        mark_sweep(full);
    }
    p.end(
        full,
        match (concur, full) {
            (true, _) => "minor, concurrent full mark started",
            (false, true) => "full",
            (false, false) => "minor",
        },
    );
}

/// A full collection due now should mark concurrently.
fn wants_concurrent() -> bool {
    let h = heap();
    match h.concurrent {
        Concurrent::Always => true,
        Concurrent::Off => false,
        Concurrent::Auto => !h.stw_next && !h.evac_always && h.old_bytes >= CONCURRENT_MIN,
    }
}

/// A stop-the-world pause: every other mutator stopped, every TLAB
/// empty, the mark lock held (the marker thread waits).
struct Pause {
    t0: u64,
    c0: u64,
    /// `live_bytes` before.
    live: usize,
}

impl Pause {
    /// With the world lock held.
    fn begin() -> Self {
        let p = Self {
            t0: now_ns(),
            c0: sys::thread_cpu_ns() as u64,
            live: heap().live_bytes,
        };
        world::stop();
        world::reset_tlabs();
        concurrent::lock_mark();
        p
    }

    /// `full`: the pause belongs to a full collection; `kind` names the
    /// collection in the `ROKR_GC_LOG` line.
    fn end(self, full: bool, kind: &str) {
        let marked = mark().marked;
        concurrent::unlock_mark();
        let h = heap();
        let dt = now_ns() - self.t0;
        if h.log {
            let _ = writeln!(
                Stderr,
                "rok gc: #{} {kind}: {}.{:03} ms pause, {marked} objects marked, {} -> {} KiB",
                h.collections,
                dt / 1_000_000,
                dt / 1000 % 1000,
                self.live >> 10,
                h.live_bytes >> 10
            );
        }
        let cpu = sys::thread_cpu_ns() as u64 - self.c0;
        h.gc_cpu_ns += cpu;
        h.gc_ns += dt;
        h.max_ns = h.max_ns.max(dt);
        if full {
            h.full_pauses += 1;
            h.full_ns += dt;
            h.full_max_ns = h.full_max_ns.max(dt);
            h.full_cpu_ns += cpu;
            h.full_cpu_max_ns = h.full_cpu_max_ns.max(cpu);
        }
        world::resume();
    }
}

/// Queue the pinned objects and every root. `pin`: their referents must
/// not move (an evacuating collection).
fn mark_roots(pin: bool) {
    // SAFETY: the world is stopped: nobody is inside `pin` / `unpin`.
    let (pins, npins, _) = unsafe { *PINS.list.get() };
    for i in 0..npins {
        // Through the lookup, so a young one's block gets parsed.
        // SAFETY: `i < npins`.
        push(unsafe { *pins.add(i) } as usize, pin);
    }
    // Roots pin their referents: a root word may be an inline `any`
    // payload, and compiled code / runtime calls derive pointers from
    // rooted values (`str` bytes handed to C, element addresses), so an
    // object a frame reaches directly never moves.
    world::for_each_root(|word| push(word as usize, pin));
}

/// In a pause: mark from all registered threads' shadow stacks, pinned
/// objects and (minor) the remembered set, sweep, maybe evacuate.
fn mark_sweep(full: bool) {
    build_index();
    let m = mark();
    (m.full, m.concurrent, m.evacuating, m.len, m.marked) = (full, false, full, 0, 0);
    mark_roots(true);
    // Nobody logs while the world is stopped (the barrier has no
    // safepoint): every thread's chunk joins the retired ones.
    world::for_each_wb_log(retire_log);
    let log = LOG_FULL.swap(ptr::null_mut(), Ordering::Acquire);
    if !full {
        for_each_logged(log, scan_children);
        scan_dirty_cards();
    }
    drain(usize::MAX);
    clear_dead_weak(full);
    if !full {
        // The logged (old) objects all survive: unlogged again.
        for_each_logged(log, |o| {
            // SAFETY: logged objects are live.
            unsafe {
                (*header(o))
                    .gc_flags
                    .fetch_or(FLAG_UNLOGGED, Ordering::Relaxed)
            };
        });
    }
    free_logs(log);
    let survived = sweep_blocks(full) + sweep_los(full, false);
    if mark().evacuating {
        if select_candidates() {
            evacuate();
        }
        let m = mark();
        m.evacuating = false;
        m.pinned_len = 0;
    }
    let h = heap();
    let live = if full {
        survived
    } else {
        h.old_bytes + survived
    };
    h.live_bytes = live;
    h.old_bytes = live;
    h.next_gc = (live * 2).max(MIN_HEAP);
    if full {
        h.full_at = h.next_gc;
        h.stw_next = false;
    } else {
        h.minors += 1;
    }
    h.small_cursor = 0;
    h.medium_cursor = 0;
    h.collections += 1;
    if full {
        release_free_blocks();
    }
}

/// Minor collection: queue the referents in the dirty cards of every
/// carded (old LOS) object, cleaning the cards (every survivor of this
/// collection is old, so no old -> young reference outlives it).
fn scan_dirty_cards() {
    // SAFETY: walks the LOS list; each prefix's card table covers its
    // object.
    unsafe {
        let mut o = heap().los;
        while !o.is_null() {
            let p = prefix(o);
            if (*header(o)).gc_flags.load(Ordering::Relaxed) & FLAG_CARDED != 0 {
                let n = (*p).size.div_ceil(CARD_SIZE);
                let cards = core::slice::from_raw_parts_mut((*p).cards, n);
                for (c, dirty) in cards.iter_mut().enumerate() {
                    if *dirty != 0 {
                        *dirty = 0;
                        scan_range(o, c * CARD_SIZE, (c + 1) * CARD_SIZE);
                    }
                }
            }
            o = (*p).next;
        }
    }
}

/// After marking: whether `obj` lives on (it was marked); a survivor's
/// mark is cleared and it becomes old (`STICKY | UNLOGGED`, or
/// `STICKY | CARDED` for a LOS object).
fn survives(obj: *mut u8) -> bool {
    // SAFETY: `obj` is an object header; the world is stopped.
    let flags = unsafe { &(*header(obj)).gc_flags };
    let f = flags.load(Ordering::Relaxed);
    if f & FLAG_MARK == 0 {
        return false;
    }
    let f = f & !(FLAG_MARK | FLAG_STICKY | FLAG_UNLOGGED | FLAG_CARDED | FLAG_SATB | FLAG_SCANNED);
    let old = if f & FLAG_LARGE != 0 {
        FLAG_STICKY | FLAG_CARDED
    } else {
        FLAG_STICKY | FLAG_UNLOGGED
    };
    flags.store(f | old, Ordering::Relaxed);
    true
}

/// Sweep the blocks (minor: only the `TAKEN` lines, where all young
/// objects are): survivors keep their lines, dead objects lose their
/// start bit. A concurrent mark's `BLACK` lines (full) stay unswept and
/// become `TAKEN`: their objects are young. Bytes of the swept
/// survivors.
fn sweep_blocks(full: bool) -> usize {
    let mut live = 0;
    for b in 0..heap().nblocks {
        let h = heap();
        // SAFETY: `b < nblocks`.
        let m = unsafe { &mut *h.meta.add(b) };
        if !m.used || (!full && !m.lines.contains(&TAKEN)) {
            continue;
        }
        let block = h.base + b * BLOCK_SIZE;
        let mut lines = m.lines;
        for l in &mut lines {
            *l = match *l {
                BLACK => TAKEN,
                TAKEN => FREE,
                _ if full => FREE,
                l => l,
            };
        }
        if !full && !m.parsed {
            // No young survivor here, and no start bits to clear.
            m.used = lines.contains(&LIVE);
            m.lines = lines;
            continue;
        }
        m.parsed = false;
        let (old_lines, starts) = (m.lines, m.starts);
        for_each_start(&old_lines, &starts, full, |off| {
            let obj = (block + off) as *mut u8;
            let size = obj_size(obj);
            if survives(obj) {
                live += size;
                lines[off / LINE_SIZE..(off + size).div_ceil(LINE_SIZE)].fill(LIVE);
            } else {
                // Its bytes stay until the line is handed out again.
                m.starts[off / 8 / 64] &= !(1 << (off / 8 % 64));
            }
        });
        m.used = lines.iter().any(|&l| l != FREE);
        m.lines = lines;
    }
    live
}

/// Sweep the LOS list: free dead objects (minor: old ones stay
/// unexamined). Bytes of the swept survivors. `concurrent`: after a
/// concurrent mark, whose younger objects (in front of `los_mark_head`)
/// stay unexamined and whose survivors keep their dirty cards (they may
/// reference objects allocated during the mark, which are young).
fn sweep_los(full: bool, concurrent: bool) -> usize {
    let h = heap();
    let mut live = 0;
    let mut link: *mut *mut u8 = &mut h.los;
    let mut young = concurrent;
    // SAFETY: walks the LOS list; freed objects are unlinked before their
    // memory is released.
    unsafe {
        while !(*link).is_null() {
            let o = *link;
            let p = prefix(o);
            young &= o != h.los_mark_head;
            let old = (*header(o)).gc_flags.load(Ordering::Relaxed) & FLAG_STICKY != 0;
            if young || (!full && old) {
                link = &mut (*p).next;
            } else if survives(o) {
                live += (*p).size;
                if !concurrent {
                    // Clean: everything it references is old now.
                    ptr::write_bytes((*p).cards, 0, (*p).size.div_ceil(CARD_SIZE));
                }
                link = &mut (*p).next;
            } else {
                *link = (*p).next;
                h.los_count -= 1;
                if (*header(o)).gc_flags.load(Ordering::Relaxed) & FLAG_WIPE != 0 {
                    wipe(o);
                    #[cfg(test)]
                    tests::WIPED.fetch_add(1, Ordering::Relaxed);
                }
                let (base, total) = (o.sub((*p).offset), los_total(&*p));
                if total >= LOS_MAP_MIN {
                    sys::unmap(base, total);
                } else {
                    sys::free_aligned(base);
                }
            }
        }
    }
    live
}

/// After a full collection's sweep: choose the evacuation candidates
/// (opportunistic defragmentation) from the exact line
/// states the sweep left: when free lines are at least a quarter of the
/// used blocks' lines, every block at most half occupied - if together
/// they hold an eighth of the used lines free. `ROKR_GC_EVAC=always`:
/// every used block. Whether there is any.
// Occupancy counts lines, not live bytes (no Immix hole histogram).
#[allow(clippy::naive_bytecount)] // 256 bytes per block
fn select_candidates() -> bool {
    let h = heap();
    if h.nblocks == 0 {
        return false;
    }
    // SAFETY: `nblocks` initialized metas.
    let metas = unsafe { core::slice::from_raw_parts_mut(h.meta, h.nblocks) };
    if h.evac_always {
        let mut any = false;
        for m in metas {
            m.evac = m.used;
            any |= m.used;
        }
        h.fresh_blocks = usize::MAX;
        return any;
    }
    let occupied = |m: &BlockMeta| m.lines.iter().filter(|&&l| l == LIVE).count();
    let (mut used_lines, mut free_lines, mut gain, mut moved) = (0, 0, 0, 0);
    for m in metas.iter().filter(|m| m.used) {
        let occ = occupied(m);
        used_lines += LINES;
        free_lines += LINES - occ;
        if occ <= LINES / 2 {
            gain += LINES - occ;
            moved += occ;
        }
    }
    if used_lines == 0 || free_lines * 4 < used_lines || gain * 8 < used_lines {
        return false;
    }
    for m in metas {
        m.evac = m.used && occupied(m) <= LINES / 2;
    }
    // At most half full each, the candidates fit in half as many blocks:
    // fresh ones may be taken when the holes and free blocks run out.
    h.fresh_blocks = moved.div_ceil(LINES) + 1;
    true
}

/// Copy every movable survivor out of the candidate blocks (leaving its
/// new address in the old header's `gc_word`, `FORWARDED`), rewrite
/// every precise reference to a moved object, then recompute the
/// candidates' lines. A survivor stays put if it is pinned, address-
/// taken (`NOMOVE`), reached through a root or an ambiguous word, or no
/// free line is left for it.
fn evacuate() {
    let h = heap();
    h.evacuations += 1;
    h.copy_cursor = 0;
    h.copy_limit = 0;
    h.copy_line = 0;
    h.copy_phase = 0;
    let (pinned, npinned) = (mark().pinned, mark().pinned_len);
    for i in 0..npinned {
        // SAFETY: `pinned_len` recorded live block objects.
        let o = unsafe { *pinned.add(i) };
        if in_candidate(o) {
            // SAFETY: as above.
            unsafe {
                (*header(o as *mut u8))
                    .gc_flags
                    .fetch_or(FLAG_NOEVAC, Ordering::Relaxed)
            };
        }
    }
    let (base, nblocks) = (h.base, h.nblocks);
    let mut moved = 0;
    for b in 0..nblocks {
        // SAFETY: `b < nblocks`; copied out, as `copy_alloc` may grow
        // (move) the meta table.
        let (evac, lines, starts) = unsafe {
            let m = &*heap().meta.add(b);
            (m.evac, m.lines, m.starts)
        };
        if !evac {
            continue;
        }
        let block = base + b * BLOCK_SIZE;
        for_each_start(&lines, &starts, true, |off| {
            let obj = (block + off) as *mut u8;
            // SAFETY: a survivor's header.
            let flags = unsafe { &(*header(obj)).gc_flags };
            let f = flags.fetch_and(!FLAG_NOEVAC, Ordering::Relaxed);
            if f & (FLAG_NOEVAC | FLAG_NOMOVE | FLAG_PINNED) != 0 {
                return;
            }
            let size = obj_size(obj);
            let Some(to) = copy_alloc(size) else {
                return;
            };
            // SAFETY: `to` is `size` free bytes outside every candidate.
            unsafe {
                ptr::copy_nonoverlapping(obj, to, size);
                (*header(obj)).gc_word = to as u64;
            }
            flags.fetch_or(FLAG_FORWARDED, Ordering::Relaxed);
            moved += size;
        });
    }
    if moved > 0 {
        fix_references();
    }
    for b in 0..nblocks {
        // SAFETY: `b < nblocks`; nothing else touches the table here.
        let m = unsafe { &mut *heap().meta.add(b) };
        if !m.evac {
            continue;
        }
        m.evac = false;
        let block = base + b * BLOCK_SIZE;
        let (lines, starts) = (m.lines, m.starts);
        m.lines = [FREE; LINES];
        for_each_start(&lines, &starts, true, |off| {
            let obj = (block + off) as *mut u8;
            // SAFETY: an object header (moved or not).
            let f = unsafe { (*header(obj)).gc_flags.load(Ordering::Relaxed) };
            if f & FLAG_FORWARDED != 0 {
                m.starts[off / 8 / 64] &= !(1 << (off / 8 % 64));
            } else {
                let end = (off + obj_size(obj)).div_ceil(LINE_SIZE);
                m.lines[off / LINE_SIZE..end].fill(LIVE);
            }
        });
        m.used = m.lines.contains(&LIVE);
    }
    heap().moved_bytes += moved as u64;
}

/// After the copies: rewrite every precise (or weak) reference word of
/// every survivor (copies included, old copies not) that points into a
/// moved object. Roots and ambiguous words only reach objects that
/// stayed; a weak word's referent is live (or the word was cleared).
// A second pass over the live heap: recording the slots that
// point into candidates during the mark needs the candidates before
// the mark, from stale line states (young objects refill the holes and
// look live, so sparse blocks are rarely seen).
#[allow(clippy::cast_ptr_alignment)] // reference words are 8-aligned
fn fix_references() {
    let fix = |o: *mut u8| {
        for_each_ref(o, 0, usize::MAX, |off, kind| {
            if kind == AMBIGUOUS {
                return;
            }
            // SAFETY: `off` is a reference word inside the live `o`.
            unsafe {
                let slot = o.add(off).cast::<usize>();
                let w = slot.read_unaligned();
                if !in_candidate(w) {
                    return;
                }
                if let Some(old) = object_in::<true>(w, false, true) {
                    slot.write_unaligned(forwarded(old as usize) + (w - old as usize));
                }
            }
        });
    };
    let h = heap();
    let (base, nblocks) = (h.base, h.nblocks);
    for b in 0..nblocks {
        // SAFETY: `b < nblocks`; `fix` only reads the table.
        let (used, lines, starts) = unsafe {
            let m = &*heap().meta.add(b);
            (m.used, m.lines, m.starts)
        };
        if !used {
            continue;
        }
        let block = base + b * BLOCK_SIZE;
        for_each_start(&lines, &starts, true, |off| {
            let obj = (block + off) as *mut u8;
            // SAFETY: a survivor's (or an old copy's) header.
            if unsafe { (*header(obj)).gc_flags.load(Ordering::Relaxed) } & FLAG_FORWARDED == 0 {
                fix(obj);
            }
        });
    }
    // SAFETY: walks the LOS list.
    unsafe {
        let mut o = heap().los;
        while !o.is_null() {
            fix(o);
            o = (*prefix(o)).next;
        }
    }
}

/// The new address of the object at `o` if it was evacuated, else `o`.
fn forwarded(o: usize) -> usize {
    // SAFETY: `o` is an object header.
    unsafe {
        let h = &*header(o as *mut u8);
        if h.gc_flags.load(Ordering::Relaxed) & FLAG_FORWARDED != 0 {
            h.gc_word as usize
        } else {
            o
        }
    }
}

/// Evacuation's bump allocator: `size` bytes in the free lines of a
/// block that is not a candidate (holes of used blocks first, then
/// free blocks, then fresh blocks within the budget). Its lines
/// become `LIVE` and its start bit is set. `None`: no room.
fn copy_alloc(size: usize) -> Option<*mut u8> {
    let h = heap();
    if size > h.copy_limit - h.copy_cursor && !next_copy_run(size.div_ceil(LINE_SIZE)) {
        return None;
    }
    let h = heap();
    let to = h.copy_cursor;
    h.copy_cursor += size;
    let b = (to - h.base) / BLOCK_SIZE;
    let off = to - h.base - b * BLOCK_SIZE;
    // SAFETY: `b < nblocks`.
    let m = unsafe { &mut *h.meta.add(b) };
    m.lines[off / LINE_SIZE..(off + size).div_ceil(LINE_SIZE)].fill(LIVE);
    m.starts[off / 8 / 64] |= 1 << (off / 8 % 64);
    m.used = true;
    m.released = false;
    Some(to as *mut u8)
}

/// Point the copy allocator at the next run of at least `lines` free
/// lines; false when there is none (and no fresh block may be taken).
fn next_copy_run(lines: usize) -> bool {
    let h = heap();
    while h.copy_phase < 2 {
        let total = h.nblocks * LINES;
        while h.copy_line < total {
            let (b, l) = (h.copy_line / LINES, h.copy_line % LINES);
            // SAFETY: `b < nblocks`.
            let m = unsafe { &*h.meta.add(b) };
            if m.evac || m.used != (h.copy_phase == 0) {
                h.copy_line = (b + 1) * LINES;
                continue;
            }
            let mut end = l;
            while end < LINES && m.lines[end] == FREE {
                end += 1;
            }
            h.copy_line = b * LINES + end.max(l + 1);
            if end - l >= lines {
                h.copy_cursor = h.base + b * BLOCK_SIZE + l * LINE_SIZE;
                h.copy_limit = h.base + b * BLOCK_SIZE + end * LINE_SIZE;
                return true;
            }
        }
        h.copy_phase += 1;
        h.copy_line = 0;
    }
    if h.fresh_blocks == 0 {
        return false;
    }
    h.fresh_blocks -= 1;
    let b = new_block();
    let h = heap();
    h.copy_cursor = h.base + b * BLOCK_SIZE;
    h.copy_limit = h.copy_cursor + BLOCK_SIZE;
    true
}

/// After a full collection: give the pages of the free blocks beyond
/// the next cycle's allocation budget back to the OS. They stay
/// reserved and writable, and fault back in zeroed when reused.
fn release_free_blocks() {
    let h = heap();
    let mut keep = h.next_gc.saturating_sub(h.live_bytes).div_ceil(BLOCK_SIZE);
    let mut b = 0;
    while b < h.nblocks {
        // SAFETY: `b < nblocks`.
        let m = unsafe { &*h.meta.add(b) };
        if m.used || m.released || keep > 0 {
            keep -= usize::from(!m.used && !m.released);
            b += 1;
            continue;
        }
        let first = b;
        while b < h.nblocks {
            // SAFETY: `b < nblocks`.
            let m = unsafe { &mut *h.meta.add(b) };
            if m.used || m.released {
                break;
            }
            m.released = true;
            b += 1;
        }
        // SAFETY: whole committed blocks of our reservation that hold no
        // object.
        let at = (h.base + first * BLOCK_SIZE) as *mut u8;
        if !unsafe { sys::reset(at, (b - first) * BLOCK_SIZE) } {
            rokr_common::oom();
        }
    }
}

/// The remembered set: old objects that had a reference
/// stored into them since the last collection, each logged once (its
/// `UNLOGGED` bit cleared by the barrier). Each thread appends to its
/// own chunk through `rok_tls_wb_log_cursor` / `_limit` without
/// a lock; a full chunk (and every thread's chunk at a collection or
/// thread exit) is retired onto the lock-free [`LOG_FULL`] stack.
#[repr(C)]
struct LogChunk {
    next: *mut LogChunk,
    /// End of the logged objects (`*mut u8` each, from `self + 1`).
    end: usize,
}

/// Words per log chunk (header included).
const LOG_CHUNK_WORDS: usize = 512;

/// Retired chunks: pushed by any thread, taken with the world stopped.
static LOG_FULL: AtomicPtr<LogChunk> = AtomicPtr::new(ptr::null_mut());

/// Retire the chunk behind the log slots `[cursor, limit]` (none if
/// null) onto `stack` and clear them.
fn retire_to([cursor, limit]: [*mut usize; 2], stack: &AtomicPtr<LogChunk>) {
    // SAFETY: the slots are the owner's (or a stopped / exiting
    // thread's); a non-null limit ends a chunk from `log_word`.
    unsafe {
        let end = limit.read();
        if end == 0 {
            return;
        }
        let c = (end - LOG_CHUNK_WORDS * size_of::<usize>()) as *mut LogChunk;
        (*c).end = cursor.read();
        push_chunks(stack, c, c);
        cursor.write(0);
        limit.write(0);
    }
}

/// Push the chunk list `first..=last` onto `stack`.
fn push_chunks(stack: &AtomicPtr<LogChunk>, first: *mut LogChunk, last: *mut LogChunk) {
    let mut head = stack.load(Ordering::Relaxed);
    loop {
        // SAFETY: `last` is a chunk nobody else holds.
        unsafe { (*last).next = head };
        match stack.compare_exchange_weak(head, first, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return,
            Err(h) => head = h,
        }
    }
}

/// Retire a thread's wb log chunk (the remembered set).
fn retire_log(slots: [*mut usize; 2]) {
    retire_to(slots, &LOG_FULL);
}

/// Append `word` to the calling thread's log behind `slots`, retiring a
/// full chunk onto `stack`.
fn log_word(slots: [*mut usize; 2], stack: &AtomicPtr<LogChunk>, word: usize) {
    // SAFETY: this thread's own slots: `cursor < limit` inside its chunk
    // (or both 0).
    unsafe {
        if slots[0].read() == slots[1].read() {
            retire_to(slots, stack);
            let c = libc::malloc(LOG_CHUNK_WORDS * size_of::<usize>()).cast::<LogChunk>();
            if c.is_null() {
                rokr_common::oom();
            }
            slots[0].write(c.add(1) as usize);
            slots[1].write(c as usize + LOG_CHUNK_WORDS * size_of::<usize>());
        }
        let c = slots[0].read();
        (c as *mut usize).write(word);
        slots[0].write(c + size_of::<usize>());
    }
}

/// `f` on every word logged in the chunk list `log`.
fn for_each_logged(mut log: *mut LogChunk, mut f: impl FnMut(*mut u8)) {
    // SAFETY: retired chunks hold words up to their `end`.
    unsafe {
        while !log.is_null() {
            let mut e = log.add(1).cast::<*mut u8>();
            while (e as usize) < (*log).end {
                f(*e);
                e = e.add(1);
            }
            log = (*log).next;
        }
    }
}

fn free_logs(mut log: *mut LogChunk) {
    while !log.is_null() {
        // SAFETY: chunks come from `log_word`'s malloc.
        unsafe {
            let next = (*log).next;
            libc::free(log.cast());
            log = next;
        }
    }
}

/// Before the calling thread leaves the registry: retire its logs so
/// the next collection (or the running mark) still sees them.
pub fn retire_own_logs() {
    retire_log(rokr_common::tls::wb_log());
    concurrent::retire_satb(rokr_common::tls::satb_log());
}

/// Write barrier for runtime code: call before storing a GC reference
/// into the word at `slot` of the GC object `obj`.
///
/// What compiled code inlines: the slow path for `UNLOGGED`, then the
/// card for `CARDED` (a concurrent mark sets both on old LOS objects).
///
/// # Safety
/// `obj` must be a live GC object and `slot` a word inside it.
pub unsafe fn write_barrier(obj: *mut u8, slot: *const u8) {
    // SAFETY: caller contract.
    let flags = unsafe { (*header(obj)).gc_flags.load(Ordering::Relaxed) };
    if flags & FLAG_UNLOGGED != 0 {
        // SAFETY: caller contract.
        unsafe { write_barrier_slow(obj) };
    }
    if flags & FLAG_CARDED != 0 {
        // SAFETY: a carded object is a LOS object with a card table.
        unsafe {
            let cards = (*prefix(obj)).cards;
            *cards.add((slot as usize - obj as usize) / CARD_SIZE) = 1;
        }
    }
}

/// `rok_gc_wb_slow`: remember `obj` for the next minor collection.
///
/// `obj` (old, `UNLOGGED`) is about to hold a reference that may be
/// young. Not a safepoint: no collection runs between the barrier and
/// the store that follows it. While a concurrent mark runs, every old
/// object is `UNLOGGED` and this is also its snapshot barrier
/// ([`concurrent::snapshot`]).
///
/// # Safety
/// `obj` must be a live GC object.
pub unsafe fn write_barrier_slow(obj: *mut u8) {
    // SAFETY: caller contract.
    let flags = unsafe { &(*header(obj)).gc_flags };
    if concurrent::MARKING.load(Ordering::Relaxed) {
        // SAFETY: caller contract.
        unsafe { concurrent::snapshot(obj) };
        return;
    }
    // Racing stores: exactly one thread logs it.
    if flags.fetch_and(!FLAG_UNLOGGED, Ordering::Relaxed) & FLAG_UNLOGGED == 0 {
        return;
    }
    log_word(rokr_common::tls::wb_log(), &LOG_FULL, obj as usize);
}
/// Bytes held by GC objects (headers included) as of the last
/// collection, plus everything handed out since (whole TLABs).
pub fn live_bytes() -> usize {
    world::lock();
    let n = heap().live_bytes;
    world::unlock();
    n
}

pub fn collections() -> u64 {
    world::lock();
    let n = heap().collections;
    world::unlock();
    n
}

/// Bytes evacuation has copied so far.
pub fn moved_bytes() -> u64 {
    world::lock();
    let n = heap().moved_bytes;
    world::unlock();
    n
}

#[cfg(test)]
#[allow(
    clippy::missing_const_for_fn,
    clippy::cast_ptr_alignment,
    clippy::items_after_statements
)]
mod tests {
    use super::*;

    #[repr(C)]
    struct Frame {
        prev: *mut Frame,
        count: usize,
    }

    #[test]
    #[allow(clippy::cast_ptr_alignment)] // GC objects are 8-aligned
    fn collect_frees_unreachable_and_keeps_traced_children() {
        // ScanTable: total 32, version 1, one STRONG_REF at offset 16.
        static TABLE: [u32; 4] = [32, 1 | (1 << 16), 16, STRONG_REF];
        rokr_common::tls::thread_init();
        world::attach_current();
        register_type(USER_TYPE_BASE, TABLE.as_ptr().cast());
        collect();
        let base = live_bytes();
        let a = alloc(32, 8, USER_TYPE_BASE);
        let b = alloc(24, 8, USER_TYPE_BASE + 1); // unregistered: leaf
        let garbage = alloc(24, 8, USER_TYPE_BASE + 1);
        // SAFETY: `a` has a 16-byte payload; frame layout matches
        // `Frame` + one slot; the frame is unlinked before it dies.
        unsafe {
            *garbage.add(16) = 7;
            *a.add(16).cast::<*mut u8>() = b;
            let mut junk = 0x1234usize; // inline `any` bits: ignored
            let mut root = a as usize;
            let mut frame: (Frame, *mut usize, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 2,
                },
                &mut root,
                &mut junk,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            assert_eq!(live_bytes(), base + 32 + 24);
            assert_eq!((*header(b)).type_id, USER_TYPE_BASE + 1);
            assert_eq!(object_in::<false>(garbage as usize + 16, false, true), None);
            *frame.1 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
        scan_words_buffers_and_interior_pointers();
        medium_and_large_objects();
        free_lines_are_reused();
        minor_collection_keeps_logged_children();
        minor_collection_scans_dirty_cards();
        evacuation_moves_objects_and_fixes_references();
        sum_payloads_scan_the_active_variant();
        concurrent_mark_keeps_the_snapshot();
        evacuation_defragments_sparse_blocks();
        weak_cells_clear_and_follow_moves();
        secrets_stay_in_place_and_are_wiped();
    }

    /// Secret buffers freed by a sweep, which wipes them first.
    pub static WIPED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

    /// A secret buffer: zeroed, a LOS object (never moved), kept by a
    /// collection while reachable, zeroed by `wipe`, and wiped by the
    /// sweep that frees it.
    fn secrets_stay_in_place_and_are_wiped() {
        let base = live_bytes();
        let s = alloc_secret(37);
        // SAFETY: `s` has a payload of 40 bytes; the frame is unlinked
        // before it dies.
        unsafe {
            let f = (*header(s)).gc_flags.load(Ordering::Relaxed);
            assert_eq!(f & (FLAG_LARGE | FLAG_WIPE), FLAG_LARGE | FLAG_WIPE);
            assert_eq!((*prefix(s)).size, HEADER_SIZE + 40);
            let pay = core::slice::from_raw_parts_mut(s.add(HEADER_SIZE), 40);
            assert!(pay.iter().all(|&b| b == 0));
            pay.fill(0xa5);
            let mut root = s as usize;
            let mut frame: (Frame, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 1,
                },
                &mut root,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            assert!(pay.iter().all(|&b| b == 0xa5));
            wipe(s);
            assert!(pay.iter().all(|&b| b == 0));
            pay.fill(0x5a);
            let wiped = WIPED.load(Ordering::Relaxed);
            *frame.1 = 0;
            collect();
            assert_eq!(WIPED.load(Ordering::Relaxed), wiped + 1);
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
    }

    /// Weak cells: a minor collection clears the one whose young referent
    /// died and keeps an old referent; an evacuation updates a moved
    /// referent's; a full collection clears it once nothing else holds the
    /// referent. A referent read during a concurrent mark survives it.
    #[allow(clippy::many_single_char_names)]
    fn weak_cells_clear_and_follow_moves() {
        let leaf = USER_TYPE_BASE + 1;
        heap().evac_always = true;
        let base = live_bytes();
        let a = alloc(32, 8, USER_TYPE_BASE); // one STRONG_REF @16
        let (x, y) = (alloc(24, 8, leaf), alloc(24, 8, leaf));
        set(y, 16, 0xA);
        set(a, 16, y as usize);
        let (cx, cy) = (weak_new(x), weak_new(y));
        let cell = |c: *mut u8| word(c, 16);
        // SAFETY: frame layout matches `Frame` + three slots, unlinked
        // before it dies.
        unsafe {
            let (mut ra, mut rx, mut ry) = (a as usize, cx as usize, cy as usize);
            let mut frame: (Frame, *mut usize, *mut usize, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 3,
                },
                &mut ra,
                &mut rx,
                &mut ry,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect_young();
            assert_eq!(cell(cx), 0);
            assert_eq!(cell(cy), y as usize);
            collect();
            let y2 = word(a, 16);
            assert_ne!(y2, y as usize);
            assert_eq!(cell(cy), y2);
            assert_eq!(word(weak_get(cy), 16), 0xA);
            collect_young();
            assert_eq!(cell(cy), y2);
            // A concurrent mark (held off): `y` is weakly reachable only
            // when it starts; read through the cell and stored into an
            // object younger than the mark (never scanned), it survives.
            set(a, 16, 0);
            heap().concurrent = Concurrent::Always;
            concurrent::HOLD.store(true, Ordering::Relaxed);
            world::lock();
            collect_locked(true, true);
            world::unlock();
            assert!(heap().marking);
            concurrent::lock_mark();
            let c = alloc(32, 8, USER_TYPE_BASE);
            set(c, 16, weak_get(cy) as usize);
            *frame.2 = c as usize;
            concurrent::unlock_mark();
            concurrent::HOLD.store(false, Ordering::Relaxed);
            world::lock();
            if heap().marking {
                concurrent::finish(false);
            }
            world::unlock();
            heap().concurrent = Concurrent::Auto;
            assert_eq!(cell(cy), word(c, 16));
            assert_eq!(word(weak_get(cy), 16), 0xA);
            *frame.2 = 0;
            collect();
            assert_eq!(cell(cy), 0);
            assert!(weak_get(cy).is_null());
            *frame.1 = 0;
            *frame.2 = 0;
            *frame.3 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
        heap().evac_always = false;
    }

    fn word(o: *mut u8, off: usize) -> usize {
        // SAFETY: tests read words inside their own objects.
        unsafe { o.add(off).cast::<usize>().read_unaligned() }
    }

    fn set(o: *mut u8, off: usize, v: usize) {
        // SAFETY: tests write words inside their own objects.
        unsafe { o.add(off).cast::<usize>().write_unaligned(v) };
    }

    /// `ROKR_GC_EVAC=always`: a full collection moves what it may and
    /// fixes every precise reference (header, interior `str`, boxed
    /// `any` payload, array-table buffer element); roots, ambiguous
    /// words, `NOMOVE` objects and inline `any` payloads behave.
    #[allow(clippy::many_single_char_names)]
    fn evacuation_moves_objects_and_fixes_references() {
        // A: {ref @16, str ptr @24, any tag @32 + payload @40}.
        static A: [u32; 8] = [
            48,
            1 | (3 << 16),
            16,
            STRONG_REF,
            24,
            INTERIOR_REF,
            32,
            ANY_SLOT,
        ];
        // R: a buffer of references (array table, 8-byte elements).
        static R: [u32; 4] = [8, 1 | TABLE_ARRAY as u32 | (1 << 16), 0, STRONG_REF];
        let (ta, tr, leaf) = (USER_TYPE_BASE + 10, USER_TYPE_BASE + 11, USER_TYPE_BASE + 1);
        register_type(ta, A.as_ptr().cast());
        register_type(tr, R.as_ptr().cast());
        heap().evac_always = true;
        let base = live_bytes();
        let a = alloc(48, 8, ta);
        let a2 = alloc(48, 8, ta);
        let (b, c, d, e, f, g) = (
            alloc(24, 8, leaf),
            alloc(32, 8, leaf),
            alloc(24, 8, leaf),
            alloc(24, 8, leaf),
            alloc(24, 8, leaf),
            alloc(24, 8, leaf),
        );
        let r = alloc(16 + 16, 8, tr);
        let w = alloc(16 + 8, 8, SCAN_WORDS_TYPE_ID);
        for (o, v) in [(b, 0xB), (c, 0xC), (d, 0xD), (e, 0xE), (f, 0xF)] {
            set(o, 16, v);
        }
        set(a, 16, b as usize);
        set(a, 24, c as usize + 20);
        // SAFETY: tests write their own objects' headers / words.
        unsafe {
            a.add(32).cast::<u32>().write(0x1234 | ANY_BOXED);
            a2.add(32).cast::<u32>().write(0x1234); // inline: payload is bits
            no_move(f);
        }
        set(a, 40, r as usize);
        set(a2, 40, g as usize);
        set(r, 16, e as usize);
        set(r, 24, f as usize);
        set(w, 16, d as usize);
        // SAFETY: frame layout matches `Frame` + three slots, unlinked
        // before it dies.
        unsafe {
            let (mut ra, mut ra2, mut rw) = (a as usize, a2 as usize, w as usize);
            let mut frame: (Frame, *mut usize, *mut usize, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 3,
                },
                &mut ra,
                &mut ra2,
                &mut rw,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            let moved = heap().moved_bytes;
            collect();
            assert!(heap().moved_bytes > moved);
            // Rooted objects stay; what they reach precisely moves.
            assert_eq!((ra, ra2, rw), (a as usize, a2 as usize, w as usize));
            let b2 = word(a, 16) as *mut u8;
            assert_ne!(b2, b);
            assert_eq!(word(b2, 16), 0xB);
            let c2 = (word(a, 24) - 20) as *mut u8;
            assert_ne!(c2, c);
            assert_eq!(word(c2, 16), 0xC);
            let r2 = word(a, 40) as *mut u8;
            assert_ne!(r2, r);
            assert_eq!((*header(r2)).type_id, tr);
            let e2 = word(r2, 16) as *mut u8;
            assert_ne!(e2, e);
            assert_eq!(word(e2, 16), 0xE);
            // Address-taken, ambiguous: in place.
            assert_eq!(word(r2, 24), f as usize);
            assert_eq!(word(f, 16), 0xF);
            assert_eq!(word(w, 16), d as usize);
            assert_eq!(word(d, 16), 0xD);
            // `g` was only behind an inline `any` payload: dead.
            assert_eq!(object_in::<false>(g as usize, false, true), None);
            assert_eq!(object_in::<false>(b as usize, false, true), None);
            assert_eq!(live_bytes(), base + 48 * 2 + 24 * 4 + 32 + 32 + 24);
            // A second round moves the copies again.
            collect();
            let b3 = word(a, 16) as *mut u8;
            assert_eq!(word(b3, 16), 0xB);
            assert_eq!(word(word(word(a, 40) as *mut u8, 16) as *mut u8, 16), 0xE);
            *frame.1 = 0;
            *frame.2 = 0;
            *frame.3 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
        heap().evac_always = false;
    }

    /// A concurrent mark (held off by the test, so the stores below
    /// happen mid-mark): a reference moved out of a scanned-later old
    /// object into a new one survives through the snapshot barrier; a new
    /// object stored into an old one is young afterwards and survives the
    /// next minor collection through the carried remembered set.
    fn concurrent_mark_keeps_the_snapshot() {
        let leaf = USER_TYPE_BASE + 1;
        let base = live_bytes();
        let a = alloc(32, 8, USER_TYPE_BASE);
        let b = alloc(24, 8, leaf);
        set(b, 16, 0xB);
        set(a, 16, b as usize);
        // SAFETY: frame layout matches `Frame` + two slots, unlinked
        // before it dies.
        unsafe {
            let (mut ra, mut rc) = (a as usize, 0usize);
            let mut frame: (Frame, *mut usize, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 2,
                },
                &mut ra,
                &mut rc,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            // `b` may have moved (evacuation); `a` is rooted.
            let b = word(a, 16) as *mut u8;
            heap().concurrent = Concurrent::Always;
            concurrent::HOLD.store(true, Ordering::Relaxed);
            world::lock();
            collect_locked(true, true);
            world::unlock();
            assert!(heap().marking);
            concurrent::lock_mark();
            let c = alloc(32, 8, USER_TYPE_BASE);
            set(c, 16, b as usize);
            *frame.2 = c as usize;
            write_barrier(a, a.add(16));
            set(a, 16, 0);
            let d = alloc(24, 8, leaf);
            set(d, 16, 0xD);
            write_barrier(a, a.add(16));
            set(a, 16, d as usize);
            concurrent::unlock_mark();
            concurrent::HOLD.store(false, Ordering::Relaxed);
            world::lock();
            if heap().marking {
                concurrent::finish(false);
            }
            world::unlock();
            heap().concurrent = Concurrent::Auto;
            assert_eq!(object_in::<false>(b as usize, false, true), Some(b));
            assert_eq!(word(b, 16), 0xB);
            collect_young();
            assert_eq!(word(a, 16), d as usize);
            assert_eq!(object_in::<false>(d as usize, false, true), Some(d));
            assert_eq!(word(d, 16), 0xD);
            assert_eq!(live_bytes(), base + 32 * 2 + 24 * 2);
            *frame.1 = 0;
            *frame.2 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
    }

    /// `SUM_PAYLOAD`: only the active variant's words are references -
    /// they move and are fixed; the same word under a scalar variant
    /// keeps nothing alive.
    #[allow(clippy::many_single_char_names)]
    fn sum_payloads_scan_the_active_variant() {
        // {u32 tag @16, payload @24..40}: variant 0 a reference @+8,
        // variant 2 a `str` @+16, variant 1 scalars. Descriptor at word
        // 4, variant entries at words 11 and 13.
        let (ts, leaf) = (USER_TYPE_BASE + 13, USER_TYPE_BASE + 1);
        static U: [u32; 15] = [
            40,
            1 | (1 << 16),
            16,
            SUM_PAYLOAD | (4 << SUM_DESC_SHIFT),
            2,
            0,
            1,
            11,
            2,
            1,
            13,
            8,
            STRONG_REF,
            16,
            INTERIOR_REF,
        ];
        register_type(ts, U.as_ptr().cast());
        heap().evac_always = true;
        let base = live_bytes();
        let (a, b, c) = (alloc(40, 8, ts), alloc(40, 8, ts), alloc(40, 8, ts));
        let (x, y, z) = (alloc(24, 8, leaf), alloc(24, 8, leaf), alloc(32, 8, leaf));
        set(x, 16, 0xA);
        set(z, 16, 0xC);
        // SAFETY: tests write their own objects' words.
        unsafe {
            a.add(16).cast::<u32>().write(0);
            b.add(16).cast::<u32>().write(1);
            c.add(16).cast::<u32>().write(2);
        }
        set(a, 24, x as usize);
        set(b, 24, y as usize); // scalar bits under variant 1
        set(c, 32, z as usize + 20); // `str` into `z`
        // SAFETY: frame layout matches `Frame` + three slots.
        unsafe {
            let (mut ra, mut rb, mut rc) = (a as usize, b as usize, c as usize);
            let mut frame: (Frame, *mut usize, *mut usize, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 3,
                },
                &mut ra,
                &mut rb,
                &mut rc,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            let x2 = word(a, 24) as *mut u8;
            assert_ne!(x2, x);
            assert_eq!(word(x2, 16), 0xA);
            let z2 = (word(c, 32) - 20) as *mut u8;
            assert_ne!(z2, z);
            assert_eq!(word(z2, 16), 0xC);
            assert_eq!(word(b, 24), y as usize);
            assert_eq!(object_in::<false>(y as usize, false, true), None);
            assert_eq!(live_bytes(), base + 40 * 3 + 24 + 32);
            *frame.1 = 0;
            *frame.2 = 0;
            *frame.3 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
        heap().evac_always = false;
    }

    /// Default mode: after most of a block-filling population dies, the
    /// sparse blocks are emptied into one and the rest freed.
    fn evacuation_defragments_sparse_blocks() {
        static R: [u32; 4] = [8, 1 | TABLE_ARRAY as u32 | (1 << 16), 0, STRONG_REF];
        let (tr, leaf) = (USER_TYPE_BASE + 12, USER_TYPE_BASE + 1);
        register_type(tr, R.as_ptr().cast());
        assert!(!heap().evac_always);
        collect();
        let base = live_bytes();
        const N: usize = 4096;
        let r = alloc(16 + 8 * N, 8, tr);
        let used = || {
            // SAFETY: `nblocks` initialized metas.
            unsafe { core::slice::from_raw_parts(heap().meta, heap().nblocks) }
                .iter()
                .filter(|m| m.used)
                .count()
        };
        // SAFETY: as above.
        unsafe {
            let mut root = r as usize;
            let mut frame: (Frame, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 1,
                },
                &mut root,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            let before = used();
            for i in 0..N {
                let o = alloc(64, 8, leaf);
                set(o, 16, i);
                set(r, 16 + 8 * i, o as usize);
            }
            collect();
            let full = used();
            assert!(full >= before + N * 64 / BLOCK_SIZE, "{before} -> {full}");
            for i in (0..N).filter(|i| i % 16 != 0) {
                set(r, 16 + 8 * i, 0);
            }
            let moved = heap().moved_bytes;
            collect(); // sweeps the dead: blocks 1/16 live
            collect(); // evacuates them
            assert!(heap().moved_bytes - moved >= (N / 16 * 64) as u64);
            assert!(used() <= before + 2, "{before} -> {full} -> {}", used());
            for i in (0..N).step_by(16) {
                assert_eq!(word(word(r, 16 + 8 * i) as *mut u8, 16), i);
            }
            assert_eq!(live_bytes(), base + 16 + 8 * N + N / 16 * 64);
            *frame.1 = 0;
            collect();
            *top = ptr::null_mut();
        }
    }

    /// Generational: a young object reachable only through an old one
    /// (stored behind the write barrier) survives minor collections;
    /// unreachable young objects do not.
    #[allow(clippy::cast_ptr_alignment)] // GC objects are 8-aligned
    fn minor_collection_keeps_logged_children() {
        let base = live_bytes();
        let old = alloc(32, 8, USER_TYPE_BASE);
        // SAFETY: as above.
        unsafe {
            let mut root = old as usize;
            let mut frame: (Frame, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 1,
                },
                &mut root,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            let flags = (*header(old)).gc_flags.load(Ordering::Relaxed);
            assert_eq!(
                flags & (FLAG_STICKY | FLAG_UNLOGGED),
                FLAG_STICKY | FLAG_UNLOGGED
            );
            let young = alloc(24, 8, USER_TYPE_BASE + 1);
            let garbage = alloc(24, 8, USER_TYPE_BASE + 1);
            write_barrier(old, old.add(16));
            *old.add(16).cast::<*mut u8>() = young;
            let minors = heap().minors;
            collect_young();
            collect_young();
            assert_eq!(heap().minors, minors + 2);
            assert_eq!((*header(young)).type_id, USER_TYPE_BASE + 1);
            assert_eq!(object_in::<false>(garbage as usize, false, true), None);
            assert_eq!(live_bytes(), base + 32 + 24);
            *old.add(16).cast::<*mut u8>() = ptr::null_mut();
            *frame.1 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
    }

    /// Generational: an old LOS buffer is carded; a young object stored
    /// into it (behind the barrier) survives minors through its card
    /// alone; once overwritten it dies at the next minor.
    #[allow(clippy::cast_ptr_alignment)] // GC objects are 8-aligned
    fn minor_collection_scans_dirty_cards() {
        let base = live_bytes();
        let size = 64 << 10;
        let buf = alloc(size, 8, SCAN_WORDS_TYPE_ID);
        // SAFETY: as above.
        unsafe {
            let mut root = buf as usize;
            let mut frame: (Frame, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 1,
                },
                &mut root,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            let flags = (*header(buf)).gc_flags.load(Ordering::Relaxed);
            assert_eq!(
                flags & (FLAG_STICKY | FLAG_CARDED),
                FLAG_STICKY | FLAG_CARDED
            );
            let young = alloc(24, 8, USER_TYPE_BASE + 1);
            let slot = buf.add(40_000);
            write_barrier(buf, slot);
            *slot.cast::<*mut u8>() = young;
            assert_eq!(*(*prefix(buf)).cards.add(40_000 / CARD_SIZE), 1);
            collect_young();
            assert_eq!((*header(young)).type_id, USER_TYPE_BASE + 1);
            assert_eq!(live_bytes(), base + size + 24);
            let young2 = alloc(24, 8, USER_TYPE_BASE + 1);
            write_barrier(buf, slot);
            *slot.cast::<*mut u8>() = young2;
            collect_young();
            // `young` is old now (it survived a minor): only a full
            // collection frees it.
            assert_eq!(live_bytes(), base + size + 48);
            collect();
            assert_eq!(live_bytes(), base + size + 24);
            *frame.1 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
    }

    // Called from the test above: the heap is a single-mutator global,
    // so GC tests must not run on parallel test threads.
    #[allow(clippy::cast_ptr_alignment)] // GC objects are 8-aligned
    fn scan_words_buffers_and_interior_pointers() {
        let base = live_bytes();
        let buf = alloc(16 + 16, 8, SCAN_WORDS_TYPE_ID);
        let s = alloc(24, 8, USER_TYPE_BASE + 7);
        // SAFETY: as above; the buffer holds an interior pointer into `s`
        // and a non-pointer word; only the buffer's payload is rooted.
        unsafe {
            *buf.add(16).cast::<usize>() = 0x99;
            *buf.add(24).cast::<*mut u8>() = s.add(18);
            let mut root = buf as usize + 20; // interior root
            let mut frame: (Frame, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 1,
                },
                &mut root,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            assert_eq!(live_bytes(), base + 32 + 24);
            *frame.1 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
    }

    /// Medium objects (several lines) and LOS objects are found through
    /// interior pointers and freed when unreachable.
    fn medium_and_large_objects() {
        let base = live_bytes();
        let medium = alloc(1000, 8, USER_TYPE_BASE + 1);
        let large = alloc(LOS_THRESHOLD + 8, 8, USER_TYPE_BASE + 1);
        // SAFETY: as above.
        unsafe {
            assert_ne!(
                (*header(large)).gc_flags.load(Ordering::Relaxed) & FLAG_LARGE,
                0
            );
            let mut r1 = medium as usize + 900;
            let mut r2 = large as usize + 5000;
            let mut frame: (Frame, *mut usize, *mut usize) = (
                Frame {
                    prev: ptr::null_mut(),
                    count: 2,
                },
                &mut r1,
                &mut r2,
            );
            let top = rokr_common::tls::shadow_stack_top();
            *top = (&raw mut frame).cast();
            collect();
            assert_eq!(live_bytes(), base + 1000 + LOS_THRESHOLD + 8);
            *frame.1 = 0;
            *frame.2 = 0;
            collect();
            assert_eq!(live_bytes(), base);
            *top = ptr::null_mut();
        }
        // An over-aligned payload starts on its alignment.
        for align in [16, 32, 64, 4096] {
            let o = alloc(40, align, USER_TYPE_BASE + 1);
            assert_eq!((o as usize + HEADER_SIZE) % align, 0);
        }
        collect();
        assert_eq!(live_bytes(), base);
        // A big one has pages of its own, gone once it dies.
        let big = alloc(1 << 20, 8, USER_TYPE_BASE + 1);
        // SAFETY: the object and its prefix are live until the collection.
        let at = unsafe { big.sub((*prefix(big)).offset) };
        assert_eq!(at as usize % 4096, 0);
        collect();
        assert_eq!(live_bytes(), base);
        #[cfg(unix)]
        {
            // SAFETY: msync only queries the range; ENOMEM: not mapped.
            let rc = unsafe { libc::msync(at.cast(), 4096, libc::MS_ASYNC) };
            assert_eq!(rc, -1);
        }
    }

    /// After a collection the dead objects' lines are handed out again
    /// (zeroed) instead of growing the heap.
    fn free_lines_are_reused() {
        collect();
        let blocks = heap().nblocks;
        for _ in 0..3 {
            for _ in 0..(4 * BLOCK_SIZE / 64) {
                let p = alloc(64, 8, USER_TYPE_BASE + 1);
                // SAFETY: a fresh 64-byte object.
                unsafe {
                    assert_eq!(*p.add(40), 0);
                    *p.add(40) = 1;
                }
            }
            collect();
        }
        assert!(heap().nblocks <= blocks + 5, "{} blocks", heap().nblocks);
    }
}
