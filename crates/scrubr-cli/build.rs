//! Gives the program its icon on Windows: the one Explorer, the taskbar and shortcuts show.
//! `assets/scrubr.res` is a ready-made resource file, linked as it is, so the build needs no
//! resource compiler. `assets/make_icon.py` says how it is made.

fn main() {
    let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/scrubr.res");
    println!("cargo:rerun-if-changed={}", res.display());
    let is = |name: &str, value: &str| std::env::var(name).as_deref() == Ok(value);
    // Microsoft's linker takes a resource file as one more input. Other toolchains go without.
    if is("CARGO_CFG_TARGET_OS", "windows") && is("CARGO_CFG_TARGET_ENV", "msvc") {
        println!("cargo:rustc-link-arg-bins={}", res.display());
    }
}
