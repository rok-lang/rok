//! Mutator TLS (spec 008-abi/803).
//!
//! The slots of 803 §8.3.1 are one exported `thread_local` block,
//! `rok_tls`, at the offsets of `tls_abi`. A rok function gets the
//! block's address from its caller as a hidden last argument (8.3.8);
//! code entered from outside rok reads it once. Defined in assembly
//! because stable `no_std` Rust cannot export TLS symbols. The runtime
//! reaches its own block through the hidden `rok_tls_addr`, another
//! thread's through the thread registry (`rokr_gc::world`).
//!
//! Mach-O: `__thread_vars`. ELF: initial-exec on `x86_64`, local-exec
//! elsewhere (the runtime is always linked into the executable; arm reads
//! `TPIDRURO`). COFF: `.tls$` through `_tls_index`, which rokc's startup
//! object defines with the TLS directory. wasm32: a plain global, or wasm
//! TLS on `wasm32-wasi-threads` (defined by rokc's startup object).
//!
//! `{size}` in the assembly is the block's size.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::tls_abi::{self, Slot};

pub const MUTATOR_RUNNING: u8 = 0;
pub const MUTATOR_GC_COOPERATING: u8 = 1;
pub const MUTATOR_PARKED: u8 = 2;

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(0);

/// The block `rok_tls` and its accessor `rok_tls_addr`.
#[cfg(not(target_family = "wasm"))]
macro_rules! tls_block {
    () => {
        #[cfg(all(target_vendor = "apple", target_arch = "aarch64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@macho),
            ".private_extern _rok_tls_addr\n",
            ".globl _rok_tls_addr\n",
            ".p2align 2\n",
            "_rok_tls_addr:\n",
            "adrp x0, _rok_tls@TLVPPAGE\n",
            "ldr x0, [x0, _rok_tls@TLVPPAGEOFF]\n",
            "ldr x16, [x0]\n",
            // Tail-call the TLV thunk: it returns the address in x0.
            "br x16\n",
        ), size = const SIZE);
        #[cfg(all(target_vendor = "apple", target_arch = "x86_64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@macho),
            ".private_extern _rok_tls_addr\n",
            ".globl _rok_tls_addr\n",
            "_rok_tls_addr:\n",
            "mov rdi, qword ptr [rip + _rok_tls@TLVP]\n",
            "jmp qword ptr [rdi]\n",
        ), size = const SIZE);
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@elf "@"),
            ".globl rok_tls_addr\n",
            ".hidden rok_tls_addr\n",
            ".type rok_tls_addr,@function\n",
            "rok_tls_addr:\n",
            "mov rax, qword ptr fs:[0]\n",
            "add rax, qword ptr [rip + rok_tls@GOTTPOFF]\n",
            "ret\n",
        ), size = const SIZE);
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@elf "@"),
            ".globl rok_tls_addr\n",
            ".hidden rok_tls_addr\n",
            ".type rok_tls_addr,%function\n",
            "rok_tls_addr:\n",
            "mrs x0, tpidr_el0\n",
            "add x0, x0, :tprel_hi12:rok_tls\n",
            "add x0, x0, :tprel_lo12_nc:rok_tls\n",
            "ret\n",
        ), size = const SIZE);
        #[cfg(all(target_os = "linux", target_arch = "x86"))]
        core::arch::global_asm!(concat!(
            tls_block!(@elf "@"),
            ".globl rok_tls_addr\n",
            ".hidden rok_tls_addr\n",
            ".type rok_tls_addr,@function\n",
            "rok_tls_addr:\n",
            "mov eax, dword ptr gs:[0]\n",
            "lea eax, [eax + rok_tls@NTPOFF]\n",
            "ret\n",
        ), size = const SIZE);
        #[cfg(all(target_os = "linux", target_arch = "arm"))]
        core::arch::global_asm!(concat!(
            tls_block!(@elf "%"),
            ".globl rok_tls_addr\n",
            ".hidden rok_tls_addr\n",
            ".type rok_tls_addr,%function\n",
            ".p2align 2\n",
            "rok_tls_addr:\n",
            "mrc p15, 0, r0, c13, c0, 3\n",
            "ldr r1, 1f\n",
            "add r0, r0, r1\n",
            "bx lr\n",
            "1: .long rok_tls(TPOFF)\n",
        ), size = const SIZE);
        #[cfg(all(target_os = "linux", target_arch = "riscv64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@elf "@"),
            ".globl rok_tls_addr\n",
            ".hidden rok_tls_addr\n",
            ".type rok_tls_addr,@function\n",
            "rok_tls_addr:\n",
            "lui a0, %tprel_hi(rok_tls)\n",
            "add a0, a0, tp, %tprel_add(rok_tls)\n",
            "addi a0, a0, %tprel_lo(rok_tls)\n",
            "ret\n",
        ), size = const SIZE);
        #[cfg(all(windows, target_arch = "x86_64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@coff ""),
            ".globl rok_tls_addr\n",
            ".p2align 4\n",
            "rok_tls_addr:\n",
            "mov eax, dword ptr [rip + _tls_index]\n",
            "mov rcx, qword ptr gs:[0x58]\n",
            "mov rcx, qword ptr [rcx + 8*rax]\n",
            "lea rax, [rcx + rok_tls@SECREL32]\n",
            "ret\n",
        ), size = const SIZE);
        #[cfg(all(windows, target_arch = "aarch64"))]
        core::arch::global_asm!(concat!(
            tls_block!(@coff ""),
            ".globl rok_tls_addr\n",
            ".p2align 2\n",
            "rok_tls_addr:\n",
            "adrp x8, _tls_index\n",
            "ldr w8, [x8, :lo12:_tls_index]\n",
            "ldr x9, [x18, #0x58]\n",
            "ldr x9, [x9, x8, lsl #3]\n",
            "add x9, x9, :secrel_hi12:rok_tls\n",
            "add x0, x9, :secrel_lo12:rok_tls\n",
            "ret\n",
        ), size = const SIZE);
        // C names carry a leading underscore on x86 COFF.
        #[cfg(all(windows, target_arch = "x86"))]
        core::arch::global_asm!(concat!(
            tls_block!(@coff "_"),
            ".globl _rok_tls_addr\n",
            ".p2align 4\n",
            "_rok_tls_addr:\n",
            "mov eax, dword ptr [__tls_index]\n",
            "mov ecx, dword ptr fs:[0x2c]\n",
            "mov ecx, dword ptr [ecx + 4*eax]\n",
            "lea eax, [ecx + _rok_tls@SECREL32]\n",
            "ret\n",
        ), size = const SIZE);
    };
    // The template and the block's TLV descriptor.
    (@macho) => {
        concat!(
            ".tbss _rok_tls$tlv$init, {size}, 3\n",
            ".section __DATA,__thread_vars,thread_local_variables\n",
            ".globl _rok_tls\n",
            "_rok_tls:\n",
            ".quad __tlv_bootstrap\n.quad 0\n.quad _rok_tls$tlv$init\n",
            ".text\n",
        )
    };
    // `$t`: the `@` / `%` of `.type`.
    (@elf $t:literal) => {
        concat!(
            ".pushsection .tbss,\"awT\",", $t, "nobits\n",
            ".p2align 3\n",
            ".globl rok_tls\n",
            ".type rok_tls,", $t, "object\n",
            ".size rok_tls,{size}\n",
            "rok_tls:\n",
            ".zero {size}\n",
            ".popsection\n",
        )
    };
    (@coff $u:literal) => {
        concat!(
            ".section .tls$,\"dw\"\n",
            ".p2align 3\n",
            ".globl ", $u, "rok_tls\n",
            $u, "rok_tls:\n",
            ".zero {size}\n",
            ".text\n",
        )
    };
}

