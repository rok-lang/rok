# Changelog

## 0.2.0 (2026-11-04)

This release is breaking: Rok 0.2 cleans up the language, the standard library and the tools
in one step, with no deprecated forms left behind. Everything that 0.1.6 warned about is now
an error or gone (one, `try? f() ?? d`, changes meaning), and every error says what to write
instead. Code that builds warning-free on 0.1.6 has the least to change; see "Migrating from
0.1" at the end. Items under "Same code, new result" change behaviour without a compile error.

The 0.1 series (0.1.0 to 0.1.6) is no longer supported: it gets no fixes or security
releases. `rok toolchain update` moves to 0.2.0; the 0.1 toolchain stays installed until
removed.

### Highlights

- Less to write: literals take their type from their uses, integers default to `i64`, `let`
  redeclares, and `if let` / `x != none` narrow an optional.
- Python-style syntax: `in` / `!in`, chained comparisons, tuple assignment `(a, b) = (b, a)`,
  `using x = e;`, patterns in parameters, `throw "msg"` and a throwing `main`.
- `some Trait` in arguments, returns and `type X = some Trait`, beside `any Trait`.
- Arrays: `Array<T, N>`, multi-axis slicing and assignment, NumPy-style broadcasting, fused
  `.op` chains, `.=` and `**`.
- A larger library: iterator methods on every collection, counts and grouping,
  `d[k, default:]`, `dbg`, `f"{x=}"`, `Complex`, `random`.
- Errors that recognise Python and Rust habits and say what to write; lost writes and resource
  misuse are caught at compile time.

### Language

- Keywords: `macro`, `defer`, `dyn`, `gen` and `do` are reserved, and `move` is reserved outside
  capture lists; write `r#name` to keep such a name.
- Literals: a leading-zero decimal (`0755`) is an error; write `0o755` or `755`. An unsuffixed
  integer into a sum with several integer members and a float is an error; write `1u16` or
  `1.0`. String literals as const type arguments (`f::<"text">`) are removed. Const parameter
  and argument types are checked; comparisons and conditionals in const arguments are errors.
  `b'x'` is a byte literal of type `u8`, in patterns and ranges too.
- Inference: unsuffixed integer literals default to `i64` (was `i32`). A
  literal takes its type from its uses in the whole function: `let c = 0` then `takes_u8(c)` is
  a `u8`, also through arithmetic, branches, list / tuple / struct elements, call arguments
  (`Atomic::new(0)`), closures, generic bounds and impl selection. An empty `Dict::new()`,
  `channel()`, `Mutex::new([])` or `d[k] = v` is typed by later uses; `for i in 0..10` by the
  body. A use that needs one type now (an overload, an `any` slot, a trait-qualified call) takes
  the one number type that works, else the default; two uses that disagree name both sites.
  An empty `[]` argument takes its element type from the other arguments (`first_or([], 9)`).
  `Set::from(xs)`, `Dict::from(pairs)` and other static methods of a generic trait impl infer
  their type from the arguments; a type parameter with a default that a `Type::f(..)` call
  leaves open takes the default (`Set::default()` is a `Set<T>`).
- Optionals: `if let x = opt`, `while let x = opt` and `let x = opt else { .. }` bind the value
  without `none` (one layer). `x != none`, `x == none` with an `else`, and early-return guards
  (`if x == none { return; }`) narrow `x`; write `x != none` for `!(x is none)`. `x == none` and
  `x != none` test for `none` on any optional, with no `PartialEq` on the other members.
- `some Trait`: `-> some Tr` (also `some (A + B)`, `some fn(A) -> R`, `pub`) returns one hidden
  concrete type; callers see only the bounds, across packages too. `fn f(x: some Tr)` is an
  anonymous type parameter, one per `some`; `::<..>` names only the named parameters. `some` in
  a `let`, field, type argument or closure parameter is an error that names
  `type Name = some Tr;`.
  Named opaque types: `type X = some Tr;` is one concrete type, defined by the functions of X's
  module that return `X` (E0146), usable in fields, lists and signatures; outside the module
  only the bounds are visible, and naming the concrete type is E0147. `pub type X = some Tr;`
  exports it. `type X = any Tr` is unchanged.
- `try`: `try? f() ?? d` is `(try? f()) ?? d` (also `try!`); write `try? (f() ?? d)` for the 0.1
  meaning. `try` still covers `??`. When `f` returns `T | none`, `try? f()` is `(T | none)?`;
  write `(try? f()) ?? none` to merge an error with a returned `none`. The `try` of `try for`
  covers only the iterable and each `next`; throwing calls in the body need their own `try`
  (fix-it offered). `for` over a `TryIterator` without `try` is an error; write `try for` or
  `.results()`. `try?` on a call whose error can hold a must-close value is an error; catch the
  error and close the value.
- Trailing closures: a bare `return;` inside a statement's trailing closure is an error; write
  `return@name` (new labeled return). A trailing-closure call that starts a statement ends at its
  `}`. A closure with `-> T` needs a block body. A capture list may hold value captures
  (`[a, b] || ..`); `x.member { body }` is a trailing closure.
- Prelude: the implicit import is `rok::core::prelude`. Other core names need a module path or
  a `use`: `task::sleep`, `ops::Add`, `future::Poll`, `sync::LoadOrdering`; operator traits are
  implemented as `impl ops::Add for V`. `oom` is `panic::oom()`. A bare variant resolves from
  the expected type for any enum and is an error without one; write `Ordering::Less`. A bare
  variant in a pattern matches that variant.
- Traits: a method of a trait not in scope is an error, also `Type::f()`; `use` the trait.
  `unsafe trait` declares a trait whose impls are `unsafe impl`. Impl method parameter types,
  return type and `unsafe` must match the trait (E0074); two concrete impls of one trait for one
  type are E0075. An impl that leaves out an associated type the trait declares without a
  default is E0072; the help writes the line (`type Item = i32;`). An impl that leaves out a
  trait parameter without a default is E0082, and an associated type the trait does not declare
  is E0073. Trait items and trait impl items cannot be `pub` (E0016). `#[sealed]`
  traits (such as `AtomicPrimitive`, `NicheInt`, `NicheFloat`, `Transfer`, `Pinnable`) are
  implemented only in their package. Trait methods with their own type parameters are
  callable on `any Trait`. A method written in an impl whose generic parameter is named like
  the impl's (`impl<K> W<K> { fn f<K>(..) }`) is E0142; rename it. A trait's default method
  keeps its own parameters there. An impl method written without `throws` takes the trait
  method's. In `T: Add<U, Output = V>`, `V` is inferred once the trait arguments are known.
  The orphan rule allows `impl ForeignTrait<LocalType> for ForeignType`
  (`impl Mul<Matrix> for f64`); dispatch among impls of one trait checks every argument.
- Ownership: a value whose type implements `Drop`, or holds one, has one owner. Using it after
  `let b = a`, a store, a capture or a call that keeps it (also through a function value) is
  E0134; `.clone()` / `.try_clone()` is suggested when the type has one. The compiler releases
  it where its owner leaves scope and when a task ends. A `File` (any `io::Close` type), socket
  or tls connection left open on a path is E0135; close it (`try f.close()`) or bind it with
  `using`. `using` of one value twice is an error. A channel closes when its last `Sender` is
  released; `close()` closes only that sender. Each field is its own owner, also through a
  binding that narrows a sum (`if let x: Pair = p`, `match`, `let .. else`): closing `x.a`
  leaves `x.b` open, and the error names the field an arm never closes.
- Threads and closures: `[move x]` in a capture list moves `x` into the closure when it is made
  (a copy of every collection it reaches); using `x` afterwards is E0105. A moved capture needs
  only `Send`, so a list, a `Command` or a `Child` can go to `thread::spawn([move xs] || ..)`,
  `task::spawn_blocking` or `par::Scope::spawn`. Plain captures still need `Send + Sync`;
  `[ref x]` is never `Send`; `[ref x, move x]` is an error. `fn(..) + Send` no longer implies
  `Sync`: write `+ Send + Sync` for a fn value that several threads share (a `static`, `par::*`).
  A closure that moved values in, or a `fn(..) + Send` value that is not `Sync`, passed to a
  `fn(..) + Send` parameter (`thread::spawn`, `task::spawn_blocking`, `par::join`, your own
  functions) is handed over once; a later use is E0134. The error for a non-`Sync` capture
  suggests `[move x]`.
- Stream views: `r.lines()`, `bytes`, `split`, `take`, `lines_async`, `bytes_async` and
  `TcpListener::incoming` take `ref self` and return a view tied to the stream's scope, like a
  lock guard's `get()`. Returning one, storing it past the stream, sending it to a task, or using
  it after the stream was closed is E0140. The owning forms `into_lines()`, `into_bytes()`,
  `into_split()`, `into_take()`, `into_lines_async()`, `into_bytes_async()` and
  `TcpListener::into_incoming()` return `io::Owned<V, R>`, which iterates like the view, closes
  the stream with itself and gives it back with `into_inner()`.
- Lock guards and thread-locals: throwing a guarded value out of `using`, or passing a guard's
  `get()` view to a function that keeps it, is an error; keep `get_clone()`. `ThreadLocal::with`
  rejects closures that let the value escape and `async` closures (E0137).
- Ownership checks follow every value flow: a guard's value or a stream view leaving through a
  `break`, a labelled block, an `if` / `match` arm, a `catch`, a closure result, or a tuple or
  struct literal is E0106 / E0140; moving a stream (assigned, passed by value, captured,
  stored) ends its views. A `[move ..]` closure handed over twice in any form (`par::join(f, f)`,
  through a returned argument, a list, an `if` arm) is E0134, and the futures of an
  `async [move ..]` closure share its owner. A must-close temporary that a call only borrows
  (`(try File::open(p)).read_to_str()`, `BufReader::new(try File::open(p)).lines()`) is E0135;
  bind it with `using`. A call that does not keep a stream (`r.capacity()`, `f"{r:?}"`) no
  longer ends its views.
- `SliceMut<T>` is a writable view of a list, like `&mut [T]`; a list or `xs[a:b]` passed where
  one is expected is a view. Using one after its list resized is E0138 where visible, a panic
  otherwise.
