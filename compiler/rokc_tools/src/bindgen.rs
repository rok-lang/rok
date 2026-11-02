//! `rokc bindgen`: rok declarations from a C header via
//! libclang loaded at run time.
//!
//! Binds what the header and its non-system includes declare (a system
//! header: with its own tree), plus the types they use, in clang's target
//! layout: typedefs as `type`, structs / unions `#[repr(C)]` (bitfields as
//! bytes; inexpressible layouts as opaque blobs), enums as an integer alias
//! with `const`s, functions and globals `extern "C"` (`"system"` for
//! stdcall), object pointers `CPtr<T>`, function pointers
//! `unsafe extern "C" fn(..) | none`, and number / string macros as
//! `const`s. Anything else is skipped with a comment, including by-value
//! passing of blob records (C classifies them by their real fields).

mod clang;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_int;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clang::{Clang, Cursor, Type, Unit};

pub use clang::LIBRARY;

/// What to bind and for which target.
pub struct Options<'a> {
    pub header: &'a Path,
    /// The target's name, for the header comment.
    pub target: &'a str,
    /// clang's `-target`.
    pub triple: &'a str,
    /// `extern "system"` is stdcall (32-bit x86 Windows).
    pub stdcall: bool,
    /// Apple targets: the SDK (`-isysroot`).
    pub sdk: Option<PathBuf>,
    /// More compiler arguments (`-I`, `-D`, ...).
    pub clang_args: &'a [String],
    /// The machine's own system include directories describe the
    /// target's C library; else clang gets none (`-nostdlibinc`).
    pub system_headers: bool,
}

/// libclang and clang's resource directory (`lib/clang/<major>`,
/// its built-in headers) beside it.
pub struct Libclang {
    pub lib: PathBuf,
    pub resources: PathBuf,
}

/// Load libclang and bind `opts.header`.
pub fn generate(libclang: &Libclang, opts: &Options<'_>) -> Result<String, String> {
    let clang = Clang::load(&libclang.lib)?;
    let header =
        rokc_core::real_path(opts.header).map_err(|e| format!("{}: {e}", opts.header.display()))?;
    let header = header.to_string_lossy().into_owned();
    // libclang does not find its resource directory from where it was
    // loaded, so it is given.
    let mut args = vec![
        "-target".to_owned(),
        opts.triple.to_owned(),
        "-resource-dir".to_owned(),
        libclang.resources.to_string_lossy().into_owned(),
    ];
    if let Some(sdk) = &opts.sdk {
        args.extend(["-isysroot".to_owned(), sdk.to_string_lossy().into_owned()]);
    }
    if !opts.system_headers {
        args.push("-nostdlibinc".to_owned());
    }
    args.extend(opts.clang_args.iter().cloned());
    let unit = clang.parse(&header, &args, &[], true).map_err(|e| {
        if opts.system_headers || !e.contains("file not found") {
            return e;
        }
        format!(
            "{e}\nnote: {} is not this machine's platform, so its system headers are not used; \
             give the target's after `--` (`-isystem <dir>` or `--sysroot <dir>`)",
            opts.target
        )
    })?;
    let decls = top_level(&clang, unit.cursor());
    let roots = Roots {
        c: &clang,
        dir: Path::new(&header)
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf(),
        files: RefCell::new(HashMap::new()),
    };
    let macros = eval_macros(&roots, &unit, &header, &args, &decls);
    let mut g = Gen {
        c: &clang,
        roots: &roots,
        ptr_bytes: i64::from(unit.pointer_width() / 8),
        stdcall: opts.stdcall,
        underscore: opts.triple.contains("-apple-") || opts.stdcall,
        items: String::new(),
        blocks: [String::new(), String::new()],
        types: HashSet::new(),
        values: HashSet::new(),
        typedefs: HashMap::new(),
        anon: HashMap::new(),
        layouts: HashMap::new(),
        queue: VecDeque::new(),
        cptr: false,
    };
    for &d in &decls {
        if !roots.has(d) {
            continue;
        }
        match d.kind {
            clang::MACRO_DEFINITION => {
                let name = clang.spelling(d);
                if let Some((ty, value)) = macros.get(&name) {
                    let id = ident(&name);
                    if g.values.insert(id.clone()) {
                        let _ = writeln!(g.items, "pub const {id}: {ty} = {value};");
                    }
                }
            }
            clang::STRUCT_DECL | clang::UNION_DECL | clang::ENUM_DECL => g.tag_decl(d),
            clang::TYPEDEF_DECL => {
                let name = ident(&clang.spelling(d));
                if g.types.insert(name.clone()) {
                    g.typedef(d, &name);
                }
            }
            clang::FUNCTION_DECL => g.function(d),
            clang::VAR_DECL => g.var(d),
            clang::UNEXPOSED_DECL
            | clang::MACRO_EXPANSION
            | clang::INCLUSION_DIRECTIVE
            | clang::STATIC_ASSERT => {}
            k => {
                let name = clang.spelling(d);
                g.skip("declaration", &name, &format!("cursor kind {k} is not C"));
            }
        }
        g.drain();
    }
    Ok(g.finish(&header, opts))
}

/// The unit's declarations in order, `extern "C++"`-style linkage
/// blocks flattened.
fn top_level(clang: &Clang, c: Cursor) -> Vec<Cursor> {
    let mut out = Vec::new();
    for d in clang.children(c) {
        if d.kind == clang::LINKAGE_SPEC {
            out.extend(top_level(clang, d));
        } else {
            out.push(d);
        }
    }
    out
}

/// What gets bound: non-system headers' declarations, and those of the
/// header and headers beside or below it.
struct Roots<'a> {
    c: &'a Clang,
    dir: PathBuf,
    /// By file name.
    files: RefCell<HashMap<String, bool>>,
}