// `wasm32-wasi-threads` gets the block from rokc (`wasm_tls_object`,
// rokc_driver toolchain.rs).
#[cfg(not(target_family = "wasm"))]
tls_block!();

// The assembly's `{size}` (wasm32 has no assembly).
#[cfg(not(target_family = "wasm"))]
const SIZE: usize = tls_abi::SIZE.offset(size_of::<usize>() as u32) as usize;

#[cfg(not(any(
    target_vendor = "apple",
    target_family = "wasm",
    all(
        any(target_os = "linux", windows),
        any(target_arch = "x86_64", target_arch = "aarch64", target_arch = "x86")
    ),
    all(target_os = "linux", any(target_arch = "arm", target_arch = "riscv64"))
)))]
compile_error!("rokr: no `rok_tls` definition for this target");

/// Address of this thread's block (on `wasm32-wasi-threads` rokc's
/// startup object defines the block and the accessor: stable Rust cannot
/// define wasm TLS).
#[cfg(not(rok_solo))]
fn block() -> *mut u8 {
    unsafe extern "C" {
        fn rok_tls_addr() -> *mut u8;
    }
    // SAFETY: the accessor takes no arguments and only returns the
    // address of this thread's block.
    unsafe { rok_tls_addr() }
}

/// One thread: the block is a plain global (what LLVM makes of compiled
/// code's `thread_local` globals without the atomics feature).
#[cfg(rok_solo)]
fn block() -> *mut u8 {
    const N: usize = tls_abi::SIZE.offset(4) as usize;
    #[repr(C, align(8))]
    struct Block(core::cell::UnsafeCell<[u8; N]>);
    // SAFETY: one thread.
    unsafe impl Sync for Block {}
    #[unsafe(export_name = "rok_tls")]
    static BLOCK: Block = Block(core::cell::UnsafeCell::new([0; N]));
    BLOCK.0.get().cast()
}