- Indexing: an index may have several parts with slices (`a[1:, ::2]`, `a[.., 0]`,
  `a[:, none]`, `a[mask]`) on a type implementing `index::AxisIndexed`, where integers are
  `isize`, `..` is `index::Rest`, `none` is `index::NewAxis` and `a:b:k` is `index::SliceArg`.
  On `List`, `Slice`, fixed arrays and `str` such an index is E0144 (it was a parse error);
  write `m[1:][0]`. Slice assignment writes a range: `xs[1:3] = ys`, `a[:, 0] = 0.0`.
  `IndexSet<I>` with `type Value` is `IndexSet<I, V>`, so one index takes several value types.
- Broadcasting: a dot expression over `Array`, or any type implementing `ops::Broadcastable`, is
  one loop with no intermediate arrays; axes of length 1 stretch and a lower rank lines up at
  the last axes (`m .+ row`), as in NumPy. `y .= expr`, `.+=`, `.-=`, `.*=`, `./=`, `.%=`,
  `.**=` write in place into a `List`, fixed array or `Broadcastable` target (views included,
  `m[:, 0] .= 0.0`) without allocating; a source overlapping the target is read from a copy.
  `.??` fills `none` elements (`col .?? 0.0`); NaN passes through. Broadcast misuse is E0091
  with the form to write.
- Repeat literals: `[v; N]` of an element type that is not `Clone` is E0069, and of a
  must-close or `Drop` element too (one owner per value); the help writes
  `array::from_fn::<T, N>(|_| ..)`. A generic `[x; N]` needs `T: Clone`.
- Literal suffixes: a `pub` `#[literal]` handler (a units suffix) applies in packages that
  depend on its package directly; others stay in their package (a dependency's private
  suffixes used to reach its dependents and fail there). A library using its own suffix builds
  as a dependency.
- `ref self`: a method on a list element or a `Dict` entry changes the element (it changed a
  copy). A `ref self` or mutating call on a const or static is an error; copy it into a
  variable. A field write on a value-captured struct is an error; write `[ref p]`. `async`
  methods may take `ref self`; the receiver is written back when the future completes.
- Functions: a `pub` / `pub(pkg)` fn writes its parameter types, and without `->` returns `()`;
  a body that gives a value is E0139, whose fix-it writes `-> T`. Return paths of different
  types without `->` are E0041; write the sum. Parameter defaults are `const` expressions and
  trailing. A positional argument after a named one is E0049. `fn main` cannot be `unsafe`, nor
  `throws` with `-> i32`.
- Statements: `break value` in `while` / `for` is E0059. A top-level `if` / `match` / block
  statement leaving a value is E0067. Return, `break` or `continue` leaving `finally` is an
  error. A block item and a `use` of its name are E0032. A bare `return;` in a script's
  top-level statements ends it with exit code 0 (`return v;` stays E0018).
- Bindings: `let` may redeclare a name in the same scope; the initializer reads the old binding,
  which stays as it was (closures, views and guards made from it keep it). A must-close value
  left open by the redeclaration is E0135; close it or move it in
  (`let f = io::BufReader::new(f);`). E0038 is only a variable named like a nested function.
- Assignment: assigning to a value that is not a place (`f() += 1`,
  `d.entry(k).or_insert(0) += 1`, `p().x = 1`, `arr()[0] = 1`) is E0141; the compound forms
  compiled and changed a temporary. Write `d[k, default: 0] += 1` or `let v = f(); v.x += 1;`.
  A field set on a copy that nothing reads afterwards (a by-value `self` or parameter, a `for`
  binding, a `let` of `xs[i]`, a closure's struct parameter in
  `m.with(|a| { a.total += 1; })`) is E0057, in every build; write `ref self`,
  `for ref p in ps`, `ref p`, return the value, or write the element back. A write through a
  lock guard's `get()` is E0141, whose help writes `g.update(|v| { ..; v })` or `g.set(v)`.
  Destructuring assignment `(a, b) = (b, a + b)` takes variables, fields, elements, nested
  tuples and `_`; the right side is evaluated first.
- `for ref x in xs` binds each element of a `List`, `SliceMut` or fixed array as a place; writes
  and `ref self` calls reach the element. A resize of the list in the body is E0138 where
  visible, a panic otherwise.
- Operators: `x in xs` / `x !in xs` test membership (`i in 0..n` needs no parentheses; every
  range kind implements `Contains`); `! in` and `not in` are errors naming `!in`. Comparisons
  chain in one direction (`0 <= i < n`, `a == b == c`), each operand evaluated once; mixed
  directions, `!=` and membership chains stay E0019, whose help writes the `&&` form. `**` is
  the power operator, with `**=` and `.**` (`Pow`, `PowAssign`, `DotPow`): right-associative,
  tighter than prefix `-` (`-2 ** 2` is -4); an integer base takes a `u32` exponent and a
  negative literal exponent is E0145. `x**y` lexes as one token, so a user operator registered
  as `**` conflicts.
- `using x = e;` binds for the rest of its block or script; the block form stays.
- Parameters may be patterns: `|(k, v)| ..`, `fn f((a, b): (i32, i32))`, `fn g(P { x, y }: P)`;
  `ref` parameters stay names.
- `throw "msg"` throws `ValueError::new(msg)`. `fn main()` without `->` may throw without
  `throws`; the error is printed and the exit code is 1.
- Patterns: a pattern name that is a const or static, or a near-miss of a variant, is an error;
  write `x if x == C` or the variant path. A struct pattern on another package's
  `#[non_exhaustive]` struct ends in `..`; a `match` on such an enum needs a `_` arm.
  `if let` with an irrefutable pattern compiles (a bare name on an optional strips `none`, see
  Optionals). Capitalised bindings get `non_snake_case` and
  `unused_variables`.
- Sums: `T | U` with generic terms no longer nests when the substituted types overlap: with
  `T = i32 | none` and `U = i32` it is `i32 | none`, not `i32 | (i32 | none)`. `PartialOrd` /
  `Ord` on sums with two or more non-`none` terms are removed; use `sort_by_key` / `sort_by`.
- Conversions: `x as bool` from an integer, float or char is an error; write `x != 0`.
  `n as f64` on a big integer (`n: int`) is an error; write `n.to_f64()` (rounds),
  `try f64::try_from(n)` or `f64::saturating_from(n)`. A fixed-width integer still converts
  with `as f64`.
- Unsafe and FFI: reading or writing an extern static is `unsafe`. A gc type (`List`, `Dict`,
  closure, sum, `any`) in an extern item is an error. `#[link]` arguments are validated.
- Shadowing: a bare associated type name that shadows an outer type, inside its trait or impl,
  is an error; write `Self::X`.
- Meta: another package's bare meta name is an error; write `dep::name!` or
  `use dep::name!;`. Names a template writes resolve in the meta block's module, not at the use
  site; write `@use_site(name)` for a use-site name. A package's `pub` templates name existing,
  reachable rooted paths. `#[derive(dep::X)]` that `dep` does not define is an error. A meta
  block cannot be named like a built-in attribute (`bench`, `path`, `niche`, `cfg_attr`,
  `discardable`, `sealed`, `internal`, ..). An unknown `#[rokon(..)]` key is an error.
- `#[internal]` items are callable only inside the toolchain; E0034 for them and for
  `pub(pkg)` items names what to write.
- Not-found errors name items removed by `#[cfg]` and the cfg they need. Shown types leave out
  trailing type arguments equal to their default.

Same code, new result:

- Binary operator precedence is Rust's: `1 + 2 << 3` is `(1 + 2) << 3` (24, was 17);
  `a ^ b & c` is `a ^ (b & c)`.
- `none` sorts first in `T | none`; `sort`, `min` / `max` / `clamp` and `<` on optionals change.
- Sum terms follow the spec's canonical order, so sums print and tag in that order.
- A value of type `T` stored into a sum holding `T` as a nested layer goes into the layer.
- A template's bare name that exists at both sites means the definition's.
- In a trait's default method, `self.m(..)` calls the impl's `m`, not an inherent `m` of the
  type or another impl of the trait (`'z' !in s` called `str::contains(str)`).
- A `pub` fn without `->` whose body never returns has type `()`, not `!`.
- `try? f() ?? d` is `(try? f()) ?? d`: an error gives `d` (0.1: `none`). 0.1.6 warned
  (`try_coalesce_precedence`).
- An unsuffixed integer that nothing types is an `i64`: it overflows at the `i64` bounds, is
  `i64` in an `any` slot (`x is i32` is false), and prints and reflects as `i64`.
- `for await` over a stream in a variable or field, `try for` over a `TryIterator` in one, and
  either over a `match` / `if let` binding advance that place, not a copy: a second loop goes
  on where the first stopped.
- `#[derive(Default)]` uses declared field defaults, `default()` only for fields without one.
- A repeat literal `[v; N]` evaluates `v` once and clones it into each element, so
  `[List::<i32>::new(); 3]` is three lists and a write through one element leaves the others
  unchanged (0.1 shared one collection); `[T; N]::default()` defaults each element. Copy and
  plain value elements are filled as before, with no `clone` call.

### Standard library

- io: all io is in `rok::core::io` (`io` in the prelude); `rok::std::io` is removed. `Reader` /
  `Writer` are `Read` / `Write`; stream methods come from the traits, and the inherent `read`,
  `write`, `write_all`, `flush`, `seek`, .. (and `_async` forms) on `File`, sockets and tls
  connections are removed. `Read::read` takes `SliceMut<u8>`; `Write::write` takes
  `B: AsBytes`; `write_str` / `write_bytes` / `write_fmt` are `write_all`; user `Read` /
  `Write` impls change signature. `BufRead::read_line(out: StrBuf) -> usize`;
  `io::read_line()` returns `str | none` without the newline. `lines()` exists only on
  `BufRead` (wrap a `File` in `io::BufReader`); `lines()` and `bytes()` are `TryIterator`s.
  `io::byte_buf(n)` is `List::filled(n, 0u8)`; `fs::SeekFrom` is `io::SeekFrom`;
  `BufReader::buffered` is `buffer_len`; `Cursor::get_ref` is `inner`. `StdinLock`,
  `Stdin::lock`, `io::RawFd`, `Read::read_into` and the `raw_fd` trait methods are removed.
