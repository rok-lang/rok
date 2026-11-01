//! Target specifications: everything compiler, linker and runtime build
//! know about a target, in one value.
//!
//! `--target` takes a built-in name (`rokc --print target-list`) or a
//! `<name>.rokon` holding a full `TargetSpec { .. }` (`rokc --print
//! target-spec` prints one). Nothing else looks at a target's name.

mod builtin;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::OnceLock;

use rokc_rokon::{Decoder, Kind, Value};

pub use builtin::BUILTIN;

/// The target this rokc runs on, the default `--target`.
pub const HOST: &str = env!("ROK_TARGET_HOST");

/// An enum of the spec, written `Type::Variant` in ROKON.
trait Named: Copy + Eq + 'static {
    const TYPE: &'static str;
    const ALL: &'static [(Self, &'static str)];
}

macro_rules! named_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident,)* }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub enum $name { $($(#[$vm])* $v,)* }

        impl Named for $name {
            const TYPE: &'static str = stringify!($name);
            const ALL: &'static [(Self, &'static str)] = &[$((Self::$v, stringify!($v)),)*];
        }
    };
}

named_enum! {
    /// Platform C calling convention for aggregates (spec 8.6.3).
    CAbi {
        Aarch64,
        X86_64SysV,
        X86_64Win,
        /// i386 System V (cdecl).
        X86SysV,
        /// Windows x86 (MSVC cdecl).
        X86Win,
        /// 32-bit ARM AAPCS, hard-float variant.
        ArmHf,
        /// The WebAssembly C ABI (clang's): a one-scalar aggregate as that
        /// scalar, others by reference / `sret`.
        Wasm32,
        /// RISC-V LP64D: the hardware floating-point convention.
        Riscv64,
        /// Aggregates cannot cross the C boundary.
        Other,
    }
}

impl CAbi {
    /// A by-value C struct of two words (a rok `str`) goes by reference
    /// and comes back through `sret`.
    #[must_use]
    pub const fn pair_by_ref(self) -> bool {
        matches!(self, Self::X86_64Win | Self::Wasm32)
    }
}

named_enum! {
    /// The object file format.
    ObjectFormat {
        MachO,
        Elf,
        Coff,
        Wasm,
    }
}

named_enum! {
    /// Who links an executable: one of lld's flavors.
    Linker {
        /// `ld64.lld`, against the macOS SDK.
        Ld64,
        /// `ld.lld`.
        Ld,
        /// `lld-link`, against import libraries rokc makes.
        LldLink,
        /// `wasm-ld`: a core module.
        WasmLd,
        /// `wasm-component-ld` (shipped in the target's directory) with
        /// rokc as its `wasm-ld`: a WASI preview 2 component.
        WasmComponentLd,
    }
}

impl Linker {
    /// lld's flavor name (`args[0]`).
    #[must_use]
    pub const fn flavor(self) -> &'static str {
        match self {
            Self::Ld64 => "ld64.lld",
            Self::Ld => "ld.lld",
            Self::LldLink => "lld-link",
            Self::WasmLd | Self::WasmComponentLd => "wasm-ld",
        }
    }
}

named_enum! {
    /// The C library an executable links.
    Libc {
        /// macOS's `libSystem`, from the SDK.
        LibSystem,
        /// Windows' UCRT, through import libraries rokc carries.
        Ucrt,
        /// glibc: link stubs, run against the system's.
        Glibc,
        /// musl, linked statically.
        Musl,
        WasiLibc,
        FreeBsd,
        None,
    }
}

impl Libc {
    /// It ships in the toolchain, in the target's directory
    /// (`lib/rok/targets/<name>/`) with the C runtime objects.
    #[must_use]
    pub const fn shipped(self) -> bool {
        matches!(
            self,
            Self::Glibc | Self::Musl | Self::WasiLibc | Self::FreeBsd
        )
    }
}

named_enum! {
    /// How a function with a large frame probes the stack's guard page.
    StackProbes {
        /// LLVM's inline probe loop.
        Inline,
        /// A call to `__chkstk` (the startup object rokc links defines it).
        Call,
        None,
    }
}

