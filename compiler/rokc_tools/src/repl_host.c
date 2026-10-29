// The `rokc repl` session host (repl.rs): one process, runtime up once.
// rokc compiles each cell to an object of the host's format (Mach-O,
// COFF, ELF; arm64 / x86_64) and the host maps, links and runs it
// (`jit_cell`). Commands arrive on stdin (a socket; Windows: a named
// pipe):
//
//   `cell <n>\n` + n bytes of object: link it and run its
//     `rok_repl_cell` once; later cells link against it.
//   `lib <n>\n` + an n-byte path: link every member of a static library,
//     or load a shared one; reply `ok 0\n` or `err\n`.
//
// A cell replies `pid <p>\n`, then `ok <n>\n<shown value>` or `err <n>\n`
// and why it failed (`why`).
// `SIGINT` (Windows: Ctrl-Break to the host's group) ends a running cell
// and is ignored between cells.
//
// A failed cell leaves the session as it was. POSIX: the cell runs in the
// host while a forked copy waits and takes over if the cell's process
// dies. Windows has no fork: the host saves memory (`checkpoint`), runs
// the cell from a recovery point, and on failure ends its threads, puts
// memory back (`rollback`) and returns there (`run_cell`).
//
// A symbol redefined by a later cell is renamed `<name>.r<cell>`; later
// references resolve to the newest (907), earlier code keeps its own.
//
// The loader is one linker over generic sections, symbols and relocations
// (`struct obj`, `fix`), fed by per-format readers (`map_object`,
// `link_object`). Thread-locals are the host's: via TLV descriptors
// (Mach-O), thread-pointer offsets (ELF initial exec) or TLS block offsets
// (COFF `_tls_index`, SECREL).
#if defined(_WIN32)
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#else
#define _GNU_SOURCE
#if defined(__linux__)
// The host links against glibc 2.36 stubs, which lack the __isoc23_*
// entry points 2.38+ headers pick under _GNU_SOURCE.
#include <features.h>
#undef __GLIBC_USE_C2X_STRTOL
#define __GLIBC_USE_C2X_STRTOL 0
#undef __GLIBC_USE_C23_STRTOL
#define __GLIBC_USE_C23_STRTOL 0
#endif
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <ftw.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#if defined(__APPLE__)
#include <sys/event.h>
#else
#include <sys/syscall.h>
#ifndef SYS_pidfd_open
#define SYS_pidfd_open 434
#endif
#endif
#endif

#if !defined(__aarch64__) && !defined(__x86_64__)
#error "the REPL host loads arm64 and x86_64 objects only"
#endif

#if defined(_WIN32)
// No Windows SDK or MSVC headers: what the host calls of the UCRT and
// kernel32, declared here (rokc's import libraries resolve them).
typedef void *HANDLE;
typedef uint32_t DWORD;
typedef int BOOL;
typedef long long ssize_t;
typedef struct FILE FILE;

void *malloc(size_t);
void *calloc(size_t, size_t);
void free(void *);
char *strdup(const char *);
int strcmp(const char *, const char *);
size_t strlen(const char *);
int strncmp(const char *, const char *, size_t);
int memcmp(const void *, const void *, size_t);
void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
unsigned long strtoul(const char *, char **, int);
void qsort(void *, size_t, size_t, int (*)(const void *, const void *));
FILE *fopen(const char *, const char *);
size_t fread(void *, size_t, size_t, FILE *);
int fseek(FILE *, long, int);
long ftell(FILE *);
int fclose(FILE *);
int _resetstkoflw(void);
FILE *__acrt_iob_func(unsigned);
int __stdio_common_vfprintf(uint64_t, FILE *, const char *, void *, va_list);
int __stdio_common_vsprintf(uint64_t, char *, size_t, const char *, void *, va_list);
int __stdio_common_vsscanf(uint64_t, const char *, size_t, const char *, void *, va_list);

HANDLE GetStdHandle(DWORD);
BOOL SetStdHandle(DWORD, HANDLE);
HANDLE CreateFileA(const char *, DWORD, DWORD, void *, DWORD, DWORD, HANDLE);
BOOL ReadFile(HANDLE, void *, DWORD, DWORD *, void *);
BOOL WriteFile(HANDLE, const void *, DWORD, DWORD *, void *);
DWORD GetCurrentProcessId(void);
DWORD GetCurrentThreadId(void);
HANDLE GetCurrentProcess(void);
HANDLE GetModuleHandleA(const char *);
HANDLE LoadLibraryA(const char *);
void *GetProcAddress(HANDLE, const char *);
void *VirtualAlloc(void *, size_t, DWORD, DWORD);
BOOL FlushInstructionCache(HANDLE, const void *, size_t);
unsigned char RtlAddFunctionTable(void *, DWORD, uint64_t);
BOOL SetConsoleCtrlHandler(BOOL (*)(DWORD), BOOL);
void *SetUnhandledExceptionFilter(long (*)(void *));
void *AddVectoredExceptionHandler(uint32_t, long (*)(void *));
__attribute__((returns_twice)) void RtlCaptureContext(void *);
void RtlRestoreContext(void *, void *);

#define SEEK_SET 0
#define SEEK_END 2
#define stderr __acrt_iob_func(2)

// The UCRT's `printf` family is inline in its headers, over these.
static int fprintf(FILE *f, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = __stdio_common_vfprintf(0, f, fmt, NULL, ap);
    va_end(ap);
    return n;
}

static int snprintf(char *buf, size_t cap, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    // 2: C99 `snprintf` (truncates, terminates, returns the full length).
    int n = __stdio_common_vsprintf(2, buf, cap, fmt, NULL, ap);
    va_end(ap);
    return n;
}

static int sscanf(const char *s, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = __stdio_common_vsscanf(0, s, (size_t)-1, fmt, NULL, ap);
    va_end(ap);
    return n;
}
#endif

void rok_runtime_init(void);
int rok_stdio_flush(void);
int rok_session_hold(int64_t);
void rok_session_unhold(void);
void rok_session_resume(void);
void rok_session_restored(const uint64_t *, size_t);
void rok_set_exit_hook(void (*)(int));
_Noreturn void rok_exit(int);

// Set by Ctrl-C (SIGINT, Ctrl-Break) while `quiesce` waits.
static volatile int quiesce_cancel;
static volatile int quiescing;

// Stop the world at a quiet moment before the cell (`rok_session_hold`: no
// task mid-poll, no job mid-run, no thread in a critical section) so a
// restored session loses nothing its threads did. 0: Ctrl-C cancelled the
// wait (and the cell).
static int quiesce(void) {
    quiesce_cancel = 0;
    quiescing = 1;
    int ok = 1;
    for (int tries = 0; !rok_session_hold(100000000); tries++) {
        if (quiesce_cancel) {
            rok_session_unhold();
            ok = 0;
            break;
        }
        if (tries == 10)
            fprintf(stderr, "note: waiting for running tasks, blocking jobs and lock holders to reach a safe point "
                            "before the cell (Ctrl-C cancels it)\n");
    }
    quiescing = 0;
    return ok;
}

// The cell's shown value (a rok `str`), which the cell sets.
struct {
    const char *ptr;
    size_t len;
} __rok_repl_out;

static unsigned cells;

// Why the running cell is ending early, for rokc: `exit <code>`, `panic`,
// `interrupted` or `throw\n<where>\n<error>`; empty: it failed otherwise.
static char why[480];
static size_t nwhy;

static void add_why(const char *s, size_t n) {
    if (n > sizeof why - nwhy) n = sizeof why - nwhy;
    memcpy(why + nwhy, s, n);
    nwhy += n;
}

static void set_why(const char *s) {
    nwhy = 0;
    add_why(s, strlen(s));
}

// A rok `str`.
struct rok_str {
    const char *ptr;
    size_t len;
};

// Cells reach `exit` and an error escaping the cell through these, which
// note why the cell ends (`resolve`).
static _Noreturn void host_exit(int code) {
    char s[32];
    set_why("");
    add_why(s, (size_t)snprintf(s, sizeof s, "exit %d", code));
    rok_exit(code);
}

static _Noreturn void host_uncaught_at(struct rok_str what, struct rok_str at) {
    set_why("throw\n");
    add_why(at.ptr, at.len);
    add_why("\n", 1);
    add_why(what.ptr, what.len);
    rok_exit(1);
}

static _Noreturn void host_uncaught(struct rok_str what) {
    struct rok_str none = {"", 0};
    host_uncaught_at(what, none);
}

// Symbols the session defines: name → its newest definition's name
// (and a definition's address under its own name).
struct sym {
    char *name, *now;
    void *addr;
    struct sym *next;
};
static struct sym *syms[4096];

static struct sym **sym_slot(const char *name) {
    unsigned h = 2166136261u;
    for (const char *s = name; *s; s++) h = (h ^ (unsigned char)*s) * 16777619u;
    struct sym **p = &syms[h % (sizeof syms / sizeof *syms)];
    while (*p && strcmp((*p)->name, name)) p = &(*p)->next;
    return p;
}

static const char *sym_now(const char *name) {
    struct sym *s = *sym_slot(name);
    return s ? s->now : NULL;
}

static void sym_set(const char *name, const char *now) {
    struct sym **p = sym_slot(name);
    if (!*p) {
        *p = calloc(1, sizeof **p);
        (*p)->name = strdup(name);
    }
    free((*p)->now);
    (*p)->now = strdup(now);
}

static void *sym_addr(const char *now) {
    struct sym *s = *sym_slot(now);
    return s ? s->addr : NULL;
}

// A definition of `name` at `addr`: renamed `<name>.r<cell>` when an
// earlier cell has one (later cells use the newest).
static void define(const char *name, void *addr) {
    char fresh[1024];
    const char *as = name;
    if (sym_now(name)) {
        snprintf(fresh, sizeof fresh, "%s.r%u", name, cells);
        as = fresh;
    }
    sym_set(name, as);
    if (as != name) sym_set(as, as);
    (*sym_slot(as))->addr = addr;
}

// Executable memory: Apple arm64 flips a `MAP_JIT` mapping per thread,
// elsewhere RWX. `map_near`: `size` bytes at `at` if free, else anywhere;
// NULL: none.
#if defined(_WIN32)
#define jit_writable(on) ((void)0)
#define flush_code(p, n) FlushInstructionCache(GetCurrentProcess(), p, n)

static char *map_near(char *at, size_t size, int code) {
    // `VirtualAlloc` takes an address, not a hint: the first free 64 KiB
    // boundary from `at` on, within 1 GiB.
    const DWORD commit_reserve = 0x3000, rwx = 0x40, rw = 0x04;
    uintptr_t a = ((uintptr_t)at + 0xffff) & ~(uintptr_t)0xffff;
    for (uintptr_t end = a + ((uintptr_t)1 << 30); a < end; a += 0x10000) {
        char *p = VirtualAlloc((void *)a, size, commit_reserve, code ? rwx : rw);
        if (p) return p;
    }
    return VirtualAlloc(NULL, size, commit_reserve, code ? rwx : rw);
}
#else
#if defined(__APPLE__) && defined(__aarch64__)
#include <libkern/OSCacheControl.h>
#define MAP_CODE MAP_JIT
#define jit_writable(on) pthread_jit_write_protect_np(!(on))
#define flush_code(p, n) sys_icache_invalidate(p, n)
#else
#define MAP_CODE 0
#define jit_writable(on) ((void)0)
// x86_64 keeps fetch coherent; arm64 cleans and invalidates caches by line
// (no libgcc `__clear_cache` here).
static void flush_code(char *p, size_t n) {
#if defined(__aarch64__)
    uint64_t ctr;
    __asm__ volatile("mrs %0, ctr_el0" : "=r"(ctr));
    uintptr_t d = (uintptr_t)4 << ((ctr >> 16) & 15), i = (uintptr_t)4 << (ctr & 15), end = (uintptr_t)p + n;
    for (uintptr_t a = (uintptr_t)p & ~(d - 1); a < end; a += d) __asm__ volatile("dc cvau, %0" ::"r"(a) : "memory");
    __asm__ volatile("dsb ish" ::: "memory");
    for (uintptr_t a = (uintptr_t)p & ~(i - 1); a < end; a += i) __asm__ volatile("ic ivau, %0" ::"r"(a) : "memory");
    __asm__ volatile("dsb ish\n\tisb" ::: "memory");
#else
    (void)p;
    (void)n;
#endif
}
#endif