- Async io: `AsyncRead`, `AsyncBufRead`, `AsyncWrite`, `AsyncSeek` and the inherent async
  socket and tls methods take `ref self`. New: `io::AsyncClose`, `close_async`,
  `shutdown_async`, `UdpSocket::connect_async`, Unix socket twins, `Child::wait_async`,
  `Command::output_async` / `status_async` (they run a private copy, so the command stays
  usable).
- fs: `OpenOptions` and every `open_with` are removed; each way of opening is a function:
  `File::open(p)` (read), `File::open(p, WriteOnly | ReadWrite | Append)`, `File::create(p)`,
  `File::create(p, CreateFlags { read: true })`, `File::create_new(p)`,
  `File::open_or_create(p)`, `File::append(p)`. The Unix create mode is
  `CreateFlags { permissions: Permissions::from_mode(m) }`. `ReadDir` and `fs::walk` are
  `TryIterator`s (`try for e in fs::read_dir(p)`, items are `DirEntry`); `fs::read_dir_async`
  returns a `ReadDirStream`. `File::mmap(len, offset)` is `mmap(offset, len)`;
  `Mmap::slice(a, b)` is `m.as_bytes()[a:b]`; `Dir::cwd` is `Dir::current`.
  `Permissions::set_readonly` and `PermissionsExt::set_mode` set in place (`p.set_readonly(true)`,
  not `p = p.set_readonly(true)`); `from_mode` / `mode` come only from `PermissionsExt`.
  `Metadata::mtime` / `atime` / `ctime` are `modified` / `accessed` / `status_changed`.
- net: `TcpStream::connect`, `TcpListener::bind`, `UdpSocket::bind` / `connect` / `send_to` take
  any `ToSocketAddrs` (text, addresses, `(IpAddr, u16)`, a list). `SocketAddr::v4` / `v6` are
  `SocketAddr::new(ip.into(), port)`; `with_port` is a copy plus `set_port`; `SocketAddrV4` /
  `V6` and `Ipv4Addr` / `Ipv6Addr` fields are getters (`ip()`, `port()`, `octets()`,
  `segments()`). `TcpListener::incoming` takes `ref self` and returns a view, a `TryIterator`
  of `TcpStream`; `into_incoming()` owns the listener. Socket and tls `close()` throw
  `IoError` (`try s.close()`); `try_close` is removed. `set_nonblocking` and the timeout setters
  no longer throw. `net::resolve_one` is `net::resolve(f"{host}:{port}")[0]`.
- tls: `ClientConfig::new()` throws `TlsError` and `try_new` is removed; `ClientConfig` /
  `ServerConfig` are `#[non_exhaustive]` with field defaults. `resumed` / `early_data_accepted`
  are `is_resumed` / `is_early_data_accepted`; `Certificate::der` is `to_der`; `not_before` /
  `not_after` return `SystemTime`; `enable_pq` is `enable_post_quantum`.
- OS-specific parts: Unix sockets are in `os::unix::net`; `sendfile` / `splice` /
  `write_zerocopy` in `os::linux::net::TcpStreamExt`; `process_group`, `ExitStatus::signal`,
  `Child::pidfd` in `CommandExt`, `ExitStatusExt`, `ChildExt` (in the prelude on their OS);
  `env::parent_pid` is `os::unix::process::parent_id`; Ctrl-Break is
  `os::windows::process::on_ctrl_break`. `fd()` and `File::from_fd` are `AsRawFd` /
  `FromRawFd` (`AsRawHandle` / `AsRawSocket` on Windows).
- process and env: `env::exit` / `env::pid` are `process::exit` / `process::id`.
  `process::kill`, `process::signal`, `Child::signal` and `process::signal(i32, ..)` are
  `send_signal` and `process::on_signal(Signal, ..)`; `process::raise` takes a `Signal`.
  `env::set_var` / `remove_var` are `unsafe fn` and throw `IoError`; `env::try_set_var` is
  removed. `Command` is `#[must_use]`. `env::join_paths` and `Path::from_components` take
  `IntoIterator`.
- process: `Child.stdin` / `stdout` / `stderr` are private; take a pipe with `take_stdin()` /
  `take_stdout()` / `take_stderr()` (`ChildStdin | none`, ..). The taker owns and closes it;
  pipes never taken close when the `Child` is released. A `File` moved into `Stdio::File`
  belongs to the command: `spawn` / `output` / `status` (and the async forms) close it once the
  child starts, so a spawn no longer leaks a descriptor; running a copy of that command again is
  `InvalidInput`. `Child::wait` takes `ref self`. New `Child::handle()` gives a `ChildHandle`
  (`id`, `kill`, `send_signal`; `Send + Sync`) for another thread while the owner waits.
  `process::on_signal` and `os::windows::process::on_ctrl_break` take `fn() + Send + Sync`.
- net: `TcpStream::split`, `UnixStream::split`, `tls::Client::split` and `tls::Server::split`
  give a `ReadHalf<T>` and a `WriteHalf<T>`, each owning one direction. Closing the write half
  shuts writing down (tls: `close_notify` first) while the read half reads on; the last close
  closes the socket; `shutdown(how)` works on either half; `ReadHalf::unsplit(w)` joins them.
  `WriteHalf` of a tls connection has `key_update` / `key_update_async`.
- io: `Read::chain` owns both readers, closes both, and `into_inner()` returns the pair;
  `Take::into_inner` is removed (use `into_take()`).
- os: `IntoRawFd` (Unix) and `IntoRawHandle` / `IntoRawSocket` (Windows) on every type with the
  matching `AsRaw*` except the standard streams: the value is consumed and nothing closes the
  descriptor.
- Errors: error types have private fields, built with `new(..)` and read with getters and
  `message()` (`e.msg` is `e.message()`). `IoError::new(kind, msg)` takes the kind first;
  `with_kind` is `new`, `from_os` is `from_os_code`, `code` is `os_code() -> i32 | none`.
  `thread::JOIN_LOST` / `JOIN_TAKEN` are `JoinError::kind()`. `CancellationError`,
  `TimeoutError`, `channel::Empty`, `channel::Closed` are unit structs (`TimeoutError`, not
  `TimeoutError {}`). `FromBytesWithNulError` is an enum. `path::NonUtf8Error`,
  `StripPrefixError`, `SystemTimeError` and `IndexError` are removed. `IoErrorKind` gains
  `WriteZero` and `ResourceBusy` (EBUSY, ETXTBSY, Windows sharing violations).
- One failure channel per function: `FormatSpec::try_parse`, `Float::try_parse` /
  `parse_or_none` and `task::try_id` are removed (`FormatSpec::parse` throws, `try? s.parse()`,
  `task::current_id`). `Int::parse_radix` throws `ParseError`; integer `TryFrom<str>` throws
  `ParseError`. `task::block_on_try` is `task::try_block_on`. `Path::new(s)` takes the text and
  `Path::empty()` is the old `Path::new()`; `Path::strip_prefix` returns `Path | none`.
- Bytes and encoding: byte parameters take any `AsBytes` (`rok::core::convert::AsBytes`);
  `crypto::ByteInput`, `crypto::Bytes` and the `rok::std::io::AsBytes` re-export are removed.
  `hex_encode` / `base64_encode` and their decoders are `encoding::hex::{encode, decode}` and
  `encoding::base64::{encode, decode}` (`base64::URL_SAFE_NO_PAD` for `url_safe: true, pad:
  false`); decoders throw `encoding::DecodeError`. `CString::from_str` is `CString::new(s)`;
  `RawStr::as_bytes_slice` / `as_wide_slice` are `as_bytes` / `as_wide`;
  `RawPath::try_to_path` is `to_path()`; `from_wide` takes `Slice<u16>` (`w[:]`).
- Numbers and chars: `char::to_uppercase` / `to_lowercase` return `str`
  (`to_simple_uppercase` for a `char`; `to_uppercase_str` is removed). `char::from_digit`
  returns `char | none`, `to_digit` returns `u32 | none`. `From<i64> for isize` and
  `From<u64> for usize` are `try_from`. The `signed:` argument of `Int::to/from_*_bytes` is the
  `*_unsigned` form; `from_le_bytes` take any `AsBytes`; new `write_le_bytes` /
  `write_be_bytes` into a `SliceMut<u8>`. `Int::new` is `Int::zero()`. `f64::nan()`,
  `infinity()`, `epsilon()`, `powf` are `NAN`, `INFINITY`, `EPSILON`, `pow`.
- Time: `Duration` and `Instant` are in `rok::core::time` (`rok::core::task::time` and the
  `rok::std::time` re-export are removed). `Duration::sec` / `min` / `from_secs_f64` are
  `secs` / `mins` / `secs_f64`. `SystemTime::duration_since`, `elapsed`, `unix_nanos` no longer
  throw (a later `earlier` gives a negative `Duration`); `unix_seconds` is `unix_secs`.
  `checked_duration_since` / `saturating_duration_since` are removed.
- Collections: `OrderedDict` / `OrderedSet` are `Dict` / `Set` (both keep insertion order).
  `List::set` / `Deque::set` are `xs[i] = v`. `Dict::entries` is `iter`.
  `BinaryHeap::by` is `by_ordering(|a, b| ..)` returning `Ordering`, with a `Send` comparator.
  `List::dot_add_assign` and the other `dot_*_assign` methods are removed; write `xs .+= ys`
  or `xs .= xs .%% ys`.
- Collections and iterators: collections answer iterator methods directly
  (`xs.map(f).sum()`, `xs.max()`), and `collect()` with nothing to pick the collection makes a
  `List`. New `to_list`, `to_set`, `to_dict`, `sorted(reverse:)`, `sorted_by_key`,
  `sorted_by`, `counts`, `most_common`, `group_by`, `enumerate(start:)`, `pairwise`,
  `combinations`, `permutations`. Iterator `min` / `max` need only `PartialOrd`, so float
  iterators have them (NaN propagates). `d[k, default: v]` reads with a default and, as an
  assignment target or receiver, inserts it first; `Dict::get_or` / `get_or_insert`. `Set`
  operators `|`, `&`, `-`, `^`; `List + List`; `List::from_fn`. `zip` and `chain` take any
  `IntoIterator` (`xs.zip(ys)` on two lists); `c in s` tests a `char` in a `str`.
  `array::from_fn::<T, N>(f)` builds a fixed array element by element.
