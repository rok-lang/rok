// `ROK_COMMIT_INFO` (` (<commit> <date>)` from git, else
// `ROK_COMMIT_HASH` / `ROK_COMMIT_DATE`, else empty) and `ROK_HOST` for
// `--version`. `rok` includes this file as its build script too.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !s.trim().is_empty()).then(|| s.trim().to_owned())
}

fn main() {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    println!("cargo:rerun-if-env-changed=ROK_COMMIT_HASH");
    println!("cargo:rerun-if-env-changed=ROK_COMMIT_DATE");
    println!("cargo:rerun-if-changed=build.rs");
    // Rerun when HEAD or the branch it names moves (watching a missing
    // file would rerun every build).
    let branch = git(&["symbolic-ref", "-q", "HEAD"]);
    for p in ["HEAD", "packed-refs"].into_iter().chain(branch.as_deref()) {
        if let Some(f) = git(&["rev-parse", "--git-path", p]) {
            if std::path::Path::new(&f).exists() {
                println!("cargo:rerun-if-changed={f}");
            }
        }
    }
    let hash = env("ROK_COMMIT_HASH").or_else(|| git(&["rev-parse", "--short=9", "HEAD"]));
    let date = env("ROK_COMMIT_DATE").or_else(|| git(&["log", "-1", "--format=%cs"]));
    let info = match (hash, date) {
        (Some(h), Some(d)) => format!(" ({h} {d})"),
        (Some(h), None) => format!(" ({h})"),
        _ => String::new(),
    };
    println!("cargo:rustc-env=ROK_COMMIT_INFO={info}");
    // musl threads get 128 KiB of stack; LLVM's pass pipeline on worker
    // threads recurses deeper. PT_GNU_STACK sets musl's default.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("musl") {
        println!("cargo:rustc-link-arg=-Wl,-z,stack-size=8388608");
    }
    println!(
        "cargo:rustc-env=ROK_HOST={}",
        std::env::var("TARGET").unwrap_or_default()
    );
    // The prefix's lld for identical code folding, as link.exe does under
    // /OPT:ICF: MLIR/LLVM template instances fold by the thousand. No
    // prefix: no LLVM. A static (musl) rokc keeps the system linker.
    let env_is = |k: &str, v: &str| std::env::var(k).as_deref() == Ok(v);
    if env_is("CARGO_PKG_NAME", "rokc") {
        println!("cargo:rerun-if-env-changed=DEP_ROKC_LLVM_PREFIX");
        let gnu_or_apple =
            !env_is("CARGO_CFG_TARGET_ENV", "msvc") && !env_is("CARGO_CFG_TARGET_ENV", "musl");
        if let (Ok(prefix), true) = (std::env::var("DEP_ROKC_LLVM_PREFIX"), gnu_or_apple) {
            let link = |flag: String| println!("cargo:rustc-link-arg-bin=rokc={flag}");
            let bin = format!("{prefix}/bin");
            if env_is("CARGO_CFG_TARGET_VENDOR", "apple") {
                link(format!("-fuse-ld={bin}/ld64.lld"));
            } else {
                link(format!("-B{bin}"));
                link("-fuse-ld=lld".into());
            }
            link("-Wl,--icf=safe".into());
        }
    }
}