static char *map_near(char *at, size_t size, int code) {
    at = (char *)(((uintptr_t)at + 0x3fff) & ~(uintptr_t)0x3fff);
    char *p = mmap(at, size, PROT_READ | PROT_WRITE | (code ? PROT_EXEC : 0),
                   MAP_PRIVATE | MAP_ANON | (code ? MAP_CODE : 0), -1, 0);
    return p == MAP_FAILED ? NULL : p;
}
#endif

#if defined(__APPLE__)
#include <mach-o/dyld.h>
#include <mach-o/loader.h>
#include <mach-o/nlist.h>
#include <mach-o/arm64/reloc.h>
#include <mach-o/reloc.h>
#include <mach-o/x86_64/reloc.h>

// C names carry a leading `_`.
#define SYM_PREFIX 1

// Look a host symbol up in its symbol table, not `dlsym`: a thread-local
// needs its TLV descriptor, not this thread's instance.
static void *host_symbol(const char *name) {
    const struct mach_header_64 *mh = (const void *)_dyld_get_image_header(0);
    intptr_t slide = _dyld_get_image_vmaddr_slide(0);
    const struct load_command *lc = (const void *)(mh + 1);
    const struct segment_command_64 *linkedit = NULL;
    const struct symtab_command *st = NULL;
    for (uint32_t i = 0; i < mh->ncmds; i++, lc = (const void *)((const char *)lc + lc->cmdsize)) {
        if (lc->cmd == LC_SEGMENT_64 && !strcmp(((const struct segment_command_64 *)lc)->segname, "__LINKEDIT"))
            linkedit = (const void *)lc;
        if (lc->cmd == LC_SYMTAB) st = (const void *)lc;
    }
    if (!linkedit || !st) return NULL;
    const char *base = (const char *)(linkedit->vmaddr + slide - linkedit->fileoff);
    const struct nlist_64 *nl = (const void *)(base + st->symoff);
    const char *strs = base + st->stroff;
    // Linear scan: runs once per thread-local the session uses.
    for (uint32_t i = 0; i < st->nsyms; i++)
        if (!(nl[i].n_type & N_STAB) && (nl[i].n_type & N_TYPE) == N_SECT &&
            !strcmp(strs + nl[i].n_un.n_strx, name))
            return (void *)(nl[i].n_value + slide);
    return NULL;
}

// A thread-local's value for a cell: its TLV descriptor.
static void *host_tls(const char *name) { return host_symbol(name); }
#elif defined(_WIN32)
#define SYM_PREFIX 0

static void host_yield(void), host_leave_native(void);

// The modules a Windows program links (rokc's import libraries), the
// UCRT first as there: `ntdll` repeats some C functions.
static const char *const modules[] = {
    "ucrtbase.dll", "kernel32.dll", "api-ms-win-core-synch-l1-2-0.dll", "ws2_32.dll", "advapi32.dll",
    "bcrypt.dll",   "userenv.dll",  "shell32.dll", "ole32.dll", "user32.dll", "ntdll.dll",
};

// The DLLs `lib` loaded (native packages' runtime files).
static HANDLE shared[64];
static int nshared;

// The host's exports (it exports everything), else loaded DLLs', else the
// modules'. Cells reach the safepoint slow path and the return from
// blocking calls through the host, which ends interrupted cells there
// (`host_yield`).
static void *host_symbol(const char *name) {
    if (!strcmp(name, "rok_runtime_yield")) return (void *)host_yield;
    if (!strcmp(name, "rok_leave_native")) return (void *)host_leave_native;
    void *a = GetProcAddress(GetModuleHandleA(NULL), name);
    for (int i = 0; !a && i < nshared; i++) a = GetProcAddress(shared[i], name);
    for (size_t i = 0; !a && i < sizeof modules / sizeof *modules; i++) {
        HANDLE m = LoadLibraryA(modules[i]);
        if (m) a = GetProcAddress(m, name);
    }
    return a;
}

// The TLS directory rokc's startup object defines.
extern const struct {
    char *start, *end;
    void *index, *callbacks;
    uint32_t zero_fill, flags;
} _tls_used;

// A thread-local's value for a cell: its offset in the thread's TLS
// block (what SECREL gives), the same in every thread.
static void *host_tls(const char *name) {
    char *a = host_symbol(name);
    return a ? (void *)(a - _tls_used.start) : NULL;
}
#else
#include <elf.h>

#define SYM_PREFIX 0

static void *thread_pointer(void) {
    void *tp;
#if defined(__x86_64__)
    __asm__("mov %%fs:0, %0" : "=r"(tp));
#else
    __asm__("mrs %0, tpidr_el0" : "=r"(tp));
#endif
    return tp;
}

// A thread-local for a cell: its offset from the thread pointer, the same
// in every thread (the host's TLS is static).
static void *host_tls(const char *name) {
    char *a = dlsym(RTLD_DEFAULT, name);
    return a ? (void *)(a - (char *)thread_pointer()) : NULL;
}

static void *host_symbol(const char *name) { return dlsym(RTLD_DEFAULT, name); }
#endif

// An undefined symbol: the session's newest definition, else the
// host's (a thread-local: what `host_tls` gives).
static void *resolve(const char *name, int tls) {
    const char *bare = name + SYM_PREFIX;
    if (!strcmp(bare, "rok_exit")) return (void *)host_exit;
    if (!strcmp(bare, "rok_uncaught_error")) return (void *)host_uncaught;
    if (!strcmp(bare, "rok_uncaught_error_at")) return (void *)host_uncaught_at;
    const char *now = sym_now(name);
    void *a = now ? sym_addr(now) : NULL;
    if (a) return a;
    if (tls) return host_tls(name);
#if defined(_WIN32)
    return host_symbol(name);
#else
    a = dlsym(RTLD_DEFAULT, name + SYM_PREFIX);
    return a ? a : host_symbol(name);
#endif
}

// Relocation kinds, by what they compute (S symbol, A addend, P the
// place, G a slot holding S, T a slot holding the thread-local's value).
enum kind {
    K_ABS64,      // S + A, 64 bits
    K_ABS32,      // S + A, 32 bits
    K_PC32,       // S + A - P, 32 bits
    K_CALL32,     // S + A - P, x86_64 `call` / `jmp`: far through a stub
    K_GOT_PC32,   // G + A - P
    K_TLS_PC32,   // T + A - P
    K_B26,        // arm64 `b` / `bl` to S + A: far through a stub
    K_PAGE21,     // arm64 `adrp` of S + A
    K_LO12,       // arm64 low 12 bits of S + A (scaled for a load / store)
    K_GOT_PAGE21, // ... of G + A
    K_GOT_LO12,
    K_TLS_PAGE21, // ... of T + A
    K_TLS_LO12,
    K_RVA32,      // S + A - the object's data mapping, 32 bits: far through a stub
    K_HI12,       // arm64 `add` of bits 12-23 of S + A
};

// An object mapped, its symbols defined, not yet linked.
struct obj {
    const unsigned char *buf;
    const char *what;
    uint32_t nsects, nsyms;
    // Each section's mapped address (NULL: not mapped) and its address
    // in the object.
    char **base;
    uint64_t *at;
    // Each symbol's address (NULL: undefined here), and its GOT slot and
    // stub once made.
    char **addr, **got, **stub;
    // Code (executable) and data (writable) mappings; GOT slots go after
    // the data, branch stubs after the code.
    char *code, *data;
    size_t code_cap, data_cap, stubs, slots;
    void *entry;
};

// Section `k` of what is mapped: `size` bytes aligned to `align`, code
// or data, copied from `src` (NULL: zero).
struct place {
    uint64_t size, align;
    int code;
    const void *src;
};

// Map the sections (`p[k].size` 0 and `src` NULL: not mapped), with room
// for a stub and a slot per relocation (`nrel`).
static int map_sections(struct obj *o, const struct place *p, size_t nrel) {
    size_t code = 0, data = 0, *off = calloc(o->nsects + 1, sizeof *off);
    for (uint32_t k = 0; k < o->nsects; k++) {
        if (!p[k].align) continue;
        size_t *end = p[k].code ? &code : &data;
        *end = (*end + p[k].align - 1) & ~(size_t)(p[k].align - 1);
        off[k] = *end + 1;
        *end += p[k].size;
    }
    o->stubs = code = (code + 15) & ~(size_t)15;
    o->slots = data = (data + 15) & ~(size_t)15;
    o->code_cap = code + 16 * nrel + 16;
    o->data_cap = data + 8 * nrel + 16;
    // Data and code near each other and the host (cells may address runtime
    // data directly): `adrp` reaches 4 GiB, rip-relative 2 GiB. Only a hint;
    // a reference out of range is reported. Code after data, since COFF's
    // image-relative references count from the data.
    static char *near;
    if (!near) near = (char *)(((uintptr_t)&cells + ((size_t)1 << 29)) & ~(uintptr_t)0xffff);
    o->data = map_near(near, o->data_cap, 0);
    o->code = o->data ? map_near(o->data + o->data_cap, o->code_cap, 1) : NULL;
    if (o->code) near = o->code + o->code_cap;
    if (!o->code) {
        fprintf(stderr, "error: %s: cannot map executable memory\n", o->what);
        free(off);
        return 0;
    }
    o->base = calloc(o->nsects + 1, sizeof *o->base);
    jit_writable(1);
    for (uint32_t k = 0; k < o->nsects; k++) {
        if (!off[k]) continue;
        o->base[k] = (p[k].code ? o->code : o->data) + off[k] - 1;
        if (p[k].src) memcpy(o->base[k], p[k].src, p[k].size);
    }
    jit_writable(0);
    free(off);
    return 1;
}

static void alloc_syms(struct obj *o, uint32_t n) {
    o->nsyms = n;
    o->addr = calloc(n + 1, sizeof *o->addr);
    o->got = calloc(n + 1, sizeof *o->got);
    o->stub = calloc(n + 1, sizeof *o->stub);
}

// Symbol `i` defined at `addr`; `global`: the session's too.
static void def_sym(struct obj *o, uint32_t i, const char *name, char *addr, int global) {
    o->addr[i] = addr;
    if (global) define(name, addr);
    if (!strcmp(name + SYM_PREFIX, "rok_repl_cell")) o->entry = addr;
}

// The address of symbol `i` (`name`), defined here or resolved; 0:
// undefined (reported).
static uint64_t sym_value(struct obj *o, uint32_t i, const char *name, int tls) {
    if (o->addr[i] && !tls) return (uint64_t)(uintptr_t)o->addr[i];
    void *a = resolve(name, tls);
    if (!a) fprintf(stderr, "error: %s: undefined symbol `%s`\n", o->what, name + SYM_PREFIX);
    return (uint64_t)(uintptr_t)a;
}

static char *slot(struct obj *o, int64_t sym, uint64_t value) {
    char *g = sym >= 0 ? o->got[sym] : NULL;
    if (!g) {
        g = o->data + o->slots;
        o->slots += 8;
        memcpy(g, &value, 8);
        if (sym >= 0) o->got[sym] = g;
    }
    return g;
}

static char *stub(struct obj *o, int64_t sym, uint64_t target) {
    char *t = sym >= 0 ? o->stub[sym] : NULL;
    if (!t) {
        t = o->code + o->stubs;
        o->stubs += 16;
#if defined(__x86_64__)
        // jmp *2(%rip): the address at +8, after the 6-byte jmp and 2 pad
        static const unsigned char jmp[8] = {0xff, 0x25, 2, 0, 0, 0, 0x90, 0x90};
        memcpy(t, jmp, 8);
#else
        static const uint32_t br[2] = {0x58000050, 0xd61f0200}; // ldr x16, #8; br x16
        memcpy(t, br, 8);
#endif
        memcpy(t + 8, &target, 8);
        if (sym >= 0) o->stub[sym] = t;
    }
    return t;
}

