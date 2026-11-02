//! Error codes (spec 9.9.4): one serial code per kind of error, in the
//! order they were added. A code is never reused: a kind that goes away
//! keeps its number in the table, marked retired, and new kinds take the
//! next free number. `rokc --explain` and the docs error pages read the
//! titles and explanations here.

use crate::ErrorKind;

/// An error code, `E0001` up.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct Code(u16);

/// One row of the table.
#[derive(Debug)]
pub struct Entry {
    pub code: Code,
    /// The constant's name, `MISMATCHED_TYPES`.
    pub name: &'static str,
    /// The area the code belongs to, the JSON `kind`.
    pub kind: ErrorKind,
    /// One line: what went wrong.
    pub title: &'static str,
    /// What it means and what to write instead.
    pub text: &'static str,
    /// No longer emitted; the number stays reserved.
    pub retired: bool,
}

macro_rules! codes {
    ($($n:literal $name:ident $kind:ident $(#[$retired:ident])? $title:literal $text:literal;)*) => {
        impl Code {
            $(pub const $name: Self = Self($n);)*
        }

        static TABLE: &[Entry] = &[$(Entry {
            code: Code($n),
            name: stringify!($name),
            kind: ErrorKind::$kind,
            title: $title,
            text: $text,
            retired: codes!(@retired $($retired)?),
        }),*];
    };
    (@retired) => { false };
    (@retired retired) => { true };
}

impl Code {
    /// Every code, retired ones included, in order.
    #[must_use]
    pub fn all() -> &'static [Entry] {
        TABLE
    }

    /// The code's row.
    #[must_use]
    pub fn entry(self) -> &'static Entry {
        &TABLE[usize::from(self.0) - 1]
    }

    #[must_use]
    pub fn kind(self) -> ErrorKind {
        self.entry().kind
    }

    #[must_use]
    pub fn title(self) -> &'static str {
        self.entry().title
    }

    /// The code `n`, if the table has it.
    #[must_use]
    pub fn from_number(n: u16) -> Option<Self> {
        (1..=TABLE.len())
            .contains(&usize::from(n))
            .then_some(Self(n))
    }

    /// `E0042`, `e0042` or `42`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let digits = s.strip_prefix(['E', 'e']).unwrap_or(s);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Self::from_number(digits.parse().ok()?)
    }

    /// The long explanation `rokc --explain` prints.
    #[must_use]
    pub fn explain(self) -> String {
        let e = self.entry();
        let mut out = format!("{self}: {}\n\n{}\n", e.title, e.text.trim());
        if e.retired {
            out.push_str("\nThis code is no longer emitted.\n");
        }
        out
    }
}

/// `rokc --explain <arg>`: the explanation, or what to write instead.
///
/// # Errors
/// `arg` names no code.
pub fn explain(arg: &str) -> Result<String, String> {
    let last = Code::all().last().map_or(1, |e| e.code.0);
    Code::parse(arg.trim()).map(Code::explain).ok_or_else(|| {
        format!(
            "`{arg}` is not an error code: codes run from E0001 to {}, as in \
             `--explain E0001`",
            Code(last)
        )
    })
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "E{:04}", self.0)
    }
}

impl rokc_core::codec::Codec for Code {
    fn enc(&self, e: &mut rokc_core::codec::Enc) {
        e.u(u64::from(self.0));
    }
    fn dec(d: &mut rokc_core::codec::Dec<'_>) -> Option<Self> {
        Self::from_number(u16::try_from(d.u()?).ok()?)
    }
}

