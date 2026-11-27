// Compiles the vendored libopenaptx 0.2.0 C source into a static library.
// The compiler and target flags come from `cc` (CC_<target>, CFLAGS_<target>).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/openaptx/openaptx.c");
    println!("cargo:rerun-if-changed=vendor/openaptx/openaptx.h");

    cc::Build::new()
        .file("vendor/openaptx/openaptx.c")
        // openaptx.c includes its header as <openaptx.h>.
        .include("vendor/openaptx")
        .opt_level(2)
        .warnings(false)
        .compile("q2openaptx");
}