// Apply one relocation at `p`: symbol index `sym` (-1: none) with value
// `s`, addend `a` (0: failed, reported).
static int fix(struct obj *o, char *p, enum kind k, int64_t sym, uint64_t s, int64_t a) {
    uint64_t pc = (uint64_t)(uintptr_t)p;
    switch (k) {
    case K_GOT_PC32: case K_GOT_PAGE21: case K_GOT_LO12:
    case K_TLS_PC32: case K_TLS_PAGE21: case K_TLS_LO12:
        s = (uint64_t)(uintptr_t)slot(o, sym, s);
        break;
    default:
        break;
    }
    uint64_t target = s + (uint64_t)a;
    switch (k) {
    case K_ABS64: {
        memcpy(p, &target, 8);
        return 1;
    }
    case K_ABS32: {
        uint32_t v = (uint32_t)target;
        memcpy(p, &v, 4);
        return 1;
    }
    case K_RVA32: {
        // An unwind handler in the host's code: through a stub.
        if (target - (uint64_t)(uintptr_t)o->data > UINT32_MAX && sym >= 0)
            target = (uint64_t)(uintptr_t)stub(o, sym, target);
        uint64_t v = target - (uint64_t)(uintptr_t)o->data;
        if (v > UINT32_MAX) {
            fprintf(stderr, "error: %s: an image-relative reference out of range\n", o->what);
            return 0;
        }
        uint32_t w = (uint32_t)v;
        memcpy(p, &w, 4);
        return 1;
    }
    case K_CALL32: case K_PC32: case K_GOT_PC32: case K_TLS_PC32: {
        int64_t d = (int64_t)(target - pc);
        if ((d > INT32_MAX || d < INT32_MIN) && k == K_CALL32) {
            // Far (the host's code): through a stub. The field ends the
            // instruction, so the destination is `target + 4`.
            d = stub(o, sym, target + 4) - (p + 4);
        }
        if (d > INT32_MAX || d < INT32_MIN) {
            fprintf(stderr, "error: %s: a pc-relative reference out of range\n", o->what);
            return 0;
        }
        int32_t v = (int32_t)d;
        memcpy(p, &v, 4);
        return 1;
    }
    default:
        break;
    }
    uint32_t ins;
    memcpy(&ins, p, 4);
    switch (k) {
    case K_B26: {
        int64_t d = (int64_t)(target - pc);
        if (d >= (1 << 27) || d < -(1 << 27)) d = stub(o, sym, target) - p;
        ins = (ins & 0xfc000000) | ((uint32_t)(d >> 2) & 0x03ffffff);
        break;
    }
    case K_PAGE21: case K_GOT_PAGE21: case K_TLS_PAGE21: {
        int64_t pages = (int64_t)(target >> 12) - (int64_t)(pc >> 12);
        if (pages >= (1 << 20) || pages < -(1 << 20)) {
            fprintf(stderr, "error: %s: a page reference out of range\n", o->what);
            return 0;
        }
        ins = (ins & 0x9f00001f) | (((uint32_t)pages & 3) << 29) | ((((uint32_t)pages >> 2) & 0x7ffff) << 5);
        break;
    }
    case K_HI12:
        ins = (ins & 0xffc003ff) | ((uint32_t)((target >> 12) & 0xfff) << 10);
        break;
    case K_LO12: case K_GOT_LO12: case K_TLS_LO12: {
        uint32_t lo = (uint32_t)(target & 0xfff);
        // A load / store scales its offset by the access size.
        if ((ins & 0x3b000000) == 0x39000000) {
            uint32_t scale = ins >> 30;
            if ((ins & 0x04800000) == 0x04800000) scale = 4; // 128-bit vector
            lo >>= scale;
        }
        ins = (ins & 0xffc003ff) | ((lo & 0xfff) << 10);
        break;
    }
    default:
        return 0;
    }
    memcpy(p, &ins, 4);
    return 1;
}

#if defined(__APPLE__)
// Mach-O (MH_OBJECT): the segment's sections, per-section relocations,
// addends in the instruction (x86_64) or a preceding ARM64_RELOC_ADDEND.
static int map_object(struct obj *o, const unsigned char *buf, size_t n, const char *what) {
    memset(o, 0, sizeof *o);
    o->buf = buf;
    o->what = what;
    const struct mach_header_64 *mh = (const void *)buf;
#if defined(__x86_64__)
    const cpu_type_t cpu = CPU_TYPE_X86_64;
#else
    const cpu_type_t cpu = CPU_TYPE_ARM64;
#endif
    if (n < sizeof *mh || mh->magic != MH_MAGIC_64 || mh->cputype != cpu) {
        fprintf(stderr, "error: %s: not a Mach-O object of this machine\n", what);
        return 0;
    }
    const struct section_64 *sects = NULL;
    const struct symtab_command *st = NULL;
    const struct load_command *lc = (const void *)(mh + 1);
    for (uint32_t i = 0; i < mh->ncmds; i++, lc = (const void *)((const char *)lc + lc->cmdsize)) {
        if (lc->cmd == LC_SEGMENT_64) {
            sects = (const void *)((const struct segment_command_64 *)lc + 1);
            o->nsects = ((const struct segment_command_64 *)lc)->nsects;
        }
        if (lc->cmd == LC_SYMTAB) st = (const void *)lc;
    }
    // What runs is mapped: not debug info, unwind tables or the like.
    struct place *p = calloc(o->nsects + 1, sizeof *p);
    o->at = calloc(o->nsects + 1, sizeof *o->at);
    size_t nrel = 0;
    for (uint32_t k = 0; k < o->nsects; k++) {
        const struct section_64 *s = &sects[k];
        o->at[k] = s->addr;
        if ((s->flags & S_ATTR_DEBUG) || !strcmp(s->segname, "__LD") || !strcmp(s->sectname, "__eh_frame"))
            continue;
        p[k].size = s->size;
        p[k].align = (uint64_t)1 << s->align;
        p[k].code = (s->flags & (S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS)) != 0;
        p[k].src = (s->flags & SECTION_TYPE) == S_ZEROFILL ? NULL : buf + s->offset;
        nrel += s->nreloc;
    }
    int ok = map_sections(o, p, nrel);
    free(p);
    if (!ok) return 0;
    const struct nlist_64 *nl = st ? (const void *)(buf + st->symoff) : NULL;
    const char *strs = st ? (const char *)buf + st->stroff : "";
    alloc_syms(o, st ? st->nsyms : 0);
    for (uint32_t i = 0; i < o->nsyms; i++) {
        const struct nlist_64 *s = &nl[i];
        if ((s->n_type & N_STAB) || (s->n_type & N_TYPE) != N_SECT) continue;
        uint32_t k = s->n_sect - 1;
        if (k >= o->nsects || !o->base[k]) continue;
        def_sym(o, i, strs + s->n_un.n_strx, o->base[k] + (s->n_value - o->at[k]), s->n_type & N_EXT);
    }
    return 1;
}

static int link_object(struct obj *o) {
    const struct mach_header_64 *mh = (const void *)o->buf;
    const struct section_64 *sects = NULL;
    const struct symtab_command *st = NULL;
    const struct load_command *lc = (const void *)(mh + 1);
    for (uint32_t i = 0; i < mh->ncmds; i++, lc = (const void *)((const char *)lc + lc->cmdsize)) {
        if (lc->cmd == LC_SEGMENT_64) sects = (const void *)((const struct segment_command_64 *)lc + 1);
        if (lc->cmd == LC_SYMTAB) st = (const void *)lc;
    }
    const struct nlist_64 *nl = st ? (const void *)(o->buf + st->symoff) : NULL;
    const char *strs = st ? (const char *)o->buf + st->stroff : "";
    int ok = 1;
    jit_writable(1);
    for (uint32_t k = 0; k < o->nsects && ok; k++) {
        if (!o->base[k]) continue;
        const struct relocation_info *rel = (const void *)(o->buf + sects[k].reloff);
        for (uint32_t r = 0; r < sects[k].nreloc && ok; r++) {
            struct relocation_info ri = rel[r];
            int64_t a = 0;
#if defined(__aarch64__)
            if (ri.r_type == ARM64_RELOC_ADDEND) {
                a = ((int64_t)ri.r_symbolnum << 40) >> 40;
                ri = rel[++r];
            }
#endif
            char *p = o->base[k] + ri.r_address;
            enum kind kind;
            int tls = 0;
#if defined(__x86_64__)
            int32_t field = 0;
            if (ri.r_length == 2) memcpy(&field, p, 4);
            int extra = 0; // SIGNED_<n>: n bytes of immediate after the field
            switch (ri.r_type) {
            case X86_64_RELOC_UNSIGNED: kind = ri.r_length == 3 ? K_ABS64 : K_ABS32; break;
            case X86_64_RELOC_SIGNED: kind = K_PC32; break;
            case X86_64_RELOC_SIGNED_1: kind = K_PC32; extra = 1; break;
            case X86_64_RELOC_SIGNED_2: kind = K_PC32; extra = 2; break;
            case X86_64_RELOC_SIGNED_4: kind = K_PC32; extra = 4; break;
            case X86_64_RELOC_BRANCH: kind = K_CALL32; break;
            case X86_64_RELOC_GOT_LOAD: case X86_64_RELOC_GOT: kind = K_GOT_PC32; break;
            case X86_64_RELOC_TLV: kind = K_TLS_PC32; tls = 1; break;
            default: kind = (enum kind)-1;
            }
            if (kind == K_ABS64) {
                int64_t v;
                memcpy(&v, p, 8);
                a = v;
            } else if (kind == K_ABS32) {
                a = field;
            } else {
                // From the end of the instruction, as the field counts.
                a = (int64_t)field - 4;
            }
#else
            switch (ri.r_type) {
            case ARM64_RELOC_UNSIGNED: kind = ri.r_length == 3 ? K_ABS64 : K_ABS32; break;
            case ARM64_RELOC_BRANCH26: kind = K_B26; break;
            case ARM64_RELOC_PAGE21: kind = K_PAGE21; break;
            case ARM64_RELOC_PAGEOFF12: kind = K_LO12; break;
            case ARM64_RELOC_GOT_LOAD_PAGE21: kind = K_GOT_PAGE21; break;
            case ARM64_RELOC_GOT_LOAD_PAGEOFF12: kind = K_GOT_LO12; break;
            case ARM64_RELOC_TLVP_LOAD_PAGE21: kind = K_TLS_PAGE21; tls = 1; break;
            case ARM64_RELOC_TLVP_LOAD_PAGEOFF12: kind = K_TLS_LO12; tls = 1; break;
            default: kind = (enum kind)-1;
            }
            if (kind == K_ABS64 || kind == K_ABS32) {
                // The bytes hold the addend (section-relative: the
                // object address).
                int64_t v = 0;
                memcpy(&v, p, kind == K_ABS64 ? 8 : 4);
                if (kind == K_ABS32) v = (int32_t)v;
                a += v;
            }
#endif
            if (kind == (enum kind)-1) {
                fprintf(stderr, "error: %s: relocation type %u is not supported\n", o->what, ri.r_type);
                ok = 0;
                break;
            }
            uint64_t s;
            int64_t sym = -1;
            if (ri.r_extern) {
                sym = ri.r_symbolnum;
                s = sym_value(o, (uint32_t)sym, strs + nl[sym].n_un.n_strx, tls);
                if (!s) {
                    ok = 0;
                    break;
                }
            } else {
                uint32_t i = ri.r_symbolnum - 1;
                if (i >= o->nsects || !o->base[i]) continue;
                // Section-relative: the target's object address moved by
                // its section's.
                s = (uint64_t)(uintptr_t)o->base[i] - o->at[i];
#if defined(__x86_64__)
                if (ri.r_pcrel) {
                    // The field held target - (place + 4 + extra), both
                    // object addresses.
                    uint64_t place = o->at[k] + ri.r_address;
                    s += place + 4 + (uint64_t)extra + (uint64_t)(int64_t)field;
                    a = -4 - extra;
                }
#endif
            }
            ok = fix(o, p, kind, sym, s, a);
        }
    }
    jit_writable(0);
    flush_code(o->code, o->code_cap);
    return ok;
}
#elif defined(_WIN32)
// COFF (and `/bigobj`): sections with relocations, addends in place;
// symbols with aux records (18-byte, 20 in a big object).
#if defined(__x86_64__)
#define COFF_MACHINE 0x8664
#define PDATA_ENTRY 12
#else
#define COFF_MACHINE 0xaa64
#define PDATA_ENTRY 8
#endif

struct coff_section {
    char name[8];
    uint32_t vsize, vaddr, size, off, reloff, lineoff;
    uint16_t nrel, nline;
    uint32_t flags;
};

struct __attribute__((packed)) coff_reloc {
    uint32_t at, sym;
    uint16_t type;
};

enum {
    SCN_CODE = 0x20,
    SCN_BSS = 0x80,
    SCN_INFO = 0x200,
    SCN_REMOVE = 0x800,
    SCN_NRELOC_OVFL = 0x01000000,
    SCN_DISCARDABLE = 0x02000000,
    SCN_EXECUTE = 0x20000000,
    SYM_EXTERNAL = 2,
    SYM_WEAK = 105,
};

// The header's fields the loader reads: `big` for a big object.
struct coff_layout {
    int big;
    uint16_t machine;
    uint32_t nsects, symoff, nsyms, sections;
};

