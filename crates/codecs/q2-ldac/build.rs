//! Compiles the vendored AOSP libldac encoder into a static library. The
//! compiler, archiver and target flags are whatever `cc` takes from the
//! cargo and `CC_<target>` environment.

use std::path::Path;

fn main() {
    let root = Path::new("vendor/libldac");
    let src = root.join("src");
    let inc = root.join("inc");

    println!("cargo:rerun-if-changed=build.rs");
    for dir in [&src, &inc] {
        println!("cargo:rerun-if-changed={}", dir.display());
        for entry in std::fs::read_dir(dir).expect("vendored libldac directory") {
            let path = entry.expect("directory entry").path();
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let mut build = cc::Build::new();
    build
        // ldaclib.c and ldacBT.c #include every other .c file; upstream's
        // Android.bp lists the same two sources.
        .file(src.join("ldaclib.c"))
        .file(src.join("ldacBT.c"))
        .include(&inc)
        .include(&src)
        .opt_level(2)
        // The float path is full of a*b+c. GCC's default contraction turns
        // those into fused multiply-adds on targets that have them, and the
        // output then differs from the committed vectors.
        .flag_if_supported("-ffp-contract=off");
    if std::env::var_os("CARGO_FEATURE_FIXED_POINT").is_some() {
        build.define("_32BIT_FIXED_POINT", None);
    }
    build.compile("ldacBT_enc");

    // The float path calls sqrt() and fabs().
    if std::env::var_os("CARGO_CFG_UNIX").is_some() {
        println!("cargo:rustc-link-lib=m");
    }
}