- Arrays: `Array<T, N>` in `rok::core::array` (`Vector`, `Matrix`) is an n-dimensional array
  over one packed buffer with shape and strides: `full`, `zeros`, `ones`, `from_fn`,
  `from_list`, `reshape`, `t`, `permute`, `broadcast_to`, `copy`, `map`, `sum`, views, masks
  and every dot operator. A nested list literal in an `Array` slot builds the array; ragged
  rows are E0143. `ops::Broadcastable` and `ops::Materialize` let a type of your own join fused
  dot expressions.
- Printing and text: print functions take several values with `sep:` / `end:` and fall back
  to `Debug`; `dbg` is in the prelude; f-strings gain `{x=}`, digit grouping (`{n:,}`,
  `{n:_}`) and percent (`{x:.1%}`); every `Display` type has `to_str()`; `str` gains
  `reversed`, `count`, `replacen`, `to_titlecase`. `#[display("..")]` and `#[derive(Error)]`.
- More std: `Mutex::with`, `RwLock::read_with` / `write_with` (the value lent as in
  `ThreadLocal::with`); `fs::lines`, `fs::glob`, `io::input`, `process::run` with
  `ExitStatusError`; `Duration` constructors take any integer that converts to `i64` without
  loss. Numbers: `num::Num`, `num::FloatingPoint`, `total_cmp`, `num::linspace` /
  `arange`, integer `div_floor` / `rem_floor`, `num::Complex<T>` with the `i` suffix
  (`3.0 + 4.0i`). `random`: `Rng`, seedable `Xoshiro256PlusPlus` (`seed`, `split`), `OsRng`
  and one-call forms.
- `Stdio` implements `Close`, and a `Stdio::File` value goes to one command only: giving it to
  two commands, or two streams of one, is E0134 (`Inherit`, `Null`, `Piped` may be reused).
- Hashing: `Hash::hash(self) -> u64` is removed; an impl writes to a hasher,
  `fn hash<H: Hasher>(self, ref h: H)`, and a value hashes through
  `RandomState::new().build_hasher()`. `Dict` / `Set` take a hasher parameter
  `S = RandomState`.
- Crypto: keys are typed: `ed25519::SigningKey` / `VerifyingKey`, `x25519::PrivateKey` /
  `PublicKey`, `ecdsa::SigningKey` / `VerifyingKey`, `mlkem::DecapsulationKey768` /
  `EncapsulationKey768`, `rsa::PublicKey` / `PrivateKey` replace `Ed25519`, `X25519`, `Ecdsa`,
  `MlKem768` and `RsaPublicKey` / `RsaPrivateKey`. `Hkdf` is `crypto::hkdf::extract` /
  `expand`. SHA-3 is `HashAlgorithm::Sha3_256` / `Sha3_512` with `Hmac::mac` / `verify`
  (`Sha3Algorithm`, `mac_sha3` removed). `rok::std::crypto` is `rok::std::random`;
  `random::fill` takes `SliceMut<u8>`.
- Concurrency: `AtomicOrdering` is `LoadOrdering`, `StoreOrdering` or `UpdateOrdering`; a
  wrong ordering is a compile error. `Atomic::wait(.., timeout_ns)` is
  `wait_timeout(expected, Duration)`. `Receiver::try_recv` returns `T | Empty | Closed`
  (`try_recv_status` removed). `task::Blocking<T>` is `Task<T>`; `task::workers` is
  `worker_count`; `task::join` / `race` / `select` take any `IntoIterator`; `task::race` of no
  tasks panics. `par::reduce(iter, f)` returns `T | none`; `par::fold` takes `T: Send`.
  `ThreadLocal::get()` for non-`Copy` `T` is `with(|v| ..)`, `replace` or `take`. New
  `unsafe fn marker::share`, `marker::replace` / `take`, guard `get_clone()`.
  New `task::spawn_blocking_with_arg(arg, f)` hands `arg` to the blocking work like a channel
  send.
- Ranges and small additions: `Range` / `RangeInclusive` / `RangeFrom` `nth` and `step_by` are
  O(1). `bare::alloc::alloc_array` returns null when the size overflows. `Positive<T>` gets
  `TryFrom<T>` and `From<Positive<T>>`; `Result` gets `Clone`, `PartialEq`, `Eq` and `Hash`.
- rokon: `rokon::to_string` / `try_to_string` are `to_str` (throws); `write::flat` is
  `to_str_flat`; `encode` / `decode` are `to_value` / `from_value`; `Value::to_string` /
  `kind` are `to_str` / `kind_name`. `Value` is `#[non_exhaustive]` and its maps are `Dict`.
- Formatting: `{x:g}` is the general format (`1.23e+03`); `g` on a non-number is an error.
  `{x:#?}` pretty-prints. `FormatSpec` fields are getters; `Formatter::pad_zero` is
  `zero_pad`; `to_sink` is `from_sink`. `Formatter::debug_entry` / `debug_pair` /
  `debug_close` are removed.
- Renamed types: iterator types are named after their method (`Chars`, `Split`, `Keys`,
  `slice::Windows`, `ReceiverIter`, `DictIter`, `SetIter`, `HeapIter`, ..), reflection value
  forms end in `_of_val`, `Pinned::as_c_str` is `as_c_ptr`, `Bench::bytes` / `items` are
  `set_bytes` / `set_items` (table in the migration section).
- One path per item: facade modules (`sync::atomic`, `channel::sender`, `future::poll`,
  `stream::adapter`, `ops::broadcast`, ..) are their parent module. `rok::bare::ptr::size_of`
  / `align_of` and `mem::size_of_val` are `rok::bare::mem::size_of` / `align_of` and
  `x.size_of_val()`; volatile access is `rok::bare::volatile`. `reflect::fields_of` /
  `methods_of` are `T::fields()` / `T::methods()`.
- Unsafe: `CStr::from_ptr` and `File::from_fd` are `unsafe fn`;
  `CStr::from_bytes_with_nul_unchecked` is removed. `Pod` is an `unsafe trait`
  (`unsafe impl Pod for T {}`; `#[derive(Pod)]` is unchanged) and no longer covers `bool` and
  `char`. `rok::bare::mlir` is removed; acceleration libraries get the backend interface in
  0.2.2.
- `#[non_exhaustive]`: `IoErrorKind`, `TlsErrorKind`, `CipherSuite`, `AeadAlgorithm`,
  `EcCurve`, `HashAlgorithm`, `rokon::Value`, `process::Stdio`, `FieldInfo`, `MethodInfo`.
- Internals are no longer public: task internals, compiler hooks, `task::readable` /
  `writable` / `IoReady`, literal handlers, `marker::Transfer` / `Memo`, `marker::hand_over`,
  `GenIter::close`, `Bench::new`, `Pinnable::gc_object` (pin with `pin(x)`), `NicheFloat`
  helpers, `GcBuf` (use `rok::bare::mem`), `IoErrorKind::index`, `Waker`'s inherent `clone`,
  `marker::Handoff::new` (move the value into the closure, or use `spawn_blocking_with_arg`).
- `Ord::clamp(low, high)` and `f64::clamp` panic when `low > high`. `Duration::secs_f64` and
  `f64 * Duration` panic on overflow when overflow checks are on.

Same code, new result:

- Every hash value changes: `Dict` / `Set` use keyed SipHash-1-3; iteration order is unchanged.
- `Iterator::max`, `TryIterator::max` and `Ord::max` return the last of equal maxima;
  `binary_search` returns the first of equal elements.
- Float `sum` adds pairwise in a fixed order; results can differ in the last bits.
- `task::race` and `task::timeout` cancel the losers (awaiting one panics); a task cancelled
  before its first poll never runs.
- `ReadDir` ends after a thrown entry error. Opening a directory with `File::open` is
  `IsADirectory` everywhere. Windows `File::create` keeps hidden / system attributes; Windows
  `rename` / `write_atomic` replace a file another handle holds open.
- `derive(Rokon)` skips unknown fields unless `#[rokon(deny_unknown_fields)]`; ROKON rejects
  floats past the f64 range.
- `SocketAddr` `==` compares parts, not the printed text. `TcpListener::bind` sets
  `SO_REUSEADDR`.
- Copies of a `Command` are independent. `BufReader`, `BufWriter`, `LineWriter`, `Cursor`
  copies share their buffer.
- `powi` rounds as repeated multiplication. Float `Debug` prints `1.0` for a whole number.
  `f64::NAN` / `f32::NAN` are the positive quiet NaN on every target (`f64::nan()` had the sign
  bit set on x86), so `total_cmp` orders it after every number there too.
- `SystemTime::elapsed` measures on the monotonic clock for a time from `now()`.
- A resolver failure other than an unknown name is `IoErrorKind::Other`.
- Panic messages and secret `Debug` output (`Secret { len: 32, .. }`) are reworded.
- On Windows, killing a child that has exited but not been waited on is a no-op, as on Unix (it
  was `PermissionDenied`).
- `Command::output()` closes a `Stdio::Piped` stdin right after the spawn, so a child that reads
  to end of input finishes instead of hanging; when reading the output fails, the child is
  killed and reaped before the error is thrown.
- A tls connection answers a KeyUpdate with its next records, not during the read that received
  it; tls sync reads and writes use the non-blocking pump (same results and timeouts).
- A cancelled `File` async read or write no longer touches the caller's buffer; dropping a
  `ReadDirStream` mid-refill waits for the refill. `hex::decode` reports a bad digit before an
  odd length. `process::id` panics on wasm32.

### Lints and diagnostics

- Error codes are per kind (`E0001`..`E0147`), not per area (`E200`); `rok --explain <code>`
  prints a code's page. The codes added last: E0141 assignment to a value that is not a place,
  E0142 method generic parameter named like its impl's, E0143 rows of an array literal differ in
  length, E0144 multi-axis index on a type indexed along one axis, E0145 integer raised to a
  negative power, E0146 named `some` type not defined by its module, E0147 named `some` type used
  as its concrete type. E0038 is only a variable named like a nested function; plain `=` to a
  value moved from E0057 to E0141, and E0057 now covers a lost write to a copy. E0072 covers a
  missing associated type and E0073 an associated type the trait does not declare. JSON
  diagnostics (`--message-format=json`) are schema 2: `code` is the kind's code, `notes` no
  longer repeats `help`, and the `spec` field is gone; test, bench, deps and version records stay
  schema 1. Lint findings always print `E0132`. Diagnostic and help texts no longer cite spec
  sections.