static struct coff_layout coff_layout(const unsigned char *buf, size_t n) {
    struct coff_layout l = {0};
    uint16_t sig1, sig2, version;
    if (n < 20) return l;
    memcpy(&sig1, buf, 2);
    memcpy(&sig2, buf + 2, 2);
    memcpy(&version, buf + 4, 2);
    l.big = sig1 == 0 && sig2 == 0xffff && version >= 2 && n >= 56;
    if (l.big) {
        memcpy(&l.machine, buf + 6, 2);
        memcpy(&l.nsects, buf + 44, 4);
        memcpy(&l.symoff, buf + 48, 4);
        memcpy(&l.nsyms, buf + 52, 4);
        l.sections = 56;
    } else {
        uint16_t nsects, optsize;
        memcpy(&l.machine, buf, 2);
        memcpy(&nsects, buf + 2, 2);
        memcpy(&l.symoff, buf + 8, 4);
        memcpy(&l.nsyms, buf + 12, 4);
        memcpy(&optsize, buf + 16, 2);
        l.nsects = nsects;
        l.sections = 20;
        if (optsize) l.machine = 0;
    }
    return l;
}

static const struct coff_section *coff_section(const struct obj *o, uint32_t k) {
    return (const struct coff_section *)(o->buf + coff_layout(o->buf, SIZE_MAX).sections) + k;
}

// A symbol record's fields.
struct coff_symbol {
    const char *name;
    uint32_t value;
    int32_t sect;
    uint8_t class, naux;
    const unsigned char *aux;
};

static struct coff_symbol coff_symbol(const struct obj *o, uint32_t i) {
    struct coff_layout l = coff_layout(o->buf, SIZE_MAX);
    size_t size = l.big ? 20 : 18;
    const unsigned char *p = o->buf + l.symoff + (size_t)i * size;
    struct coff_symbol s = {(const char *)p, 0, 0, p[size - 2], p[size - 1], p + size};
    memcpy(&s.value, p + 8, 4);
    if (l.big) {
        memcpy(&s.sect, p + 12, 4);
    } else {
        int16_t sect;
        memcpy(&sect, p + 12, 2);
        s.sect = sect;
    }
    return s;
}

static const char *coff_strings(const struct obj *o) { return coff_symbol(o, coff_layout(o->buf, SIZE_MAX).nsyms).name; }

// A symbol's or section's name: inline (up to 8 bytes) or in the string
// table (symbols: 4 zero bytes and an offset; sections: `/<offset>`).
static const char *coff_name(const struct obj *o, const char raw[8], int section, char buf[9]) {
    uint32_t zero, off;
    memcpy(&zero, raw, 4);
    memcpy(&off, raw + 4, 4);
    if (!section && !zero) return coff_strings(o) + off;
    memcpy(buf, raw, 8);
    buf[8] = 0;
    if (section && buf[0] == '/') return coff_strings(o) + strtoul(buf + 1, NULL, 10);
    return buf;
}

static const struct coff_reloc *coff_relocs(const struct obj *o, uint32_t k, uint32_t *n) {
    const struct coff_section *s = coff_section(o, k);
    const struct coff_reloc *r = (const void *)(o->buf + s->reloff);
    *n = s->nrel;
    // More than 0xffff: the count (this entry included) in the first.
    if ((s->flags & SCN_NRELOC_OVFL) && s->nrel == 0xffff) {
        *n = r->at - 1;
        r++;
    }
    return r;
}

static int map_object(struct obj *o, const unsigned char *buf, size_t n, const char *what) {
    memset(o, 0, sizeof *o);
    o->buf = buf;
    o->what = what;
    struct coff_layout h = coff_layout(buf, n);
    if (h.machine != COFF_MACHINE) {
        fprintf(stderr, "error: %s: not a COFF object of this machine\n", what);
        return 0;
    }
    o->nsects = h.nsects;
    struct place *p = calloc(o->nsects + 1, sizeof *p);
    o->at = calloc(o->nsects + 1, sizeof *o->at);
    size_t nrel = 0;
    for (uint32_t k = 0; k < o->nsects; k++) {
        const struct coff_section *s = coff_section(o, k);
        uint32_t r;
        coff_relocs(o, k, &r);
        nrel += r;
        // What runs is mapped: not debug info or linker directives, nor
        // thread-locals (a cell uses the host's).
        if ((s->flags & (SCN_INFO | SCN_REMOVE | SCN_DISCARDABLE)) || !strncmp(s->name, ".tls", 4)) continue;
        uint32_t align = (s->flags >> 20) & 15;
        p[k].size = s->size;
        p[k].align = align ? (uint64_t)1 << (align - 1) : 16;
        p[k].code = (s->flags & (SCN_CODE | SCN_EXECUTE)) != 0;
        p[k].src = (s->flags & SCN_BSS) ? NULL : buf + s->off;
    }
    int ok = map_sections(o, p, nrel);
    free(p);
    if (!ok) return 0;
    alloc_syms(o, h.nsyms);
    char name[9];
    for (uint32_t i = 0; i < o->nsyms; i += 1 + coff_symbol(o, i).naux) {
        struct coff_symbol s = coff_symbol(o, i);
        if (s.sect <= 0 || (uint32_t)s.sect > o->nsects || !o->base[s.sect - 1]) continue;
        def_sym(o, i, coff_name(o, s.name, 0, name), o->base[s.sect - 1] + s.value, s.class == SYM_EXTERNAL);
    }
    // A weak definition: a weak external whose default (its auxiliary
    // record's first word) is defined here.
    for (uint32_t i = 0; i < o->nsyms; i += 1 + coff_symbol(o, i).naux) {
        struct coff_symbol s = coff_symbol(o, i);
        uint32_t tag;
        if (s.class != SYM_WEAK || !s.naux) continue;
        memcpy(&tag, s.aux, 4);
        if (tag < o->nsyms && o->addr[tag]) def_sym(o, i, coff_name(o, s.name, 0, name), o->addr[tag], 1);
    }
    return 1;
}

static int by_start(const void *a, const void *b) {
    uint32_t x, y;
    memcpy(&x, a, 4);
    memcpy(&y, b, 4);
    return (x > y) - (x < y);
}

static int link_object(struct obj *o) {
    int ok = 1;
    char name[9];
    jit_writable(1);
    for (uint32_t k = 0; k < o->nsects && ok; k++) {
        if (!o->base[k]) continue;
        uint32_t nrel;
        const struct coff_reloc *rel = coff_relocs(o, k, &nrel);
        for (uint32_t r = 0; r < nrel && ok; r++) {
            char *p = o->base[k] + rel[r].at;
            enum kind kind;
            int tls = 0;
            int32_t field;
            memcpy(&field, p, 4);
            int64_t a = field;
#if defined(__x86_64__)
            switch (rel[r].type) {
            case 0: continue; // ABSOLUTE
            case 1: kind = K_ABS64; memcpy(&a, p, 8); break;
            case 2: kind = K_ABS32; a = (uint32_t)field; break;
            case 3: kind = K_RVA32; a = (uint32_t)field; break;
            case 4: case 5: case 6: case 7: case 8: case 9: {
                // REL32_<n>: n bytes of immediate after the field; a
                // `call` / `jmp` reaches far through a stub.
                int extra = rel[r].type - 4;
                int branch = !extra && rel[r].at && ((unsigned char)p[-1] == 0xe8 || (unsigned char)p[-1] == 0xe9);
                kind = branch ? K_CALL32 : K_PC32;
                a = (int64_t)field - 4 - extra;
                break;
            }
            case 0xb: kind = K_ABS32; tls = 1; break; // SECREL
            default: kind = (enum kind)-1;
            }
#else
            uint32_t ins = (uint32_t)field;
            // A load / store scales its offset by the access size.
            uint32_t scale = (ins & 0x04800000) == 0x04800000 ? 4 : ins >> 30;
            switch (rel[r].type) {
            case 0: continue; // ABSOLUTE
            case 1: kind = K_ABS32; a = (uint32_t)field; break;
            case 2: kind = K_RVA32; a = (uint32_t)field; break;
            case 3: kind = K_B26; a = (int64_t)((int32_t)(ins << 6) >> 6) * 4; break;
            case 4: // PAGEBASE_REL21
                kind = K_PAGE21;
                a = ((int64_t)(((ins >> 29) & 3) | ((ins >> 3) & 0x1ffffc)) << 43) >> 43;
                break;
            case 6: kind = K_LO12; a = (ins >> 10) & 0xfff; break;           // PAGEOFFSET_12A
            case 7: kind = K_LO12; a = ((ins >> 10) & 0xfff) << scale; break; // PAGEOFFSET_12L
            case 8: kind = K_ABS32; a = (uint32_t)field; tls = 1; break;     // SECREL
            case 9: kind = K_LO12; a = (ins >> 10) & 0xfff; tls = 1; break;  // SECREL_LOW12A
            case 0xa: kind = K_HI12; a = (int64_t)((ins >> 10) & 0xfff) << 12; tls = 1; break;
            case 0xb: kind = K_LO12; a = ((ins >> 10) & 0xfff) << scale; tls = 1; break;
            case 0xe: kind = K_ABS64; memcpy(&a, p, 8); break;
            case 0x11: kind = K_PC32; a = (int64_t)field - 4; break; // REL32: from the next byte
            default: kind = (enum kind)-1;
            }
#endif
            if (kind == (enum kind)-1) {
                fprintf(stderr, "error: %s: relocation type %u is not supported\n", o->what, rel[r].type);
                ok = 0;
                break;
            }
            uint32_t i = rel[r].sym;
            struct coff_symbol s = coff_symbol(o, i);
            uint64_t v;
            int64_t sym = -1;
            if (s.class == SYM_EXTERNAL || s.class == SYM_WEAK) {
                sym = i;
                const char *n = coff_name(o, s.name, 0, name);
                // An undefined `__imp_<name>` (dllimport): an import slot of
                // this object holding `<name>`.
                void *imp = !tls && !o->addr[i] && !strncmp(n, "__imp_", 6) && !resolve(n, 0) ? resolve(n + 6, 0) : NULL;
                v = imp ? (uint64_t)(uintptr_t)slot(o, sym, (uint64_t)(uintptr_t)imp) : sym_value(o, i, n, tls);
                if (!v) {
                    ok = 0;
                    break;
                }
            } else {
                // A section or local symbol of this object.
                if (!o->addr[i]) continue;
                v = (uint64_t)(uintptr_t)o->addr[i];
            }
            ok = fix(o, p, kind, sym, v, a);
        }
    }
    // One unwind table from every `.pdata` section (an entry per function),
    // image-relative to the data mapping and sorted, so exceptions pass
    // through the object's frames.
    size_t entries = 0;
    for (uint32_t k = 0; k < o->nsects; k++)
        if (o->base[k] && !strncmp(coff_name(o, coff_section(o, k)->name, 1, name), ".pdata", 6))
            entries += coff_section(o, k)->size / PDATA_ENTRY;
    char *table = ok && entries ? malloc(entries * PDATA_ENTRY) : NULL;
    if (table) {
        size_t at = 0;
        for (uint32_t k = 0; k < o->nsects; k++) {
            const struct coff_section *s = coff_section(o, k);
            if (!o->base[k] || strncmp(coff_name(o, s->name, 1, name), ".pdata", 6)) continue;
            memcpy(table + at, o->base[k], s->size / PDATA_ENTRY * PDATA_ENTRY);
            at += s->size / PDATA_ENTRY * PDATA_ENTRY;
        }
        qsort(table, entries, PDATA_ENTRY, by_start);
        RtlAddFunctionTable(table, (DWORD)entries, (uint64_t)(uintptr_t)o->data);
    }
    jit_writable(0);
    flush_code(o->code, o->code_cap);
    return ok;
}
#else
// ELF (ET_REL, 64-bit little-endian): allocated sections; RELA sections
// name the section they apply to.
#if defined(__x86_64__)
#define ELF_MACHINE EM_X86_64
#else
#define ELF_MACHINE EM_AARCH64
#endif

static const Elf64_Shdr *elf_sections(const unsigned char *buf) {
    return (const void *)(buf + ((const Elf64_Ehdr *)buf)->e_shoff);
}

