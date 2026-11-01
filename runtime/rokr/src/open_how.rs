//! The fixed choice of an fs open function (`rok_os_open`), shared by
//! the POSIX and Windows halves.

use rokr_common::object_header::HEADER_SIZE;

/// The library's `OpenHow` record, a `GcBuf<u32>` of `HOW_LEN` slots:
/// access, disposition, nofollow, directory, mode, custom flags, then
/// the Windows share mode, attributes and SQOS flags.
const HOW_LEN: usize = 9;

/// `OpenHow.access`: 0 read, 1 write, 2 read+write, 3 append, 4 read+append.
/// `OpenHow.disposition`: 0 open existing, 1 create or truncate,
/// 2 create new, 3 open or create.
#[cfg_attr(not(windows), allow(dead_code))] // share, attrs, sqos
pub struct How {
    pub access: u32,
    pub disposition: u32,
    pub nofollow: bool,
    pub directory: bool,
    pub mode: u32,
    pub custom: u32,
    pub share: u32,
    pub attrs: u32,
    pub sqos: u32,
}

impl How {
    /// # Safety
    /// `how` must be a live `GcBuf<u32>` of `HOW_LEN` elements.
    pub unsafe fn read(how: *const u8) -> Self {
        let mut v = [0u32; HOW_LEN];
        // SAFETY: caller contract.
        unsafe {
            core::ptr::copy_nonoverlapping(how.add(HEADER_SIZE).cast(), v.as_mut_ptr(), HOW_LEN);
        };
        Self {
            access: v[0],
            disposition: v[1],
            nofollow: v[2] != 0,
            directory: v[3] != 0,
            mode: v[4],
            custom: v[5],
            share: v[6],
            attrs: v[7],
            sqos: v[8],
        }
    }
}