/// Address of slot `s` of this thread's block.
fn at(s: Slot) -> *mut u8 {
    // SAFETY: the slot lies inside the block.
    unsafe { block().add(s.offset(size_of::<usize>() as u32) as usize) }
}

/// Address of this thread's `rok_tls_stack_guard` (`[lo, hi)`).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn stack_guard() -> *mut [usize; 2] {
    at(tls_abi::STACK_GUARD).cast()
}

/// Address of this thread's `rok_tls_shadow_stack_top` (the head of its
/// shadow-stack frame chain, pushed / popped by compiled code).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn shadow_stack_top() -> *mut *mut u8 {
    at(tls_abi::SHADOW_STACK_TOP).cast()
}

/// The `rok_tls_yield_flag` bit of a stop (the runtime raises and clears
/// it).
///
/// Other bits are the embedder's: the Windows REPL host raises one to
/// end a running cell. Compiled code's polls take the slow path for any
/// nonzero flag.
pub const YIELD_STOP: u8 = 1;

/// This thread's `rok_tls_yield_flag` (set by a collecting thread).
pub fn yield_flag() -> &'static AtomicU8 {
    // SAFETY: the slot is a live, 1-byte TLS slot of this thread;
    // `AtomicU8` has the layout of `u8`.
    unsafe { &*at(tls_abi::YIELD_FLAG).cast::<AtomicU8>() }
}

/// This thread's `rok_tls_mutator_state` (read by a collecting thread).
pub fn mutator_state() -> &'static AtomicU8 {
    // SAFETY: as above.
    unsafe { &*at(tls_abi::MUTATOR_STATE).cast::<AtomicU8>() }
}

/// This thread's depth of bracketed `#[no_safepoint]` calls.
pub fn no_safepoint() -> *mut u8 {
    at(tls_abi::NO_SAFEPOINT)
}

/// `attached()`: started by rok, or `rok_thread_init`.
pub const ATTACHED_INIT: u8 = 1;
/// `attached()`: a thread rok did not start, attached by a callback.
pub const ATTACHED_CALLBACK: u8 = 2;