static int map_object(struct obj *o, const unsigned char *buf, size_t n, const char *what) {
    memset(o, 0, sizeof *o);
    o->buf = buf;
    o->what = what;
    const Elf64_Ehdr *eh = (const void *)buf;
    if (n < sizeof *eh || memcmp(eh->e_ident, ELFMAG, SELFMAG) || eh->e_ident[EI_CLASS] != ELFCLASS64 ||
        eh->e_type != ET_REL || eh->e_machine != ELF_MACHINE) {
        fprintf(stderr, "error: %s: not an ELF object of this machine\n", what);
        return 0;
    }
    const Elf64_Shdr *sh = elf_sections(buf);
    o->nsects = eh->e_shnum;
    struct place *p = calloc(o->nsects + 1, sizeof *p);
    o->at = calloc(o->nsects + 1, sizeof *o->at);
    size_t nrel = 0;
    const Elf64_Shdr *symtab = NULL;
    const char *names = (const char *)buf + sh[eh->e_shstrndx].sh_offset;
    for (uint32_t k = 0; k < o->nsects; k++) {
        const Elf64_Shdr *s = &sh[k];
        if (s->sh_type == SHT_SYMTAB) symtab = s;
        if (s->sh_type == SHT_RELA) nrel += s->sh_size / sizeof(Elf64_Rela);
        // What runs is mapped: not unwind tables, nor thread-locals (a
        // cell uses the host's).
        if (!(s->sh_flags & SHF_ALLOC) || (s->sh_flags & SHF_TLS) || !strcmp(names + s->sh_name, ".eh_frame"))
            continue;
        p[k].size = s->sh_size;
        p[k].align = s->sh_addralign ? s->sh_addralign : 1;
        p[k].code = (s->sh_flags & SHF_EXECINSTR) != 0;
        p[k].src = s->sh_type == SHT_NOBITS ? NULL : buf + s->sh_offset;
    }
    int ok = map_sections(o, p, nrel);
    free(p);
    if (!ok) return 0;
    if (!symtab) return 1;
    const Elf64_Sym *sy = (const void *)(buf + symtab->sh_offset);
    const char *strs = (const char *)buf + sh[symtab->sh_link].sh_offset;
    alloc_syms(o, (uint32_t)(symtab->sh_size / sizeof *sy));
    for (uint32_t i = 1; i < o->nsyms; i++) {
        const Elf64_Sym *s = &sy[i];
        if (s->st_shndx == SHN_UNDEF || s->st_shndx >= SHN_LORESERVE || !o->base[s->st_shndx]) continue;
        if (ELF64_ST_TYPE(s->st_info) == STT_TLS) continue;
        int global = ELF64_ST_BIND(s->st_info) != STB_LOCAL && s->st_name;
        def_sym(o, i, strs + s->st_name, o->base[s->st_shndx] + s->st_value, global);
    }
    return 1;
}

static int link_object(struct obj *o) {
    const Elf64_Ehdr *eh = (const void *)o->buf;
    const Elf64_Shdr *sh = elf_sections(o->buf);
    int ok = 1;
    jit_writable(1);
    for (uint32_t k = 0; k < o->nsects && ok; k++) {
        const Elf64_Shdr *rs = &sh[k];
        if (rs->sh_type != SHT_RELA || rs->sh_info >= eh->e_shnum || !o->base[rs->sh_info]) continue;
        const Elf64_Shdr *symtab = &sh[rs->sh_link];
        const Elf64_Sym *sy = (const void *)(o->buf + symtab->sh_offset);
        const char *strs = (const char *)o->buf + sh[symtab->sh_link].sh_offset;
        const Elf64_Rela *rel = (const void *)(o->buf + rs->sh_offset);
        for (size_t r = 0; r < rs->sh_size / sizeof *rel && ok; r++) {
            uint32_t type = ELF64_R_TYPE(rel[r].r_info), i = ELF64_R_SYM(rel[r].r_info);
            char *p = o->base[rs->sh_info] + rel[r].r_offset;
            enum kind kind;
            int tls = 0;
            switch (type) {
#if defined(__x86_64__)
            case R_X86_64_NONE: continue;
            case R_X86_64_64: kind = K_ABS64; break;
            case R_X86_64_32: case R_X86_64_32S: kind = K_ABS32; break;
            case R_X86_64_PC32: kind = K_PC32; break;
            case R_X86_64_PLT32: kind = K_CALL32; break;
            case R_X86_64_GOTPCREL: case R_X86_64_GOTPCRELX: case R_X86_64_REX_GOTPCRELX: kind = K_GOT_PC32; break;
            case R_X86_64_GOTTPOFF: kind = K_TLS_PC32; tls = 1; break;
#else
            case R_AARCH64_NONE: continue;
            case R_AARCH64_ABS64: kind = K_ABS64; break;
            case R_AARCH64_ABS32: kind = K_ABS32; break;
            case R_AARCH64_PREL32: kind = K_PC32; break;
            case R_AARCH64_CALL26: case R_AARCH64_JUMP26: kind = K_B26; break;
            case R_AARCH64_ADR_PREL_PG_HI21: kind = K_PAGE21; break;
            case R_AARCH64_ADD_ABS_LO12_NC: case R_AARCH64_LDST8_ABS_LO12_NC: case R_AARCH64_LDST16_ABS_LO12_NC:
            case R_AARCH64_LDST32_ABS_LO12_NC: case R_AARCH64_LDST64_ABS_LO12_NC: case R_AARCH64_LDST128_ABS_LO12_NC:
                kind = K_LO12; break;
            case R_AARCH64_ADR_GOT_PAGE: kind = K_GOT_PAGE21; break;
            case R_AARCH64_LD64_GOT_LO12_NC: kind = K_GOT_LO12; break;
            case R_AARCH64_TLSIE_ADR_GOTTPREL_PAGE21: kind = K_TLS_PAGE21; tls = 1; break;
            case R_AARCH64_TLSIE_LD64_GOTTPREL_LO12_NC: kind = K_TLS_LO12; tls = 1; break;
#endif
            default:
                fprintf(stderr, "error: %s: relocation type %u is not supported\n", o->what, type);
                ok = 0;
                continue;
            }
            const Elf64_Sym *s = &sy[i];
            uint64_t v;
            int64_t sym = -1;
            if (ELF64_ST_BIND(s->st_info) == STB_LOCAL && ELF64_ST_TYPE(s->st_info) != STT_TLS) {
                // A section or local symbol of this object.
                if (s->st_shndx >= o->nsects || !o->base[s->st_shndx]) continue;
                v = (uint64_t)(uintptr_t)o->base[s->st_shndx] + s->st_value;
            } else {
                sym = i;
                v = sym_value(o, i, strs + s->st_name, tls);
                if (!v) {
                    ok = 0;
                    break;
                }
            }
            ok = fix(o, p, kind, sym, v, rel[r].r_addend);
        }
    }
    jit_writable(0);
    flush_code(o->code, o->code_cap);
    return ok;
}
#endif

static void free_obj(struct obj *o) {
    free(o->base);
    free(o->at);
    free(o->addr);
    free(o->got);
    free(o->stub);
}

// Map and link the cell; its entry point (NULL: failed, reported).
static void (*jit_cell(const char *obj, size_t n))(void) {
    cells++;
    struct obj o;
    int ok = map_object(&o, (const unsigned char *)obj, n, "cell") && link_object(&o) && o.entry;
    void *entry = o.entry;
    free_obj(&o);
    return ok ? (void (*)(void))entry : NULL;
}

// A shared library, loaded by the system: later symbols resolve
// against it too.
static int load_shared(const char *path) {
#if defined(_WIN32)
    HANDLE m = nshared < (int)(sizeof shared / sizeof *shared) ? LoadLibraryA(path) : NULL;
    if (!m) {
        fprintf(stderr, "error: cannot load %s\n", path);
        return 1;
    }
    shared[nshared++] = m;
    return 0;
#else
    if (dlopen(path, RTLD_NOW | RTLD_GLOBAL)) return 0;
    fprintf(stderr, "error: %s\n", dlerror());
    return 1;
#endif
}

// Link every member of an archive (BSD, GNU or COFF `ar`) now, all mapped
// first so they can refer to each other; other files are shared libraries
// (`load_shared`).
static int link_lib(const char *path) {
    FILE *f = fopen(path, "rb");
    if (!f) return 1;
    fseek(f, 0, SEEK_END);
    size_t size = (size_t)ftell(f);
    char *a = malloc(size + 1);
    fseek(f, 0, SEEK_SET);
    int whole = a && fread(a, 1, size, f) == size;
    fclose(f);
    if (!whole || size < 8 || memcmp(a, "!<arch>\n", 8)) {
        free(a);
        return whole ? load_shared(path) : 1;
    }
    struct obj objs[256];
    int n = 0, bad = 0;
    cells++;
    for (size_t off = 8; off + 60 <= size && n < 256 && !bad;) {
        const char *h = a + off;
        size_t len = strtoul(h + 48, NULL, 10), skip = 0;
        if (!memcmp(h, "#1/", 3)) skip = strtoul(h + 3, NULL, 10);
        const char *name = skip ? h + 60 : h;
        const unsigned char *body = (const unsigned char *)h + 60 + skip;
        // Not the symbol table (BSD `__.SYMDEF`, GNU and COFF `/`,
        // `/SYM64/`) nor the long-name table `//`.
        int index = !strncmp(name, "__.SYMDEF", 9) || (name[0] == '/' && (name[1] == ' ' || name[1] == '/')) ||
                    !strncmp(name, "/SYM64/", 7);
        if (!index && len > skip) {
            if (map_object(&objs[n], body, len - skip, path)) n++;
            else bad = 1;
        }
        off += 60 + len;
        off += off & 1;
    }
    for (int i = 0; i < n; i++) {
        if (!bad && !link_object(&objs[i])) bad = 1;
        free_obj(&objs[i]);
    }
    free(a);
    return bad;
}

// The command channel: our stdin.
#if defined(_WIN32)
static HANDLE ctl;

static ssize_t ctl_read(void *buf, size_t n) {
    DWORD got;
    return ReadFile(ctl, buf, (DWORD)n, &got, NULL) ? (ssize_t)got : -1;
}

static void ctl_write(const void *buf, size_t n) {
    DWORD put;
    WriteFile(ctl, buf, (DWORD)n, &put, NULL);
}
#else
static int ctl;

static ssize_t ctl_read(void *buf, size_t n) { return read(ctl, buf, n); }
#endif

// A command line: `<word> <n>`.
static int read_command(char *line, size_t cap) {
    size_t n = 0;
    char c;
    while (ctl_read(&c, 1) == 1 && c != '\n')
        if (n < cap - 1) line[n++] = c;
    line[n] = 0;
    return n > 0;
}

static char *read_ir(size_t n) {
    char *ir = malloc(n + 1);
    size_t got = 0;
    while (ir && got < n) {
        ssize_t r = ctl_read(ir + got, n - got);
        if (r <= 0) {
            free(ir);
            return NULL;
        }
        got += (size_t)r;
    }
    return ir;
}

#if defined(_WIN32)
void rok_runtime_yield(void);
void rok_leave_native(void);
// This thread's runtime TLS block (803) via the runtime's accessor: clang
// guards C `extern __declspec(thread)` with `__dyn_tls_on_demand_init`,
// which needs the MSVC CRT. `ROK_TLS_YIELD_FLAG` comes from build.rs.
unsigned char *rok_tls_addr(void);

// Windows stand-in for fork: before a cell, with runtime threads quiesced
// and the rest suspended, copy the session's writable memory (GC and
// malloc heaps, statics, TLS blocks, runtime and loader state, sealed image
// sections: `sealed_part`) and the other threads' registers and stacks. On
// failure: end threads started since, put the copy back, decommit memory
// committed since, and resume the main thread at its recovery point. A
// thread waiting where it was (an idle worker) gets its state back; a
// runtime thread that moved on is ended and the runtime told
// (`rok_session_restored`); others go on. Not saved: thread stacks and
// TEBs, cell code, this bookkeeping.
//
// Copy, not copy-on-write: the kernel writes into buffers (ReadFile, recv)
// without faulting, so a protected page would fail the call. The runtime's
// own memory is write-watched (`rok_watch_writes`) against a shadow copy,
// so a checkpoint copies only pages written since the last one and a
// rollback only those written since the checkpoint (everything when
// `rok_page_epoch` moved). Other writable memory is copied whole.

typedef struct {
    char *base, *alloc;
    DWORD alloc_protect;
    uint16_t partition;
    size_t size;
    DWORD state, protect, type;
} MEMORY_BASIC_INFORMATION;

