//! `rokls`: the rok language server on stdio, the same as `rok lsp`
//! (with the project's dependencies).

fn main() {
    let args: Result<Vec<String>, String> = std::env::args_os()
        .skip(1)
        .enumerate()
        .map(|(i, a)| rokc_core::cli::utf8_arg(i + 1, a))
        .collect();
    let args = args.unwrap_or_else(|e| {
        eprintln!("rokls: {e}\nrun `rokls --help` for usage");
        std::process::exit(2);
    });
    match args.first().map(String::as_str) {
        Some("-V" | "--version") => println!("rokls {}", rok::VERSION_LINE),
        Some("-h" | "--help") => print!(
            "usage: rokls [rok options]\n\nThe rok language server on stdin / stdout (LSP), \
             with the dependencies of the\nproject it starts in; the same as `rok lsp`. Editors \
             start it.\n"
        ),
        _ => {
            let args = std::iter::once("lsp".to_owned()).chain(args);
            std::process::exit(rok::main_with(args.collect()));
        }
    }
}
