import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]


class BuildSidecarIdentityTest(unittest.TestCase):
    def test_gateway_lookup_uses_desktop_target_when_architectures_coexist(self):
        rustc = os.environ.get("RUSTC") or shutil.which("rustc")
        self.assertIsNotNone(rustc, "controlled test PATH must provide rustc")
        source = (ROOT / "desktop/src-tauri/src/runtime/proxy_lifecycle/binary.rs").read_text()
        lookup = source.split("pub(crate) fn gateway_bin_path<", 1)[0]
        lookup += "pub(crate) fn gateway_bin_path_from" + source.split(
            "pub(crate) fn gateway_bin_path_from", 1
        )[1]
        harness = r'''
use std::path::{Path, PathBuf};
fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap());
    let staged_dir = root.join("desktop/src-tauri/binaries");
    let debug_dir = root.join("desktop/gateway/target/debug");
    std::fs::create_dir_all(&staged_dir).unwrap();
    std::fs::create_dir_all(&debug_dir).unwrap();
    let wrong_target = if env!("CSSWITCH_BUILD_TARGET") == "x86_64-apple-darwin" {
        "aarch64-apple-darwin"
    } else { "x86_64-apple-darwin" };
    let wrong = staged_dir.join(format!("csswitch-gateway-{wrong_target}"));
    std::fs::write(&wrong, b"wrong").unwrap();
    assert_eq!(find_gateway_in(&staged_dir), None);
    let staged = staged_dir.join(format!("csswitch-gateway-{}", env!("CSSWITCH_BUILD_TARGET")));
    std::fs::write(&staged, b"current").unwrap();
    std::fs::write(debug_dir.join("csswitch-gateway"), b"old standalone").unwrap();
    assert_eq!(find_gateway_in(&staged_dir), Some(staged.clone()));
    assert_eq!(gateway_bin_path_from(None, None, None, Some(root)), Some(staged));
    let plain = staged_dir.join("csswitch-gateway");
    std::fs::write(&plain, b"packaged").unwrap();
    assert_eq!(find_gateway_in(&staged_dir), Some(plain));
}
'''
        with tempfile.TemporaryDirectory(prefix="csswitch-target-lookup-") as raw:
            temporary = Path(raw)
            rust_file = temporary / "lookup.rs"
            rust_file.write_text(lookup + harness)
            for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
                with self.subTest(target=target):
                    executable = temporary / target
                    subprocess.run(
                        [rustc, "--edition=2021", str(rust_file), "-o", str(executable)],
                        env={**os.environ, "CSSWITCH_BUILD_TARGET": target},
                        check=True, capture_output=True,
                    )
                    subprocess.run([str(executable), str(temporary / f"fixture-{target}")],
                                   check=True, capture_output=True)

    def test_external_target_dir_cannot_stage_stale_default_gateway(self):
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
            with self.subTest(target=target):
                self.check_sidecar_staging(target)

    def check_sidecar_staging(self, target):
        rustc = os.environ.get("RUSTC") or shutil.which("rustc")
        self.assertIsNotNone(rustc, "controlled test PATH must provide rustc")
        with tempfile.TemporaryDirectory(prefix="csswitch-build-sidecar-") as raw:
            temp = Path(raw)
            manifest_dir = temp / "desktop" / "src-tauri"
            gateway_dir = temp / "desktop" / "gateway"
            manifest_dir.mkdir(parents=True)
            (gateway_dir / "src").mkdir(parents=True)
            (gateway_dir / "Cargo.toml").write_text(
                "[package]\nname='fake-gateway'\nversion='0.0.0'\n",
                encoding="utf-8",
            )

            stub = temp / "tauri_build.rs"
            stub.write_text("pub fn build() {}\n", encoding="utf-8")
            stub_rlib = temp / "libtauri_build.rlib"
            subprocess.run(
                [
                    rustc,
                    "--edition=2021",
                    "--crate-name",
                    "tauri_build",
                    "--crate-type",
                    "rlib",
                    str(stub),
                    "-o",
                    str(stub_rlib),
                ],
                check=True,
                cwd=ROOT,
                capture_output=True,
            )
            build_script = temp / "desktop-build-script"
            subprocess.run(
                [
                    rustc,
                    "--edition=2021",
                    str(ROOT / "desktop" / "src-tauri" / "build.rs"),
                    "--extern",
                    f"tauri_build={stub_rlib}",
                    "-o",
                    str(build_script),
                ],
                check=True,
                cwd=ROOT,
                capture_output=True,
            )

            current_bytes = b"CURRENT-NESTED-GATEWAY\n"
            decoy_bytes = b"STALE-DEFAULT-GATEWAY\n"
            default_binary = (
                gateway_dir
                / "target"
                / target
                / "release"
                / "csswitch-gateway"
            )
            default_binary.parent.mkdir(parents=True)
            default_binary.write_bytes(decoy_bytes)

            fake_cargo = temp / "fake-cargo"
            fake_cargo.write_text(
                """#!/usr/bin/python3
import os
from pathlib import Path
import sys

args = sys.argv[1:]
assert "--locked" in args, "nested build must use the checked-in lockfile"
target = args[args.index("--target") + 1]
target_root = Path(os.environ["CARGO_TARGET_DIR"])
binary = target_root / target / "release" / "csswitch-gateway"
binary.parent.mkdir(parents=True, exist_ok=True)
binary.write_bytes(b"CURRENT-NESTED-GATEWAY\\n")
binary.chmod(0o700)
""",
                encoding="utf-8",
            )
            fake_cargo.chmod(
                fake_cargo.stat().st_mode
                | stat.S_IXUSR
                | stat.S_IXGRP
                | stat.S_IXOTH
            )

            out_dir = temp / "desktop-out"
            external_target = temp / "external-parent-target"
            env = os.environ.copy()
            env.update(
                {
                    "CARGO": str(fake_cargo),
                    "CARGO_MANIFEST_DIR": str(manifest_dir),
                    "CARGO_TARGET_DIR": str(external_target),
                    "OUT_DIR": str(out_dir),
                    "TARGET": target,
                }
            )
            result = subprocess.run(
                [str(build_script)],
                check=True,
                cwd=manifest_dir,
                env=env,
                capture_output=True,
            )
            self.assertIn(
                f"cargo:rustc-env=CSSWITCH_BUILD_TARGET={target}",
                result.stdout.decode(),
            )

            built = (
                out_dir
                / "gateway-target"
                / target
                / "release"
                / "csswitch-gateway"
            )
            staged = (
                manifest_dir
                / "binaries"
                / f"csswitch-gateway-{target}"
            )
            self.assertTrue(built.is_file(), "this Desktop build must own one nested target root")
            self.assertEqual(built.read_bytes(), current_bytes)
            self.assertEqual(staged.read_bytes(), current_bytes)
            self.assertNotEqual(staged.read_bytes(), decoy_bytes)
            self.assertEqual(default_binary.read_bytes(), decoy_bytes)


if __name__ == "__main__":
    unittest.main()