size_t VirtualQuery(const void *, MEMORY_BASIC_INFORMATION *, size_t);
BOOL VirtualProtect(void *, size_t, DWORD, DWORD *);
BOOL VirtualFree(void *, size_t, DWORD);
int32_t NtGetNextThread(HANDLE, HANDLE, DWORD, DWORD, DWORD, HANDLE *);
int32_t NtQueryInformationThread(HANDLE, int, void *, DWORD, DWORD *);
DWORD GetThreadId(HANDLE);
DWORD SuspendThread(HANDLE);
DWORD ResumeThread(HANDLE);
BOOL TerminateThread(HANDLE, DWORD);
BOOL GetThreadContext(HANDLE, void *);
DWORD WaitForSingleObject(HANDLE, DWORD);
BOOL CloseHandle(HANDLE);
void ExitProcess(unsigned);
int _putenv_s(const char *, const char *);
BOOL SetThreadContext(HANDLE, const void *);
unsigned GetWriteWatch(DWORD, void *, size_t, void **, uintptr_t *, DWORD *);
unsigned ResetWriteWatch(void *, size_t);
void rok_watch_writes(void);
extern volatile uint64_t rok_page_epoch;
size_t rok_world_threads(uint64_t *, size_t);
void rok_io_quiesce(void);

enum {
    MEM_COMMIT = 0x1000,
    MEM_RESERVE = 0x2000,
    MEM_DECOMMIT = 0x4000,
    MEM_RELEASE = 0x8000,
    MEM_FREE = 0x10000,
    MEM_WRITE_WATCH = 0x200000,
    MEM_PRIVATE = 0x20000,
    MEM_IMAGE = 0x1000000,
    PAGE_NOACCESS = 1,
    PAGE_READONLY = 2,
    PAGE_READWRITE = 4,
    PAGE_WRITECOPY = 8,
    PAGE_GUARD = 0x100,
    THREADS = 256,
    PAGE = 4096,
};

// A region of the address space at the checkpoint; `copy`: where its
// bytes are saved (SIZE_MAX: not saved).
struct region {
    char *base, *alloc;
    size_t size, copy;
    DWORD state, type, protect;
    // Write-watched: saved in its allocation's shadow, not the copy.
    int watched;
};

// A write-watched allocation's shadow: its saved bytes at the same
// offsets (committed as its regions are).
struct shadow {
    char *alloc, *copy;
    size_t size;
    int seen;
};

enum { SHADOWS = 1024, DIRTY = 65536 };

// A CONTEXT (x64: 1232 bytes, arm64: 912) and what is read of it.
typedef struct {
    _Alignas(16) unsigned char b[1232];
} context;

#if defined(__x86_64__)
enum { CTX_FLAGS = 0x30, CTX_SP = 0x98, CTX_PC = 0xf8, CTX_FULL = 0x10000b, CTX_CONTROL = 0x100001 };
// Rbx, Rbp, Rsi, Rdi, R12-R15.
static const unsigned short ctx_kept[] = {0x90, 0xa0, 0xa8, 0xb0, 0xd8, 0xe0, 0xe8, 0xf0};
#else
enum { CTX_FLAGS = 0, CTX_SP = 0x100, CTX_PC = 0x108, CTX_FULL = 0x400007, CTX_CONTROL = 0x400001 };
// X19-X28, Fp.
static const unsigned short ctx_kept[] = {0xa0, 0xa8, 0xb0, 0xb8, 0xc0, 0xc8, 0xd0, 0xd8, 0xe0, 0xe8, 0xf0};
#endif

static uint64_t ctx_word(const context *c, unsigned off) {
    uint64_t v;
    memcpy(&v, c->b + off, 8);
    return v;
}

// A thread from before the cell: its registers, and its stack from the
// stack pointer up to the base (saved at `copy` in the copy).
struct before {
    DWORD id;
    int rok;
    char *sp, *base;
    size_t copy;
    context ctx;
};

// Bookkeeping that outlives a rollback: its own allocation, not saved.
static struct book {
    // The recovery point, and a CONTEXT to read suspended threads into.
    context recover, probe;
    volatile int running, ended, overflowed;
    // `why` the cell failed, which a rollback would undo.
    char why[sizeof why];
    size_t nwhy;
    // The threads stopped now.
    HANDLE handle[THREADS];
    DWORD id[THREADS];
    size_t nthreads;
    // The other threads at the checkpoint, and the runtime's (OS ids).
    struct before before[THREADS];
    size_t nbefore;
    uint64_t rok[THREADS];
    size_t nrok;
    // The runtime's threads the last rollback ended or found ended.
    uint64_t lost[THREADS];
    size_t nlost;
    // Allocations not saved or freed: stacks, TEBs, the bookkeeping.
    char *excl[2 * THREADS + 4];
    size_t nexcl;
    char *copy;
    size_t copy_cap;
    struct shadow shadows[SHADOWS];
    size_t nshadows;
    // `rok_page_epoch` at the checkpoint; written pages found.
    uint64_t epoch;
    void *dirty[DIRTY];
    size_t nregions, cap;
    struct region regions[];
} *book;

enum { BOOK_SIZE = 16 << 20 };

static DWORD main_thread;
// ntdll's image: where the system thread pool's workers start.
static const char *ntdll;
// The main thread's yield flag, and the bit of it that interrupts the
// cell (the runtime raises and clears only bit 0).
static unsigned char *main_yield;
enum { INTERRUPT = 2 };

static int listed(const DWORD *ids, size_t n, DWORD id) {
    for (size_t i = 0; i < n; i++)
        if (ids[i] == id) return 1;
    return 0;
}

static int listed64(const uint64_t *ids, size_t n, uint64_t id) {
    for (size_t i = 0; i < n; i++)
        if (ids[i] == id) return 1;
    return 0;
}

// Suspend every other thread, including new ones, into `book->handle` /
// `id`. Takes no lock a stopped thread may hold (no heap, no stdio).
static void stop_others(void) {
    book->nthreads = 0;
    for (int more = 1; more;) {
        more = 0;
        HANDLE cur = NULL, next;
        int kept = 0;
        while (!NtGetNextThread((HANDLE)-1, cur, 0x1fffff, 0, 0, &next)) {
            if (cur && !kept) CloseHandle(cur);
            cur = next;
            kept = 0;
            DWORD id = GetThreadId(cur);
            if (id == main_thread || listed(book->id, book->nthreads, id) || book->nthreads == THREADS) continue;
            if (WaitForSingleObject(cur, 0) == 0) continue; // it has ended
            SuspendThread(cur);
            // Suspension completes by the time a context can be read.
            *(DWORD *)(book->probe.b + CTX_FLAGS) = CTX_CONTROL;
            GetThreadContext(cur, &book->probe);
            book->handle[book->nthreads] = cur;
            book->id[book->nthreads++] = id;
            kept = more = 1;
        }
        if (cur && !kept) CloseHandle(cur);
    }
}

static void resume_others(void) {
    for (size_t i = 0; i < book->nthreads; i++) {
        if (!book->handle[i]) continue;
        ResumeThread(book->handle[i]);
        CloseHandle(book->handle[i]);
    }
    book->nthreads = 0;
}

static void exclude(const void *at) {
    MEMORY_BASIC_INFORMATION m;
    if (at && VirtualQuery(at, &m, sizeof m) == sizeof m) book->excl[book->nexcl++] = m.alloc;
}

static int excluded(const char *alloc) {
    for (size_t i = 0; i < book->nexcl; i++)
        if (book->excl[i] == alloc) return 1;
    return 0;
}

// A thread's TEB (NULL: gone).
static char *teb_of(HANDLE h) {
    struct {
        int32_t status;
        char *teb;
        void *rest[4];
    } basic;
    return NtQueryInformationThread(h, 0, &basic, sizeof basic, NULL) ? NULL : basic.teb;
}

// A thread's TEB and stack (its NT_TIB's StackLimit).
static void exclude_thread(HANDLE h) {
    char *teb = teb_of(h);
    if (!teb) return;
    exclude(teb);
    exclude(*(char **)(teb + 0x10));
}

static void exclusions(void) {
    book->nexcl = 0;
    exclude(book);
    exclude(book->copy);
    for (size_t i = 0; i < book->nshadows; i++) exclude(book->shadows[i].copy);
    exclude_thread((HANDLE)-2);
    for (size_t i = 0; i < book->nthreads; i++)
        if (book->handle[i]) exclude_thread(book->handle[i]);
}

static int saved(const MEMORY_BASIC_INFORMATION *m) {
    DWORD p = m->protect & 0xff;
    return m->state == MEM_COMMIT && (m->type == MEM_PRIVATE || m->type == MEM_IMAGE) &&
           (p == PAGE_READWRITE || p == PAGE_WRITECOPY) && !(m->protect & PAGE_GUARD) && !excluded(m->alloc);
}

// The part of region `m` in image section `k` that is read-only now but
// marked writable: ntdll's `.mrdata`, unprotected only while written,
// holds list heads (function tables, loader) whose nodes are heap memory,
// so it is saved with the heap.
static int sealed_part(const MEMORY_BASIC_INFORMATION *m, uint32_t k, char **lo, char **hi) {
    const unsigned char *img = (const unsigned char *)m->alloc;
    uint32_t pe, vsize, va, flags;
    uint16_t nsect, optsize;
    memcpy(&pe, img + 0x3c, 4);
    memcpy(&nsect, img + pe + 6, 2);
    memcpy(&optsize, img + pe + 20, 2);
    if (k >= nsect) return 0;
    const unsigned char *s = img + pe + 24 + optsize + 40 * (size_t)k;
    memcpy(&vsize, s + 8, 4);
    memcpy(&va, s + 12, 4);
    memcpy(&flags, s + 36, 4);
    if (!(flags & 0x80000000u)) return 0; // IMAGE_SCN_MEM_WRITE
    char *a = (char *)img + va, *b = a + (((size_t)vsize + PAGE - 1) & ~(size_t)(PAGE - 1));
    *lo = a > m->base ? a : m->base;
    *hi = b < m->base + m->size ? b : m->base + m->size;
    return *lo < *hi;
}

static struct shadow *shadow_of(const char *alloc) {
    for (size_t i = 0; i < book->nshadows; i++)
        if (book->shadows[i].alloc == alloc) return &book->shadows[i];
    return NULL;
}

// The pages of `base..base + size` (at most DIRTY) written since the
// last reset, into `book->dirty`; -1: not write-watched.
static long dirty_pages(char *base, size_t size, int reset) {
    uintptr_t n = DIRTY;
    DWORD granularity;
    return GetWriteWatch(reset, base, size, book->dirty, &n, &granularity) ? -1 : (long)n;
}

// The checkpoint's extent of allocation `alloc`.
static size_t extent_of(const char *alloc) {
    size_t extent = 0;
    for (size_t i = 0; i < book->nregions; i++) {
        const struct region *q = &book->regions[i];
        if (q->alloc == alloc && (size_t)(q->base + q->size - alloc) > extent) extent = (size_t)(q->base + q->size - alloc);
    }
    return extent;
}

// Bring watched regions' shadows up to date (pages written since the last
// checkpoint; all of a new region, or all when `rok_page_epoch` moved),
// resetting the watch. 0: no room.
static int save_watched(void) {
    uint64_t epoch = rok_page_epoch;
    for (size_t i = 0; i < book->nshadows; i++) book->shadows[i].seen = 0;
    for (size_t i = 0; i < book->nregions; i++) {
        struct region *r = &book->regions[i];
        if (!r->watched) continue;
        size_t extent = extent_of(r->alloc);
        struct shadow *sh = shadow_of(r->alloc);
        int whole = epoch != book->epoch;
        if (sh && sh->size < extent) {
            VirtualFree(sh->copy, 0, MEM_RELEASE);
            *sh = book->shadows[--book->nshadows];
            sh = NULL;
        }
        if (!sh) {
            char *c = book->nshadows < SHADOWS ? VirtualAlloc(NULL, extent, MEM_RESERVE, PAGE_NOACCESS) : NULL;
            if (!c) return 0;
            sh = &book->shadows[book->nshadows++];
            *sh = (struct shadow){r->alloc, c, extent, 0};
            whole = 1;
        }
        sh->seen = 1;
        char *copy = sh->copy + (r->base - r->alloc);
        if (!VirtualAlloc(copy, r->size, MEM_COMMIT, PAGE_READWRITE)) return 0;
        if (whole) {
            memcpy(copy, r->base, r->size);
            ResetWriteWatch(r->base, r->size);
            continue;
        }
        for (size_t off = 0; off < r->size; off += (size_t)DIRTY * PAGE) {
            size_t len = r->size - off < (size_t)DIRTY * PAGE ? r->size - off : (size_t)DIRTY * PAGE;
            long n = dirty_pages(r->base + off, len, 1);
            if (n < 0) return 0; // the walk found it watched
            for (long k = 0; k < n; k++) {
                char *page = book->dirty[k];
                memcpy(copy + (page - r->base), page, PAGE);
            }
        }
    }
    // Allocations that are gone.
    for (size_t i = 0; i < book->nshadows;) {
        if (book->shadows[i].seen) {
            i++;
            continue;
        }
        VirtualFree(book->shadows[i].copy, 0, MEM_RELEASE);
        book->shadows[i] = book->shadows[--book->nshadows];
    }
    book->epoch = epoch;
    return 1;
}

