// The browser About page uses the version of the native player it represents.
fn main() {
    const MANIFEST: &str = "../q2nd/Cargo.toml";
    println!("cargo:rerun-if-changed={MANIFEST}");
    let manifest = std::fs::read_to_string(MANIFEST).expect("native player manifest");
    let package = manifest.lines().map(str::trim)
        .skip_while(|line| *line != "[package]").skip(1)
        .take_while(|line| !line.starts_with('['));
    let version = package
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim() == "version")
        .and_then(|(_, value)| value.trim().strip_prefix('"')?.strip_suffix('"'))
        .expect("string package version in the native player manifest");
    println!("cargo:rustc-env=Q2_NATIVE_APP_VERSION={version}");
}