impl Roots<'_> {
    fn has(&self, d: Cursor) -> bool {
        let api = self.c.api();
        // SAFETY: a cursor of a live unit.
        if unsafe { (api.clang_Location_isInSystemHeader)((api.clang_getCursorLocation)(d)) } == 0 {
            return true;
        }
        let (file, _) = self.c.file_line(d);
        let mut files = self.files.borrow_mut();
        *files
            .entry(file)
            .or_insert_with_key(|f| rokc_core::real_path(f).is_ok_and(|p| p.starts_with(&self.dir)))
    }
}

/// Object-like macros of `roots` clang evaluates to a number or string,
/// `name -> (rok type, value)`, via a second unit that includes the header.
fn eval_macros(
    roots: &Roots<'_>,
    unit: &Unit<'_>,
    header: &str,
    args: &[String],
    decls: &[Cursor],
) -> HashMap<String, (String, String)> {
    let clang = roots.c;
    let api = clang.api();
    let mut names = Vec::new();
    for &d in decls {
        // SAFETY: cursors of a live unit.
        let plain = unsafe {
            d.kind == clang::MACRO_DEFINITION
                && (api.clang_Cursor_isMacroFunctionLike)(d) == 0
                && (api.clang_Cursor_isMacroBuiltin)(d) == 0
        } && roots.has(d);
        // The tokens after the macro's name.
        if plain && unit.tokens(d).get(1..).is_some_and(expression_like) {
            names.push(clang.spelling(d));
        }
    }
    let mut out = HashMap::new();
    if names.is_empty() {
        return out;
    }
    let mut src = format!("#include \"{}\"\n", header.replace('\\', "/"));
    for (i, n) in names.iter().enumerate() {
        let _ = writeln!(
            src,
            "static const __typeof__(({n})) __rok_bindgen_{i} = ({n});"
        );
    }
    let file = Path::new(header)
        .with_file_name("__rok_bindgen_macros.c")
        .to_string_lossy()
        .into_owned();
    let Ok(probe) = clang.parse(&file, args, &[(file.clone(), src)], false) else {
        return out;
    };
    for v in clang.children(probe.cursor()) {
        let Some(i) = clang
            .spelling(v)
            .strip_prefix("__rok_bindgen_")
            .and_then(|i| i.parse::<usize>().ok())
        else {
            continue;
        };
        if v.kind != clang::VAR_DECL || i >= names.len() {
            continue;
        }
        if let Some(c) = macro_value(clang, v) {
            out.insert(names[i].clone(), c);
        }
    }
    out
}