// Save the session before the cell. 0: no room; -1: Ctrl-C while waiting
// for quiet, so the cell doesn't run.
static int checkpoint(void) {
    if (!quiesce()) return -1;
    book->nrok = rok_world_threads(book->rok, THREADS);
    stop_others();
    book->nbefore = book->nthreads;
    for (size_t i = 0; i < book->nthreads; i++) {
        struct before *t = &book->before[i];
        char *teb = teb_of(book->handle[i]);
        *(DWORD *)(t->ctx.b + CTX_FLAGS) = CTX_FULL;
        GetThreadContext(book->handle[i], &t->ctx);
        t->id = book->id[i];
        t->rok = listed64(book->rok, book->nrok, t->id);
        t->sp = (char *)ctx_word(&t->ctx, CTX_SP);
        t->base = teb ? *(char **)(teb + 0x8) : t->sp;
        if (t->base < t->sp) t->base = t->sp;
    }
    exclusions();
    MEMORY_BASIC_INFORMATION m;
    size_t n = 0, total = 0;
    int ok = 1;
    for (char *a = NULL; ok && VirtualQuery(a, &m, sizeof m) == sizeof m; a = m.base + m.size) {
        if (m.state == MEM_FREE) continue;
        if (n == book->cap) ok = 0;
        else if (m.state == MEM_COMMIT && m.type == MEM_IMAGE && (m.protect & 0xff) == PAGE_READONLY) {
            char *lo, *hi;
            for (uint32_t k = 0; ok && k < 96; k++) {
                if (!sealed_part(&m, k, &lo, &hi)) continue;
                if (n == book->cap) ok = 0;
                else {
                    book->regions[n++] =
                        (struct region){lo, m.alloc, (size_t)(hi - lo), total, m.state, m.type, m.protect, 0};
                    total += (size_t)(hi - lo);
                }
            }
        } else {
            int keep = saved(&m);
            int watched = keep && m.type == MEM_PRIVATE && dirty_pages(m.base, PAGE, 0) >= 0;
            book->regions[n++] = (struct region){
                m.base, m.alloc, m.size, keep && !watched ? total : SIZE_MAX, m.state, m.type, m.protect, watched};
            if (keep && !watched) total += m.size;
        }
    }
    book->nregions = n;
    for (size_t i = 0; i < book->nbefore; i++) {
        book->before[i].copy = total;
        total += (size_t)(book->before[i].base - book->before[i].sp);
    }
    if (ok && total > book->copy_cap) {
        if (book->copy) VirtualFree(book->copy, 0, MEM_RELEASE);
        book->copy_cap = total + total / 4;
        book->copy = VirtualAlloc(NULL, book->copy_cap, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if (!book->copy) book->copy_cap = 0, ok = 0;
    }
    for (size_t i = 0; ok && i < n; i++) {
        struct region *r = &book->regions[i];
        if (r->copy != SIZE_MAX) memcpy(book->copy + r->copy, r->base, r->size);
    }
    for (size_t i = 0; ok && i < book->nbefore; i++) {
        struct before *t = &book->before[i];
        memcpy(book->copy + t->copy, t->sp, (size_t)(t->base - t->sp));
    }
    ok = ok && save_watched();
    resume_others();
    rok_session_resume();
    return ok;
}

// The checkpoint's region holding `p` (NULL: none, it was free).
static struct region *region_at(const char *p) {
    size_t lo = 0, hi = book->nregions;
    while (lo < hi) {
        size_t mid = lo + (hi - lo) / 2;
        struct region *r = &book->regions[mid];
        if (p < r->base) hi = mid;
        else if (p >= r->base + r->size) lo = mid + 1;
        else return r;
    }
    return NULL;
}

static void lost(void) {
    static const char msg[] = "error: the session cannot be restored\n";
    DWORD put;
    WriteFile(GetStdHandle((DWORD)-12), msg, sizeof msg - 1, &put, NULL);
    ExitProcess(1);
}

// Restore a saved region: recommit (re-reserving a released allocation),
// reprotect, rewrite; a watched one from its shadow, pages written since
// the checkpoint (`all`: every differing page).
static void put_back(const struct region *r, int all) {
    if (r->type == MEM_PRIVATE && !VirtualAlloc(r->base, r->size, MEM_COMMIT, PAGE_READWRITE)) {
        VirtualAlloc(r->alloc, extent_of(r->alloc), MEM_RESERVE | (r->watched ? MEM_WRITE_WATCH : 0), PAGE_NOACCESS);
        if (!VirtualAlloc(r->base, r->size, MEM_COMMIT, PAGE_READWRITE)) lost();
        all = 1;
    }
    DWORD old, p = r->protect & 0xff;
    // A sealed section is written unprotected, then sealed again.
    int sealed = p != PAGE_READWRITE && p != PAGE_WRITECOPY;
    if (!VirtualProtect(r->base, r->size, sealed ? PAGE_WRITECOPY : r->protect, &old)) lost();
    const char *from = r->watched ? shadow_of(r->alloc)->copy + (r->base - r->alloc) : book->copy + r->copy;
    for (size_t off = 0; r->watched && !all && off < r->size; off += (size_t)DIRTY * PAGE) {
        size_t len = r->size - off < (size_t)DIRTY * PAGE ? r->size - off : (size_t)DIRTY * PAGE;
        long n = dirty_pages(r->base + off, len, 0);
        if (n < 0) all = 1;
        for (long k = 0; k < n; k++) {
            char *page = book->dirty[k];
            memcpy(page, from + (page - r->base), PAGE);
        }
    }
    // Pages the cell did not change are left alone (an image's stay
    // shared).
    for (size_t off = 0; (!r->watched || all) && off < r->size; off += PAGE)
        if (memcmp(r->base + off, from + off, PAGE)) memcpy(r->base + off, from + off, PAGE);
    if (r->watched) ResetWriteWatch(r->base, r->size);
    if (sealed && !VirtualProtect(r->base, r->size, r->protect, &old)) lost();
}

// Private memory committed since the checkpoint: decommitted, or
// released with its allocation when that is new.
static void free_new(void) {
    MEMORY_BASIC_INFORMATION m;
    for (char *a = NULL; VirtualQuery(a, &m, sizeof m) == sizeof m; a = m.base + m.size) {
        if (m.state != MEM_COMMIT || m.type != MEM_PRIVATE || excluded(m.alloc)) continue;
        const struct region *first = region_at(m.alloc);
        if (!first || first->alloc != m.alloc) {
            VirtualFree(m.alloc, 0, MEM_RELEASE);
            continue;
        }
        for (char *p = m.base, *end = m.base + m.size; p < end;) {
            const struct region *r = region_at(p);
            char *to = r && r->base + r->size < end ? r->base + r->size : end;
            if (!r || r->state != MEM_COMMIT) VirtualFree(p, (size_t)(to - p), MEM_DECOMMIT);
            p = to;
        }
    }
}

// Back to the checkpoint: the threads started since end, and memory is
// as it was then.
static void end_thread(size_t i) {
    TerminateThread(book->handle[i], 1);
    WaitForSingleObject(book->handle[i], 5000);
    CloseHandle(book->handle[i]);
    book->handle[i] = NULL;
}

// The thread from before the cell with id `id` (NULL: none).
static struct before *before_of(DWORD id) {
    for (size_t i = 0; i < book->nbefore; i++)
        if (book->before[i].id == id) return &book->before[i];
    return NULL;
}

// Is stopped thread `i` where it was at the checkpoint? Then its registers
// and stack go back.
static int put_thread_back(size_t i, const struct before *t) {
    context *now = &book->probe;
    *(DWORD *)(now->b + CTX_FLAGS) = CTX_FULL;
    if (!GetThreadContext(book->handle[i], now) || ctx_word(now, CTX_PC) != ctx_word(&t->ctx, CTX_PC) ||
        ctx_word(now, CTX_SP) != ctx_word(&t->ctx, CTX_SP))
        return 0;
    size_t n = (size_t)(t->base - t->sp);
    int same = !memcmp(t->sp, book->copy + t->copy, n);
    for (size_t k = 0; k < sizeof ctx_kept / sizeof *ctx_kept; k++)
        same = same && ctx_word(now, ctx_kept[k]) == ctx_word(&t->ctx, ctx_kept[k]);
    if (!same) {
        memcpy(t->sp, book->copy + t->copy, n);
        SetThreadContext(book->handle[i], &t->ctx);
    }
    return 1;
}

// Restore a gone runtime thread's stack (re-mapping freed memory) so what
// restored memory links to there (a waiter entry) stays readable.
static void restore_stack(const struct before *t) {
    char *lo = (char *)((uintptr_t)t->sp & ~(uintptr_t)(PAGE - 1));
    size_t n = (size_t)(t->base - lo);
    if (!n) return;
    if (!VirtualAlloc(lo, n, MEM_COMMIT, PAGE_READWRITE) && !VirtualAlloc(lo, n, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE))
        lost();
    memcpy(t->sp, book->copy + t->copy, (size_t)(t->base - t->sp));
}

// Is thread `i` a system thread-pool worker (starts in ntdll), which a
// busy machine may start after the checkpoint? Ending one kills the
// process (STATUS_INVALID_THREAD) and letting it run faults on the
// restored pool state, so it stays suspended (one extra suspension;
// `stop_others` finds it each time, so its stack is never put back).
static int pool_worker(size_t i) {
    void *start = NULL;
    MEMORY_BASIC_INFORMATION m;
    // 9: ThreadQuerySetWin32StartAddress.
    return !NtQueryInformationThread(book->handle[i], 9, &start, sizeof start, NULL) &&
           VirtualQuery(start, &m, sizeof m) == sizeof m && m.alloc == ntdll;
}

static void rollback(void) {
    stop_others();
    // The I/O poller's requests in flight write into memory about to be
    // put back.
    rok_io_quiesce();
    book->nlost = 0;
    for (size_t i = 0; i < book->nthreads; i++) {
        const struct before *t = before_of(book->id[i]);
        if (!t) { // started since
            if (pool_worker(i)) SuspendThread(book->handle[i]);
            else end_thread(i);
        }
        else if (!put_thread_back(i, t) && t->rok) {
            book->lost[book->nlost++] = t->id;
            end_thread(i);
        }
    }
    // A runtime thread from before that has ended since.
    for (size_t i = 0; i < book->nbefore; i++)
        if (book->before[i].rok && !listed(book->id, book->nthreads, book->before[i].id))
            book->lost[book->nlost++] = book->before[i].id;
    exclusions();
    // New memory first: what was released since may have been reused.
    free_new();
    // Read before the runtime's statics go back.
    int all = rok_page_epoch != book->epoch;
    for (size_t i = 0; i < book->nregions; i++)
        if (book->regions[i].copy != SIZE_MAX || book->regions[i].watched) put_back(&book->regions[i], all);
    for (size_t i = 0; i < book->nbefore; i++)
        if (listed64(book->lost, book->nlost, book->before[i].id)) restore_stack(&book->before[i]);
    resume_others();
}

// End the running cell: the session as before it, then back to the
// recovery point (`run_cell`).
static void fail_cell(void) {
    book->running = 0;
    book->ended = 1;
    memcpy(book->why, why, nwhy);
    book->nwhy = nwhy;
    rollback();
    RtlRestoreContext(&book->recover, NULL);
}

// The runtime is about to end the process (panic, uncaught error, `exit`,
// stack overflow): on the cell's thread end only the cell, after the
// stdout flush `exit` would do.
static void on_exit_hook(int status) {
    if (!book->running || GetCurrentThreadId() != main_thread) return;
    if (!nwhy && status == 101) set_why("panic");
    if (!book->overflowed) rok_stdio_flush();
    fail_cell();
}

// A stack overflow on the cell's thread, noted before the runtime's
// handler reports it (and ends the cell through `on_exit_hook`).
static long on_overflow(void *pointers) {
    if (book->running && GetCurrentThreadId() == main_thread && **(DWORD **)pointers == 0xc00000fd)
        book->overflowed = 1;
    return 0; // EXCEPTION_CONTINUE_SEARCH
}

// A fault nothing handled (unwinding through the cell's frames by its
// unwind tables): on the cell's thread it ends the cell.
static long on_fault(void *pointers) {
    if (!book->running || GetCurrentThreadId() != main_thread) return 0; // EXCEPTION_CONTINUE_SEARCH
    // EXCEPTION_POINTERS: the record first, its code first.
    DWORD code = **(DWORD **)pointers;
    fprintf(stderr, "error: the cell raised exception 0x%08x\n", (unsigned)code);
    fail_cell();
    return 0;
}

// End the cell if it was interrupted and this is its thread.
static void interrupted(void) {
    if (!(*main_yield & INTERRUPT) || GetCurrentThreadId() != main_thread) return;
    set_why("interrupted");
    fail_cell();
}

// The safepoint slow path and the way back from a blocking call that
// cells call: the runtime's, then the end of an interrupted cell.
static void host_yield(void) {
    rok_runtime_yield();
    interrupted();
}

static void host_leave_native(void) {
    rok_leave_native();
    interrupted();
}

// Ctrl-Break / Ctrl-C on its own thread: the cell ends at its next poll or
// return from a blocking call, both of which see the raised yield flag.
static BOOL on_break(DWORD event) {
    if (event > 1) return 0; // not CTRL_C_EVENT / CTRL_BREAK_EVENT
    if (quiescing) quiesce_cancel = 1;
    if (book->running) __atomic_fetch_or(main_yield, INTERRUPT, __ATOMIC_SEQ_CST);
    return 1;
}

// Link and run the cell from a recovery point; 0: it failed (`fail_cell`
// came back here).
static int run_cell(const char *obj, size_t n) {
    book->ended = 0;
    RtlCaptureContext(&book->recover);
    if (book->ended) return 0;
    void (*cell)(void) = jit_cell(obj, n);
    if (!cell) fail_cell();
    book->running = 1;
    cell();
    book->running = 0;
    return 1;
}

// rokc waits for the host before removing the session directory, and
// a running host.exe could not delete itself anyway.
static void remove_session(void) {}

int main(void) {
    const DWORD std_input = (DWORD)-10;
    ctl = GetStdHandle(std_input);
    // The cells' stdin: NUL (GENERIC_READ, shared, OPEN_EXISTING).
    SetStdHandle(std_input, CreateFileA("NUL", 0x80000000u, 3, NULL, 3, 0, NULL));
    // No concurrent marking: its marker thread would live on across a
    // rollback with its own idea of the heap.
    _putenv_s("ROK_GC_CONCURRENT", "off");
    rok_watch_writes();
    rok_runtime_init();
    book = VirtualAlloc(NULL, BOOK_SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!book) return 1;
    book->cap = (BOOK_SIZE - sizeof *book) / sizeof *book->regions;
    main_thread = GetCurrentThreadId();
    ntdll = (const char *)GetModuleHandleA("ntdll.dll");
    main_yield = rok_tls_addr() + ROK_TLS_YIELD_FLAG;
    rok_set_exit_hook(on_exit_hook);
    SetUnhandledExceptionFilter(on_fault);
    AddVectoredExceptionHandler(1, on_overflow);
    SetConsoleCtrlHandler(on_break, 1);
    for (;;) {
        char line[8192], out[64];
        size_t n = 0;
        if (!read_command(line, sizeof line)) {
            remove_session();  // rokc has ended the session, or died
            return 0;
        }
        char *ir = NULL;
        if (sscanf(line, "lib %zu", &n) == 1) {
            char *path = read_ir(n);
            if (!path) return 1;
            path[n] = 0;
            int bad = link_lib(path);
            free(path);
            ctl_write(bad ? "err\n" : "ok 0\n", bad ? 4 : 5);
            continue;
        }
        if (sscanf(line, "cell %zu", &n) == 1) ir = read_ir(n);
        if (!ir) return 1;
        rok_stdio_flush();
        ctl_write(out, (size_t)snprintf(out, sizeof out, "pid %u\n", (unsigned)GetCurrentProcessId()));
        __atomic_fetch_and(main_yield, ~INTERRUPT, __ATOMIC_SEQ_CST);
        __rok_repl_out.len = 0;
        set_why("");
        book->nwhy = 0;
        int saved = checkpoint();
        if (!saved) fprintf(stderr, "error: no room to save the session before the cell\n");
        if (saved < 0) {
            memcpy(book->why, "interrupted", 11);
            book->nwhy = 11;
        }
        int ok = saved > 0 && run_cell(ir, n);
        free(ir);
        if (!ok) {
            // The stack's guard page again, after an overflow.
            if (book->overflowed) _resetstkoflw();
            book->overflowed = 0;
            // Memory is as it was at the checkpoint: the world stopped.
            if (saved > 0) {
                rok_session_resume();
                rok_session_restored(book->lost, book->nlost);
            }
            ctl_write(out, (size_t)snprintf(out, sizeof out, "err %zu\n", book->nwhy));
            ctl_write(book->why, book->nwhy);
            continue;
        }
        rok_stdio_flush();
        ctl_write(out, (size_t)snprintf(out, sizeof out, "ok %zu\n", __rok_repl_out.len));
        ctl_write(__rok_repl_out.ptr, __rok_repl_out.len);
    }
}
#else
static void on_sigint(int sig) {
    (void)sig;
    quiesce_cancel = 1;
}

// The running cell's end of the verdict pipe (`await_verdict`); -1: none.
static int verdict_fd = -1;

// A cell that ends early tells the copy waiting for it `!` and `why`, in
// one write (at most PIPE_BUF bytes: read whole).
static void tell_why(void) {
    char msg[1 + sizeof why];
    msg[0] = '!';
    memcpy(msg + 1, why, nwhy);
    if (verdict_fd >= 0 && write(verdict_fd, msg, 1 + nwhy) < 0) {}
    verdict_fd = -1;
}

// The runtime is about to end the process (panic, uncaught error, `exit`).
static void on_exit_hook(int status) {
    if (verdict_fd < 0) return;
    if (!nwhy && status == 101) set_why("panic");
    tell_why();
}

static void on_cell_sigint(int sig) {
    (void)sig;
    set_why("interrupted");
    tell_why();
    _exit(130);
}

// rokc, and its session directory (the host's own). The host ends with
// rokc: between cells on the channel's EOF, during a cell through the
// copy waiting for it (`await_verdict`). Whichever ends last removes the
// directory, unless rokc keeps temporaries.
static pid_t rokc_pid;
static char session_dir[4096];

static int remove_entry(const char *p, const struct stat *st, int flag, struct FTW *f) {
    (void)st;
    (void)flag;
    (void)f;
    remove(p);
    return 0;
}

static void remove_session(void) {
    char keep[4200];
    snprintf(keep, sizeof keep, "%s/.rok-keep", session_dir);
    if (session_dir[0] && access(keep, F_OK) != 0)
        nftw(session_dir, remove_entry, 16, FTW_DEPTH | FTW_PHYS);
}

// `exe`'s directory, if it is a rokc temporary (`rokc-<pid>-...`).
static void set_session_dir(const char *exe) {
    const char *slash = strrchr(exe, '/');
    if (!slash || (size_t)(slash - exe) >= sizeof session_dir) return;
    memcpy(session_dir, exe, (size_t)(slash - exe));
    session_dir[slash - exe] = 0;
    const char *base = strrchr(session_dir, '/');
    if (!base || strncmp(base + 1, "rokc-", 5) != 0) session_dir[0] = 0;
}

// A descriptor readable once rokc exits; -1 if it has, -2 if it can't be
// watched (then polled).
static int watch_rokc(void) {
#if defined(__APPLE__)
    int kq = kqueue();
    if (kq < 0) return -2;
    struct kevent ev;
    EV_SET(&ev, rokc_pid, EVFILT_PROC, EV_ADD | EV_ONESHOT, NOTE_EXIT, 0, NULL);
    if (kevent(kq, &ev, 1, NULL, 0, NULL) == 0) return kq;
#else
    long kq = syscall(SYS_pidfd_open, rokc_pid, 0);  // Linux 5.3+
    if (kq >= 0) return (int)kq;
#endif
    int gone = errno == ESRCH;
#if defined(__APPLE__)
    close(kq);
#endif
    return gone ? -1 : -2;
}

// The copy from before a cell waits for its verdict on `fd`, read into
// `buf` (`cap` bytes): a 0 byte, else `tell_why`'s or nothing (it died).
// Should rokc die meanwhile, nothing would end the cell: end it and the
// session.
static ssize_t await_verdict(int fd, char *buf, size_t cap, pid_t host) {
    int w = watch_rokc();
    while (w != -1) {
        struct pollfd p[2] = {{fd, POLLIN, 0}, {w, POLLIN, 0}};
        int n = poll(p, w >= 0 ? 2 : 1, w >= 0 ? -1 : 1000);
        if (n < 0 && errno != EINTR) break;
        if (n > 0 && p[0].revents) {
            if (w >= 0) close(w);
            ssize_t r;
            while ((r = read(fd, buf, cap)) < 0 && errno == EINTR) {}
            return r;
        }
        if (w >= 0 ? n > 0 : kill(rokc_pid, 0) != 0 && errno == ESRCH) break;
    }
    if (getppid() == host) kill(host, SIGKILL);
    remove_session();
    _exit(0);
}

int main(int argc, char **argv) {
    rokc_pid = getppid();
    if (argc > 0) set_session_dir(argv[0]);
    ctl = dup(0);
    int nul = open("/dev/null", O_RDONLY);
    dup2(nul, 0);  // the cells' stdin
    close(nul);
    // No concurrent marking: its marker thread is not in a snapshot.
    setenv("ROK_GC_CONCURRENT", "off", 1);
    rok_runtime_init();
    rok_set_exit_hook(on_exit_hook);
    signal(SIGINT, SIG_IGN);
    for (;;) {
        char line[8192], got[1 + sizeof why];
        size_t n = 0;
        if (!read_command(line, sizeof line)) return 0;
        char *ir = NULL;
        if (sscanf(line, "lib %zu", &n) == 1) {
            char *path = read_ir(n);
            if (!path) return 1;
            path[n] = 0;
            int bad = link_lib(path);
            free(path);
            dprintf(ctl, bad ? "err\n" : "ok 0\n");
            continue;
        }
        if (sscanf(line, "cell %zu", &n) == 1) ir = read_ir(n);
        if (!ir) return 1;
        rok_stdio_flush();
        // A copy of the session from before the cell takes over if the
        // cell's process dies (panic, signal, exit, compile failure), else
        // is told to go. The JIT has no threads; other threads are quiesced
        // while the copy is made and forgotten when it takes over (the
        // executor restarts its own, their locks are released).
        int verdict[2];
        if (pipe(verdict) != 0) return 1;
        dprintf(ctl, "pid %d\n", (int)getpid());
        signal(SIGINT, on_sigint);
        int quiet = quiesce();
        signal(SIGINT, SIG_IGN);
        if (!quiet) {
            free(ir);
            close(verdict[0]);
            close(verdict[1]);
            dprintf(ctl, "err 11\ninterrupted");
            continue;
        }
        pid_t host = getpid();
        pid_t snap = fork();
        rok_session_resume();
        if (snap == 0) {
            close(verdict[1]);
            ssize_t r = await_verdict(verdict[0], got, sizeof got, host);
            if (r > 0 && !got[0]) _exit(0);
            close(verdict[0]);
            free(ir);
            rok_session_restored(NULL, SIZE_MAX);
            size_t len = r > 1 ? (size_t)r - 1 : 0;
            dprintf(ctl, "err %zu\n", len);
            if (write(ctl, got + 1, len) < 0) return 1;
            continue;
        }
        close(verdict[0]);
        nwhy = 0;
        verdict_fd = verdict[1];
        signal(SIGINT, on_cell_sigint);
        void (*cell)(void) = jit_cell(ir, n);
        free(ir);
        if (!cell) _exit(1);  // the copy, from before the cell, goes on
        __rok_repl_out.len = 0;
        cell();
        signal(SIGINT, SIG_IGN);
        verdict_fd = -1;
        rok_stdio_flush();
        if (write(verdict[1], "", 1) != 1) {}
        close(verdict[1]);
        if (snap > 0) waitpid(snap, NULL, 0);
        dprintf(ctl, "ok %zu\n", __rok_repl_out.len);
        if (write(ctl, __rok_repl_out.ptr, __rok_repl_out.len) < 0) return 1;
    }
}
#endif