- A `catch` arm that earlier arms already cover warns.
- `unused_result` is on: a dropped value other than `()` / `none` warns; write `_ = expr;` or
  mark the fn `#[discardable]`. Dropping a std iterator, stream, future, `Command` or `Result`
  warns `unused_must_use`. New lint `missing_docs` (allow). `nan_comparison` and
  `self_assignment` deny by default.
- Lints whose warning is now an error or gone are removed: `keyword_idents`,
  `zero_prefixed_literal`, `try_coalesce_precedence`, `trailing_closure_operator`,
  `trailing_closure_return`, `capture_field_write`, `const_item_mutation`,
  `pattern_binds_const`, `ambiguous_int_literal`, `finally_jump`, `float_to_bool`,
  `implicit_core_fn`, `unclosed_resource`, `unclosed_sender`, `using_twice`,
  `discarded_ref_write`, `fstring_g_precision`, `extern_static_unsafe`, `improper_ctypes`,
  `guard_escape`, `thread_local_across_await`, `trait_not_in_scope`, `unqualified_meta`,
  `builtin_attribute_name`, `write_to_copy` (now E0057), `shadowed_loop_var` (redeclaring is
  allowed) and `none_comparison` (`x != none` is the narrowing form). A removed name in
  `#[allow]`, `-A` / `-W` / `-D` or `rok.rokon` is ignored.
- Python habits stop with one error that writes the Rok form: `and` / `or` / `not`,
  `is not`, `a if c else b`, `lambda`, `//`, comprehensions and generator expressions (the
  help builds the iterator chain), `f(end="")`, `elif`, `def`, `except T as e`, `with open(p)
  as f`, `assert c, m`, `for i, x in enumerate(xs)`, `"""`, `from m import x`, Python module
  imports, `!r`, `pass`, builtins called as functions (`len(xs)`, `sorted`, `isinstance`, ..),
  `int(s)`, class-style construction, `sep.join(xs)`, truthiness (`if xs`), `s * n`,
  `Counter(xs)` and `most_common(n)` (help writes `xs.counts()`), and a method without `self`.
- Rust habits likewise: `&x` / `&mut x` / `*x`, `&T` / `&str` / `&[T]` types, `&self` /
  `&mut self`, `mut` bindings, lifetimes, `impl Trait` (write `some Trait`), `dyn Trait` /
  `Box<dyn Trait>`, `Fn(A) -> R`, `move ||` (help lists `[move x]`), `println!` / `format!` /
  `assert_eq!` / `write!` (help writes the f-string), `vec![..]`, `todo!`, `macro_rules!`,
  `pub(crate)`, `crate::` / `super::` paths, `Vec` / `String` / `HashMap` / `Box` / `Rc` /
  `Arc<Mutex<T>>` / `RefCell` and their constructors, `use std::..`, `Ok(x)` / `Err(e)` in a
  `throws` function or pattern, `.unwrap()` / `.expect(..)` / `?` on a throwing call. A meta
  macro named like a Rust macro (`vec!`) is still that macro.
- Mixed-number errors on a variable typed by its literal point at the declaration (`let c: i64 =
  0;`) instead of a cast at the use. `Ord` on a float names the working form (`reduce(|a, b|
  a.max(b))`, `sort_by` with `partial_cmp`, the key form). Ownership errors name what took a value
  (a thread, `par` job or task) and no longer show std's private fields. An unknown literal suffix
  says what to write. No "this `try` covers no throwing call" warning in library code when an impl
  throws less than its trait.
