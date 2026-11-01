//! The Win32 / NT / Winsock / UCRT declarations the runtime uses
//! (`x86_64` and `aarch64` `pc-windows-msvc`). The final link supplies
//! `kernel32`, `ntdll`, `ws2_32` and the UCRT.

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::upper_case_acronyms
)]

use core::ffi::c_void;

pub type HANDLE = isize;
pub type BOOL = i32;
pub type DWORD = u32;
pub type NTSTATUS = i32;
pub type SOCKET = usize;

pub const INVALID_HANDLE_VALUE: HANDLE = -1;
pub const INFINITE: DWORD = u32::MAX;
pub const FLS_OUT_OF_INDEXES: DWORD = u32::MAX;
pub const INVALID_SOCKET: SOCKET = !0;

// Error codes (winerror.h).
pub const ERROR_FILE_NOT_FOUND: i32 = 2;
pub const ERROR_PATH_NOT_FOUND: i32 = 3;
pub const ERROR_ACCESS_DENIED: i32 = 5;
pub const ERROR_INVALID_HANDLE: i32 = 6;
pub const ERROR_INVALID_FUNCTION: i32 = 1;
pub const ERROR_NOT_ENOUGH_MEMORY: i32 = 8;
pub const ERROR_INVALID_DRIVE: i32 = 15;
pub const ERROR_NOT_SAME_DEVICE: i32 = 17;
pub const ERROR_NO_MORE_FILES: i32 = 18;
pub const ERROR_WRITE_PROTECT: i32 = 19;
pub const ERROR_SHARING_VIOLATION: i32 = 32;
pub const ERROR_LOCK_VIOLATION: i32 = 33;
pub const ERROR_HANDLE_DISK_FULL: i32 = 39;
pub const ERROR_NOT_SUPPORTED: i32 = 50;
pub const ERROR_FILE_EXISTS: i32 = 80;
pub const ERROR_INVALID_PARAMETER: i32 = 87;
pub const ERROR_BROKEN_PIPE: i32 = 109;
pub const ERROR_DISK_FULL: i32 = 112;
pub const ERROR_CALL_NOT_IMPLEMENTED: i32 = 120;
pub const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
pub const ERROR_INVALID_NAME: i32 = 123;
pub const ERROR_DIR_NOT_EMPTY: i32 = 145;
pub const ERROR_ALREADY_EXISTS: i32 = 183;
pub const ERROR_ENVVAR_NOT_FOUND: i32 = 203;
pub const ERROR_FILENAME_EXCED_RANGE: i32 = 206;
pub const ERROR_NO_DATA: i32 = 232;
pub const ERROR_DIRECTORY: i32 = 267;
pub const ERROR_DIRECTORY_NOT_SUPPORTED: i32 = 336;
pub const ERROR_STOPPED_ON_SYMLINK: i32 = 681;
pub const ERROR_OPERATION_ABORTED: i32 = 995;
pub const ERROR_IO_PENDING: i32 = 997;
pub const PIPE_ACCESS_INBOUND: DWORD = 0x1;
pub const FILE_FLAG_FIRST_PIPE_INSTANCE: DWORD = 0x0008_0000;
pub const PIPE_REJECT_REMOTE_CLIENTS: DWORD = 0x8;
pub const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
pub const ERROR_TIMEOUT: i32 = 1460;
pub const ERROR_CANT_RESOLVE_FILENAME: i32 = 1921;
pub const ERROR_NOT_A_REPARSE_POINT: i32 = 4390;
pub const WSAEINTR: i32 = 10004;
pub const WSAEACCES: i32 = 10013;
pub const WSAEINVAL: i32 = 10022;
pub const WSAEWOULDBLOCK: i32 = 10035;
pub const WSAEINPROGRESS: i32 = 10036;
pub const WSAENOTSOCK: i32 = 10038;
pub const WSAEOPNOTSUPP: i32 = 10045;
pub const WSAEAFNOSUPPORT: i32 = 10047;
pub const WSAEADDRINUSE: i32 = 10048;
pub const WSAECONNABORTED: i32 = 10053;
pub const WSAECONNRESET: i32 = 10054;
pub const WSAESHUTDOWN: i32 = 10058;
pub const WSAETIMEDOUT: i32 = 10060;
pub const WSAHOST_NOT_FOUND: i32 = 11001;
pub const WSANO_DATA: i32 = 11004;

