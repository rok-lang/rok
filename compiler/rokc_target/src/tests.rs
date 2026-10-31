use crate::{BUILTIN, CAbi, HOST, Libc, Linker, RENAMED, TargetSpec};

fn t(name: &str) -> TargetSpec {
    TargetSpec::builtin(name).unwrap()
}

/// Every built-in spec prints as ROKON and reads back the same, under
/// its own name; its hash is stable and its own.
#[test]
fn builtins_round_trip() {
    let mut hashes = Vec::new();
    for name in BUILTIN {
        let spec = t(name);
        assert_eq!(spec.name, *name);
        let text = spec.to_rokon();
        let back = TargetSpec::from_rokon(&text, "t.rokon", name).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(back, spec, "{name}");
        assert_eq!(back.to_rokon(), text, "{name}");
        assert_eq!(back.hash(), spec.hash(), "{name}");
        hashes.push(spec.hash());
    }
    hashes.sort_unstable();
    hashes.dedup();
    assert_eq!(hashes.len(), BUILTIN.len());
    assert!(TargetSpec::builtin("x86_64-unknown-linux-gnux32").is_none());
    assert!(BUILTIN.contains(&HOST), "{HOST}");
}

/// A copy under another name differs only by name (and hash).
#[test]
fn renamed_copy() {
    let a = t("x86_64-unknown-linux-gnu");
    let b = TargetSpec::from_rokon(&a.to_rokon(), "my.rokon", "my").unwrap();
    assert_eq!(b.name, "my");
    assert_ne!(a.hash(), b.hash());
    assert_eq!(
        TargetSpec {
            name: a.name.clone(),
            ..b
        },
        a
    );
}

#[test]
fn custom_spec_errors() {
    let text = t("wasm32-wasi").to_rokon();
    let unknown = text.replacen("TargetSpec {", "TargetSpec {\n    colour: \"red\",", 1);
    let e = TargetSpec::from_rokon(&unknown, "x.rokon", "x").unwrap_err();
    assert!(
        e.contains("unknown field `colour`") && e.contains("x.rokon:2:5"),
        "{e}"
    );
    let missing = text.replacen("    threads: false,\n", "", 1);
    let e = TargetSpec::from_rokon(&missing, "x.rokon", "x").unwrap_err();
    assert!(e.contains("missing field `threads`"), "{e}");
    let bad = text.replacen("Linker::WasmComponentLd", "Linker::Gold", 1);
    let e = TargetSpec::from_rokon(&bad, "x.rokon", "x").unwrap_err();
    assert!(e.contains("expected one of Linker::Ld64"), "{e}");
    let bad = text.replacen("\"little\"", "\"middle\"", 1);
    let e = TargetSpec::from_rokon(&bad, "x.rokon", "x").unwrap_err();
    assert!(e.contains("`endian` must be"), "{e}");
    let e = TargetSpec::resolve(Some("nope-nope")).unwrap_err();
    assert!(e.contains("unknown target `nope-nope`"), "{e}");
    let e = TargetSpec::resolve(Some("/no/such/t.rokon")).unwrap_err();
    assert!(e.contains("cannot read target /no/such/t.rokon"), "{e}");
}

#[test]
fn tier1_properties() {
    let i686 = t("i686-unknown-linux-gnu");
    assert_eq!(
        (
            i686.arch.as_str(),
            i686.pointer_width,
            i686.wide_align,
            i686.c_abi
        ),
        ("x86", 32, 4, CAbi::X86SysV)
    );
    let w = t("i686-pc-windows-msvc");
    assert_eq!(
        (w.wide_align, w.c_abi, w.c_symbol_prefix.as_str()),
        (8, CAbi::X86Win, "_")
    );
    assert_eq!(w.coff_machine(), Some(0x14c));
    assert_eq!(w.runtime_lib(), "rokr.lib");
    let arm = t("armv7-unknown-linux-gnueabihf");
    assert_eq!(
        (arm.arch.as_str(), arm.env.as_str(), arm.c_abi),
        ("arm", "gnu", CAbi::ArmHf)
    );
    let mac = t("aarch64-apple-darwin");
    assert_eq!(mac.llvm_triple(), "arm64-apple-macosx11.0");
    assert_eq!(
        (mac.os.as_str(), mac.runtime_lib().as_str()),
        ("macos", "librokr.a")
    );
    let musl = t("aarch64-unknown-linux-musl");
    assert_eq!(
        (musl.llvm_target.as_str(), musl.libc),
        ("aarch64-unknown-linux-musl", Libc::Musl)
    );
    assert!(!musl.dynamic_linking && musl.native_backend);
    let b = t("wasm32-unknown-unknown");
    assert!(b.reactor() && b.profile_runtime.is_empty());
    assert_eq!((b.os.as_str(), b.family.as_str()), ("none", "wasm"));
    assert_eq!(t("wasm32-wasi").linker, Linker::WasmComponentLd);
    assert!(t("wasm32-wasi-threads").threads && !t("wasm32-wasi").threads);
    assert_eq!(t("s390x-unknown-linux-gnu").endian, "big");
}

/// A target's 0.2.0 name is an alias: the same spec, under the old name.
#[test]
fn new_names_are_aliases() {
    for (old, new) in RENAMED {
        assert!(BUILTIN.contains(old) && !BUILTIN.contains(new), "{old}");
        assert_eq!(t(new), t(old), "{new}");
        assert_eq!(TargetSpec::resolve(Some(new)).unwrap().name, *old);
    }
}
