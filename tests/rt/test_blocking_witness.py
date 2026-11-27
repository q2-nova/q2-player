#!/usr/bin/env python3
"""`rt::Blocking` cannot be made by the code that wants to block.

Compiles crates/q2nd/src/rt/blocking.rs alone with rustc: each case must
compile or fail with its stated error, and the constructors must reject a
thread marked as an executor in a release build.

    python3 -m unittest discover -s tests/rt -v

Needs rustc only (the repository toolchain). Build output goes to
build/blocking-witness.
"""
import pathlib
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
BLOCKING_RS = ROOT / "crates/q2nd/src/rt/blocking.rs"
FIXTURE = pathlib.Path(__file__).resolve().parent / "witness_fixture.rs"
BUILD = ROOT / "build/blocking-witness"

RELEASE_CONTROL = r'''
#![allow(dead_code)]
mod rt {
    #[path = "BLOCKING_RS"] pub mod blocking;
    pub fn reject_executor() {
        let _executor = blocking::enter_executor();
        let _ = blocking::Blocking::CONSTRUCTOR();
    }
    pub fn accepts_owners() {
        let _ordinary = blocking::Blocking::dedicated_thread();
        let _pool = blocking::enter_pool();
        let _worker = blocking::Blocking::new();
    }
}
fn main() {
    assert!(!cfg!(debug_assertions));
    std::panic::set_hook(Box::new(|_| {}));
    rt::accepts_owners();
    let rejected = std::panic::catch_unwind(rt::reject_executor).is_err();
    assert!(!rt::blocking::on_executor(), "executor scope did not unwind");
    rt::accepts_owners();
    if !rejected { std::process::exit(23); }
}
'''


def rustc(*args):
    # cwd=ROOT so rustup selects the toolchain in rust-toolchain.toml.
    return subprocess.run(["rustc", "--edition", "2024", *args], capture_output=True, text=True, timeout=60, cwd=ROOT)


class Witness(unittest.TestCase):
    def release_constructor_control(self, constructor):
        """Run the token implementation with debug assertions off."""
        BUILD.mkdir(parents=True, exist_ok=True)
        source = RELEASE_CONTROL.replace("BLOCKING_RS", str(BLOCKING_RS)).replace("CONSTRUCTOR", constructor)
        with tempfile.TemporaryDirectory(dir=BUILD) as directory:
            path = pathlib.Path(directory) / "release_witness.rs"
            binary = pathlib.Path(directory) / "release_witness"
            path.write_text(source)
            compiled = rustc("-C", "opt-level=3", "-C", "debug-assertions=no", "-C", "panic=unwind",
                             "-o", str(binary), str(path))
            self.assertEqual(compiled.returncode, 0, compiled.stderr)
            result = subprocess.run([str(binary)], capture_output=True, text=True, timeout=5)
            self.assertEqual(result.returncode, 0,
                             f"{constructor} failed release executor rejection: {result.returncode}: {result.stderr}")

    def test_private_constructor_rejects_executor_in_release(self):
        self.release_constructor_control("new")

    def test_dedicated_constructor_rejects_executor_in_release(self):
        self.release_constructor_control("dedicated_thread")

    def compile(self, case):
        BUILD.mkdir(parents=True, exist_ok=True)
        source = FIXTURE.read_text().replace("BLOCKING_RS", str(BLOCKING_RS)).replace("// CASE", case)
        with tempfile.TemporaryDirectory(dir=BUILD) as directory:
            path = pathlib.Path(directory) / "fixture.rs"
            path.write_text(source)
            return rustc("--crate-type", "lib", "--emit=metadata", "--out-dir", directory, str(path))

    def test_a_pool_closure_receives_the_witness(self):
        result = self.compile("rt::on_the_pool(|blocking| write_to_the_card(blocking));")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_witness_cannot_escape_as_a_pool_result(self):
        result = self.compile("let _ = rt::pool_output(|_| Blocking::dedicated_thread());")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("E0277", result.stderr)
        self.assertIn("Send", result.stderr)

    def test_a_borrowed_witness_cannot_be_shared_with_another_thread(self):
        result = self.compile("rt::on_the_pool(|blocking| std::thread::scope(|scope| { scope.spawn(move || write_to_the_card(blocking)); }));")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("E0277", result.stderr)
        self.assertIn("Sync", result.stderr)

    def test_the_thread_bound_witness_remains_zero_sized(self):
        result = self.compile("let _: [(); 0] = [(); std::mem::size_of::<Blocking>()];")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_forged_witness_does_not_compile(self):
        result = self.compile("write_to_the_card(&Blocking { _private: Default::default() });")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("E0451", result.stderr)

    def test_the_constructor_is_private_to_rt(self):
        result = self.compile("write_to_the_card(&Blocking::new());")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("E0624", result.stderr)

    def test_calling_without_a_witness_does_not_compile(self):
        result = self.compile("write_to_the_card();")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("E0061", result.stderr)


if __name__ == "__main__":
    unittest.main()