- More errors name what to write: an `Array` index form with no impl (`m[:, none, :]`),
  `n.to_f64()` on an integer (`n as f64`), a bare field in a struct's `#[display("{code}")]`
  (`{self.code}`), an operator on a non-optional sum (`match` / `is`), an assignment as a
  closure body (`|v| v += i`), an untyped list of closures returned as `List<fn(..)>`, and a
  `,` after a struct field (the help keeps the whole type). A missing operator impl is written
  `impl ops::Add for T`. `rok --explain` pages match 0.2 (E0069 names `array::from_fn`, E0134
  owns a narrowed sum's fields one by one).
- `dead_code` counts uses by meta templates and points at the function name; lints no longer
  fire on code another package's derive expanded. `redundant_clone` no longer fires on
  `v.clone().len()` or a non-primitive clone in an f-string hole.

### Tools

- Toolchains: `rok toolchain update` and `install` take a full version (`0.2.0`) or nothing
  (the newest release); a series (`0.2`) or a requirement is an error. `install.sh --version`
  and `install.ps1 -Version` take a full version. Pins in `rok.rokon` stay prefixes
  (`toolchain: "0.2"`). `envs/CURRENT` is no longer read or written. Release candidates are
  GitHub prereleases, installed only by full version. `current` is an ordinary environment
  name.
- `rok publish` publishes (it was a dry run); preview with `--dry-run`. `--commit` is removed;
  `--index <dir>` is `--index-dir <dir>`; `rok yank --vers <v> [name]` is
  `rok yank <name>@<v>`. `rok install` of a package with an executable and a library needs
  `--bin` or `--lib`.
- rokc: `--out` is `-o`; `--emit=ast` / `expanded` are `--emit=source` / `expansions`;
  `rokc rokhir-text` is `rokc inspect --dump`. `--load-mlir-plugin`,
  `--load-mlir-pass-plugin`, `--mlir-pass` and `ROK_MLIR_PLUGIN_PATH` are removed. An internal
  compiler error exits 70 (a program panic stays 101). `--extern` accepts a second name for one
  path. `rokc test --format=rokbench` is a usage error. `rokc inspect` prints
  `rokc version: <version> (<compiler id>)`. `rokc check --check-library` denies
  `missing_docs`.
- Variables: `ROKC_TARGET_DIR`, `ROKC_VERBOSE`, `ROKC_INCREMENTAL`, `ROKC_INCR_VERIFY` are
  the `ROK_*` names; `ROKC_NUM_JOBS` is removed. Programs read only `ROKR_*` (`ROKR_WORKERS`,
  ..), not `ROK_WORKERS`, `ROK_GC_*` or `ROK_HEAP_DUMP`. The unprefixed hand-offs `ROK`,
  `ROK_CLI`, `ROK_ENV`, `ROK_ENV_LIBS` are gone (rok sets `__ROK`, `__ROK_CLI`, ..), and the
  `__ROK*` hand-offs stay out of program environments.
- Targets: `wasm32-wasi`, `wasm32-wasi-threads`, `riscv32imac-unknown-none` are
  `wasm32-wasip2`, `wasm32-wasip1-threads`, `riscv32imac-unknown-none-elf` (also under
  `target/`). `cfg(target_os = "none")` no longer matches wasm32-unknown-unknown; write
  `target_os = "unknown"`.
- Packages: two dependents may call one package by different names; one dependent naming a
  package twice is an error. `rok deps` prints a line per name. Unknown `rok.rokon` fields warn
  instead of failing.
- REPL: `:opt z` is rejected. On Linux, cells map near the host, fixing "a page reference out
  of range" after the first cells. A cell's literal takes its default when the cell ends, so
  type a `let` (`let x: i32 = 1`) that a later cell needs as another type. `:save` writes cells
  as typed. On Windows, a failed cell no longer loses the session when a system thread-pool
  worker exits during the cell.
- rokls: hover on a `let` shows that binding's type, and shows `some` parameters and named
  `some` types as written. Member completion on an existing member (`h.count|`) lists the
  receiver's members, not the member's own methods.
- Builds: the incremental caches are byte-identical between identical clean builds. The
  compiler's hash maps use a fixed-seed hasher, so build work no longer varies between runs
  and the std build takes about 5% fewer instructions.
- Check speed: `rokc check` on a recursive enum walked by a recursive function (the docs
  interpreter example) takes 1.1 s, was 12.9 s; impl overlap and a per-name method index cut
  the std check from 15.8 s to 7.5 s.
- Library stage and incremental cache formats changed; the first build rebuilds.

### Runtime and codegen

- Integer overflow panics in release builds too; opt out with `-C overflow-checks=no` or
  `profile: { "release": { overflow_checks: false } }`.
- `f64` / `f32` `floor`, `ceil`, `round`, `trunc`, `abs`, `copysign`, `min`, `max` and
  `mul_add` compile to single instructions at `-O1` and above.
- `List` resizes bump an internal version (one extra store per `push` / `pop`).
- Fused dot expressions fill a preallocated result by index and vectorize. At `-O2` on 4M
  `f64`, `a .* x .+ b .* 2.0` takes 2.1 ms over `List` (was 12-16 ms) and 2.4 ms over `Array`
  (27 ms one array per operator); `y .= ..` into an existing list takes 1.0 ms.
- Heap objects of 8 KiB or more are 64-byte aligned.
- rokc no longer exports its LLVM / MLIR symbols, and the `rok-mlir-dev` archive is no longer
  published.
- Runtime hooks `rok_hash_seed` and `rok_str_hash` are removed.
- wasm32: the garbage collector takes its blocks from `malloc`, which fixes a rare crash on
  wasm32-wasip1-threads when threads allocate large and small objects at once.

### Registry

- The index is in the 0.2 format (`version` / `tree`, no descriptive fields); 0.1 clients
  cannot read it. The publish body is `Publish { entry, meta }`; yank uses `version`.
- `rok.lock` is format 2; a 0.1 lock is rewritten on the first build.
- A bare requirement always means rokstones.io; `ROK_REGISTRY` and `config.rokon` `registry`
  are ignored (use `sources.rokon` for a mirror). `ROK_REGISTRY_TOKEN` is sent only to
  rokstones.io.
- `credentials.rokon` is gone: logins from 0.1.0 to 0.1.5 run `rok login` again (0.1.6 logins
  in `tokens.rokon` carry over).
- A dependency with `target` is left out of builds for other targets, with the features it
  would enable.

### Migrating from 0.1

1. Build on 0.1.6 first and clear its warnings; each names the 0.2 form.
   `rok toolchain update 0.1.6 && rok build` -> no warnings left.
2. Install 0.2.0 and pin it.
   `rok toolchain update` (or `rok toolchain update 0.2.0`), then in `rok.rokon`
   `toolchain: "0.1"` -> `toolchain: "0.2"`. `rok +0.1 ..` still runs the old toolchain.
3. Registry: log in again if your token is from 0.1.0-0.1.5, and move a mirror.
   `credentials.rokon` -> `rok login`; `ROK_REGISTRY=<url>` -> `sources.rokon`.
4. Bump registry deps to their 0.2 releases; 0.1 releases do not build on 0.2.
   `"json": "0.1"` -> `"json": "0.2"`.
5. Build once to rewrite `rok.lock` to format 2, and commit it.
   `rok build` -> `rok.lock` format 2.
6. Prelude imports: name non-prelude core items by module or `use` them, and qualify variants
   with no expected type.
   `sleep(d)` -> `task::sleep(d)`; `impl Add for V` -> `impl ops::Add for V`;
   `let o = Less;` -> `let o = Ordering::Less;`; a `H: Hasher` bound -> add
   `use rok::core::hash::Hasher;` (method calls need no `use`).
7. io moves: everything is in `rok::core::io`, through the `Read` / `Write` traits.
   `use rok::std::io::Writer;` -> `use rok::core::io::Write;`;
   `w.write_str(s)` -> `try w.write_all(s)`; `for l in f.lines()` ->
   `try for l in io::BufReader::new(f).lines()`; `io::byte_buf(n)` -> `List::filled(n, 0u8)`.
8. fs open functions: one function per way of opening.
   `OpenOptions::new().append(true).create(true).open(p)` -> `File::append(p)`;
   `.write(true).create(true).truncate(true)` -> `File::create(p)`;
   `.read(true).write(true)` -> `File::open(p, ReadWrite)`;
   `.write(true).create_new(true)` -> `File::create_new(p)`.
9. Close resources: files, sockets and tls connections are closed by their owner, and `close`
   throws.
   `let f = try File::open(p); read(f);` -> `using f = try File::open(p) { read(f); }` or
   `try f.close();`; `s.close();` -> `try s.close();`; `s.try_close()` -> `try s.close()`;
   `let b = a; a.read(..)` -> `let b = try a.try_clone(); a.read(..)`.
10. Threads, child pipes and hand-offs: move non-`Sync` values into a spawned closure, take
    child pipes, and mark shared fn values `Sync`.
    `thread::spawn(|| use(xs))` with a non-`Sync` `xs` -> `thread::spawn([move xs] || use(xs))`;
    `child.stdout` -> `child.take_stdout()` (then close it);
    `static F: fn() + Send` shared by threads -> `fn() + Send + Sync`;
    `Handoff::new(v)` -> `[move v]` or `task::spawn_blocking_with_arg(v, f)`.
11. Stream views: an adapter that outlives its stream takes the owning form.
    `return r.lines()` -> `return r.into_lines()`; `listener.incoming()` stored in a struct ->
    `listener.into_incoming()`; `take.into_inner()` -> `r.into_take(n)` .. `.into_inner()`.
12. `try?` / `try!` / `try for` rules.
    `try? f() ?? d` now means `(try? f()) ?? d`; to keep the 0.1 meaning write
    `try? (f() ?? d)`; `try? f()` on `T | none` merging -> `(try? f()) ?? none`;
    `try for x in it { g(x); }` with a throwing `g` -> `try for x in it { try g(x); }`;
    `for b in r.bytes()` -> `try for b in r.bytes()`; `try? open_conn()` whose error holds a
    socket -> `catch` it and close the socket.
13. Error getters: fields are private.
    `e.msg` -> `e.message()`; `e.code` -> `e.os_code()`;
    `IoError::with_kind(k, m)` -> `IoError::new(k, m)`; `IoError { kind: k, .. }` match ->
    `e.kind()`; `match k { NotFound => .. }` on `IoErrorKind` -> add `_ => ..`;
    `TimeoutError {}` -> `TimeoutError`.
14. Renames (no aliases are left):

    | 0.1 | 0.2 |
    |-----|-----|
    | `Reader` / `Writer` | `io::Read` / `io::Write` |
    | `OrderedDict` / `OrderedSet` | `Dict` / `Set` |
    | `AtomicOrdering` | `LoadOrdering` / `StoreOrdering` / `UpdateOrdering` |
    | `Duration::sec` / `min` / `from_secs_f64` | `secs` / `mins` / `secs_f64` |
    | `SystemTime::unix_seconds` | `unix_secs` |
    | `env::exit` / `env::pid` | `process::exit` / `process::id` |
    | `env::parent_pid` | `os::unix::process::parent_id` |
    | `process::kill` / `Child::signal` | `send_signal(Signal)` |
    | `process::signal(n, f)` | `process::on_signal(Signal, f)` |
    | `rok::std::crypto` | `rok::std::random` |
    | `rok::core::task::time`, `rok::std::time::Duration` | `rok::core::time` |
    | `hex_encode` / `base64_decode` | `encoding::hex::encode` / `encoding::base64::decode` |
    | `rokon::to_string` / `encode` / `decode` | `rokon::to_str` / `to_value` / `from_value` |
    | `Dict::entries` | `Dict::iter` |
    | `List::set(i, v)` | `xs[i] = v` |
    | `task::block_on_try` / `task::try_id` / `task::workers` | `try_block_on` / `current_id` / `worker_count` |
    | `task::Blocking<T>` | `Task<T>` |
    | `Metadata::mtime` / `atime` / `ctime` | `modified` / `accessed` / `status_changed` |
    | `Dir::cwd` | `Dir::current` |
    | `BufReader::buffered`, `Cursor::get_ref` | `buffer_len`, `inner` |
    | `Bench::bytes` / `items` | `set_bytes` / `set_items` |
    | `Formatter::to_sink`, `pad_zero` | `from_sink`, `zero_pad` |
    | `CharsIter`, `SplitIter`, `KeysIter`, `ChannelIter`, .. | `Chars`, `Split`, `Keys`, `ReceiverIter`, .. |
    | `type_name_of`, `size_of`, `field_names_of` | `type_name_of_val`, `size_of_val`, `field_names_of_val` |
    | `Pinned::as_c_str` | `as_c_ptr` |
    | `f64::nan()` / `infinity()` / `epsilon()` / `powf` | `NAN` / `INFINITY` / `EPSILON` / `pow` |
    | `char::to_uppercase_str` | `to_uppercase` |
    | `CString::from_str(s)` | `CString::new(s)` |
    | `Path::new()` | `Path::empty()` |
    | `RsaPublicKey` / `RsaPrivateKey` | `rsa::PublicKey` / `rsa::PrivateKey` |
    | `Certificate::der`, `resumed` | `to_der`, `is_resumed` |
    | `enable_pq` | `enable_post_quantum` |
    | `rok::bare::ptr::size_of` | `rok::bare::mem::size_of` |
    | `rokc --out`, `--emit=ast`, `--emit=expanded` | `-o`, `--emit=source`, `--emit=expansions` |
    | `ROKC_*` / `ROK_WORKERS` | `ROK_*` / `ROKR_WORKERS` |
    | `wasm32-wasi`, `wasm32-wasi-threads` | `wasm32-wasip2`, `wasm32-wasip1-threads` |

15. `pub fn` types: a `pub` / `pub(pkg)` fn writes its parameter and return types.
    `pub fn add(a, b) { a + b }` -> `pub fn add(a: i32, b: i32) -> i32 { a + b }`.
16. Hash impls write to a hasher.
    `fn hash(self) -> u64 { .. }` -> `fn hash<H: Hasher>(self, ref h: H) { self.id.hash(ref h); }`.
17. `unsafe trait` and `Pod`; unsafe std functions.
    `impl Pod for P {}` -> `unsafe impl Pod for P {}`;
    `env::set_var(k, v)` -> `unsafe { try env::set_var(k, v) }`.
18. Trait impls define every associated type without a default (E0072) and only the trait's
    own items (E0073).
    `impl Iterator for C { fn next(ref self) -> i32 | none { .. } }` -> add `type Item = i32;`;
    a `type X = ..` the trait does not declare -> remove it, or write the trait parameter in
    the header.
19. Other compile errors to expect, each with its fix: reserved keywords (`gen` -> `r#gen`),
    `0755` -> `0o755`, `x as bool` -> `x != 0`, `n as f64` on an `int` -> `n.to_f64()`,
    `return;` in a trailing closure -> `return@name`, value-captured field write -> `[ref p]`,
    a dropped result -> `_ = expr;`.
20. Writes to copies and temporaries are errors (E0057, E0141).
    `fn bump(self) { self.n += 1; }` -> `fn bump(ref self)`;
    `for p in ps { p.hits += 1; }` -> `for ref p in ps { p.hits += 1; }`;
    `let e = xs[i]; e.x = 1;` -> `xs[i].x = 1;`;
    `d.entry(k).or_insert(0) += 1` -> `d[k, default: 0] += 1`; `f().x += 1` ->
    `let v = f(); v.x += 1;`;
    `xs.for_each(|a| { a.total = 2; })` -> `for ref a in xs { a.total = 2; }`;
    `m.with(|a| { a.total += 5; })` (it changed a copy) -> return `a` from the closure and use
    the result, or write through a guard; `g.get().total += 7` ->
    `g.update(|v| { v.total += 7; v })`.
21. Integer literals default to `i64`: type the literal where `i32` mattered.
    `let n = 5; store_any(n)` read back as `i32` -> `let n: i32 = 5;` or `5i32`;
    `x is i32` on a literal-typed `x` -> `x is i64`.
22. Optionals: write the narrowing comparison.
    `!(x is none)` -> `x != none`; `if let x = opt` binds the value without `none`, so drop a
    following `x ?? ..` or `x is none` check.
23. Index and broadcast traits.
    `impl IndexSet<I> for G { type Value = V; fn index_set(..) }` ->
    `impl IndexSet<I, V> for G { fn index_set(..) }`;
    `xs.dot_add_assign(ys)` -> `xs .+= ys`; `r.dot_modulo_assign(k)` -> `r .= r .%% k`;
    `m[1:, 0]` on a `List` -> `m[1:][0]` (or an `Array`); a user operator `**` -> another
    name.
24. Ownership through every flow.
    `(try File::open(p)).read_to_str()` -> `using f = try File::open(p); f.read_to_str()`;
    one `Stdio::File(f)` for two commands -> a `try f.try_clone()` for the second;
    `impl<K> W<K> { fn f<K>(..) }` -> `fn f<K2>(..)`.
25. Repeat literals clone their value into each element.
    `[Bag { .. }; 3]` of a non-`Clone` `Bag` -> `#[derive(Clone)]` on `Bag`, or
    `array::from_fn::<Bag, 3>(|_| Bag { .. })`; `[try File::open(p); 2]` -> open each file;
    `fn f<T>(x: T) -> [T; 4] { [x; 4] }` -> `fn f<T: Clone>(..)`; code that wrote through one
    element of `[xs; n]` to change them all -> keep one list and index it.
26. Drop removed lint names from `rok.rokon` and `#[allow]` (they are ignored):
    `write_to_copy`, `shadowed_loop_var`, `none_comparison` and the 0.1.6 lints listed above.
27. Check what changed silently: operator precedence (`1 + 2 << 3` is now 24), `none` sorting
    first, hash values, `max` on ties, overflow panics in release, `try? f() ?? d`, literals as
    `i64`, `for await` / `try for` advancing the variable, `#[derive(Default)]` field defaults,
    pairwise float `sum`, `[v; N]` cloning its value per element, a trait default method's
    `self.m(..)` calling the impl's `m`, `f64::NAN` sorting last on x86. Add parentheses where
    you meant the 0.1 grouping: `1 + (2 << 3)`.

## 0.1.6 (2026-11-01)

This release gets code ready for 0.2.0: what 0.2.0 removes or changes now warns, and the
replacement is already here. Code that built with 0.1.5 still builds; with `-D warnings` the
new lints below can now stop a build.

- Lints: `warnings` is every warning, as in rustc, so `-D warnings` also covers the lints
  added in this release. Compiler warnings tied to a lint obey `-A`, `-W` and `-D`; a level
  note says where it came from (`rok.rokon` or the command line); an unknown lint suggests
  the closest name; `rok lint --list` shows the release that added each lint.
- New warnings: `keyword_idents` (the six words 0.2.0 reserves; write `r#name`),
  `zero_prefixed_literal`, `try_coalesce_precedence`, `trailing_closure_operator`,
  `trailing_closure_return`, `fstring_g_precision`, `literal_spread_mismatch`,
  `capture_field_write`, `const_item_mutation`, `pattern_binds_const`, `discarded_ref_write`,
  `ambiguous_int_literal`, `trait_not_in_scope`, `finally_jump`, `float_to_bool`,
  `implicit_core_fn`, `unclosed_sender`, `using_twice`, `guard_escape`,
  `thread_local_across_await`, `extern_static_unsafe`, `improper_ctypes`, `deprecated_safe`
  and `unqualified_meta`. `unclosed_resource` follows every exit path. Fixes are offered
  where the edit is concrete.
- Raw identifiers: `r#name` names an item after a keyword.
- `#[deprecated(since, note)]` reaches methods, fields, types, consts, statics, generics,
  dependencies and std; `#[deprecated_safe("..")]` marks a safe function that becomes
  `unsafe` in 0.2.0. The language server tags deprecated items.
- Meta: another package's macro, derive or attribute can be named in full (`dep::name!`,
  `#[dep::attr]`, `#[derive(dep::Name)]`, `use dep::name!`); meta definitions are per
  package, and an ambiguous bare name lists the qualified forms.
