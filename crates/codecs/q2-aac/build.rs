//! Compiles the vendored vo-aacenc C sources (vendor/vo-aacenc/UPSTREAM.json lists them).
//! The compiler and its target flags are whatever `cc` takes from the environment.

const SOURCES: &[&str] = &[
    "common/cmnMemory.c",
    "aacenc/basic_op/basicop2.c",
    "aacenc/basic_op/oper_32b.c",
    "aacenc/src/aac_rom.c",
    "aacenc/src/aacenc.c",
    "aacenc/src/aacenc_core.c",
    "aacenc/src/adj_thr.c",
    "aacenc/src/band_nrg.c",
    "aacenc/src/bit_cnt.c",
    "aacenc/src/bitbuffer.c",
    "aacenc/src/bitenc.c",
    "aacenc/src/block_switch.c",
    "aacenc/src/channel_map.c",
    "aacenc/src/dyn_bits.c",
    "aacenc/src/grp_data.c",
    "aacenc/src/interface.c",
    "aacenc/src/line_pe.c",
    "aacenc/src/memalign.c",
    "aacenc/src/ms_stereo.c",
    "aacenc/src/pre_echo_control.c",
    "aacenc/src/psy_configuration.c",
    "aacenc/src/psy_main.c",
    "aacenc/src/qc_main.c",
    "aacenc/src/quantize.c",
    "aacenc/src/sf_estim.c",
    "aacenc/src/spreading.c",
    "aacenc/src/stat_bits.c",
    "aacenc/src/transform.c",
];

/// Compiled with `-D__unused=`, as upstream's Makefile.am does for every file: tns.c
/// marks two variables with the BSD/Bionic `__unused` attribute macro. The definition
/// is kept to this one file, which includes no libc header, because libc headers may
/// use `__unused` as an identifier.
const TNS: &str = "aacenc/src/tns.c";

const INCLUDES: &[&str] = &["aacenc/inc", "aacenc/basic_op", "common/include"];

fn main() {
    let root = "vendor/vo-aacenc";
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={root}");
    let base = || {
        let mut build = cc::Build::new();
        build.opt_level(2).warnings(false);
        // The fixed-point code relies on wrapping signed addition in a few places (UBSan
        // reports tns.c and basic_op.h); GCC and Clang define it with this flag.
        build.flag_if_supported("-fwrapv");
        for dir in INCLUDES {
            build.include(format!("{root}/{dir}"));
        }
        build
    };
    let mut tns = base();
    tns.define("__unused", Some(""))
        .file(format!("{root}/{TNS}"));
    let mut build = base();
    // Upstream's spelling. With it voAACEncInit uses the malloc/free operators in
    // common/cmnMemory.c when the caller passes no memory operator.
    build.define("USE_DEAULT_MEM", None);
    for file in SOURCES {
        build.file(format!("{root}/{file}"));
    }
    for object in tns.compile_intermediates() {
        build.object(object);
    }
    build.compile("q2_vo_aacenc");
}
