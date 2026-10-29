//! `rokc test` / `rokc bench` harness (spec 901, 6.2.5): the entry point is
//! replaced by a generated `main` running the `#[test]` named by
//! `argv[1]`; the runner spawns one process per test, so a panic (101) or
//! escaping error (1) fails only that test. `bench_main` times rounds of
//! calls instead.

use rokc_ast::{Item, ThrowsKind};
use rokc_diagnostic::{ErrorKind, RokDiagnostic};
use rokc_lexer::Lexer;
use rokc_parser::{ExtOps, parse_program_ext};

/// Whether `attrs` carries `#[test]`.
pub fn is_test(attrs: &[rokc_ast::Attribute]) -> bool {
    has_attr(attrs, "test")
}

/// Whether `attrs` carries `#[name]` (one path segment).
pub fn has_attr(attrs: &[rokc_ast::Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path.len() == 1 && a.path[0] == name)
}

/// Turn the user items into a test (`bench`: benchmark) program; the
/// `::`-qualified names in source order.
pub fn install(
    items: &mut Vec<(String, Item)>,
    ext: &ExtOps,
    diags: &mut Vec<RokDiagnostic>,
    bench: bool,
) -> Vec<String> {
    let attr = if bench { "bench" } else { "test" };
    let mut tests = Vec::new();
    for (module, item) in items.iter() {
        let Item::Fn(f) = item else { continue };
        if !has_attr(&f.attrs, attr) {
            continue;
        }
        // A bench takes one `Bench` handle; a test takes nothing.
        let params_ok = if bench {
            f.params.len() == 1
        } else {
            f.params.is_empty()
        };
        if !params_ok || !f.generics.params.is_empty() {
            let want = if bench {
                "one `Bench` parameter and no generics"
            } else {
                "no parameters or generics"
            };
            diags.push(RokDiagnostic::error(
                ErrorKind::Type,
                f.span,
                format!("`#[{attr}] fn {}` must take {want}", f.name),
            ));
            continue;
        }
        let name = if module.is_empty() {
            f.name.clone()
        } else {
            format!("{module}::{}", f.name)
        };
        tests.push((name, !matches!(f.throws, ThrowsKind::None)));
    }
    // The package's own entry point is replaced (top-level statements
    // were already dropped by the caller).
    items.retain(|(m, i)| !(m.is_empty() && matches!(i, Item::Fn(f) if f.name == "main")));

    let src = if bench {
        bench_main(&tests)
    } else {
        test_main(&tests)
    };
    let (tokens, mut d) = Lexer::new(&src, "<test harness>").lex_all();
    diags.append(&mut d);
    let (program, mut d) = parse_program_ext(&tokens, "<test harness>", ext);
    diags.append(&mut d);
    items.extend(program.items.into_iter().map(|i| (String::new(), i)));
    tests.into_iter().map(|(n, _)| n).collect()
}

/// `main` running the test named by `argv[1]` once.
fn test_main(tests: &[(String, bool)]) -> String {
    let throws = tests.iter().any(|(_, t)| *t);
    let mut src = String::from("fn main() ");
    if throws {
        src.push_str("throws ");
    }
    src.push_str("{\n    let name = rok::std::env::args()[1];\n");
    for (name, throws) in tests {
        let call = if *throws { "try " } else { "" };
        src.push_str(&format!(
            "    if name == \"{name}\" {{ {call}{name}(); return; }}\n"
        ));
    }
    src.push_str("    panic(\"rokc test: unknown test name\");\n}\n");
    src
}

/// `rokc bench`'s `main`, `argv` = `name iters samples tty width` (`tty`
/// 1: the human line, 0: the `#rokbench` one): builds the `Bench` handle
/// and hands it to the named benchmark, which measures and reports itself
/// (`rok::core::bench`). Exit 3: it never called `b.iter` (the runner
/// says so).
fn bench_main(benches: &[(String, bool)]) -> String {
    let mut src = String::from(if benches.is_empty() {
        "fn main() {\n"
    } else {
        "fn main() -> i32 {\n"
    });
    src.push_str(
        "\x20   let argv = rok::std::env::args();\n\
         \x20   let name = argv[1];\n\
         \x20   let iters: i64 = try! argv[2].parse();\n\
         \x20   let samples: i64 = try! argv[3].parse();\n\
         \x20   let tty = argv[4] == \"1\";\n\
         \x20   let width: usize = try! argv[5].parse();\n\
         \x20   let b = rok::core::bench::Bench::new(name, iters, samples, tty, width);\n",
    );
    for (k, (name, throws)) in benches.iter().enumerate() {
        let call = if *throws { "try! " } else { "" };
        let kw = if k == 0 { "if" } else { "else if" };
        src.push_str(&format!(
            "    {kw} name == \"{name}\" {{ {call}{name}(b); }}\n"
        ));
    }
    if !benches.is_empty() {
        src.push_str("    else ");
    }
    src.push_str("{ panic(\"rokc bench: unknown bench name\"); }\n");
    if !benches.is_empty() {
        src.push_str("    if rok::core::bench::measured() { 0 } else { 3 }\n");
    }
    src.push_str("}\n");
    src
}