- Std: every name that 0.2.0 replaces has its replacement, and the old name warns with it.
  New are `env::args_os` / `vars_os` (bytes kept; `args` / `vars` are lossy), getters for
  error and address fields, `process::exit`, a `Signal` API, `FileType`, `created` /
  `status_changed`, `try_close`, `Reader::read_into`, `Writer::write_bytes`,
  `strip_prefix_or_none`, `rok::std::os::unix::fs` with Rust's `PermissionsExt` /
  `OpenOptionsExt`, `env::try_set_var`, `Duration::secs` / `mins`, `par::fold`,
  `sort_by_key`, `parse_or_none`, `FormatSpec` getters and `zero_pad`, `TryFrom` for every
  width, atomic `wait_timeout` and `try_recv_status`. `CStr::from_ptr`, `File::from_fd`,
  `set_var` and `remove_var` are marked `deprecated_safe`.
- Toolchains: a pin can be a prefix (`"0.1"`), a requirement (`"^0.1.5"`) or a directory
  name, and a script's `//! rokon` block can pin one; `rok new` and `rok init` write
  `toolchain: "<series>"`. A bare `rok toolchain update` installs the newest release, as
  rustup does, and says so when that moves to a new series, keeping the old toolchain;
  `rok toolchain update 0.1` stays in the series. `rok toolchain install <version>` downloads
  without `--from`, and `install.sh` / `install.ps1` accept a series.
- One toolchain per run: a pinned project hands the command to the pinned toolchain's own
  `rok`; the proxies come from the newest installed toolchain; shared `ROK_HOME` files keep
  fields an older or newer `rok` wrote.
- CLI: `rok login` / `logout --registry`, `logout --all` (tokens per index in
  `tokens.rokon`), `rok publish --dry-run` / `--registry` / `--index-dir`, `rok yank
  name@version`, `rok install --bin`, `rokc --emit=source` / `expansions`, `rokc inspect
  --dump`. A bare `rok publish` warns about what 0.2.0 changes. Tool variables are `ROK_*`
  (`ROK_TEST_THREADS`, `ROK_INCREMENTAL_VERIFY`, ...) and runtime variables `ROKR_*`; the old
  names still work and warn once. `wasm32-wasip2`, `wasm32-wasip1-threads` and
  `riscv32imac-unknown-none-elf` are accepted as target names.
- Machine output: JSON diagnostics add `line_end`, `col_end`, `help`, `lint` and a `schema`
  number; `rok test` / `rok bench --format json` write case and summary records;
  `rok deps --format json`; `rok --version -v --format json`.
- Package and environment names that a supported OS rejects (Windows device names, reserved
  characters, more than 64 characters) are errors in `rok new`, `add` and `publish`, and a
  warning in an existing manifest. Environment names are checked everywhere.
- Fixes: cleanup in `finally` and `using` finishes after a cancel; a `for` closes the
  generator it made when it leaves early; `File`, socket and `Child` copies share one handle
  safely; C threads can call into Rok; `Secret` moves between tasks; a `ref` argument of a
  narrowed variable writes back; non-UTF-8 arguments no longer panic; the lone-file cache is
  private to the user; extension operators led by an ASCII operator lex by longest match;
  out-of-range literals and mixed-number errors name the wider type. `Dict` hashing is
  seeded per process; iteration order is unchanged.

## 0.1.5 (2026-10-31)

- Colour: `rok`, `rokc` and the installers colour status verbs, errors, warnings, notes and
  help; `--color`, `NO_COLOR`, `CLICOLOR_FORCE` and `TERM=dumb` decide, and piped output stays
  plain. `rok` takes `--color` and passes it on.
- `rok help <command>` and `rok <command> --help` show that command's usage, examples and
  options; `rok --help` is a short list. `-q`, `-v` and `--color` work after the command, and
  unknown commands, options and stones suggest the closest name.
- Errors say what to write: the method you meant (`len` for `length`, `xs.iter().map` on a
  `List`), how to get a value out of `T | none`, `==` for `=` in a condition, the signature in
  an argument-count error, the `match` arms to add. A type mismatch underlines the value and
  shows the conversion; an unused variable underlines its name. Messages use short type names
  and leave spec citations to the JSON output, which also lists `fixes`.
- Outside a package `rok fmt` needs a path; inside one it formats the package from any
  directory and skips `target/`, hidden directories, `node_modules/` and `.gitignore`d paths.
  `rok update`, `rok env list`, a missing script and a bare `rok jupyter` say what to run.
- `rok toolchain update` and `default` warn when another `rok` comes first on PATH;
  `rok --version -v` shows the binary, toolchain, `rokc` and host.
- Build output: a failed build ends with `could not build <name> (N errors, M warnings)`, the
  first std build is announced, `rok run hi.rok` shows `Running hi.rok`, and `rok build hi.rok`
  writes `hi` next to the script. `rok new` prints the next command, `rok add` the version
  added; `rok doc --open` opens the docs and `rok doc --std` works outside a package.
- `rok bench` prints its human output when piped too (`--format=rokbench` for the protocol)
  and warns about a bench that never calls `b.iter`.
- Index and key panics name the value: `index 10 out of bounds for length 3`, and a missing
  `Dict` key says to use `d.get(k)`.
- The REPL draws no `%` marker after output without a newline; `:load` of a `:save` file
  replays its cells; Jupyter numbers `<repl cell N>` by its execution count.
- The language server completes locals, `use` paths and library members, reaches the
  standard library for hover and definition, and adds quick fixes, signature help and
  rename checks; `rokls` started by hand says it is for an editor.
- An out-of-range ROKON float names the f64 range; corpus cases run with a time limit.

## 0.1.4 (2026-10-29)

- The REPL starts with the toolchain line and a hint (`-q` drops them); prompts are
  `>>>` and `...`, and a prompt always starts on a fresh line.
- A cell ends by tokens: char literals, comments and an item header without its body no
  longer split or join cells; a cell's last `let`, assignment, `use`, `const`, `static` or
  `type` needs no `;`.
- Values show through `Display`, else `Debug`, else their type; `it` holds the last value;
  a cell with a top-level `await` keeps its bindings.
- Panics and throws in earlier cells point at `<repl cell N>`; a redefinition that breaks
  an earlier item names it and the fix; a cell may hold several items.
- REPL commands: `exit`, `:exit`, `:doc`, `:time`, `:vars`, `:load`, `:reload`, `:edit`,
  `:save`, `:opt`; an unknown name the package defines hints at `rok repl .`.
