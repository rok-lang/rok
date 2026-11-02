//! Shared runtime primitives: object header layout, mutator TLS block,
//! the OS layer (`sys`), the raw fd writer and the fatal-error path.

#![no_std]

pub mod gc_abi;
pub mod object_header;
pub mod tls;
pub mod tls_abi;
#[cfg(windows)]
pub mod win;
pub mod wtf8;

#[cfg_attr(any(unix, target_os = "wasi"), path = "sys_unix.rs")]
#[cfg_attr(windows, path = "sys_windows.rs")]
pub mod sys;

pub use sys::{errno, write_all};

/// `len` bytes at `msg` to stderr. Diagnostics: a failure is dropped.
///
/// # Safety
/// `msg` must be valid for reads of `len` bytes (or `len` must be 0).
pub unsafe fn write_stderr(msg: *const u8, len: usize) {
    if len > 0 {
        // SAFETY: caller contract.
        write_all(2, &[unsafe { core::slice::from_raw_parts(msg, len) }]);
    }
}

/// Out of memory (fatal, exit 101): the one
/// path every allocation failure and `rok::core::panic::oom()` take.
pub fn oom() -> ! {
    let hook = OOM_HOOK.swap(core::ptr::null_mut(), core::sync::atomic::Ordering::Relaxed);
    if !hook.is_null() {
        // SAFETY: set from a `fn()` by `set_oom_hook`.
        unsafe { core::mem::transmute::<*mut (), fn()>(hook)() };
    }
    fatal(b"fatal error: out of memory")
}

/// Called once by the first [`oom`] (`ROKR_HEAP_DUMP=on_oom`).
static OOM_HOOK: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

pub fn set_oom_hook(hook: fn()) {
    OOM_HOOK.store(hook as *mut (), core::sync::atomic::Ordering::Relaxed);
}

/// Fatal runtime error: `msg` + `\n` to stderr, then `exit(101)`;
/// `exit` runs the runtime's stdout flush.
pub fn fatal(msg: &[u8]) -> ! {
    write_all(2, &[msg, b"\n"]);
    // SAFETY: plain libc call.
    unsafe { libc::exit(101) }
}

/// Env knob `name` as `parse` reads it; `None` if unset or empty. A
/// value `parse` rejects is warned about on stderr, naming `expected`,
/// and reads as unset (the default).
pub fn env_knob<T>(
    name: &core::ffi::CStr,
    expected: &str,
    parse: impl FnOnce(&[u8]) -> Option<T>,
) -> Option<T> {
    // SAFETY: `name` is NUL-terminated; getenv's result is null or
    // NUL-terminated (read before any thread may set it).
    let v = unsafe {
        let p = libc::getenv(name.as_ptr());
        if p.is_null() {
            return None;
        }
        core::ffi::CStr::from_ptr(p).to_bytes()
    };
    if v.is_empty() {
        return None;
    }
    let r = parse(v);
    if r.is_none() {
        write_all(
            2,
            &[
                b"rok: warning: ignoring ",
                name.to_bytes(),
                b"=",
                v,
                b" (expected ",
                expected.as_bytes(),
                b")\n",
            ],
        );
    }
    r
}

/// Knob `name` set to one of `choices` (`a|b|c`): that choice.
pub fn env_choice<'a>(name: &core::ffi::CStr, choices: &'a str) -> Option<&'a str> {
    env_knob(name, choices, |v| {
        choices.split('|').find(|c| c.as_bytes() == v)
    })
}

/// Knob `name` = `0|1`.
pub fn env_flag(name: &core::ffi::CStr) -> bool {
    env_choice(name, "0|1") == Some("1")
}

/// Knob `name`: a decimal number, positive if `positive`.
pub fn env_num(name: &core::ffi::CStr, positive: bool) -> Option<u64> {
    let expected = if positive {
        "a positive number"
    } else {
        "a number"
    };
    env_knob(name, expected, |v| {
        let mut n = 0u64;
        for &d in v {
            if !d.is_ascii_digit() {
                return None;
            }
            n = n.checked_mul(10)?.checked_add(u64::from(d - b'0'))?;
        }
        (n > 0 || !positive).then_some(n)
    })
}