/// Could a macro body be an expression: balanced parentheses, no braces,
/// `;` or `#`, no keyword but `sizeof`, `true` / `false` and type names?
fn expression_like(tokens: &[(c_int, String)]) -> bool {
    const KEYWORDS: &[&str] = &[
        "sizeof", "true", "false", "char", "short", "int", "long", "signed", "unsigned", "float",
        "double", "_Bool", "bool", "const",
    ];
    let mut depth = 0i32;
    for (kind, s) in tokens {
        match (*kind, s.as_str()) {
            (clang::TOKEN_PUNCTUATION, "(") => depth += 1,
            (clang::TOKEN_PUNCTUATION, ")") => depth -= 1,
            (clang::TOKEN_PUNCTUATION, "{" | "}" | ";" | "#" | "##") => return false,
            (clang::TOKEN_KEYWORD, k) if !KEYWORDS.contains(&k) => return false,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    !tokens.is_empty() && depth == 0
}

/// The rok type and literal of an evaluated macro constant.
fn macro_value(clang: &Clang, var: Cursor) -> Option<(String, String)> {
    let api = clang.api();
    // SAFETY: a cursor of a live unit; the result is disposed of once.
    unsafe {
        if (api.clang_isInvalidDeclaration)(var) != 0 {
            return None;
        }
        let ty = (api.clang_getCanonicalType)((api.clang_getCursorType)(var));
        let ty = if ty.kind == clang::T_ENUM {
            (api.clang_getEnumDeclIntegerType)((api.clang_getTypeDeclaration)(ty))
        } else {
            ty
        };
        let rok = prim(ty.kind, (api.clang_Type_getSizeOf)(ty)).unwrap_or_default();
        let res = (api.clang_Cursor_Evaluate)(var);
        if res.is_null() {
            return None;
        }
        let value = match ((api.clang_EvalResult_getKind)(res), rok) {
            (clang::EVAL_INT, "bool") => {
                Some(((api.clang_EvalResult_getAsLongLong)(res) != 0).to_string())
            }
            (clang::EVAL_INT, "c_char") => Some(format!(
                "c'\\x{:02x}'",
                (api.clang_EvalResult_getAsLongLong)(res) as u8
            )),
            (clang::EVAL_INT, _) if rok.starts_with('u') => {
                Some((api.clang_EvalResult_getAsUnsigned)(res).to_string())
            }
            (clang::EVAL_INT, _) if rok.starts_with('i') => {
                Some(int_literal((api.clang_EvalResult_getAsLongLong)(res)))
            }
            (clang::EVAL_FLOAT, "f32" | "f64") => {
                let x = (api.clang_EvalResult_getAsDouble)(res);
                x.is_finite()
                    .then(|| float_literal(if rok == "f32" { f64::from(x as f32) } else { x }))
            }
            (clang::EVAL_STR, _) => {
                let p = (api.clang_EvalResult_getAsStr)(res);
                (!p.is_null())
                    .then(|| std::str::from_utf8(std::ffi::CStr::from_ptr(p).to_bytes()).ok())
                    .flatten()
                    .map(str_literal)
            }
            _ => None,
        };
        (api.clang_EvalResult_dispose)(res);
        let rok = if value.as_deref().is_some_and(|v| v.starts_with('"')) {
            "str"
        } else {
            rok
        };
        value.map(|v| (rok.to_owned(), v))
    }
}

fn int_literal(v: i64) -> String {
    if v == i64::MIN {
        // The positive literal would not fit.
        "-9223372036854775807 - 1".to_owned()
    } else {
        v.to_string()
    }
}

fn float_literal(x: f64) -> String {
    let s = format!("{x:?}");
    if s.contains(['.', 'e']) {
        s
    } else {
        format!("{s}.0")
    }
}

fn str_literal(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The rok primitive of a builtin C type of `size` bytes.
fn prim(kind: c_int, size: i64) -> Option<&'static str> {
    let int = |signed: bool| {
        Some(match (signed, size) {
            (true, 1) => "i8",
            (true, 2) => "i16",
            (true, 4) => "i32",
            (true, 8) => "i64",
            (false, 1) => "u8",
            (false, 2) => "u16",
            (false, 4) => "u32",
            (false, 8) => "u64",
            _ => return None,
        })
    };
    match kind {
        clang::T_BOOL => Some("bool"),
        clang::T_CHAR_S | clang::T_CHAR_U => Some("c_char"),
        clang::T_SCHAR | clang::T_SHORT | clang::T_INT | clang::T_LONG | clang::T_LONGLONG => {
            int(true)
        }
        clang::T_UCHAR
        | clang::T_USHORT
        | clang::T_UINT
        | clang::T_ULONG
        | clang::T_ULONGLONG
        | clang::T_CHAR16
        | clang::T_CHAR32 => int(false),
        // Windows' is unsigned 16-bit, the others' 32-bit (either
        // signedness crosses the ABI the same way).
        clang::T_WCHAR => int(size == 4),
        clang::T_FLOAT => Some("f32"),
        clang::T_DOUBLE => Some("f64"),
        clang::T_LONGDOUBLE if size == 8 => Some("f64"),
        _ => None,
    }
}

/// A C name as a rok identifier: keywords and rok's primitive type names
/// get a trailing `_`.
fn ident(name: &str) -> String {
    const TAKEN: &[&str] = &[
        "type", "any", "in", "where", "throws", "rethrows", "catch", "finally", "const", "static",
        "extern", "mut", "union", "pkg", "sub", "sup", "rok", "i8", "i16", "i32", "i64", "i128",
        "u8", "u16", "u32", "u64", "u128", "isize", "usize", "f32", "f64", "bool", "char", "str",
        "c_char",
    ];
    if rokc_lexer::token::RESERVED.contains(&name) || TAKEN.contains(&name) {
        format!("{name}_")
    } else {
        name.to_owned()
    }
}

const fn round_up(x: i64, a: i64) -> i64 {
    (x + a - 1) / a * a
}

/// One field as rok lays it out.
#[derive(Clone)]
struct Field {
    name: String,
    ty: String,
    /// Offset, size and alignment in bytes (clang's).
    off: i64,
    size: i64,
    align: i64,
    /// Comment lines above it.
    note: Option<String>,
}

enum Dep {
    Tag(Cursor, String),
    Typedef(Cursor, String),
}

struct Gen<'a> {
    c: &'a Clang,
    roots: &'a Roots<'a>,
    ptr_bytes: i64,
    stdcall: bool,
    /// Symbols take a leading `_` (Apple, 32-bit x86 Windows).
    underscore: bool,
    items: String,
    /// The `extern "C"` and `extern "system"` blocks' bodies.
    blocks: [String; 2],
    /// Type names written or queued.
    types: HashSet<String>,
    /// Value names written (functions, statics, constants).
    values: HashSet<String>,
    /// Typedefs used, by C name: their rok name, or why they are not
    /// usable.
    typedefs: HashMap<String, Result<String, Skip>>,
    /// Names given to anonymous structs, unions and enums, by USR.
    anon: HashMap<String, String>,
    /// Records' `repr` and fields, or why they are written as bytes, by
    /// USR.
    layouts: HashMap<String, Result<(String, Vec<Field>), Skip>>,
    /// Types used but not yet written.
    queue: VecDeque<Dep>,
    cptr: bool,
}

/// Why a declaration or type is skipped.
type Skip = String;

impl Gen<'_> {
    fn skip(&mut self, what: &str, name: &str, why: &str) {
        let _ = writeln!(self.items, "// skipped {what} `{name}`: {why}");
    }

    fn drain(&mut self) {
        while let Some(d) = self.queue.pop_front() {
            match d {
                Dep::Tag(c, name) => self.tag(c, &name),
                Dep::Typedef(c, name) => self.typedef(c, &name),
            }
        }
    }

    /// A struct, union or enum declaration seen at the top level.
    fn tag_decl(&mut self, d: Cursor) {
        // Anonymous ones are named by the typedef or field that uses
        // them; an enum used by none is its constants.
        let Some(name) = self.tag_name(d) else {
            // SAFETY: a cursor of a live unit.
            if d.kind == clang::ENUM_DECL
                && unsafe { (self.c.api().clang_Cursor_isAnonymous)(d) } != 0
            {
                self.enumeration(d, None);
            }
            return;
        };
        if self.types.insert(name.clone()) {
            self.tag(d, &name);
        }
    }

    /// A tag's rok name: its own, or the one given to it when anonymous.
    fn tag_name(&self, d: Cursor) -> Option<String> {
        // SAFETY: a cursor of a live unit.
        let anonymous = unsafe { (self.c.api().clang_Cursor_isAnonymous)(d) } != 0;
        let s = self.c.spelling(d);
        if anonymous || s.is_empty() || s.contains(['(', ' ']) {
            self.anon.get(&self.c.usr(d)).cloned()
        } else {
            Some(ident(&s))
        }
    }

    fn tag(&mut self, d: Cursor, name: &str) {
        if d.kind == clang::ENUM_DECL {
            self.enumeration(d, Some(name));
        } else {
            self.record(d, name);
        }
    }

    /// Strip elaboration, attributes and unexposed sugar (not typedefs).
    fn peel(&self, mut t: Type) -> Type {
        let api = self.c.api();
        // SAFETY: types of a live unit.
        unsafe {
            loop {
                t = match t.kind {
                    clang::T_ELABORATED => (api.clang_Type_getNamedType)(t),
                    clang::T_ATTRIBUTED | clang::T_BTF_TAG_ATTRIBUTED => {
                        (api.clang_Type_getModifiedType)(t)
                    }
                    clang::T_UNEXPOSED => {
                        let c = (api.clang_getCanonicalType)(t);
                        if c.kind == clang::T_UNEXPOSED {
                            return t;
                        }
                        c
                    }
                    _ => return t,
                };
            }
        }
    }

    /// The function type `t` names, through typedefs.
    fn function_type(&self, t: Type) -> Option<Type> {
        let api = self.c.api();
        let t = self.peel(t);
        match t.kind {
            clang::T_FUNCTION_PROTO | clang::T_FUNCTION_NO_PROTO => Some(t),
            clang::T_TYPEDEF => {
                // SAFETY: a typedef type of a live unit.
                let under = unsafe {
                    (api.clang_getTypedefDeclUnderlyingType)((api.clang_getTypeDeclaration)(t))
                };
                self.function_type(under)
            }
            _ => None,
        }
    }

    fn size_align(&self, t: Type) -> (i64, i64) {
        let api = self.c.api();
        // SAFETY: a type of a live unit.
        unsafe {
            (
                (api.clang_Type_getSizeOf)(t),
                (api.clang_Type_getAlignOf)(t),
            )
        }
    }

    /// The rok spelling of a C type, queueing the declarations it names.
    fn ty(&mut self, t: Type) -> Result<String, Skip> {
        let api = self.c.api();
        let t = self.peel(t);
        let (size, _) = self.size_align(t);
        if let Some(p) = prim(t.kind, size) {
            return Ok(p.to_owned());
        }
        // SAFETY: types and cursors of a live unit.
        unsafe {
            match t.kind {
                clang::T_POINTER => self.pointer((api.clang_getPointeeType)(t)),
                clang::T_RECORD | clang::T_ENUM => {
                    let d = (api.clang_getTypeDeclaration)(t);
                    match self.tag_name(d) {
                        Some(name) => {
                            if self.types.insert(name.clone()) {
                                self.queue.push_back(Dep::Tag(d, name.clone()));
                            }
                            Ok(name)
                        }
                        // An unnamed enum is its integer type.
                        None if t.kind == clang::T_ENUM => {
                            self.ty((api.clang_getEnumDeclIntegerType)(d))
                        }
                        None => Err("an unnamed struct or union".to_owned()),
                    }
                }
                clang::T_TYPEDEF => self.typedef_ref(t),
                clang::T_CONSTANT_ARRAY => {
                    let n = (api.clang_getArraySize)(t);
                    Ok(format!(
                        "[{}; {n}]",
                        self.ty((api.clang_getArrayElementType)(t))?
                    ))
                }
                clang::T_INCOMPLETE_ARRAY => Ok(format!(
                    "[{}; 0]",
                    self.ty((api.clang_getArrayElementType)(t))?
                )),
                _ => Err(format!(
                    "type `{}` has no rok form",
                    self.c.type_spelling(t)
                )),
            }
        }
    }

    /// A typedef use: builtin typedefs outside the roots resolve
    /// (`size_t` → `usize`), others keep their name.
    fn typedef_ref(&mut self, t: Type) -> Result<String, Skip> {
        let api = self.c.api();
        // SAFETY: a typedef type of a live unit.
        let (d, canon) = unsafe {
            (
                (api.clang_getTypeDeclaration)(t),
                (api.clang_getCanonicalType)(t),
            )
        };
        let c_name = self.c.spelling(d);
        let (size, _) = self.size_align(canon);
        if let Some(p) = self.pointer_sized(&c_name, size) {
            return Ok(p.to_owned());
        }
        if !self.roots.has(d) && prim(canon.kind, size).is_some() {
            return Ok(prim(canon.kind, size).unwrap_or_default().to_owned());
        }
        if self.function_type(t).is_some() {
            return Err(format!("function type `{c_name}` used by value"));
        }
        if let Some(r) = self.typedefs.get(&c_name) {
            return r.clone();
        }
        let name = ident(&c_name);
        // Usable only if what it names is (an entry first: a typedef may
        // be reached again through what it names).
        self.typedefs.insert(c_name.clone(), Ok(name.clone()));
        // SAFETY: a typedef of a live unit.
        let under = unsafe { (api.clang_getTypedefDeclUnderlyingType)(d) };
        let r = match self.ty(under) {
            Ok(_) => Ok(name.clone()),
            Err(why) => Err(format!("`{c_name}` is {why}")),
        };
        self.typedefs.insert(c_name, r.clone());
        if r.is_ok() && self.types.insert(name.clone()) {
            self.queue.push_back(Dep::Typedef(d, name));
        }
        r
    }

    /// `size_t` and its kin of pointer size: `usize` / `isize`.
    fn pointer_sized(&self, c_name: &str, size: i64) -> Option<&'static str> {
        if size != self.ptr_bytes {
            return None;
        }
        match c_name {
            "size_t" | "uintptr_t" => Some("usize"),
            "ssize_t" | "intptr_t" | "ptrdiff_t" => Some("isize"),
            _ => None,
        }
    }

    /// `T*` with `pointee` as `T`.
    fn pointer(&mut self, pointee: Type) -> Result<String, Skip> {
        if let Some(f) = self.function_type(pointee) {
            return Ok(format!("{} | none", self.fn_type(f)?));
        }
        // SAFETY: a type of a live unit.
        let canon = unsafe { (self.c.api().clang_getCanonicalType)(pointee) };
        let inner = if canon.kind == clang::T_VOID {
            "u8".to_owned()
        } else {
            self.ty(pointee)?
        };
        self.cptr = true;
        Ok(format!("CPtr<{inner}>"))
    }

    /// The ABI of a function type: 0 `"C"`, 1 `"system"`.
    fn abi(&self, f: Type) -> Result<usize, Skip> {
        // SAFETY: a function type of a live unit.
        match unsafe { (self.c.api().clang_getFunctionTypeCallingConv)(f) } {
            clang::CC_C | clang::CC_DEFAULT => Ok(0),
            clang::CC_X86_STDCALL if self.stdcall => Ok(1),
            cc => Err(format!("calling convention {cc} has no rok ABI")),
        }
    }

    /// A function pointer type (without `| none`).
    fn fn_type(&mut self, f: Type) -> Result<String, Skip> {
        let api = self.c.api();
        let abi = ["C", "system"][self.abi(f)?];
        // SAFETY: a function type of a live unit.
        unsafe {
            if (api.clang_isFunctionTypeVariadic)(f) != 0 {
                return Err("a variadic function pointer".to_owned());
            }
            let n = (api.clang_getNumArgTypes)(f).max(0) as u32;
            let mut params = Vec::new();
            for i in 0..n {
                let p = (api.clang_getArgType)(f, i);
                params.push(self.ty(p)?);
                self.by_value(p)?;
            }
            let ret = self.ret((api.clang_getResultType)(f))?;
            self.by_value((api.clang_getResultType)(f))?;
            if ret.contains("| none") {
                return Err("a function pointer returning a function pointer".to_owned());
            }
            Ok(format!(
                "unsafe extern \"{abi}\" fn({}){ret}",
                params.join(", ")
            ))
        }
    }

    /// ` -> T`, or nothing for `void`.
    fn ret(&mut self, t: Type) -> Result<String, Skip> {
        // SAFETY: a type of a live unit.
        if unsafe { (self.c.api().clang_getCanonicalType)(t) }.kind == clang::T_VOID {
            Ok(String::new())
        } else {
            Ok(format!(" -> {}", self.ty(t)?))
        }
    }

    fn typedef(&mut self, d: Cursor, name: &str) {
        let api = self.c.api();
        // SAFETY: a typedef of a live unit.
        let under = unsafe { (api.clang_getTypedefDeclUnderlyingType)(d) };
        let peeled = self.peel(under);
        if matches!(peeled.kind, clang::T_RECORD | clang::T_ENUM) {
            // SAFETY: a tag type of a live unit.
            let tag = unsafe { (api.clang_getTypeDeclaration)(peeled) };
            match self.tag_name(tag) {
                // `typedef struct foo foo;`
                Some(n) if n == name => return self.tag(tag, name),
                Some(_) => {}
                // `typedef struct { .. } foo;`
                None => {
                    self.anon.insert(self.c.usr(tag), name.to_owned());
                    self.tag(tag, name);
                    return;
                }
            }
        }
        if matches!(
            peeled.kind,
            clang::T_FUNCTION_PROTO | clang::T_FUNCTION_NO_PROTO
        ) {
            let _ = writeln!(
                self.items,
                "// `{name}` is a function type: pointers to it are written as `extern \"C\" fn` types"
            );
            return;
        }
        let size = self.size_align(under).0;
        let ty = match self.pointer_sized(name, size) {
            Some(p) => Ok(p.to_owned()),
            None => self.ty(under),
        };
        match ty {
            Ok(t) => {
                let _ = writeln!(self.items, "pub type {name} = {t};");
            }
            Err(why) => self.skip("typedef", name, &why),
        }
    }

    /// An enum as its integer type `name` and one constant per value;
    /// without a name, the constants alone.
    fn enumeration(&mut self, d: Cursor, name: Option<&str>) {
        let api = self.c.api();
        // SAFETY: an enum of a live unit.
        let (def, int) = unsafe {
            let def = (api.clang_getCursorDefinition)(d);
            let def = if (api.clang_Cursor_isNull)(def) != 0 {
                d
            } else {
                def
            };
            (def, (api.clang_getEnumDeclIntegerType)(def))
        };
        // SAFETY: a type of a live unit.
        let canon = unsafe { (api.clang_getCanonicalType)(int) };
        let unsigned =
            prim(canon.kind, self.size_align(canon).0).is_some_and(|p| p.starts_with('u'));
        let int = match self.ty(int) {
            Ok(t) => t,
            Err(why) => return self.skip("enum", name.unwrap_or("(unnamed)"), &why),
        };
        if let Some(name) = name {
            let _ = writeln!(self.items, "pub type {name} = {int};");
        }
        let ty = name.unwrap_or(&int);
        for v in self.c.children(def) {
            if v.kind != clang::ENUM_CONSTANT_DECL {
                continue;
            }
            let id = ident(&self.c.spelling(v));
            if !self.values.insert(id.clone()) {
                continue;
            }
            // SAFETY: an enumerator of a live unit.
            let value = unsafe {
                if unsigned {
                    (api.clang_getEnumConstantDeclUnsignedValue)(v).to_string()
                } else {
                    int_literal((api.clang_getEnumConstantDeclValue)(v))
                }
            };
            let _ = writeln!(self.items, "pub const {id}: {ty} = {value};");
        }
    }

    fn record(&mut self, d: Cursor, name: &str) {
        let api = self.c.api();
        // SAFETY: a record of a live unit.
        let def = unsafe { (api.clang_getCursorDefinition)(d) };
        if unsafe { (api.clang_Cursor_isNull)(def) } != 0 {
            let _ = writeln!(
                self.items,
                "/// Opaque: declared, not defined.\npub struct {name};"
            );
            return;
        }
        let (repr, fields) = match self.layout(def, name) {
            Ok(l) => l,
            Err(why) => {
                // SAFETY: a record of a live unit.
                let (size, align) = self.size_align(unsafe { (api.clang_getCursorType)(def) });
                return self.blob(name, size, align, &why);
            }
        };
        let kw = if def.kind == clang::UNION_DECL {
            "union"
        } else {
            "struct"
        };
        let _ = writeln!(self.items, "#[repr({repr})]\npub {kw} {name} {{");
        for f in fields {
            if let Some(n) = f.note {
                let _ = writeln!(self.items, "    // {n}");
            }
            let _ = writeln!(self.items, "    pub {}: {};", f.name, f.ty);
        }
        self.items.push_str("}\n");
    }

    /// The `repr` and fields of record definition `def` named `name`, or
    /// why it has no rok form (it is then written as bytes).
    fn layout(&mut self, def: Cursor, name: &str) -> Result<(String, Vec<Field>), Skip> {
        let usr = self.c.usr(def);
        if let Some(l) = self.layouts.get(&usr) {
            return l.clone();
        }
        let l = self.compute_layout(def, name);
        self.layouts.insert(usr, l.clone());
        l
    }

    fn compute_layout(&mut self, def: Cursor, name: &str) -> Result<(String, Vec<Field>), Skip> {
        let api = self.c.api();
        let union = def.kind == clang::UNION_DECL;
        // SAFETY: a record of a live unit.
        let (size, align) = self.size_align(unsafe { (api.clang_getCursorType)(def) });
        let fields = self.fields(def, name, union)?;
        // rok's C layout of these fields, against clang's.
        let lay = |packed: bool| {
            let (mut end, mut al) = (0i64, 1);
            for f in &fields {
                let a = if packed { 1 } else { f.align };
                let off = if union { 0 } else { round_up(end, a) };
                if off != f.off {
                    return None;
                }
                end = if union { end.max(f.size) } else { off + f.size };
                al = al.max(a);
            }
            Some((end, al))
        };
        let no_form = || "clang's layout has no rok form".to_owned();
        let (repr, end, al) = match (lay(false), lay(true)) {
            (Some((end, al)), _) if align >= al => {
                let repr = if align > al {
                    format!("C, align({align})")
                } else {
                    "C".to_owned()
                };
                (repr, end, align)
            }
            (None, Some((end, _))) if align == 1 && !union => ("packed".to_owned(), end, 1),
            _ => return Err(no_form()),
        };
        if round_up(end, al) != size {
            return Err(no_form());
        }
        Ok((repr, fields))
    }

    /// `Err` when a `t` value wouldn't cross the C ABI as C passes it: a
    /// byte-blob record, alone or inside one, classifies unlike its fields.
    fn by_value(&mut self, t: Type) -> Result<(), Skip> {
        let api = self.c.api();
        // SAFETY: a type of a live unit.
        let t = unsafe { (api.clang_getCanonicalType)(t) };
        match t.kind {
            // SAFETY: an array type of a live unit.
            clang::T_CONSTANT_ARRAY => self.by_value(unsafe { (api.clang_getArrayElementType)(t) }),
            clang::T_RECORD => {
                // SAFETY: a record type of a live unit.
                let def =
                    unsafe { (api.clang_getCursorDefinition)((api.clang_getTypeDeclaration)(t)) };
                // Incomplete types aren't passed by value; unnamed ones have
                // no rok form anyway.
                // SAFETY: a cursor of a live unit.
                if unsafe { (api.clang_Cursor_isNull)(def) } != 0 {
                    return Ok(());
                }
                let Some(name) = self.tag_name(def) else {
                    return Ok(());
                };
                self.layout(def, &name).map_err(|why| {
                    format!("`{name}` is passed by value but written as bytes ({why})")
                })?;
                for f in self.c.children(def) {
                    // SAFETY: a member of a live record.
                    let member = f.kind == clang::FIELD_DECL
                        || matches!(f.kind, clang::STRUCT_DECL | clang::UNION_DECL)
                            && unsafe { (api.clang_Cursor_isAnonymousRecordDecl)(f) } != 0;
                    if member {
                        // SAFETY: a member of a live record.
                        self.by_value(unsafe { (api.clang_getCursorType)(f) })?;
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// A record's fields, bitfield runs as byte arrays. `Err` when one
    /// has no rok form.
    fn fields(&mut self, def: Cursor, name: &str, union: bool) -> Result<Vec<Field>, Skip> {
        let api = self.c.api();
        let mut out: Vec<Field> = Vec::new();
        // An open bitfield run: first and end bit, and its members.
        let mut run: Option<(i64, i64, Vec<String>)> = None;
        let mut runs = 0;
        let mut anon = 0;
        let mut close = |run: &mut Option<(i64, i64, Vec<String>)>,
                         end_byte: Option<i64>,
                         out: &mut Vec<Field>| {
            if let Some((start, end, members)) = run.take() {
                let off = start / 8;
                let size = end_byte.unwrap_or_else(|| (end + 7) / 8) - off;
                out.push(Field {
                    name: format!("_bitfield{runs}"),
                    ty: format!("[u8; {size}]"),
                    off,
                    size,
                    align: 1,
                    note: Some(format!(
                        "bitfields from bit {}: {}",
                        off * 8,
                        members.join(", ")
                    )),
                });
                runs += 1;
            }
        };
        // SAFETY: a record of a live unit.
        let def_ty = unsafe { (api.clang_getCursorType)(def) };
        for f in self.c.children(def) {
            // SAFETY: cursors and types of a live unit.
            let (fname, bits, ft) = unsafe {
                match f.kind {
                    clang::FIELD_DECL => (
                        self.c.spelling(f),
                        (api.clang_Cursor_getOffsetOfField)(f),
                        (api.clang_getCursorType)(f),
                    ),
                    // A C11 anonymous member: its implicit field is not
                    // visited, so its offset is that of its first field.
                    clang::STRUCT_DECL | clang::UNION_DECL
                        if (api.clang_Cursor_isAnonymousRecordDecl)(f) != 0 =>
                    {
                        (
                            String::new(),
                            self.anon_offset(def_ty, f),
                            (api.clang_getCursorType)(f),
                        )
                    }
                    _ => continue,
                }
            };
            if bits < 0 {
                return Err(format!("field `{fname}` has no offset"));
            }
            // SAFETY: a field of a live unit.
            if unsafe { (api.clang_Cursor_isBitField)(f) } != 0 {
                if union {
                    return Err("a bitfield in a union".to_owned());
                }
                let width = i64::from(unsafe { (api.clang_getFieldDeclBitWidth)(f) });
                let r = run.get_or_insert((bits, bits, Vec::new()));
                r.1 = r.1.max(bits + width);
                if !fname.is_empty() && width > 0 {
                    r.2.push(format!(
                        "{fname} ({}..{})",
                        bits - r.0 / 8 * 8,
                        bits + width - r.0 / 8 * 8
                    ));
                }
                continue;
            }
            close(&mut run, Some(bits / 8), &mut out);
            let field = if fname.is_empty() {
                anon += 1;
                format!("_anon{}", anon - 1)
            } else {
                ident(&fname)
            };
            // A field of an unnamed struct, union or enum type names it.
            let peeled = self.peel(ft);
            if matches!(peeled.kind, clang::T_RECORD | clang::T_ENUM) {
                // SAFETY: a tag type of a live unit.
                let tag = unsafe { (api.clang_getTypeDeclaration)(peeled) };
                if self.tag_name(tag).is_none() {
                    self.anon.insert(
                        self.c.usr(tag),
                        format!("{name}_{}", field.trim_start_matches('_')),
                    );
                }
            }
            let ty = self
                .ty(ft)
                .map_err(|why| format!("field `{field}`: {why}"))?;
            let (mut size, mut align) = self.size_align(ft);
            if peeled.kind == clang::T_INCOMPLETE_ARRAY {
                // SAFETY: an array type of a live unit.
                align = self
                    .size_align(unsafe { (api.clang_getArrayElementType)(peeled) })
                    .1;
                size = 0;
            }
            if size < 0 || align <= 0 {
                return Err(format!("field `{field}` has no size"));
            }
            out.push(Field {
                name: field,
                ty,
                off: bits / 8,
                size,
                align,
                note: None,
            });
        }
        close(&mut run, None, &mut out);
        Ok(out)
    }

    /// The bit offset in `outer` of its anonymous member `inner`: where
    /// a named field inside it is in `outer`, less where it is in `inner`.
    fn anon_offset(&self, outer: Type, inner: Cursor) -> i64 {
        let api = self.c.api();
        let Some(name) = self
            .first_field(inner)
            .and_then(|n| std::ffi::CString::new(n).ok())
        else {
            return -1;
        };
        // SAFETY: types of a live unit; field lookup sees through
        // anonymous members.
        unsafe {
            let total = (api.clang_Type_getOffsetOf)(outer, name.as_ptr());
            let within =
                (api.clang_Type_getOffsetOf)((api.clang_getCursorType)(inner), name.as_ptr());
            if total < 0 || within < 0 {
                -1
            } else {
                total - within
            }
        }
    }

    /// The first named field of a record, or of its anonymous members.
    fn first_field(&self, r: Cursor) -> Option<String> {
        self.c.children(r).into_iter().find_map(|f| match f.kind {
            clang::FIELD_DECL => Some(self.c.spelling(f)).filter(|n| !n.is_empty()),
            // SAFETY: a cursor of a live unit.
            clang::STRUCT_DECL | clang::UNION_DECL
                if unsafe { (self.c.api().clang_Cursor_isAnonymousRecordDecl)(f) } != 0 =>
            {
                self.first_field(f)
            }
            _ => None,
        })
    }

    /// A record rok cannot lay out field by field: its bytes.
    fn blob(&mut self, name: &str, size: i64, align: i64, why: &str) {
        if size < 0 || align <= 0 {
            return self.skip("struct", name, why);
        }
        let repr = if align > 1 {
            format!("C, align({align})")
        } else {
            "C".to_owned()
        };
        let _ = writeln!(
            self.items,
            "// `{name}` as bytes ({why})\n#[repr({repr})]\npub struct {name} {{\n    pub _opaque: [u8; {size}];\n}}"
        );
    }

    fn function(&mut self, d: Cursor) {
        let api = self.c.api();
        let c_name = self.c.spelling(d);
        let id = ident(&c_name);
        if self.values.contains(&id) {
            return;
        }
        // SAFETY: a function of a live unit.
        let (t, local) = unsafe {
            (
                (api.clang_getCursorType)(d),
                (api.clang_Cursor_getStorageClass)(d) == clang::SC_STATIC
                    || (api.clang_Cursor_isFunctionInlined)(d) != 0,
            )
        };
        if local {
            return self.skip("fn", &c_name, "static or inline, no symbol to link");
        }
        match self.signature(d, t) {
            Ok((abi, sig)) => {
                self.values.insert(id.clone());
                // A stdcall symbol's `@N` is rok's to add.
                let sym = if abi == 0 { self.symbol(d) } else { c_name };
                let block = &mut self.blocks[abi];
                if id != sym {
                    let _ = writeln!(block, "    #[link_name = \"{sym}\"]");
                }
                let _ = writeln!(block, "    pub fn {id}{sig};");
            }
            Err(why) => self.skip("fn", &c_name, &why),
        }
    }

    /// The symbol a declaration links to, without the target's `_`
    /// prefix: its name, or its asm label (`fopen$DARWIN_EXTSN`).
    fn symbol(&self, d: Cursor) -> String {
        // SAFETY: a cursor of a live unit.
        let m = self
            .c
            .string(unsafe { (self.c.api().clang_Cursor_getMangling)(d) });
        match m.strip_prefix('_') {
            Some(m) if self.underscore => m.to_owned(),
            _ if m.is_empty() => self.c.spelling(d),
            _ => m,
        }
    }

    /// `(abi, "(params) -> R")` of a function declaration.
    fn signature(&mut self, d: Cursor, t: Type) -> Result<(usize, String), Skip> {
        let api = self.c.api();
        let f = self.function_type(t).ok_or("not a function type")?;
        let abi = self.abi(f)?;
        let mut seen = HashSet::new();
        let mut params = Vec::new();
        // SAFETY: a function and its parameters, of a live unit.
        unsafe {
            for i in 0..(api.clang_Cursor_getNumArguments)(d).max(0) as u32 {
                let p = (api.clang_Cursor_getArgument)(d, i);
                let mut name = ident(&self.c.spelling(p));
                if name.is_empty() || !seen.insert(name.clone()) {
                    name = format!("arg{i}");
                }
                let pt = (api.clang_getCursorType)(p);
                params.push(format!("{name}: {}", self.ty(pt)?));
                self.by_value(pt)?;
            }
            if (api.clang_isFunctionTypeVariadic)(f) != 0 {
                params.push("...".to_owned());
            }
            let ret = self.ret((api.clang_getResultType)(f))?;
            self.by_value((api.clang_getResultType)(f))?;
            Ok((abi, format!("({}){ret}", params.join(", "))))
        }
    }

    fn var(&mut self, d: Cursor) {
        let api = self.c.api();
        let c_name = self.c.spelling(d);
        let id = ident(&c_name);
        // SAFETY: a variable of a live unit.
        let (t, local, tls, constant) = unsafe {
            let t = (api.clang_getCursorType)(d);
            // An array is as const as its elements.
            let mut e = t;
            while matches!(
                self.peel(e).kind,
                clang::T_CONSTANT_ARRAY | clang::T_INCOMPLETE_ARRAY
            ) {
                e = (api.clang_getArrayElementType)(self.peel(e));
            }
            (
                t,
                (api.clang_Cursor_getStorageClass)(d) == clang::SC_STATIC,
                (api.clang_getCursorTLSKind)(d) != 0,
                (api.clang_isConstQualifiedType)(e) != 0,
            )
        };
        if local || !self.values.insert(id.clone()) {
            return;
        }
        if tls {
            return self.skip("static", &c_name, "thread-local");
        }
        match self.ty(t) {
            Ok(ty) => {
                let sym = self.symbol(d);
                let block = &mut self.blocks[0];
                if id != sym {
                    let _ = writeln!(block, "    #[link_name = \"{sym}\"]");
                }
                let m = if constant { "" } else { "mut " };
                let _ = writeln!(block, "    pub static {m}{id}: {ty};");
            }
            Err(why) => self.skip("static", &c_name, &why),
        }
    }

    fn finish(self, header: &str, opts: &Options<'_>) -> String {
        let file = Path::new(header)
            .file_name()
            .map_or_else(|| header.to_owned(), |f| f.to_string_lossy().into_owned());
        let mut out = format!(
            "// Generated by `rokc bindgen` from {file} for {} ({}).\n#![allow(nonstandard_style)]\n\n",
            opts.target, opts.triple
        );
        if self.cptr {
            out.push_str("use rok::bare::ptr::CPtr;\n\n");
        }
        out.push_str(&self.items);
        for (abi, body) in ["C", "system"].iter().zip(&self.blocks) {
            if !body.is_empty() {
                let _ = write!(out, "\nextern \"{abi}\" {{\n{body}}}\n");
            }
        }
        out
    }
}

/// Where libclang is: the toolchain's `lib/`, else (a build in the
/// repository) `llvm/build-libclang.sh`'s prefix for LLVM `version`.
pub fn find_libclang(version: &str) -> Result<Libclang, String> {
    let exe = rokc_core::current_exe().unwrap_or_default();
    let root = exe
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let cache = env("ROK_LLVM_CACHE").or_else(|| {
        if cfg!(windows) {
            env("LOCALAPPDATA").map(|d| d.join("rok"))
        } else {
            env("XDG_CACHE_HOME")
                .or_else(|| env("HOME").map(|h| h.join(".cache")))
                .map(|d| d.join("rok"))
        }
    });
    let mut dirs = vec![root.join("lib")];
    dirs.extend(cache.map(|c| c.join(format!("libclang-{version}/lib"))));
    let major = version.split('.').next().unwrap_or(version);
    let found = dirs
        .iter()
        .find(|d| d.join(LIBRARY).is_file())
        .ok_or_else(|| {
            let list: Vec<String> = dirs
                .iter()
                .map(|d| d.join(LIBRARY).display().to_string())
                .collect();
            format!(
                "`rokc bindgen` needs libclang, which is missing: no {}. A toolchain ships it in \
             its lib/ (reinstall it); in the rok repository, build it with \
             llvm/build-libclang.sh (Windows: llvm\\build-libclang.ps1)",
                list.join(" nor ")
            )
        })?;
    let resources = found.join("clang").join(major);
    if !resources.join("include/stddef.h").is_file() {
        return Err(format!(
            "clang's built-in headers are missing: no {}",
            resources.join("include/stddef.h").display()
        ));
    }
    Ok(Libclang {
        lib: found.join(LIBRARY),
        resources,
    })
}