- Line editing: syntax colours, completion of core, imported and member names, bracketed
  paste as one cell, whole-cell history with `C-r` and prefix search, wide characters.
- Cells read the terminal; Jupyter streams output while a cell runs, asks for input with
  `input_request`, and reports highlighting, indent hints, logos and help links.
- The last statement before `}` may omit its `;`.
- install.sh sets up PATH like rustup (`$ROK_HOME/env`, fish `conf.d`) unless
  `--no-modify-path`, and installs shell completion; `rok completions <shell>` and
  `rokc completions <shell>` print it, and `rok toolchain update` / `default` refresh it.
- Concurrent runs of one script no longer race on the build cache; a written `ref`
  parameter is no longer reported unused.

## 0.1.3 (2026-10-29)

- `rok::core::encoding`: `hex_encode`, `hex_decode`, `base64_encode`, `base64_decode`;
  standard or url-safe, padded or not; a decode error names the byte and the fix.
- `str` gains `splitn`, `rsplit`, `rsplitn`, `match_indices`, `trim_matches`,
  `trim_start_matches` and `trim_end_matches`, all sharing the source's bytes.
- Iterator adapters `take_while`, `skip_while`, `scan`, `inspect`, `cycle`, `chunks`,
  `windows`; consumers `try_fold`, `try_for_each`, `partition`, `unzip`.
- `List` gains `contains`, `retain`, `dedup`, `fill`, `swap`, `split_at`, `binary_search`,
  `windows`, `chunks`; `Slice` gains `contains`, `split_at`, `windows`, `chunks`; `retain` and
  `extend` on `Dict`, `Set`, `OrderedDict`, `OrderedSet`; arrays gain `iter`, `first`, `last`,
  `contains`, `map`, `fill`.
- Type parameters inside an associated-type binding are inferred, also from a `where`:
  `Iterator<Item = (A, B)>` binds `A` and `B`.
- `Writer::write_display` writes a value through `Display` in chunks, with no intermediate
  `str`.
- `Inspect` gains `fields`, `variant_names`, `variant_name`, `field_names_of`, with `FieldInfo`
  and `fields_of`; `get_field` reads an enum value's payload; a missing derive says to add
  `#[derive(Inspect)]`.
- `int` gains `count_ones`, `trailing_zeros`, `bit`, `set_bit`, `is_power_of_two`,
  `next_power_of_two`, `ilog2`, `checked_ilog2` and little- and big-endian bytes; `f64` and
  `f32` gain `lerp`, `next_after`, `next_up`, `next_down`.
- `i128` and `u128` are an error that names `int` or the 64-bit type to write.
- `#[doc(hidden)]` keeps an item out of completion and `rokc doc`; the compiler hooks carry it.
- A closure that captures a `ref` parameter writes back to the caller; `[ref self]` captures
  in a `ref self` method.
- `Self::f()` in a trait default works for a private type.
- A binding in a meta template renames only within its own scope.

## 0.1.2 (2026-10-28)

- Dict spread: a defaulted parameter the dict's value type cannot fill takes its default;
  a key naming it panics.
- Clearer errors: an `if` in a typed slot names the branch that does not fit; a bare `break`
  mixed with `break <value>`, misordered fn modifiers and `mod x;` without a folder say what
  to write.
- One item imported through a re-export and directly is no longer a conflict; a real clash
  suggests an alias.
- An unimported type in an annotation is one error, without its cascade.
- A static initialized from itself through other statics is a compile error.
- A meta attribute named like a built-in (`bench`, `path`, ...) warns; it becomes an error
  in 0.2.0.
- Collections: `BinaryHeap` gains `IntoIterator`, `Debug`, `Clone`, `Default`, `FromIterator`
  and an in-place `retain`; `clear` on `Dict`, `Set`, `OrderedSet`; `+=` on `Set`,
  `OrderedSet`, `Deque`, `OrderedDict`; `Deque` gains `Clone`, `PartialEq`, `Eq`,
  `FromIterator`; `OrderedDict` and `OrderedSet` gain `Eq` and `Hash`; `Dict::from` and
  `Set::from` through `From`.
- `Iterator::exact_len`; `count()` on exact-size adapters no longer runs the pipeline.
- `ManyPtr::cast`; `ClientConfig::try_new` throws when the system roots cannot be read.
- Package metadata in `rok.rokon`: `authors`, `homepage`, `documentation`, `readme`,
  `keywords`, `publish`; `publish: false` refuses `rok publish`.
- An unreadable `config.rokon` (permissions, not UTF-8) is an error, no longer read as empty
  or overwritten by `rok toolchain default`.
- The install scripts and `rok toolchain update` print rok's status lines; `-q` and `-v`.
- The language server refreshes documents after a dependency's buffer changes.
- The build cache rebuilds when an `include_str!` input disappears.
- Install through Nix: `nix profile install github:rok-lang/rok`.
- A read-only toolchain (Nix, system packages) keeps its build cache in the user's cache
  directory.

## 0.1.1 (2026-10-27)

- `print` and `println` accept any `Display` value, and default to printing a blank line.
- The REPL prints a result through `Display`; a trailing `?` prints it through `Debug`.
- Refreshing an LLVM prefix no longer rewrites the MLIR plugin's lib directory.

## 0.1.0 (2026-10-25)

First release.

- Compiler `rokc` (AOT via MLIR / LLVM 22, REPL, Jupyter kernel, language server, `fmt`, `lint`, `doc`, `test`,
  `bench`, `bindgen`), package / build front-end `rok` (`rok.rokon` / `rok.lock`, path / git dependencies, stones,
  environments, toolchains), runtime `librokr.a`, standard library `core` / `std` / `bare`.
- Install: rustup-style layout (`~/.rok/bin` proxies, `~/.rok/toolchains/<version>-<triple>/`), `install.sh` /
  `install.ps1`, and `rok-<version>-<triple>.tar.gz` / `.tar.xz` (Windows `.zip`) archives built by `dist.sh`
  (Linux: `dist-linux.sh`, in a Debian 11 container; they need glibc 2.34 or later).
- `rok` / `rokc` / `rokls --version` print the version, commit, and date.
- License `MIT OR Apache-2.0`.

### Language

- Using a value after it was handed off (`Mutex::new`, `send`, `task::spawn`, `Handoff::new`, through calls)
  is a compile error; the check follows conditions, call outcomes, a collection's elements, copies and aliased
  arguments, and each error says what to write instead.
- A hand-off gives the receiver its own copy of every collection the value reaches (a list and its views keep
  one buffer; values built at compile time are not copied), so a route the check misses cannot race. A
  coroutine future gets a frame of its own; one that has started cannot be handed over.
- A lock guard and its value stay inside their `using` block: returning, storing, yielding or capturing them
  (closures and coroutines included) is an error.
- `Frozen<T>` shares a value read-only between threads without `unsafe`; `get` gives each reader a copy and
  keeps `Sync` parts shared.
- `par` runs items that reach one collection or future one at a time, with a warning naming the call.
- `Weak<T>` is not `Send`; `any Tr` crosses threads only when its bound has `Send + Sync`.
- Every `await` of a `Task` that was handed on gets its own copy of the result.
- Duplicate items point at both definitions; a tuple or unit struct's constructor shares the value namespace
  with functions, consts and statics. A named `use` and a local item of the same name are an error; a glob
  import gives way.
- Names from another module need a `use`; "cannot find" errors suggest the `use` to add or the missing `pub`.
- Generic argument counts are checked; a bare generic type says it needs arguments.
- Generic unions; impls on generic type aliases; an alias's `where` is checked in declaration types.
- Block-local items (`struct`, `enum`, `impl`, `use`, `const` in a block), including module glob `use`.
- Statics and consts of `fn` type are callable. `Sync` rules for statics, `RwLock` and `[ref]` cells.
- `#[cold]` keeps a function out of line.
- A `try!` panic shows the error and where it was raised.

### Tools

- Scripts: `rok <file>`, shebang lines, dependencies from the file's `//! rokon` block (`rok add <lib> <file>`),
  `mod helper;` across script files; `import` gets an error naming `mod` / `use`.
- Packages with both `src/lib.rok` and `src/main.rok`; `rok install <stone>` installs executables, keeps one
  installed at the same version and rebuilds with `--force`; `rok install <lib>` serves the REPL and Jupyter.
- The package name `rok` is reserved for the standard library; names code can't write as a path (`pkg`,
  `sup`, keywords) are refused with a suggestion. Every `rok new` template is tested end to end.
- Temporary files and directories are removed on exit, error and panic; leftovers of killed processes are swept,
  and a REPL host ends with its `rokc`.
- Build profiles: debug info by default; release folds identical code and strips.
- Lints in every compiling command; hints for habits from other languages.

### Platforms

- Verified natively: macOS arm64 (macOS 26 and 27 SDKs), Linux x86_64 and aarch64 (Ubuntu 26.04), Windows
  x86_64 (Windows 11). Windows arm64 links but has not been run.
- macOS 27: rokc's linker reads the new SDK's stubs (`arm64e.x1`).
- Windows: libclang (`rokc bindgen`), a PGO profile for the clang-cl LLVM build, `dist.sh` and
  `install.ps1 -NoModifyPath`; CI builds the prefix with clang-cl, runs `bindgen`, and a nightly job installs
  the distribution and builds a package with it.
- `riscv64gc-unknown-linux-gnu`, verified on a riscv64 kernel.

### Performance

- Bounds checks of counted loops over lists move in front of the loop; short loops and loop nests run without
  safepoint polls; a fixed-index element stays in a register.
- Calls between rok functions pass the thread block as an argument; the GC frame links only on paths that reach
  a safepoint; locals that only a loop's poll sees are rooted on its yield path only.
- `task::spawn` of a future made at the call, or kept in a local nothing else names, skips the hand-off.
- PGO profiles retrained for macOS 27 and Linux: rokc's own `-O2` builds are 5-10% faster than without.
- rok / C instructions on the bench suite: nbody 1.46x, fannkuch 1.64x, spectral_norm 1.11x, throw_parse 1.85x;
  json_tokenizer 0.69x, word_count 0.89x, binary_trees 0.55x.
