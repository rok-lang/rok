//! `rok`: see the library. Installed as `$ROK_HOME/bin/{rok,rokc,rokls}`
//! it is the toolchain proxy.

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        rokc_diagnostic::end_if_output_closed(info);
        default_hook(info);
        eprintln!("rok: internal error");
        std::process::exit(101);
    }));
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if let Some(code) = rok::proxy(&args) {
        std::process::exit(code);
    }
    std::process::exit(rok::main_with_os(args));
}