#[repr(C)]
pub struct SECURITY_ATTRIBUTES {
    pub nLength: DWORD,
    pub lpSecurityDescriptor: *mut c_void,
    pub bInheritHandle: BOOL,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FILETIME {
    pub lo: DWORD,
    pub hi: DWORD,
}

#[repr(C)]
pub struct SYSTEM_INFO {
    pub wProcessorArchitecture: u16,
    pub wReserved: u16,
    pub dwPageSize: DWORD,
    pub lpMinimumApplicationAddress: *mut c_void,
    pub lpMaximumApplicationAddress: *mut c_void,
    pub dwActiveProcessorMask: usize,
    pub dwNumberOfProcessors: DWORD,
    pub dwProcessorType: DWORD,
    pub dwAllocationGranularity: DWORD,
    pub wProcessorLevel: u16,
    pub wProcessorRevision: u16,
}

#[repr(C)]
pub struct OVERLAPPED {
    pub Internal: usize,
    pub InternalHigh: usize,
    pub Offset: DWORD,
    pub OffsetHigh: DWORD,
    pub hEvent: HANDLE,
}

#[repr(C)]
pub struct OVERLAPPED_ENTRY {
    pub lpCompletionKey: usize,
    pub lpOverlapped: *mut OVERLAPPED,
    pub Internal: usize,
    pub dwNumberOfBytesTransferred: DWORD,
}

#[repr(C)]
pub struct EXCEPTION_RECORD {
    pub ExceptionCode: u32,
    pub ExceptionFlags: u32,
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ExceptionAddress: *mut c_void,
    pub NumberParameters: u32,
    pub ExceptionInformation: [usize; 15],
}

#[repr(C)]
pub struct EXCEPTION_POINTERS {
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ContextRecord: *mut c_void,
}

pub const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;
pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

#[repr(C)]
pub struct WIN32_FIND_DATAW {
    pub dwFileAttributes: DWORD,
    pub ftCreationTime: FILETIME,
    pub ftLastAccessTime: FILETIME,
    pub ftLastWriteTime: FILETIME,
    pub nFileSizeHigh: DWORD,
    pub nFileSizeLow: DWORD,
    pub dwReserved0: DWORD,
    pub dwReserved1: DWORD,
    pub cFileName: [u16; 260],
    pub cAlternateFileName: [u16; 14],
}

#[repr(C)]
pub struct FILE_BASIC_INFO {
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub FileAttributes: DWORD,
}

#[repr(C)]
pub struct FILE_STANDARD_INFO {
    pub AllocationSize: i64,
    pub EndOfFile: i64,
    pub NumberOfLinks: DWORD,
    pub DeletePending: u8,
    pub Directory: u8,
}

#[repr(C)]
pub struct CREATEFILE2_EXTENDED_PARAMETERS {
    pub dwSize: DWORD,
    pub dwFileAttributes: DWORD,
    pub dwFileFlags: DWORD,
    pub dwSecurityQosFlags: DWORD,
    pub lpSecurityAttributes: *mut SECURITY_ATTRIBUTES,
    pub hTemplateFile: HANDLE,
}

#[repr(C)]
pub struct FILE_ATTRIBUTE_TAG_INFO {
    pub FileAttributes: DWORD,
    pub ReparseTag: DWORD,
}

// FILE_INFO_BY_HANDLE_CLASS
pub const FileBasicInfo: i32 = 0;
pub const FileStandardInfo: i32 = 1;
pub const FileEndOfFileInfo: i32 = 6;
pub const FileAttributeTagInfo: i32 = 9;
pub const FileRenameInfoEx: i32 = 22;

pub const FILE_RENAME_FLAG_REPLACE_IF_EXISTS: DWORD = 0x1;
pub const FILE_RENAME_FLAG_POSIX_SEMANTICS: DWORD = 0x2;

/// `FILE_RENAME_INFO` with `Flags` (`FileRenameInfoEx`); the name runs
/// on past the struct.
#[repr(C)]
pub struct FILE_RENAME_INFO {
    pub Flags: DWORD,
    pub RootDirectory: HANDLE,
    pub FileNameLength: DWORD,
    pub FileName: [u16; 1],
}

pub const FILE_ATTRIBUTE_READONLY: DWORD = 0x1;
pub const FILE_ATTRIBUTE_DIRECTORY: DWORD = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: DWORD = 0x80;
pub const FILE_ATTRIBUTE_REPARSE_POINT: DWORD = 0x400;
pub const INVALID_FILE_ATTRIBUTES: DWORD = u32::MAX;
pub const IO_REPARSE_TAG_SYMLINK: DWORD = 0xA000_000C;
pub const IO_REPARSE_TAG_MOUNT_POINT: DWORD = 0xA000_0003;

pub const GENERIC_READ: DWORD = 0x8000_0000;
pub const GENERIC_WRITE: DWORD = 0x4000_0000;
pub const FILE_WRITE_DATA: DWORD = 0x2;
pub const FILE_APPEND_DATA: DWORD = 0x4;
pub const FILE_WRITE_ATTRIBUTES: DWORD = 0x100;
pub const FILE_READ_ATTRIBUTES: DWORD = 0x80;
pub const DELETE: DWORD = 0x0001_0000;
pub const SYNCHRONIZE: DWORD = 0x0010_0000;
pub const FILE_SHARE_READ: DWORD = 1;
pub const FILE_SHARE_WRITE: DWORD = 2;
pub const FILE_SHARE_DELETE: DWORD = 4;
pub const CREATE_NEW: DWORD = 1;
pub const CREATE_ALWAYS: DWORD = 2;
pub const OPEN_EXISTING: DWORD = 3;
pub const OPEN_ALWAYS: DWORD = 4;
pub const TRUNCATE_EXISTING: DWORD = 5;
pub const FILE_FLAG_BACKUP_SEMANTICS: DWORD = 0x0200_0000;
pub const FILE_FLAG_OPEN_REPARSE_POINT: DWORD = 0x0020_0000;
pub const FILE_FLAG_OVERLAPPED: DWORD = 0x4000_0000;
pub const SECURITY_SQOS_PRESENT: DWORD = 0x0010_0000;

pub const FILE_TYPE_CHAR: DWORD = 2;

pub const STD_INPUT_HANDLE: DWORD = -10i32 as DWORD;
pub const STD_OUTPUT_HANDLE: DWORD = -11i32 as DWORD;
pub const STD_ERROR_HANDLE: DWORD = -12i32 as DWORD;

pub const MEM_COMMIT: DWORD = 0x1000;
pub const MEM_RESERVE: DWORD = 0x2000;
pub const MEM_DECOMMIT: DWORD = 0x4000;
pub const MEM_RELEASE: DWORD = 0x8000;
pub const MEM_WRITE_WATCH: DWORD = 0x0020_0000;
pub const PAGE_NOACCESS: DWORD = 0x01;
pub const PAGE_READONLY: DWORD = 0x02;
pub const PAGE_READWRITE: DWORD = 0x04;
pub const FILE_MAP_READ: DWORD = 0x4;

#[repr(C)]
#[allow(non_snake_case)]
pub struct MEMORY_BASIC_INFORMATION {
    pub BaseAddress: *mut c_void,
    pub AllocationBase: *mut c_void,
    pub AllocationProtect: DWORD,
    pub PartitionId: u16,
    pub RegionSize: usize,
    pub State: DWORD,
    pub Protect: DWORD,
    pub Type: DWORD,
}

pub const STACK_SIZE_PARAM_IS_A_RESERVATION: DWORD = 0x0001_0000;
pub const WAIT_OBJECT_0: DWORD = 0;
pub const WAIT_TIMEOUT: DWORD = 258;
pub const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: DWORD = 0x2;
pub const TIMER_ALL_ACCESS: DWORD = 0x001F_0003;

pub const DUPLICATE_SAME_ACCESS: DWORD = 2;
pub const HANDLE_FLAG_INHERIT: DWORD = 1;

pub const LOCKFILE_FAIL_IMMEDIATELY: DWORD = 1;
pub const LOCKFILE_EXCLUSIVE_LOCK: DWORD = 2;

pub const MOVEFILE_REPLACE_EXISTING: DWORD = 1;
pub const COPY_FILE_FAIL_IF_EXISTS: DWORD = 1;
pub const SYMBOLIC_LINK_FLAG_DIRECTORY: DWORD = 1;
pub const SYMBOLIC_LINK_FLAG_ALLOW_UNPRIVILEGED_CREATE: DWORD = 2;
pub const FILE_NAME_NORMALIZED: DWORD = 0;
pub const VOLUME_NAME_DOS: DWORD = 0;

pub const FORMAT_MESSAGE_FROM_SYSTEM: DWORD = 0x1000;
pub const FORMAT_MESSAGE_IGNORE_INSERTS: DWORD = 0x200;

pub const CREATE_NEW_PROCESS_GROUP: DWORD = 0x200;
pub const CREATE_UNICODE_ENVIRONMENT: DWORD = 0x400;
pub const EXTENDED_STARTUPINFO_PRESENT: DWORD = 0x0008_0000;
pub const STARTF_USESTDHANDLES: DWORD = 0x100;
pub const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
pub const PROCESS_TERMINATE: DWORD = 0x1;
pub const PROCESS_QUERY_LIMITED_INFORMATION: DWORD = 0x1000;
pub const STILL_ACTIVE: DWORD = 259;

pub const CTRL_C_EVENT: DWORD = 0;
pub const CTRL_BREAK_EVENT: DWORD = 1;
pub const CTRL_CLOSE_EVENT: DWORD = 2;
pub const CTRL_LOGOFF_EVENT: DWORD = 5;
pub const CTRL_SHUTDOWN_EVENT: DWORD = 6;

#[repr(C)]
pub struct STARTUPINFOW {
    pub cb: DWORD,
    pub lpReserved: *mut u16,
    pub lpDesktop: *mut u16,
    pub lpTitle: *mut u16,
    pub dwX: DWORD,
    pub dwY: DWORD,
    pub dwXSize: DWORD,
    pub dwYSize: DWORD,
    pub dwXCountChars: DWORD,
    pub dwYCountChars: DWORD,
    pub dwFillAttribute: DWORD,
    pub dwFlags: DWORD,
    pub wShowWindow: u16,
    pub cbReserved2: u16,
    pub lpReserved2: *mut u8,
    pub hStdInput: HANDLE,
    pub hStdOutput: HANDLE,
    pub hStdError: HANDLE,
}

#[repr(C)]
pub struct STARTUPINFOEXW {
    pub StartupInfo: STARTUPINFOW,
    pub lpAttributeList: *mut c_void,
}

#[repr(C)]
pub struct PROCESS_INFORMATION {
    pub hProcess: HANDLE,
    pub hThread: HANDLE,
    pub dwProcessId: DWORD,
    pub dwThreadId: DWORD,
}

#[repr(C)]
pub struct PROCESS_BASIC_INFORMATION {
    pub ExitStatus: NTSTATUS,
    pub PebBaseAddress: *mut c_void,
    pub AffinityMask: usize,
    pub BasePriority: i32,
    pub UniqueProcessId: usize,
    pub InheritedFromUniqueProcessId: usize,
}

#[repr(C)]
pub struct IO_STATUS_BLOCK {
    pub Status: NTSTATUS,
    pub Information: usize,
}

#[repr(C)]
pub struct UNICODE_STRING {
    pub Length: u16,
    pub MaximumLength: u16,
    pub Buffer: *const u16,
}

#[repr(C)]
pub struct OBJECT_ATTRIBUTES {
    pub Length: u32,
    pub RootDirectory: HANDLE,
    pub ObjectName: *const UNICODE_STRING,
    pub Attributes: u32,
    pub SecurityDescriptor: *mut c_void,
    pub SecurityQualityOfService: *mut c_void,
}

pub const STATUS_PENDING: NTSTATUS = 0x103;
pub const STATUS_CANCELLED: NTSTATUS = 0xC000_0120_u32 as NTSTATUS;
pub const FILE_OPEN: u32 = 1;

pub type ThreadProc = unsafe extern "system" fn(*mut c_void) -> DWORD;
pub type VectoredHandler = unsafe extern "system" fn(*mut EXCEPTION_POINTERS) -> i32;
pub type CtrlHandler = unsafe extern "system" fn(DWORD) -> BOOL;

unsafe extern "system" {
    pub fn GetLastError() -> DWORD;
    pub fn SetLastError(code: DWORD);
    pub fn GetStdHandle(which: DWORD) -> HANDLE;
    pub fn GetFileType(h: HANDLE) -> DWORD;
    pub fn GetConsoleMode(h: HANDLE, mode: *mut DWORD) -> BOOL;
    pub fn WriteConsoleW(
        h: HANDLE,
        buf: *const u16,
        n: DWORD,
        written: *mut DWORD,
        reserved: *mut c_void,
    ) -> BOOL;
    pub fn ReadConsoleW(
        h: HANDLE,
        buf: *mut u16,
        n: DWORD,
        read: *mut DWORD,
        control: *mut c_void,
    ) -> BOOL;
    pub fn ReadFile(
        h: HANDLE,
        buf: *mut c_void,
        n: DWORD,
        read: *mut DWORD,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn WriteFile(
        h: HANDLE,
        buf: *const c_void,
        n: DWORD,
        written: *mut DWORD,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn CloseHandle(h: HANDLE) -> BOOL;
    pub fn DuplicateHandle(
        src_process: HANDLE,
        src: HANDLE,
        dst_process: HANDLE,
        dst: *mut HANDLE,
        access: DWORD,
        inherit: BOOL,
        options: DWORD,
    ) -> BOOL;
    pub fn SetHandleInformation(h: HANDLE, mask: DWORD, flags: DWORD) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn GetCurrentProcessId() -> DWORD;
    pub fn GetCurrentThreadId() -> DWORD;
    pub fn FlsAlloc(callback: Option<unsafe extern "system" fn(*mut c_void)>) -> DWORD;
    pub fn FlsFree(index: DWORD) -> BOOL;
    pub fn FlsSetValue(index: DWORD, value: *mut c_void) -> BOOL;

    pub fn VirtualAlloc(addr: *mut c_void, size: usize, kind: DWORD, protect: DWORD)
    -> *mut c_void;
    pub fn VirtualFree(addr: *mut c_void, size: usize, kind: DWORD) -> BOOL;
    pub fn VirtualQuery(
        addr: *const c_void,
        info: *mut MEMORY_BASIC_INFORMATION,
        len: usize,
    ) -> usize;
    pub fn GetSystemInfo(info: *mut SYSTEM_INFO);
    pub fn GetActiveProcessorCount(group: u16) -> DWORD;

    pub fn AcquireSRWLockExclusive(l: *mut usize);
    pub fn ReleaseSRWLockExclusive(l: *mut usize);
    pub fn TryAcquireSRWLockExclusive(l: *mut usize) -> u8;
    pub fn SleepConditionVariableSRW(c: *mut usize, l: *mut usize, ms: DWORD, flags: u32) -> BOOL;
    pub fn WakeAllConditionVariable(c: *mut usize);

    pub fn QueryPerformanceCounter(v: *mut i64) -> BOOL;
    pub fn QueryPerformanceFrequency(v: *mut i64) -> BOOL;
    pub fn GetSystemTimePreciseAsFileTime(t: *mut FILETIME);
    pub fn GetThreadTimes(
        t: HANDLE,
        created: *mut FILETIME,
        exited: *mut FILETIME,
        kernel: *mut FILETIME,
        user: *mut FILETIME,
    ) -> BOOL;
    pub fn GetCurrentThread() -> HANDLE;

    pub fn CreateThread(
        attrs: *mut SECURITY_ATTRIBUTES,
        stack: usize,
        start: ThreadProc,
        arg: *mut c_void,
        flags: DWORD,
        id: *mut DWORD,
    ) -> HANDLE;
    pub fn WaitForSingleObject(h: HANDLE, ms: DWORD) -> DWORD;
    pub fn WaitForMultipleObjects(n: DWORD, hs: *const HANDLE, all: BOOL, ms: DWORD) -> DWORD;
    pub fn Sleep(ms: DWORD);
    pub fn SwitchToThread() -> BOOL;
    pub fn CreateWaitableTimerExW(
        attrs: *mut SECURITY_ATTRIBUTES,
        name: *const u16,
        flags: DWORD,
        access: DWORD,
    ) -> HANDLE;
    pub fn SetWaitableTimer(
        h: HANDLE,
        due: *const i64,
        period: i32,
        f: *mut c_void,
        arg: *mut c_void,
        resume: BOOL,
    ) -> BOOL;
    pub fn AddVectoredExceptionHandler(first: u32, f: VectoredHandler) -> *mut c_void;
    pub fn SetThreadStackGuarantee(size: *mut u32) -> BOOL;
    pub fn SetThreadDescription(t: HANDLE, name: *const u16) -> i32;
    pub fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    pub fn SetConsoleCtrlHandler(f: Option<CtrlHandler>, add: BOOL) -> BOOL;

    pub fn CreateFileW(
        name: *const u16,
        access: DWORD,
        share: DWORD,
        sa: *mut SECURITY_ATTRIBUTES,
        disposition: DWORD,
        flags: DWORD,
        template: HANDLE,
    ) -> HANDLE;
    pub fn CreateFile2(
        name: *const u16,
        access: DWORD,
        share: DWORD,
        disposition: DWORD,
        ext: *const CREATEFILE2_EXTENDED_PARAMETERS,
    ) -> HANDLE;
    pub fn SetFilePointerEx(h: HANDLE, dist: i64, new: *mut i64, method: DWORD) -> BOOL;
    pub fn FlushFileBuffers(h: HANDLE) -> BOOL;
    pub fn ReOpenFile(h: HANDLE, access: DWORD, share: DWORD, flags: DWORD) -> HANDLE;
    pub fn SetFileInformationByHandle(h: HANDLE, class: i32, info: *const c_void, n: DWORD)
    -> BOOL;
    pub fn GetFileInformationByHandleEx(h: HANDLE, class: i32, info: *mut c_void, n: DWORD)
    -> BOOL;
    pub fn LockFileEx(
        h: HANDLE,
        flags: DWORD,
        reserved: DWORD,
        lo: DWORD,
        hi: DWORD,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn UnlockFileEx(
        h: HANDLE,
        reserved: DWORD,
        lo: DWORD,
        hi: DWORD,
        ov: *mut OVERLAPPED,
    ) -> BOOL;
    pub fn CreateFileMappingW(
        h: HANDLE,
        sa: *mut SECURITY_ATTRIBUTES,
        protect: DWORD,
        size_hi: DWORD,
        size_lo: DWORD,
        name: *const u16,
    ) -> HANDLE;
    pub fn MapViewOfFile(
        h: HANDLE,
        access: DWORD,
        off_hi: DWORD,
        off_lo: DWORD,
        len: usize,
    ) -> *mut c_void;
    pub fn UnmapViewOfFile(addr: *const c_void) -> BOOL;
    pub fn PeekNamedPipe(
        h: HANDLE,
        buf: *mut c_void,
        n: DWORD,
        read: *mut DWORD,
        avail: *mut DWORD,
        left: *mut DWORD,
    ) -> BOOL;
    pub fn CreatePipe(
        r: *mut HANDLE,
        w: *mut HANDLE,
        sa: *mut SECURITY_ATTRIBUTES,
        size: DWORD,
    ) -> BOOL;

    pub fn CreateDirectoryW(name: *const u16, sa: *mut SECURITY_ATTRIBUTES) -> BOOL;
    pub fn DeleteFileW(name: *const u16) -> BOOL;
    pub fn RemoveDirectoryW(name: *const u16) -> BOOL;
    pub fn MoveFileExW(from: *const u16, to: *const u16, flags: DWORD) -> BOOL;
    pub fn GetFullPathNameW(
        name: *const u16,
        n: DWORD,
        buf: *mut u16,
        file_part: *mut *mut u16,
    ) -> DWORD;
    pub fn CopyFileExW(
        from: *const u16,
        to: *const u16,
        progress: *mut c_void,
        data: *mut c_void,
        cancel: *mut BOOL,
        flags: DWORD,
    ) -> BOOL;
    pub fn CreateHardLinkW(link: *const u16, target: *const u16, sa: *mut c_void) -> BOOL;
    pub fn CreateSymbolicLinkW(link: *const u16, target: *const u16, flags: DWORD) -> u8;
    pub fn GetFileAttributesW(name: *const u16) -> DWORD;
    pub fn FindFirstFileW(name: *const u16, data: *mut WIN32_FIND_DATAW) -> HANDLE;
    pub fn FindNextFileW(h: HANDLE, data: *mut WIN32_FIND_DATAW) -> BOOL;
    pub fn FindClose(h: HANDLE) -> BOOL;
    pub fn GetFinalPathNameByHandleW(h: HANDLE, buf: *mut u16, n: DWORD, flags: DWORD) -> DWORD;
    pub fn GetCurrentDirectoryW(n: DWORD, buf: *mut u16) -> DWORD;
    pub fn SetCurrentDirectoryW(name: *const u16) -> BOOL;
    pub fn GetModuleFileNameW(module: HANDLE, buf: *mut u16, n: DWORD) -> DWORD;

    pub fn GetCommandLineW() -> *const u16;
    pub fn GetEnvironmentStringsW() -> *mut u16;
    pub fn FreeEnvironmentStringsW(env: *mut u16) -> BOOL;
    pub fn GetEnvironmentVariableW(name: *const u16, buf: *mut u16, n: DWORD) -> DWORD;
    pub fn SetEnvironmentVariableW(name: *const u16, value: *const u16) -> BOOL;
    pub fn FormatMessageW(
        flags: DWORD,
        source: *const c_void,
        id: DWORD,
        lang: DWORD,
        buf: *mut u16,
        n: DWORD,
        args: *mut c_void,
    ) -> DWORD;

    pub fn CreateProcessW(
        app: *const u16,
        cmd: *mut u16,
        pa: *mut SECURITY_ATTRIBUTES,
        ta: *mut SECURITY_ATTRIBUTES,
        inherit: BOOL,
        flags: DWORD,
        env: *mut c_void,
        cwd: *const u16,
        si: *mut STARTUPINFOW,
        pi: *mut PROCESS_INFORMATION,
    ) -> BOOL;
    pub fn InitializeProcThreadAttributeList(
        list: *mut c_void,
        count: DWORD,
        flags: DWORD,
        size: *mut usize,
    ) -> BOOL;
    pub fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: DWORD,
        attr: usize,
        value: *const c_void,
        size: usize,
        prev: *mut c_void,
        ret: *mut usize,
    ) -> BOOL;
    pub fn DeleteProcThreadAttributeList(list: *mut c_void);
    pub fn GetExitCodeProcess(h: HANDLE, code: *mut DWORD) -> BOOL;
    pub fn OpenProcess(access: DWORD, inherit: BOOL, pid: DWORD) -> HANDLE;
    pub fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;

    pub fn CreateIoCompletionPort(h: HANDLE, port: HANDLE, key: usize, threads: DWORD) -> HANDLE;
    pub fn GetQueuedCompletionStatusEx(
        port: HANDLE,
        entries: *mut OVERLAPPED_ENTRY,
        n: u32,
        removed: *mut u32,
        ms: DWORD,
        alertable: BOOL,
    ) -> BOOL;
    pub fn CancelIoEx(h: HANDLE, ov: *mut OVERLAPPED) -> BOOL;
    pub fn GetOverlappedResult(h: HANDLE, ov: *mut OVERLAPPED, n: *mut DWORD, wait: BOOL) -> BOOL;
    pub fn CreateEventW(sa: *mut c_void, manual: BOOL, initial: BOOL, name: *const u16) -> HANDLE;
    pub fn CreateNamedPipeW(
        name: *const u16,
        open_mode: DWORD,
        pipe_mode: DWORD,
        instances: DWORD,
        out_size: DWORD,
        in_size: DWORD,
        timeout: DWORD,
        sa: *mut c_void,
    ) -> HANDLE;
}

unsafe extern "system" {
    pub fn NtCreateFile(
        h: *mut HANDLE,
        access: u32,
        attrs: *const OBJECT_ATTRIBUTES,
        iosb: *mut IO_STATUS_BLOCK,
        size: *const i64,
        file_attrs: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *mut c_void,
        ea_len: u32,
    ) -> NTSTATUS;
    pub fn NtDeviceIoControlFile(
        h: HANDLE,
        event: HANDLE,
        apc: *mut c_void,
        apc_ctx: *mut c_void,
        iosb: *mut IO_STATUS_BLOCK,
        code: u32,
        input: *const c_void,
        in_len: u32,
        output: *mut c_void,
        out_len: u32,
    ) -> NTSTATUS;
    pub fn NtQueryInformationProcess(
        h: HANDLE,
        class: u32,
        info: *mut c_void,
        n: u32,
        ret: *mut u32,
    ) -> NTSTATUS;
    pub fn NtSetTimerResolution(want: u32, set: u8, actual: *mut u32) -> NTSTATUS;
    pub fn NtQueryInformationFile(
        h: HANDLE,
        iosb: *mut IO_STATUS_BLOCK,
        info: *mut c_void,
        n: u32,
        class: u32,
    ) -> NTSTATUS;
    pub fn RtlNtStatusToDosError(s: NTSTATUS) -> u32;
}

// UCRT: `atexit` is a static-library shim over this export in MSVC's CRT.
unsafe extern "C" {
    #[link_name = "_crt_atexit"]
    pub fn crt_atexit(f: extern "C" fn()) -> i32;
}

// Randomness (bcrypt) and loading modules outside the import set.
pub const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 2;

unsafe extern "system" {
    pub fn BCryptGenRandom(alg: *mut c_void, buf: *mut u8, len: u32, flags: u32) -> NTSTATUS;
    pub fn LoadLibraryW(name: *const u16) -> HANDLE;
    pub fn GetProcAddress(module: HANDLE, name: *const u8) -> *mut c_void;
}