/// A spec field's ROKON form.
trait Field: Sized {
    fn to_value(&self) -> Value;
    fn decode(d: Decoder<'_>) -> Result<Self, String>;
}

impl Field for String {
    fn to_value(&self) -> Value {
        Value::str(self.as_str())
    }
    fn decode(d: Decoder<'_>) -> Result<Self, String> {
        d.string()
    }
}

impl Field for bool {
    fn to_value(&self) -> Value {
        Value::new(Kind::Bool(*self))
    }
    fn decode(d: Decoder<'_>) -> Result<Self, String> {
        d.bool()
    }
}

impl Field for u32 {
    fn to_value(&self) -> Value {
        Value::new(Kind::Int((*self).into()))
    }
    fn decode(d: Decoder<'_>) -> Result<Self, String> {
        d.int()
    }
}

impl Field for u64 {
    fn to_value(&self) -> Value {
        Value::new(Kind::Int((*self).into()))
    }
    fn decode(d: Decoder<'_>) -> Result<Self, String> {
        d.int()
    }
}

impl Field for Vec<String> {
    fn to_value(&self) -> Value {
        Value::new(Kind::List(self.iter().map(Field::to_value).collect()))
    }
    fn decode(d: Decoder<'_>) -> Result<Self, String> {
        d.list()?.into_iter().map(|e| e.string()).collect()
    }
}

impl<T: Named> Field for T {
    fn to_value(&self) -> Value {
        let name = T::ALL
            .iter()
            .find(|(v, _)| v == self)
            .map_or("", |(_, n)| n);
        Value::call(&format!("{}::{name}", T::TYPE), Vec::new(), Vec::new())
    }
    fn decode(d: Decoder<'_>) -> Result<Self, String> {
        let v = d.variant(T::TYPE)?;
        let names: Vec<String> = T::ALL
            .iter()
            .map(|(_, n)| format!("{}::{n}", T::TYPE))
            .collect();
        match T::ALL.iter().find(|(_, n)| *n == v.name) {
            Some((t, _)) if v.args.is_empty() && v.fields.names().next().is_none() => Ok(*t),
            _ => Err(d.error(&format!("expected one of {}", names.join(", ")))),
        }
    }
}

macro_rules! spec {
    ($($(#[$doc:meta])* $f:ident: $t:ty,)*) => {
        /// A compilation target. The fields are those of its ROKON form,
        /// in order; `name` is the built-in name or the file's stem.
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct TargetSpec {
            pub name: String,
            $($(#[$doc])* pub $f: $t,)*
        }

        const FIELDS: &[&str] = &[$(stringify!($f)),*];

        impl TargetSpec {
    /// `extern "system"` is `stdcall` here (32-bit x86 Windows, spec
    /// 5.9.2), not the C ABI.
    #[must_use]
    pub fn stdcall_system(&self) -> bool {
        self.arch == "x86" && self.os == "windows"
    }

            fn fields(&self) -> Vec<(&'static str, Value)> {
                vec![$((stringify!($f), self.$f.to_value())),*]
            }

            fn decode(name: &str, r: &rokc_rokon::Record<'_>) -> Result<Self, String> {
                r.only(FIELDS)?;
                Ok(Self {
                    name: name.to_owned(),
                    $($f: Field::decode(r.req(stringify!($f))?)?,)*
                })
            }
        }
    };
}

spec! {
    /// LLVM's target triple, without the OS version.
    llvm_target: String,
    /// The minimum OS version, appended to `llvm_target` (Mach-O:
    /// `MACOSX_DEPLOYMENT_TARGET` overrides it); empty elsewhere.
    deployment_target: String,
    /// `target_arch` (spec 6.3.2).
    arch: String,
    /// `target_os`; `none` without an operating system.
    os: String,
    /// `target_env`: `gnu`, `musl`, `msvc`, or empty.
    env: String,
    vendor: String,
    /// `target_family`: `unix`, `windows`, `wasm`, or empty.
    family: String,
    /// `target_endian`: `little` or `big`.
    endian: String,
    /// `target_pointer_width`, in bits.
    pointer_width: u32,
    /// ABI alignment of `i64` / `u64` / `f64`: 4 under the i386 System V ABI.
    wide_align: u32,
    /// The widest atomic operation, in bits.
    max_atomic_width: u32,
    c_abi: CAbi,
    /// LLVM's `target datalayout`.
    data_layout: String,
    /// LLVM's CPU and features (spec 0.6.4's SIMD baseline).
    cpu: String,
    features: String,
    object_format: ObjectFormat,
    linker: Linker,
    /// The target's own linker arguments, before the inputs.
    pre_link_args: Vec<String>,
    /// C runtime objects before the inputs, in the target's directory.
    crt_objects: Vec<String>,
    /// After the inputs: linker arguments (`-..`) and files in the
    /// target's directory.
    late_link_args: Vec<String>,
    /// Executables load shared libraries.
    dynamic_linking: bool,
    /// ELF: the program interpreter of a dynamic executable.
    dynamic_loader: String,
    libc: Libc,
    /// The Rust target the runtime (`rokr`) is built for.
    rust_target: String,
    exe_suffix: String,
    staticlib_prefix: String,
    staticlib_suffix: String,
    dylib_prefix: String,
    dylib_suffix: String,
    /// What the object format puts before a C name (`_` on Mach-O and
    /// 32-bit Windows).
    c_symbol_prefix: String,
    /// The main thread's stack, in bytes, where the executable sets it.
    stack_size: u64,
    stack_probes: StackProbes,
    /// Programs can start threads.
    threads: bool,
    /// rok's own backend generates its code at `-O0`.
    native_backend: bool,
    /// The REPL's session host runs on it.
    repl: bool,
    /// compiler-rt's profile runtime in the target's directory, which
    /// `-C profile-generate` links; empty: none.
    profile_runtime: String,
}

impl TargetSpec {
    /// The built-in target `name`.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        builtin::spec(name)
    }

    /// The target of `--target`: `None` for the host, a built-in name,
    /// or a `<name>.rokon` file.
    pub fn resolve(target: Option<&str>) -> Result<Self, String> {
        let mut t = match target {
            None => Self::builtin(HOST)
                .ok_or_else(|| format!("rokc's host {HOST} is not a built-in target"))?,
            Some(p) if Path::new(p).extension().is_some_and(|e| e == "rokon") => {
                Self::load(Path::new(p))?
            }
            Some(n) => Self::builtin(n).ok_or_else(|| {
                let near = rokc_core::suggest::suggest(n, BUILTIN.iter().copied())
                    .map(|s| format!("; did you mean `{s}`?"))
                    .unwrap_or_default();
                format!(
                    "unknown target `{n}`{near} (`rokc --print target-list` lists the built-in \
                     ones; a custom target is a `<name>.rokon` file)"
                )
            })?,
        };
        if !t.deployment_target.is_empty() {
            if let Some(v) = std::env::var("MACOSX_DEPLOYMENT_TARGET")
                .ok()
                .filter(|v| !v.is_empty())
            {
                t.deployment_target = v;
            }
        }
        Ok(t)
    }

    /// The machine rokc runs on ([`resolve`](Self::resolve) of `None`).
    #[must_use]
    pub fn host() -> &'static Self {
        static HOST_SPEC: OnceLock<TargetSpec> = OnceLock::new();
        HOST_SPEC.get_or_init(|| Self::resolve(None).unwrap_or_else(|e| panic!("{e}")))
    }

    /// The host's platform whatever CPU it targets: its runtime, C
    /// toolchain and cached std builds serve it.
    #[must_use]
    pub fn is_host(&self) -> bool {
        let h = Self::host();
        *h == Self {
            cpu: h.cpu.clone(),
            features: h.features.clone(),
            ..self.clone()
        }
    }

    /// `-C target-cpu` (`native`: the build machine's, with features, on
    /// its platform; `host` gives them) and `-C target-feature` (`+f` /
    /// `-f` over the baseline).
    pub fn set_cpu(
        &mut self,
        cpu: Option<&str>,
        features: &[String],
        host: impl FnOnce() -> Result<(String, String), String>,
    ) -> Result<(), String> {
        match cpu {
            Some("native") => {
                if self.arch != Self::host().arch || self.os != Self::host().os {
                    return Err(format!(
                        "-C target-cpu=native: target {} is not the build machine's platform",
                        self.name
                    ));
                }
                (self.cpu, self.features) = host()?;
            }
            Some(c) => c.clone_into(&mut self.cpu),
            None => {}
        }
        for f in features {
            if !(f.starts_with('+') || f.starts_with('-')) || f.len() < 2 {
                return Err(format!(
                    "-C target-feature: `{f}` is not `+feature` or `-feature`"
                ));
            }
            if !self.features.is_empty() {
                self.features.push(',');
            }
            self.features.push_str(f);
        }
        Ok(())
    }

    /// A custom target file; its name is the file's stem.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read target {}: {e}", path.display()))?;
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        Self::from_rokon(&text, &path.display().to_string(), name)
    }

    /// The target `name` of a `TargetSpec { .. }` file's text.
    pub fn from_rokon(text: &str, file: &str, name: &str) -> Result<Self, String> {
        let doc = rokc_rokon::Doc::parse(text, file)?;
        let root = doc.root();
        let r = root.record("TargetSpec")?;
        let t = Self::decode(name, &r)?;
        let bad = |f: &str, msg: &str| Err(r.field_error(f, msg));
        if !matches!(t.endian.as_str(), "little" | "big") {
            return bad("endian", "`endian` must be \"little\" or \"big\"");
        }
        if !matches!(t.pointer_width, 16 | 32 | 64) {
            return bad("pointer_width", "`pointer_width` must be 16, 32 or 64");
        }
        if !matches!(t.wide_align, 4 | 8) {
            return bad("wide_align", "`wide_align` must be 4 or 8");
        }
        if !matches!(t.family.as_str(), "unix" | "windows" | "wasm" | "") {
            return bad(
                "family",
                "`family` must be \"unix\", \"windows\", \"wasm\" or \"\"",
            );
        }
        if t.object_format == ObjectFormat::Elf && t.dynamic_linking && t.dynamic_loader.is_empty()
        {
            return bad(
                "dynamic_loader",
                "a dynamically linked ELF target needs a `dynamic_loader`",
            );
        }
        Ok(t)
    }

    /// The `TargetSpec { .. }` text of this target (without its name).
    #[must_use]
    pub fn to_rokon(&self) -> String {
        rokc_rokon::to_string(&Value::record(Some("TargetSpec"), self.fields()))
    }

    /// A stable hash of every field and the name: a cache key part for
    /// whatever depends on the target.
    #[must_use]
    pub fn hash(&self) -> u64 {
        let h = rokc_core::hash::fnv_add(rokc_core::hash::FNV_OFFSET, self.name.as_bytes());
        rokc_core::hash::fnv_add(h, self.to_rokon().as_bytes())
    }

    /// LLVM's triple: `llvm_target` with the deployment target.
    #[must_use]
    pub fn llvm_triple(&self) -> String {
        format!("{}{}", self.llvm_target, self.deployment_target)
    }

    /// A browser module: a wasm reactor (`_initialize`, then exports),
    /// not a command.
    #[must_use]
    pub fn reactor(&self) -> bool {
        self.object_format == ObjectFormat::Wasm && self.os == "unknown"
    }

    /// The file name of static library `name`.
    #[must_use]
    pub fn staticlib(&self, name: &str) -> String {
        format!("{}{name}{}", self.staticlib_prefix, self.staticlib_suffix)
    }

    /// The file name of shared library `name`.
    #[must_use]
    pub fn dylib(&self, name: &str) -> String {
        format!("{}{name}{}", self.dylib_prefix, self.dylib_suffix)
    }

    /// The runtime library's file name.
    #[must_use]
    pub fn runtime_lib(&self) -> String {
        self.staticlib("rokr")
    }

    /// COFF: the machine type of an object or import library.
    #[must_use]
    pub fn coff_machine(&self) -> Option<u16> {
        match (self.object_format, self.arch.as_str()) {
            (ObjectFormat::Coff, "x86_64") => Some(0x8664),
            (ObjectFormat::Coff, "aarch64") => Some(0xaa64),
            (ObjectFormat::Coff, "x86") => Some(0x14c),
            _ => None,
        }
    }
}