codes! {
    1 UNKNOWN_CHARACTER Lexical "unexpected character in source"
    "The source has a character that starts no token, such as a stray `#` or `$`, or a \
     `#!` line after the first line. Remove it, or write it inside a string or comment.";

    2 BIDI_CHARACTER Lexical "bidirectional control character in source"
    "The source has a Unicode bidirectional control character (U+202A to U+202E, U+2066 to \
     U+2069). These reorder how text is shown, so code can read differently from what the \
     compiler sees. Write it as an escape inside a string, `\"\\u{202E}\"`, instead.";

    3 MALFORMED_NUMBER Lexical "malformed numeric literal"
    "A number literal is not well formed: a digit outside its base (`0b2`), `_` not between \
     two digits, an exponent without digits, a leading `0` on a decimal (`012`), a float \
     without a leading digit (`.5`), or a suffix of the wrong kind (`1.5i32`). Write the \
     number as the message suggests, for example `0.5`, `1_000` or `0o12`.";

    4 LITERAL_OUT_OF_RANGE Lexical "literal out of range for its type"
    "A literal does not fit its type: `300u8`, `-1` where an unsigned type is expected, or a \
     float too large for `f32`. Use a wider type (`300u16`), a value that fits, or an `int` \
     for a number of any size.";

    5 UNTERMINATED Lexical "unterminated literal or comment"
    "A string, character, byte string, f-string, raw string or block comment starts but never \
     ends. Add the closing quote or `*/`. A raw string closes with as many `#` as it opened \
     with.";

    6 INVALID_ESCAPE Lexical "invalid escape in a literal"
    "A `\\` escape in a string or character literal is not one Rok knows, or a byte string \
     holds a character it cannot. Known escapes are `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, \
     `\\'`, `\\\"`, `\\xHH` and `\\u{..}`; a byte string holds ASCII only, so write other bytes \
     as `\\xHH` (at most `\\xFF`).";

    7 CHAR_LITERAL_LENGTH Lexical "a character literal holds one character"
    "A `'..'` literal holds exactly one Unicode code point, and a `c'..'` literal exactly one \
     byte. For text of any length use a string, `\"ab\"`; for a non-ASCII byte write `\\xHH`.";

    8 FSTRING_SYNTAX Lexical "malformed f-string"
    "An f-string's `{..}` holes are not well formed: a `{` without its `}`, a lone `}`, an \
     empty `{}`, a format spec with its own spec, or a hole that spans lines outside \
     `f\"\"\"..\"\"\"`. Write a literal brace as `{{` or `}}`, and put a value in every hole: \
     `f\"{x}\"`.";

    9 RAW_IDENTIFIER Lexical "name cannot be a raw identifier"
    "`r#name` keeps a keyword usable as a name, but a few names (`self`, `Self`, `crate`, \
     `super`, `_`) cannot be written that way. Pick another name.";

    10 SYNTAX Parse "unexpected token"
    "The parser expected one thing and found another: a missing `;`, `,`, `)` or `}`, a \
     keyword where a name goes, or an expression where an item goes. The message says what \
     was expected; write that, or check the line before for something left open.";

    11 NESTING_LIMIT Parse "nesting too deep"
    "An expression nests deeper, or a method chain runs longer, than the compiler handles. \
     Split it: bind a part to a `let` and continue from there.";

    12 FOREIGN_SYNTAX Parse "syntax from another language"
    "The code uses a form Rok does not have: the `c ? a : b` operator, `++` / `--`, or \
     `let mut`. The message gives the Rok form: `if c { a } else { b }`, `x += 1`, or a \
     plain `let` (variables are mutable).";

    13 STRUCT_LITERAL_IN_HEAD Parse "struct literal in a block head"
    "In the head of `if`, `while`, `match` or `for`, a `{` opens the body, so a struct \
     literal there is read wrongly. Wrap the literal in parentheses: \
     `if p == (Point { x: 0, y: 0 }) { .. }`.";

    14 MODIFIER_ORDER Parse "modifiers out of order"
    "Function modifiers go in one order, `unsafe async yield fn`, each at most once, and \
     `throws` goes before the return type: `fn f() throws -> T`. Reorder them as the help \
     shows.";

    15 MISPLACED_DOC_COMMENT Parse "doc comment documents nothing"
    "A doc comment (`///` or `/** */`) goes right before the item it documents. Move it \
     there, or make it a plain comment (`//`).";

    16 MISPLACED_VISIBILITY Parse "visibility where it has no meaning"
    "`pub` was written where it changes nothing: on a function or item inside a block (it is \
     visible only in that block) or on an enum variant (variants take the enum's \
     visibility). Remove it.";

    17 MISPLACED_ITEM Parse "item in the wrong place"
    "An item or statement appears where it is not allowed: a `static` in an `impl`, a \
     `static mut` outside an `extern` block, a statement in a module body, or a macro import \
     inside a function. Move it where the message says, usually to module level.";

    18 TOP_LEVEL_RETURN Parse "`return` in top-level statements"
    "A program's top-level statements are not a function, so `return` has nothing to return \
     from. End the program with `process::exit(code)`, or write `fn main() -> i32` and \
     return from it.";

    19 NEEDS_PARENTHESES Parse "operators that need parentheses"
    "Some operators do not chain: `a == b == c`, `a..b..c`, `try? x ?? y`, and one `[..]` \
     that both indexes with `,` and slices with `:`. Add parentheses or separate brackets to \
     say which you mean.";

    20 TRAILING_CLOSURE_RETURN Parse "`return` in a trailing closure"
    "A bare `return` in a statement's trailing closure would leave only the closure, not the \
     function around it. Leave the closure with `return@name` (`name` is the function the \
     closure is passed to), and the function with code after the call.";

    21 RESERVED_NAME Parse "reserved word used as a name"
    "A keyword or a reserved name (a path root such as `rok`, or a keyword registered with \
     `#[keyword]`) is used as a name. Pick another name, or write `r#name` to keep a \
     keyword as a name.";

    22 LET_WITHOUT_VALUE Parse "`let` without a value"
    "Every `let` gives its variable a value. Write one, `let n: i32 = 0;`, or start it empty \
     with an optional type: `let n: i32 | none = none;`.";

    23 INVALID_SIGNATURE Parse "invalid function signature"
    "The function's parameters or return type break a rule: a variadic `...` that is not \
     last, a `ref` parameter on a coroutine or a `fn` value, a `rethrows` without a throwing \
     parameter, or a signature a test attribute does not allow. The message names the rule; \
     change the signature to fit it.";

    24 INVALID_GENERIC_PARAMS Parse "invalid generic parameters"
    "The generic parameter list breaks a rule: a parameter without a default after one with \
     a default, a const parameter of a type that cannot be a constant, or a `where` clause \
     that bounds something it cannot. Reorder or change the parameters as the message says.";

    25 INVALID_ABI Parse "unsupported ABI"
    "An `extern` names an ABI Rok does not support, or leaves it out. Write `extern \"C\"` \
     (or `\"system\"` for the platform's system calls).";

    26 DUPLICATE_DEFINITION Resolve "name defined more than once"
    "Two items in one scope have the same name. Rok has no overloading: rename one of them, \
     or, for functions that differ in how many arguments they take, keep one and give its \
     last parameters defaults (`b: i32 = 0`).";

    27 UNRESOLVED_NAME Resolve "cannot find value or function"
    "No value or function of this name is in scope. Check the spelling, bring it in with \
     `use` (the help lists the items of that name), or declare it with `let` first.";

    28 UNRESOLVED_TYPE Resolve "cannot find type or trait"
    "No type or trait of this name is in scope. Check the spelling, or bring it in with \
     `use`; the help lists the items of that name.";

    29 UNRESOLVED_VARIANT Resolve "cannot find enum variant"
    "The enum has no variant of this name. Check the spelling against the enum's \
     definition.";

    30 UNRESOLVED_IMPORT Module "unresolved import"
    "A `use` names something that does not exist. Check the path: it starts at the package \
     (`crate::`), a dependency, or `rok::`.";

    31 AMBIGUOUS_NAME Resolve "ambiguous name"
    "Several glob imports (`use a::*;`) or packages provide this name, so it is unclear \
     which one is meant. Import the one you want by name, or write its path.";

    32 IMPORT_CONFLICT Module "conflicting imports"
    "A name is imported twice with different targets, or is both imported and defined in \
     the module or block. Rename one with `use .. as other;`, or drop one of them.";

    33 INVALID_REEXPORT Module "invalid re-export"
    "A `pub use` breaks a rule: it is wider than the item it re-exports, it re-exports a \
     macro, or re-exports form a cycle. Make the item as visible as the re-export, or change \
     the re-export.";

    34 PRIVATE_ITEM Visibility "item is private"
    "The item, field or method is private to another module (or `pub(pkg)` in another \
     package), so it cannot be used here. Mark it `pub` where it is defined, or use a public \
     item instead.";

    35 MODULE_FILE Module "module file problem"
    "A `mod name;` cannot be loaded: its file `name.rok` is missing or unreadable, the module \
     is declared twice, or it is both inline and a file. A module's file is named after the \
     module (Rok has no `mod.rok`), beside the file that declares it.";

    36 ENTRY_POINT Module "invalid program entry point"
    "A program starts either at the top-level statements of `main.rok` or at `fn main`, not \
     both; other files hold definitions only. `fn main` takes no parameters, is not generic \
     or `unsafe`, and returns `()` or `i32`; it may be `async`, and a `throws` `main` returns \
     `()`.";

    37 DEPENDENCY Module "invalid dependency"
    "A dependency given to the compiler (`--extern`, or a standard library tier) does not fit \
     this build: it was built under another name or for another toolchain, or it may not be \
     used from here. Rebuild it, or fix the dependency list.";

    38 REDECLARED_LOCAL Resolve "local declared twice"
    "A `let` declares a name that is already declared in the same scope, or a variable and \
     a nested function share a name. Rok does not let a second `let` hide the first: assign \
     to it (`x = ..`) or pick a new name.";

    39 OUTER_LOCAL Resolve "nested item uses an outer local"
    "A nested `fn`, `struct` or `impl` cannot use the locals or generic parameters of the \
     function around it. Pass the value as a parameter, or use a closure, which can capture \
     it.";

    40 MISMATCHED_TYPES Type "mismatched types"
    "A value of one type is used where another type is expected: an argument, a `let` with \
     a type, a return value, or the value of a function whose body drops it with `;`. \
     Convert the value (`as i32`, `.to_str()`), change the expected type, or remove the `;`.";

    41 BRANCH_TYPES Type "branches of different types"
    "The branches of an `if`, `match` or the `break`s of a loop give values of different \
     types. Rok does not invent a sum type for them: annotate the slot with the sum, \
     `let v: i32 | str = if c { 1 } else { \"a\" };`, or make the branches agree.";

    42 TYPE_ANNOTATIONS_NEEDED Type "type annotations needed"
    "The compiler cannot work out a type from the code: an empty literal, a closure \
     parameter nothing constrains, or a generic call whose type arguments nothing fixes. \
     Write the type: `let v: List<i32> = [];`, `|x: i32| ..`, or `f::<i32>(..)`.";

    43 NO_FIELD Type "no such field"
    "The type has no field of this name (or a tuple has no element at this index). Check the \
     spelling against the type's definition.";

    44 MISSING_FIELD Type "missing field in a literal"
    "A struct or variant literal leaves out a field that has no default. Give every field a \
     value, or declare a default in the struct (`n: i32 = 0`).";

    45 DUPLICATE_FIELD Type "field given more than once"
    "A struct literal names the same field twice. Remove one.";

    46 NO_METHOD Type "no such method or associated item"
    "The type has no method, associated function or associated constant of this name, or \
     none that applies to these types. Check the spelling, the `use` of the trait that \
     provides it, and its bounds.";

    47 WRONG_ITEM_FORM Type "item used in the wrong form"
    "An item is used in a form it does not have: an associated function called as a method \
     (`v.new()` for `T::new()`), a method named without a call, a tuple struct built with \
     `{..}` or a named one with `(..)`, or a type used as a value. Write the form the message \
     gives.";

    48 WRONG_ARGUMENT_COUNT Type "wrong number of arguments"
    "A call passes more or fewer arguments than the function takes, or leaves out one \
     without a default. Pass every parameter once; parameters with defaults may be left \
     out.";

    49 NAMED_ARGUMENT Type "invalid named argument"
    "A named argument (`f(x: 1)`) names no parameter of the function, gives one parameter \
     twice, is passed to a `fn` value (whose type has no parameter names), or comes before a \
     positional argument. Use the parameter names of the function's definition, each once, \
     after the positional arguments.";

    50 REF_ARGUMENT Type "`ref` parameter and argument mismatch"
    "A `ref` parameter takes an argument written `ref x`, which names a variable or a field \
     of one; a plain parameter takes a plain argument. The same variable cannot go to two \
     `ref` parameters of one call. Write `ref` exactly where the parameter has it.";

    51 SPREAD_ARGUMENT Type "invalid spread argument"
    "`f(..x)` spreads a tuple, `List`, `Slice`, array or `Dict<str, V>` into the arguments, \
     and needs a callee known at compile time. Spread one of those, or pass the arguments \
     one by one.";

    52 NOT_CALLABLE Type "value is not callable"
    "Only functions, closures and `fn` values can be called. Check that the name refers to \
     a function, not a value of the same name.";

    53 INVALID_CAST Type "invalid cast"
    "`as` converts between numbers, `bool` and `char`; other conversions are methods. Only \
     `u8` casts to `char` (use `char::from_u32(n)` for wider values). Use the conversion \
     method the message suggests.";

    54 INVALID_OPERATOR Type "operator cannot be applied"
    "An operator is used on types that do not support it. For your own types, implement the \
     operator's trait (`impl Add for Point`); for a type parameter, add that trait as a \
     bound.";

    55 AMBIGUOUS_LITERAL Type "literal fits several types"
    "An unsuffixed literal could be any of several types the call or operator accepts, so \
     its type is unclear. Add a suffix (`1i64`) or an `as`.";

    56 INVALID_INDEX Type "invalid indexing"
    "The value cannot be indexed, or the index is not an integer. Index arrays, lists and \
     slices with integers, and types that implement `Index` with their key.";

    57 INVALID_ASSIGNMENT Type "invalid assignment"
    "The left side of `=` cannot be assigned: a const, a static, a variable not declared \
     with `let`, a value captured by a closure without `[ref name]`, or an expression that \
     is not a place. Assign to a `let` variable, a field of one, or an element of one.";

    58 INVALID_CAPTURE Type "invalid closure capture"
    "A closure's capture list (`[ref x]`) names something it cannot capture: no variable of \
     that name, or a `ref` capture in a closure that may run on another thread. Capture a \
     `let` variable or parameter, and share across threads through a `Mutex` or an atomic.";

    59 OUTSIDE_CONTEXT Type "keyword outside its context"
    "`break` and `continue` work only inside a loop, `yield` only in a generator, `await` \
     only in an async body, and `self` only inside a method; `return`, `break` and \
     `continue` cannot leave a `finally` block; `break value` leaves only a `loop` or a \
     labelled block, not `while` / `for`. Move the code into such a body, or out of the \
     `finally`.";

    60 LET_ELSE_NOT_DIVERGING Type "`let .. else` that does not diverge"
    "The `else` block of `let .. else` runs when the pattern does not match, so it must leave \
     the scope: end it with `return`, `break`, `continue` or `throw`.";

    61 UNSAFE_REQUIRED Type "operation needs `unsafe`"
    "The operation can break memory safety: calling an `unsafe` or `extern` function, reading a \
     union field, or indexing a `GcBuf` without a check. Wrap it in `unsafe { .. }`, inside \
     an `unsafe fn` if it is a building block.";

    62 UNHANDLED_ERROR Type "error not handled"
    "A call can throw, and the error is neither caught nor declared. Mark the call with \
     `try`, then either catch the error or add it to the enclosing function's `throws`.";

    63 NOT_AN_ERROR Type "thrown value is not an `Error`"
    "Only types that implement `Error` can be thrown. Implement `Error` for the type, or \
     throw an error type that wraps the value.";

    64 OPTIONAL_OPERATOR Type "optional operator on a value that is not optional"
    "`?`, `??` and `?.` work on optional values (`T | none`), and `try?` / `try!` on calls \
     that throw. Use `.` on a plain value, and `throws` with `try` to pass errors on.";

    65 GUARD_NOT_RELEASED Type "lock guard not released by `using`"
    "A lock guard is released only when a `using` block ends. Bind the guard with \
     `using g = m.lock() { .. }` instead of `let`, and do not discard it.";

    66 USING_NOT_DROP Type "`using` on a value without `Drop`"
    "`using` releases its resource at the end of the block, so the resource's type must \
     implement `Drop` or hold a `Drop` field. Use a plain `let` for other values.";

    67 BLOCK_VALUE_DISCARDED Type "block statement leaves a value"
    "A block-like statement (`if`, `match`, a block) gives a value but is not the last \
     expression and has no `;`. Add `;` to drop the value, or bind it with `let`.";

    68 NOT_ITERABLE Type "value cannot be iterated"
    "`for` iterates an integer range with both bounds, an array, or a type that implements \
     `IntoIterator` or `Iterator` (whose `next` returns `T | none`). Iterate one of those, or \
     call the method that gives an iterator.";

    69 UNSATISFIED_BOUND Type "trait bound not satisfied"
    "A type does not implement a trait that a generic parameter, an associated type or a \
     slot requires. Implement the trait for the type, or use a type that has it.";

    70 NOT_A_TRAIT Type "not a trait"
    "A trait is expected here (after `as`, in an `any` bound, or in a `fn` type bound), but \
     the name is a type or something else. Name a trait.";

    71 TRAIT_AS_TYPE Type "trait used as a type"
    "A trait is not a type. For a value of any type that implements it write `any Trait`, \
     and for a generic parameter `T: Trait`.";

    72 MISSING_TRAIT_ITEM Type "impl is missing a trait item"
    "A trait impl must define every method and associated constant the trait declares \
     without a default. Add the missing items.";

    73 NOT_A_TRAIT_MEMBER Type "impl item is not in the trait"
    "A trait impl defines a method the trait does not declare. Remove it, or move it to an \
     inherent `impl` of the type.";

    74 IMPL_MISMATCH Type "impl item does not match the trait"
    "A method in a trait impl differs from the trait's declaration in its parameters, return \
     type or `throws`. Write it as the trait declares it.";

    75 CONFLICTING_IMPLS Type "conflicting implementations"
    "Two impls give the same trait, or the same method, for one type. Remove one, or narrow \
     a generic impl so they no longer overlap.";

    76 ORPHAN_IMPL Type "impl of a foreign trait for a foreign type"
    "A trait impl needs the trait or the type to be defined in this package. Wrap the \
     foreign type in a struct of your own and implement the trait for that.";

    77 MISSING_SUPERTRAIT Type "supertrait not implemented"
    "A trait requires another trait (its supertrait), and the type does not implement it. \
     Implement the supertrait for the type too.";

    78 INVALID_IMPL Type "impl not allowed"
    "This impl is not allowed: an inherent impl on a type parameter, a manual impl of \
     `Reflect`, an `unsafe impl` of a trait that is neither an auto trait (`Send`, `Sync`) \
     nor an `unsafe trait`, an `impl !Trait` of a trait that is not an auto trait, or a \
     plain `impl` of an `unsafe trait`. Remove it, or write the form the message gives.";

    79 CONST_EVAL Type "cannot evaluate at compile time"
    "A const, array length, const generic argument or `const fn` body uses something that \
     has no value at compile time: a call of a plain `fn`, a heap allocation, or a runtime \
     value. Use literals, other consts and `const fn` calls.";

    80 CYCLIC_DEFINITION Type "definition depends on itself"
    "A const, static or type alias is defined in terms of itself, directly or through \
     others, or a type contains itself by value. Break the cycle; a type that contains \
     itself holds it through a reference type such as `List`.";

    81 INVALID_CONST_ARGUMENT Type "invalid const generic argument"
    "A const generic argument has the wrong type or is out of range for its parameter. Pass \
     a constant of the parameter's type.";

    82 GENERIC_ARGUMENTS Type "wrong generic arguments"
    "A generic item is given the wrong number of type arguments, or ones that do not fit \
     its definition. Pass one argument per parameter (parameters with defaults may be left \
     out).";

    83 INVALID_ENUM_VALUE Type "invalid enum value"
    "A value enum (`V = n`) has only value variants, each with a distinct constant that fits \
     the tag type. Fix the variants the message names.";

    84 FFI Type "invalid foreign-function use"
    "A C function, `extern` declaration or `extern \"C\" fn` breaks an FFI rule: a type C \
     cannot hold (one the collector may move), a closure passed as a C callback, two \
     declarations with different signatures, or a variadic argument C cannot take. Pass \
     C-compatible types, and named functions as callbacks.";

    85 THREAD_SAFETY Type "value cannot cross threads"
    "The value may be used by two threads at once: a `static` of a non-`Sync` type, a \
     `[ref x]` capture in a spawned closure, or a non-`Send` value given to a task. Share it \
     through a `Mutex`, an atomic or a channel.";

    86 INVALID_UNION Type "invalid union"
    "A union declares at least one field, and its literal sets exactly one of them, with no \
     `..base`. Unions take only `#[repr(C)]`.";

    87 AMBIGUOUS_ITEM Type "ambiguous associated item"
    "Several traits, impls or bounds give a method, associated type or constant of this \
     name, so `T::name` is unclear. Name the trait: `<T as Trait>::name`, or call the \
     method as `Trait::name(x)`.";

    88 INVALID_FORMAT Type "invalid format"
    "An f-string hole formats a value that does not support the format (`{}` needs \
     `Display`, `{:?}` needs `Debug`), or its format spec is not valid. Implement or derive \
     the trait, or change the spec.";

    89 BUILTIN_CALL Type "invalid compiler built-in call"
    "A call of a compiler built-in (`size_of::<T>()`, the `atomic_*` and pointer operations, \
     `addr_of`, `GcBuf` functions) has the wrong arguments: a value where a place is needed, \
     an ordering that is not constant, or a missing type argument. Call it as the message \
     shows.";

    90 COROUTINE Type "invalid coroutine use"
    "A coroutine (`async` or `yield`) breaks a rule: `await` on something that is not a \
     future, or a pointer into the coroutine's frame used after a suspension point. Await a \
     future, and take pointers again after `await` or `yield`.";

    91 BROADCAST Type "invalid broadcast"
    "A broadcast call `f.(xs)` maps over a `List`, `Slice` or array, and arrays given \
     together must have the same length. Pass collections of matching length.";

    92 EXPLICIT_DROP Type "`Drop::drop` called directly"
    "`drop` runs on its own when a `using` block ends, and calling it again would release \
     the resource twice. Expose a method such as `close()` for an early release.";

    93 INSTANTIATION_LIMIT Type "generic instantiation too deep"
    "Instantiating a generic function made it instantiate itself with ever larger types. \
     Break the recursion, for example with an `any` parameter.";

    94 INVALID_REPR Type "invalid representation"
    "A `#[repr(..)]` or `#[niche]` does not fit the type: a packed struct that holds a \
     collected reference, a union with a repr other than `C`, or `#[niche]` on a type with \
     more than one field. Change the attribute or the type.";

    95 TYPE_SIZE Type "type too large"
    "A value type is larger than the compiler handles. Hold the large part through a \
     reference type such as `List`.";

    96 UNNEEDED_TRY Type "`try` or `catch` that does nothing"
    "A `try` covers no call that throws, or no error reaches a `catch`. Remove it.";

    97 ROKON_VALUE Type "ROKON value of the wrong shape"
    "A ROKON file (`rok.rokon`, a config) parses, but a value does not fit what the reader \
     expects: a missing field, a value of the wrong type, or a number out of range. Write the \
     value the message names.";

    98 NON_EXHAUSTIVE Pattern "non-exhaustive match"
    "A `match` must cover every value. Add arms for the cases the message lists, or a `_` \
     arm; an enum marked `#[non_exhaustive]` in another package always needs `_`.";

    99 UNREACHABLE_ARM Pattern "unreachable match arm"
    "Earlier arms already match every value this arm matches, so it never runs. Remove it, \
     or move it before the arm that covers it.";

    100 PATTERN_TYPE Pattern "pattern does not fit the value's type"
    "A pattern cannot match a value of this type: a struct pattern of another struct, a \
     tuple pattern on a value that is not a tuple, or a refutable pattern where only an \
     irrefutable one goes (`let`, parameters, `for`). Write a pattern of the value's type, \
     and use `match` or `let .. else` for refutable ones.";

    101 PATTERN_ARITY Pattern "wrong number of fields in a pattern"
    "A tuple or variant pattern has more or fewer parts than the value. Match each part, or \
     use `..` for the rest.";

    102 OR_PATTERN_BINDINGS Pattern "or-pattern alternatives bind differently"
    "Every alternative of `a | b` binds the same names, with the same types. Bind the same \
     names in each, or give the binding a sum type.";

    103 BINDING_SHADOWS_VARIANT Pattern "binding looks like a variant"
    "A bare name in a pattern is a new variable that matches everything, not the variant of \
     that name. Write the variant's path, `Color::Red`, to match it.";

    104 REST_PATTERN Pattern "invalid rest pattern"
    "A pattern has at most one `..`, and a rest pattern goes only where the message says. \
     Remove the extra `..`.";

    105 USE_AFTER_HANDOFF Handoff "value used after it was handed over"
    "The value was moved into a lock, sent through a channel, handed to a `Handoff` or a \
     task, and is used again here; its new owner may be changing it. Pass `.clone()` to keep \
     a separate copy, or give the variable a new value first.";

    106 USE_AFTER_UNLOCK Handoff "value used after its lock was released"
    "The value came out of a `Mutex` or `RwLock`, and the `using` block that held the lock \
     has ended, so the lock no longer protects it. Use it inside the `using` block, or take a \
     copy with `.clone()`.";

    107 SHARED_HANDOFF Handoff "shared value handed over"
    "The value is handed to another owner while other code can still reach it (a lock, a \
     thread-local, a `Sync` object, or another argument of the same call). Hand over a \
     `.clone()`, or a value nothing else holds.";

    108 CANNOT_DERIVE Meta "cannot derive"
    "A `#[derive(..)]` cannot implement the trait for this type: a field does not implement \
     it, it needs another derived trait too, or the type is of a kind the derive does not \
     support. Implement the trait by hand, or fix the field the message names.";

    109 INVALID_ATTRIBUTE Meta "malformed attribute"
    "An attribute's arguments are not in the form it takes, or the attribute is not one Rok \
     knows. The message gives the form, such as `#[path(\"file.rok\")]`.";

    110 MISPLACED_ATTRIBUTE Meta "attribute on the wrong item"
    "The attribute does not apply to this kind of item: `#[export_name]` on a generic or \
     bodiless function, `#[track_caller]` on `main`, an attribute on a statement that only \
     `#[cfg]` may take. Move it to an item it applies to, or remove it.";

    111 INVALID_LANG_ITEM Meta "invalid `#[lang]` item"
    "`#[lang(\"slot\")]` (standard library only) names a known slot, once, on an item of the \
     slot's form.";

    112 INVALID_OPERATOR_DECL Meta "invalid operator declaration"
    "An operator attribute (`#[infix(..)]`, `#[prefix(..)]`, ..) on a trait method breaks a \
     rule: a token that cannot be an operator, precedence on a built-in token, or aliases at \
     different levels. A new token gives `like = \"<existing token>\"` for its precedence.";

    113 INVALID_KEYWORD_DECL Meta "invalid `#[keyword]` declaration"
    "`#[keyword(\"name\")]` goes on a free function, its handler, and `name` is an \
     identifier that is not already a keyword.";

    114 INVALID_LITERAL_HANDLER Meta "invalid literal handler"
    "A `#[literal(..)]` handler must be a function with the parameters its kind takes, \
     registered once per suffix, priority and return type. A suffixed literal needs a \
     handler for its suffix.";

    115 INVALID_INTRINSIC Meta "invalid `#[intrinsic]`"
    "`#[intrinsic(\"rok.OP.TYPE\")]` is for the standard library, on a function of the shape \
     the operation takes.";

    116 INVALID_MACRO Meta "malformed macro definition"
    "A macro's rules are not well formed. Each rule is `( matcher ) => { template }`, a \
     metavariable is `@name: fragment`, and a repetition is `@rep[sep = \",\"] { .. }`.";

    117 NO_MACRO_RULE Meta "no macro rule matches"
    "None of the macro's rules matches the arguments of this call. Check the call against \
     the macro's matchers.";

    118 INVALID_TEMPLATE Meta "invalid meta template"
    "A meta block's template uses a `@` token wrongly: an unknown token, one outside the \
     `@for_fields` or `@for_variants` it needs, or one without its `(..)` or `{ .. }`. Write \
     it as the message says.";

    119 INVALID_META_BLOCK Meta "invalid meta block"
    "A meta block's definition or use breaks a rule: it takes a built-in attribute's name, \
     it is private to another file, its body does not start with the right `@` block, or \
     it expands to something other than items. Fix the definition or the use as the message \
     says.";

    120 EXPANSION_LIMIT Meta "recursive expansion"
    "A macro or meta block expands into itself, directly or through others, without end. \
     Break the cycle.";

    121 INVALID_CFG Meta "invalid `cfg` predicate"
    "A `#[cfg(..)]` or `cfg!(..)` predicate is not well formed. It is a flag (`unix`), a \
     `key = \"value\"` pair, or `all(..)`, `any(..)` and `not(..)` of those.";

    122 INCLUDE_FAILED Meta "file include failed"
    "`include_str!` / `include_bytes!` take one string path, relative to the file, of a file \
     that can be read.";

    123 DUPLICATE_REGISTRATION Meta "registered twice"
    "A symbol, operator, lang slot, keyword or literal handler is registered by two items. \
     Keep one.";

    124 META_NAME Meta "meta name cannot be resolved"
    "A macro or meta block name is defined by several packages, or is reached only through \
     a scope Rok no longer has. Import it from its package: `use dep::name!;`.";

    125 IO Other "cannot read or write a file"
    "The compiler could not read or write a file or directory. Check that the path exists \
     and that you may read or write it.";

    126 LINK_FAILED Linkage "link failed"
    "The linker failed, or could not find a native library. Check the `-L` directories and \
     `#[link]` names, and that the system linker is installed.";

    127 BACKEND_FAILED Codegen "code generation failed"
    "The code generator (LLVM) failed. This is usually a compiler bug; please report it with \
     the code that caused it.";

    128 TOOL_FAILED Linkage "external tool failed"
    "A tool the compiler runs (an assembler, archiver or linker) could not start or failed. \
     Its output follows the message.";

    129 INVALID_WIT Module "invalid WIT"
    "A `.wit` file named by `ffi::wit::import { .. }` or `ffi::wit::export { .. }` cannot be \
     parsed or resolved. Fix the WIT file at the place the message names.";

    130 UNSUPPORTED Unsupported "not supported by this backend"
    "The code uses something the selected code generator does not support yet. Build with \
     the LLVM backend (`-C backend=llvm`), or rewrite that part.";

    131 INTERNAL Internal "internal compiler error"
    "The compiler hit a state it should never reach. This is a compiler bug; please report \
     it with the code that caused it.";

    132 LINT Lint "lint"
    "A lint finding: code that compiles but is likely a mistake. Its note names the lint \
     (`#[warn(name)]`, the JSON `lint` field). Fix the code, or allow the lint with \
     `#[allow(name)]` or `-A name`; `rokc lint --list` describes every lint.";

    133 INVALID_OPTION Other "invalid compiler option"
    "A compiler option has a value it does not take. `rokc help` lists the options.";

    134 USE_AFTER_MOVE Handoff "resource used after it was moved or closed"
    "A value whose type implements `Drop` (a `File`, a socket, a channel `Sender`) has one \
     owner. It was moved to another owner or closed, and is used, moved or closed again \
     here. Use the new owner, do the work before it is closed, or keep a copy with \
     `.clone()` before it moves.";

    135 UNCLOSED_RESOURCE Handoff "resource not closed"
    "Closing this value can fail (its type implements `io::Close`), so it is not closed for \
     you: on some path it leaves its scope, is replaced, or is dropped with an error that \
     `try?` discards, while still open. Add `try x.close();` before it leaves, bind it with \
     `using x = .. { .. }`, or catch the error and close what it holds.";

    136 TRAIT_NOT_IN_SCOPE Resolve "trait of the method not in scope"
    "The method exists, but it comes from a trait that is not imported here, and a trait's \
     methods are reached only while the trait is in scope. Add `use path::Trait;`, as the \
     help shows, or call it as `Trait::method(x, ..)`.";

    137 THREAD_LOCAL_ESCAPE Handoff "thread-local value kept past `with`"
    "A `ThreadLocal`'s `with` lends the value to its closure only for the call. The closure \
     keeps it, returns it, hands it over or awaits with it, so it would outlive the call or \
     move to another thread. Use it inside the closure, and take a `.clone()` or what you \
     computed from it out instead.";

    138 VIEW_AFTER_RESIZE Handoff "list view used after a resize"
    "A view of a list (a `SliceMut` or a slice) is used after the list was resized; a \
     resize may move the elements, so the view no longer points at them. Make the view \
     again after the resize, or resize before making it.";

    139 PUB_SIGNATURE Type "`pub` function signature not written out"
    "A `pub` or `pub(pkg)` function is called from other modules, so its signature is \
     written out, not inferred from the body. Give every parameter a type, `x: i32` (or a \
     type parameter, `fn f<T>(x: T)`, for any type). Without `->` it returns `()`; when \
     the body gives a value, write its type, `-> i32`, as the fix-it shows. Trait impl \
     methods take the trait's types and private functions keep inference.";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_unique_and_described() {
        let mut names = std::collections::HashSet::new();
        for (i, e) in Code::all().iter().enumerate() {
            assert_eq!(usize::from(e.code.0), i + 1, "{} out of order", e.name);
            assert!(names.insert(e.name), "{} twice", e.name);
            assert!(
                !e.title.is_empty() && !e.text.trim().is_empty(),
                "{}",
                e.name
            );
            assert!(!e.title.ends_with('.'), "{}", e.name);
        }
    }

    #[test]
    fn parse_and_show() {
        assert_eq!(Code::MISMATCHED_TYPES.to_string(), "E0040");
        assert_eq!(Code::parse("E0040"), Some(Code::MISMATCHED_TYPES));
        assert_eq!(Code::parse("e40"), Some(Code::MISMATCHED_TYPES));
        assert_eq!(Code::parse("E0"), None);
        assert_eq!(Code::parse("E9999"), None);
        assert_eq!(Code::parse("E200x"), None);
        assert!(
            Code::SYNTAX
                .explain()
                .starts_with("E0010: unexpected token\n\n")
        );
    }
}