/// How this thread joined the runtime: 0 (not, or left), or one of
/// `ATTACHED_*`. A new thread's block is zeroed.
pub fn attached() -> *mut u8 {
    at(tls_abi::ATTACHED)
}

/// `rok_thread_init` slot values (803 §8.3.6). The TLAB starts empty
/// (the first allocation refills it); the write barrier logs into its
/// own remembered-set chunk, allocated by the first logged store.
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slots
pub fn thread_init() {
    yield_flag().store(0, Ordering::Relaxed);
    mutator_state().store(MUTATOR_RUNNING, Ordering::Relaxed);
    // SAFETY: each slot is this thread's own and aligned.
    unsafe {
        for s in [
            tls_abi::TLAB_CURSOR,
            tls_abi::TLAB_LIMIT,
            tls_abi::SHADOW_STACK_TOP,
            tls_abi::WB_LOG_CURSOR,
            tls_abi::WB_LOG_LIMIT,
            tls_abi::SATB_CURSOR,
            tls_abi::SATB_LIMIT,
            tls_abi::TASK_WORKER,
            tls_abi::TASK_LANE,
            tls_abi::HELD_LOCKS,
        ] {
            at(s).cast::<usize>().write(0);
        }
        worker_id().write(new_worker_id());
        no_safepoint().write(0);
        attached().write(ATTACHED_INIT);
    }
}

/// Addresses of this thread's `rok_tls_wb_log_cursor` and
/// `rok_tls_wb_log_limit` (its remembered-set chunk, `rokr_gc`).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slots
pub fn wb_log() -> [*mut usize; 2] {
    [
        at(tls_abi::WB_LOG_CURSOR).cast(),
        at(tls_abi::WB_LOG_LIMIT).cast(),
    ]
}

/// Addresses of this thread's `rok_tls_satb_cursor` and
/// `rok_tls_satb_limit` (its SATB log chunk, `rokr_gc`).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slots
pub fn satb_log() -> [*mut usize; 2] {
    [
        at(tls_abi::SATB_CURSOR).cast(),
        at(tls_abi::SATB_LIMIT).cast(),
    ]
}

/// Addresses of this thread's `rok_tls_tlab_cursor` and
/// `rok_tls_tlab_limit` (the bump range compiled code allocates from).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slots
pub fn tlab() -> [*mut usize; 2] {
    [
        at(tls_abi::TLAB_CURSOR).cast(),
        at(tls_abi::TLAB_LIMIT).cast(),
    ]
}

/// Address of this thread's `rok_tls_task_worker`.
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn task_worker() -> *mut usize {
    at(tls_abi::TASK_WORKER).cast()
}

/// Address of this thread's head of running static initializers
/// (implementation-defined).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn static_inits() -> *mut usize {
    at(tls_abi::STATIC_INITS).cast()
}

/// Address of this thread's record of the locks it holds outside the
/// task executor (`rokr_gc::world::note_lock`, implementation-defined),
/// null = none yet.
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn held_locks() -> *mut usize {
    at(tls_abi::HELD_LOCKS).cast()
}

/// Address of this thread's name, `{ptr, len}` (implementation-defined;
/// owned by `rokr::thread`).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn thread_name() -> *mut [usize; 2] {
    at(tls_abi::THREAD_NAME).cast()
}

/// Address of this thread's executor lane (implementation-defined).
#[allow(clippy::cast_ptr_alignment)] // pointer-aligned slot
pub fn task_lane() -> *mut usize {
    at(tls_abi::TASK_LANE).cast()
}

/// Address of this thread's `rok_tls_worker_id`.
#[allow(clippy::cast_ptr_alignment)] // 8-aligned slot
pub fn worker_id() -> *mut u64 {
    at(tls_abi::WORKER_ID).cast()
}

/// A fresh, process-unique worker id.
pub fn new_worker_id() -> u64 {
    NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed)
}
