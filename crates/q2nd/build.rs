fn main() {
    println!("cargo:rerun-if-changed=vendor/diagnostics/stall.c");
    println!("cargo:rerun-if-env-changed=Q2_UNWIND_PREFIX");
    println!("cargo:rustc-check-cfg=cfg(q2_stall_stacks)");
    // The player's own C (the hand-written MSA EQ and PCM leaves, sonic, the
    // stall sampler) is compiled by the SDK's GCC whatever compiler the
    // other crates' C code gets (Q2_X2000_C). With the X2000 clang the EQ
    // benchmark took 1.30 and the PCM conversion 1.06 of the GCC build's
    // time on the Q2 (2026-11-21): the leaves are MSA intrinsics, and GCC
    // selects better code for them.
    println!("cargo:rerun-if-env-changed=Q2_OS_CC");
    let q2_cc = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("mips") { std::env::var_os("Q2_OS_CC") } else { None };
    let build = || { let mut b = cc::Build::new(); if let Some(cc) = &q2_cc { b.compiler(cc); } b };
    // The stall sampler is an emulator diagnostic (feature `stall-sampler`).
    // Without it no sampler code is compiled and no unwinder is linked; an
    // unwinder prefix then names a build that cannot use it, so refuse it.
    let sampler = std::env::var_os("CARGO_FEATURE_STALL_SAMPLER").is_some();
    let unwinder = std::env::var_os("Q2_UNWIND_PREFIX");
    if !sampler {
        assert!(unwinder.is_none(),
            "Q2_UNWIND_PREFIX requires the stall-sampler feature (emulator diagnostics only); unset it for this build");
    } else {
        let mut diagnostics = build();
        diagnostics.file("vendor/diagnostics/stall.c").flag("-std=c11")
            .flag_if_supported("-fasynchronous-unwind-tables").opt_level(2);
        if let Some(prefix) = &unwinder {
            let prefix = std::path::PathBuf::from(prefix);
            assert!(prefix.join("include/libunwind.h").is_file(), "qualified unwinder headers required");
            assert!(prefix.join("lib/libunwind.a").is_file(), "qualified unwinder library required");
            diagnostics.include(prefix.join("include")).define("Q2_HAVE_UNWIND", None);
            println!("cargo:rustc-link-search=native={}", prefix.join("lib").display());
            println!("cargo:rustc-cfg=q2_stall_stacks");
        }
        diagnostics.compile("q2stall");
        if unwinder.is_some() { println!("cargo:rustc-link-lib=static=unwind"); }
    }
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-check-cfg=cfg(q2_eq_msa)");
    println!("cargo:rustc-check-cfg=cfg(q2_mp3_profile)");
    println!("cargo:rustc-check-cfg=cfg(q2_flac_profile)");
    println!("cargo:rustc-check-cfg=cfg(q2_alac_profile)");
    println!("cargo:rustc-check-cfg=cfg(q2_pcm_scalar)");
    println!("cargo:rustc-check-cfg=cfg(q2_gfx_scalar)");
    println!("cargo:rustc-check-cfg=cfg(q2_unlock_unwind_tables)");
    println!("cargo:rerun-if-changed=vendor/eq/stereo.c");
    let mut eq = build();
    eq.file("vendor/eq/stereo.c").opt_level(3).flag("-ffp-contract=off")
        .flag("-fno-tree-vectorize").flag("-fno-tree-slp-vectorize");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("mips") {
        assert_eq!(std::env::var("CARGO_CFG_TARGET_ENDIAN").as_deref(), Ok("little"));
        eq.flag("-mabi=32").flag("-march=mips32r5").flag("-mhard-float")
            .flag("-mfp64").flag("-mnan=2008").flag("-mabs=2008").flag("-mmsa");
        // MSA-only PCM conversion kernels; other targets use the scalar Rust.
        println!("cargo:rerun-if-changed=vendor/pcm/convert.c");
        eq.file("vendor/pcm/convert.c");
        println!("cargo:rustc-cfg=q2_eq_msa");
        // The --bench and --script code in its own section, which
        // platform/resident.rs does not lock (GNU ld, link/q2tools.ld).
        println!("cargo:rerun-if-changed=link/q2tools.ld");
        let script = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("link/q2tools.ld");
        println!("cargo:rustc-link-arg-bins=-Wl,-T,{}", script.display());
    }
    eq.compile("q2eq");
    println!("cargo:rerun-if-changed=vendor/sonic/sonic.c");
    println!("cargo:rerun-if-changed=vendor/sonic/sonic.h");
    build()
        .file("vendor/sonic/sonic.c")
        .include("vendor/sonic")
        .flag_if_supported("-Wno-unused-parameter")
        .opt_level(2)
        .compile("q2sonic");
}
